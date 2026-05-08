import Foundation

/// Mirrors `nestty-core::action_registry::ActionRegistry` for macOS. Two seams
/// it owns:
///
/// - **Dispatch** (`register` / `tryDispatch`) — `AppDelegate.handleCommand`
///   tries the registry first and falls through to the legacy hardcoded
///   switch on miss, so plugin-provided + first-party actions reach the
///   socket dispatcher through one path.
/// - **Completion fan-out** (PR 7 / parity-plan Tier 14.1 chain enabler) —
///   when an `EventBus` is wired via `setEventBus(_:)`, every dispatched
///   action auto-broadcasts `<method>.completed` (success) or
///   `<method>.failed` (`RPCError`) on the bus BEFORE the caller's
///   completion fires. Triggers that condition on `<action>.completed`
///   (e.g. `git.worktree_add.completed → claude.start` in
///   `examples/triggers/vision-flow-3.toml`) compose against the same
///   contract as Linux. Chokepoint matters: Linux centralizes fan-out in
///   the registry, NOT the supervisor, so we do the same — plugin RPC
///   replies funnel through `tryDispatch` because `PluginSupervisor`
///   registers each `provides[]` entry as a registry handler that calls
///   `proc.invoke(..., completion:)`. The reader thread later resolves
///   that completion, which goes through the wrapper here.
///
/// Mirrors Linux's "registered actions only" invariant (`with_completion_bus`
/// in `nestty-core/src/action_registry.rs`): legacy match-arm dispatch in
/// `AppDelegate.handleCommand` (`webview.*`, `terminal.*`, `tab.*`, etc.)
/// bypasses the registry and therefore does NOT emit completion events.
/// Bringing those into the registry is a separate parity item — until
/// then, completion-event chains are limited to plugin actions plus the
/// nestty-internal actions that landed here (`system.*`, `context.*` once
/// it ships).
///
/// Concurrency model is `@MainActor` because every call site runs on the
/// main thread already (the socket server marshals into
/// `DispatchQueue.main.async` before invoking `commandHandler`). The
/// completion *wrapper* may fire on any thread (plugin reader thread for
/// plugin RPC replies — see PR 3 deadlock-fix), but `EventBus.broadcast`
/// is `@unchecked Sendable` with internal `NSLock`, and the Rust engine
/// reached via `eventBus.onBroadcast` has its own `RwLock`, so any-thread
/// firing is safe end-to-end.
///
/// **Deferred from Linux's full surface:**
///
/// - `register_blocking` — codex round-2 flagged that macOS's socket
///   dispatch already pins the socket thread on a `DispatchSemaphore`
///   waiting for the main-actor completion to fire, so a long-running
///   handler blocks one client at a time but doesn't pin the main thread.
///   `register_blocking` would spawn a worker per dispatch (Linux
///   semantics), which conflicts with this model and risks deadlock once
///   trigger-driven re-entry shows up. Defer until the async boundary
///   redesign.
///
/// - `invoke` / `try_invoke` (sync) — only the trigger engine needs sync
///   invoke; macOS's engine path is `tryDispatch` from the `@convention(c)`
///   trampoline, which already returns a value from the wrapped
///   completion synchronously.
@MainActor
final class ActionRegistry {
    /// Action handler. Receives the parsed `params` dict and a `completion`
    /// closure that MUST be called exactly once with either:
    ///
    /// - The success result (any JSON-serializable Swift value, typically
    ///   `[String: Any]`), OR
    /// - An `RPCError` instance to surface a JSON-RPC error envelope (same
    ///   path used by webview commands today).
    ///
    /// Calling `completion` more than once is a programmer error and will
    /// race with the socket-server semaphore signal — assertion in debug
    /// builds, undefined behavior in release. Calling it zero times will
    /// hang the calling client forever.
    typealias Handler = (_ params: [String: Any], _ completion: @escaping (Any?) -> Void) -> Void

    private struct Entry {
        let handler: Handler
        /// Skip auto-broadcast of `<name>.completed` / `<name>.failed`.
        /// True for high-frequency introspection actions (`system.ping`,
        /// `system.list_actions`, future `context.snapshot`) where the
        /// resulting bus traffic would dwarf actual workflow events.
        /// Mirrors Linux's `silent` flag on `Entry`.
        let silent: Bool
    }

    private var entries: [String: Entry] = [:]

    /// EventBus for completion fan-out. nil until `setEventBus(_:)` runs —
    /// `AppDelegate` wires it during `applicationDidFinishLaunching`. Tests
    /// and unit-test contexts that don't care about chained triggers can
    /// skip wiring; the wrapper short-circuits with no-op when nil.
    private weak var eventBus: EventBus?

    /// Wire the registry to a bus so dispatched actions auto-broadcast
    /// completion/failure events. Idempotent; last writer wins. Must run
    /// before any non-silent action gets dispatched if you want the very
    /// first dispatch to fan out (otherwise it's silently dropped).
    func setEventBus(_ bus: EventBus) {
        eventBus = bus
    }

    /// Register a handler under `name`. Replaces any existing handler with
    /// the same name (last writer wins — same as Linux). Plugins that bind
    /// to a name owned by another plugin will silently overwrite; the
    /// supervisor's `resolve_provides` step on Linux is where conflict
    /// detection lives. We don't have a supervisor-level conflict resolver
    /// yet on macOS, so today only first-party `system.*` actions plus
    /// plugins register here.
    func register(_ name: String, handler: @escaping Handler) {
        entries[name] = Entry(handler: handler, silent: false)
    }

    /// Like `register` but suppresses completion-event broadcast. Use for
    /// pure-introspection actions that are polled by tooling
    /// (`system.list_actions` from a periodic UI refresh, `system.ping`
    /// from health checks) where every dispatch firing
    /// `<name>.completed` would flood the bus without surfacing a real
    /// workflow step. Matches the semantics of Linux's
    /// `register_silent` / `Entry { silent: true }`.
    func registerSilent(_ name: String, handler: @escaping Handler) {
        entries[name] = Entry(handler: handler, silent: true)
    }

    /// Try to dispatch `method`. If a handler is registered, call it with
    /// `params` and `completion`, and return `true`. If not registered,
    /// return `false` WITHOUT touching `completion` — the caller owns the
    /// fall-through path (typically `AppDelegate.handleCommand`'s legacy
    /// switch). Mirrors Linux `try_dispatch`'s bool semantics so the call
    /// site can compose the same way:
    ///
    /// ```swift
    /// if registry.tryDispatch(method, params: params, completion: completion) { return }
    /// // … fall through to hardcoded handlers …
    /// ```
    ///
    /// On dispatch the handler's completion is wrapped: when it fires with
    /// a success value, `<method>.completed` is broadcast on the
    /// `EventBus` with the value as payload (or `["value": value]` if not
    /// already a dict — keeps trigger interpolation `{event.X}` working
    /// for all return shapes). When it fires with an `RPCError`,
    /// `<method>.failed` is broadcast with `{code, message}`. Broadcast
    /// happens BEFORE the original completion fires so a chained trigger
    /// observing `.completed` runs in the same logical tick as the
    /// originating action's response.
    @discardableResult
    func tryDispatch(
        _ method: String,
        params: [String: Any],
        completion: @escaping (Any?) -> Void,
    ) -> Bool {
        guard let entry = entries[method] else { return false }
        let bus = entry.silent ? nil : eventBus
        let wrapped: (Any?) -> Void = { [weak bus] value in
            if let bus {
                Self.publishCompletion(bus: bus, method: method, value: value)
            }
            completion(value)
        }
        entry.handler(params, wrapped)
        return true
    }

    /// True if a handler is registered under `name`. Useful for diagnostics
    /// and for `system.list_actions` introspection.
    func has(_ name: String) -> Bool {
        entries[name] != nil
    }

    /// All registered action names, sorted alphabetically. Sort is stable
    /// across calls so consumers (e.g. `nestctl call system.list_actions`)
    /// can diff successive snapshots without re-sorting.
    func names() -> [String] {
        entries.keys.sorted()
    }

    var count: Int {
        entries.count
    }

    /// Translate a completed dispatch into a bus event. Mirrors Linux's
    /// `publish_completion` (`nestty-core/src/action_registry.rs:281`):
    /// `<action>.completed` on success, `<action>.failed` on `RPCError`.
    ///
    /// Payload normalization: the bus contract is `[String: Any]`. When
    /// the handler returns a dict, pass through. When it returns a
    /// non-dict (string, array, number, nil), wrap as `["value": result]`
    /// so trigger templates can still address it as `{event.value}` and
    /// engine-side serde sees a JSON object. RPCError serializes to
    /// `{code, message}` — same shape Linux ships.
    ///
    /// Source stamp: `COMPLETION_EVENT_SOURCE` ("nestty.action") — Linux's
    /// trigger engine gates await-chain promotion on this exact value
    /// (`nestty_core::trigger::try_promote_or_drop_preflight`). Anything
    /// else stalls await chains silently, so this string is part of the
    /// contract — must stay in sync with the constant in
    /// `nestty-core/src/action_registry.rs`.
    ///
    /// `nonisolated` because the wrapped completion in `tryDispatch` may
    /// fire from any thread (plugin reader thread per PR 3 deadlock-fix).
    /// EventBus.broadcast is `@unchecked Sendable` (NSLock-internal), so
    /// the off-main call is safe — the explicit modifier prevents Swift 6
    /// strict-concurrency from inferring main-actor isolation here, which
    /// would violate the contract at the first plugin RPC reply.
    private nonisolated static func publishCompletion(bus: EventBus, method: String, value: Any?) {
        let kind: String
        let data: [String: Any]
        if let err = value as? RPCError {
            kind = "\(method).failed"
            data = ["code": err.code, "message": err.message]
        } else {
            kind = "\(method).completed"
            switch value {
            case let dict as [String: Any]:
                data = dict
            case .none:
                data = [:]
            case let .some(other):
                data = ["value": other]
            }
        }
        bus.broadcast(event: kind, source: completionEventSource, data: data)
    }

    /// Mirror of `nestty_core::action_registry::COMPLETION_EVENT_SOURCE`.
    /// Hand-kept in sync — when Linux changes the constant, update here
    /// too. The string is the trust-boundary marker for await promotion.
    /// `nonisolated` so `publishCompletion` (also nonisolated) can read
    /// it from off-main threads without crossing the main-actor barrier.
    nonisolated static let completionEventSource = "nestty.action"
}
