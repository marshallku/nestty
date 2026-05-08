import Foundation

/// PR 9 / parity-plan Tier 2.2 — Swift mirror of `nestty_core::context::ContextService`.
/// Tracks the user's currently-focused panel + that panel's cwd so trigger
/// interpolation `{context.active_panel}` / `{context.active_cwd}` resolves
/// to live values on macOS the way it already does on Linux.
///
/// Wire-shape parity with Linux's `Context` struct (`nestty-core/src/context.rs`):
/// `snapshot()` returns `["active_panel": String?, "active_cwd": String?]` —
/// no extra fields. `active_cwd` is *derived* from `panelCwds[activePanel]`,
/// not stored; that means a `terminal.cwd_changed` for a non-active panel
/// caches the value silently and surfaces only when that panel becomes
/// active. Same semantics as Linux.
///
/// Update rules (mirror of `apply_event` in `nestty-core/src/context.rs:71`):
/// - `panel.focused` (payload `panel_id`) → set `activePanel`
/// - `panel.exited` (payload `panel_id`) → remove from cwd cache; if it
///   matched `activePanel`, null that out too
/// - `terminal.cwd_changed` (payload `panel_id`, `cwd`) → cache cwd
///   keyed by `panel_id`
///
/// **Apply-before-dispatch ordering** (codex pressure-test finding): macOS's
/// `EventBus.onBroadcast` fires synchronously per broadcast, BEFORE channel
/// fan-out. If `ContextService` were itself an `EventBus` subscriber, a
/// `panel.focused` trigger condition checking `{context.active_panel}` would
/// resolve to the *previous* panel because the channel hasn't been read yet.
/// Solution: `AppDelegate.onBroadcast` calls `contextService.apply` BEFORE
/// `nesttyEngine.dispatchEvent`, taking the post-apply snapshot to pass through
/// FFI. This mirrors Linux's `Pump::pump_all` (`nestty-linux/src/window.rs:589`)
/// which explicitly "drain context first, then dispatch."
///
/// **No timer.** Linux uses a 100ms GTK timer to drain bounded event-bus
/// channels into ContextService — that's a Linux bus constraint. macOS's
/// `onBroadcast` already fires synchronously per event; polling would be
/// pure cost with no semantic benefit.
///
/// Concurrency: `@unchecked Sendable` + internal NSLock. `onBroadcast` may
/// fire from any thread (plugin reader thread, main, etc.); both `apply` and
/// `snapshot` need to be safe from anywhere. Same posture as `EventBus.swift`.
final class ContextService: @unchecked Sendable {
    private let lock = NSLock()
    private var activePanel: String?
    private var panelCwds: [String: String] = [:]

    /// Apply one bus event to the context. Idempotent — replaying the same
    /// event sequence yields the same final state. Unknown event kinds are
    /// silently ignored, matching Linux's `_ => {}` arm.
    func apply(eventKind: String, data: [String: Any]) {
        switch eventKind {
        case "panel.focused":
            guard let id = data["panel_id"] as? String, !id.isEmpty else { return }
            lock.withLock { activePanel = id }
        case "panel.exited":
            guard let id = data["panel_id"] as? String, !id.isEmpty else { return }
            lock.withLock {
                panelCwds.removeValue(forKey: id)
                if activePanel == id {
                    activePanel = nil
                }
            }
        case "terminal.cwd_changed":
            guard let id = data["panel_id"] as? String, !id.isEmpty,
                  let cwd = data["cwd"] as? String, !cwd.isEmpty
            else { return }
            lock.withLock { panelCwds[id] = cwd }
        default:
            return
        }
    }

    /// Point-in-time snapshot for trigger interpolation + the
    /// `context.snapshot` socket command. Both fields are `Any?` —
    /// `NSNull()` would round-trip wrong through the FFI JSON path; we
    /// just include them as `String` when present, omit when nil. Linux's
    /// `Context` serializes nil as JSON `null`, but `[String: Any]` in
    /// Swift can't carry a nil; consumers that JSON-encode this dict get
    /// the same wire shape as Linux because missing keys round-trip to
    /// `null` in serde.
    func snapshot() -> [String: Any] {
        lock.lock()
        defer { lock.unlock() }
        var out: [String: Any] = [:]
        if let panel = activePanel {
            out["active_panel"] = panel
            // `active_cwd` is derived: cwd of the active panel, if cached.
            if let cwd = panelCwds[panel] {
                out["active_cwd"] = cwd
            }
        }
        return out
    }
}

private extension NSLock {
    @discardableResult
    func withLock<T>(_ body: () -> T) -> T {
        lock(); defer { unlock() }
        return body()
    }
}
