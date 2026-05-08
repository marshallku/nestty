import CNesttyFFI
import Foundation

/// Thin Swift facade over the nestty-ffi C-ABI staticlib.
///
/// PR 1 (Tier 2.1 spike) scope: prove the build/link path end-to-end with the
/// smallest possible call surface. PR 2+ (registry seam, supervisor, trigger
/// engine) will grow this file or split it per concern; for now everything
/// the Rust side exposes lands here so Swift callers don't poke at C pointers
/// directly.
///
/// Memory model:
/// - `version()` returns a Swift String copy; no Rust allocation involved.
/// - `callJSON(_:)` round-trips JSON. The C call returns a heap-allocated C
///   string that this facade copies into a Swift String and immediately
///   frees via `nestty_ffi_free_string`. Callers never see a raw pointer.
/// - `lastError()` reads a thread-local slot in Rust. The string is borrowed
///   from Rust and copied to Swift on read; the Rust slot stays owned by Rust.
enum NesttyFFI {
    /// Static version string baked into the Rust crate. Cheap, no allocation.
    static func version() -> String {
        // nestty_ffi_version() returns a static C string we don't own.
        guard let cstr = nestty_ffi_version() else { return "<null>" }
        return String(cString: cstr)
    }

    /// Round-trip a JSON document through the Rust crate. The Rust side adds
    /// an `echoed_at` field with a unix-epoch-millis timestamp so callers can
    /// confirm the value came from Rust (not a Swift-side passthrough).
    ///
    /// On success returns the parsed JSON object.
    /// On failure returns nil and writes the FFI error to stderr.
    static func callJSON(_ payload: [String: Any]) -> [String: Any]? {
        guard let inputData = try? JSONSerialization.data(withJSONObject: payload),
              let inputStr = String(data: inputData, encoding: .utf8)
        else {
            FileHandle.standardError.write(Data("[nestty-ffi] failed to serialize input JSON\n".utf8))
            return nil
        }

        return inputStr.withCString { (cstr: UnsafePointer<CChar>) -> [String: Any]? in
            guard let resultPtr = nestty_ffi_call_json(cstr) else {
                let err = lastError() ?? "<no error message>"
                FileHandle.standardError.write(Data("[nestty-ffi] call_json returned NULL: \(err)\n".utf8))
                return nil
            }
            // Important: copy to Swift String first, then free. If we passed
            // resultPtr to JSONSerialization directly via a Data wrapper we'd
            // have to manage the lifetime around the JSON parse, and Swift's
            // Data(bytesNoCopy:) is easy to misuse. Copy-then-free is boring
            // and obviously correct.
            defer { nestty_ffi_free_string(resultPtr) }
            let resultStr = String(cString: resultPtr)
            guard let resultData = resultStr.data(using: .utf8),
                  let parsed = try? JSONSerialization.jsonObject(with: resultData) as? [String: Any]
            else {
                FileHandle.standardError.write(Data("[nestty-ffi] failed to parse FFI response: \(resultStr)\n".utf8))
                return nil
            }
            return parsed
        }
    }

    /// Most recent error from the calling thread, copied into Swift.
    /// Returns nil if no error has been recorded since the last successful call.
    static func lastError() -> String? {
        guard let cstr = nestty_ffi_last_error() else { return nil }
        return String(cString: cstr)
    }

    /// Smoke-test the FFI bridge. Called once during app launch (PR 1 spike)
    /// to confirm the staticlib linked and basic round-trip works. Logs to
    /// stderr; doesn't crash the app on failure (the FFI is non-load-bearing
    /// at this stage — Tier 2.4 will replace this with real engine startup).
    static func runSmokeTest() {
        FileHandle.standardError.write(Data("[nestty-ffi] version = \(version())\n".utf8))
        if let echo = callJSON(["hello": "from swift", "spike": true]) {
            FileHandle.standardError.write(Data("[nestty-ffi] echo round-trip = \(echo)\n".utf8))
        }
    }
}

// MARK: - NesttyEngine (PR 5c)

/// Swift wrapper around the Rust `TriggerEngine` exposed via nestty-ffi.
///
/// Lifecycle:
/// - `init()` creates the Rust engine + retains `self` so the C action
///   callback can safely cast `user_data` back to a NesttyEngine instance.
/// - `setTriggers([...])` JSON-encodes and forwards. Hot-reload safe —
///   engine swaps the trigger list atomically.
/// - `dispatchEvent(kind:, payload:)` enters the engine; engine matches
///   triggers and fires the C callback synchronously for each match.
/// - `shutdown()` clears the callback slot, destroys the handle, releases
///   `self`. After shutdown the instance must NOT be used again.
///
/// Threading:
/// - `dispatchEvent` runs on the calling thread (main actor in our use).
///   The C action callback consequently fires inline on the main actor too,
///   which is fine because it just records into a thread-safe Swift slot
///   and dispatches to the registry asynchronously.
/// - We deliberately do NOT pin the engine to a serial DispatchQueue here:
///   the Rust `TriggerEngine` uses `RwLock` internally so concurrent
///   dispatches are safe; pin-to-serial would only be necessary if we
///   needed strict ordering between dispatch + setTriggers, which our
///   AppDelegate-only call sites don't.
/// Not `@MainActor` because EventBus.broadcast fires from any thread (plugin
/// reader threads forward `event.publish` from there). The Rust engine itself
/// is thread-safe (internal `RwLock`), so dispatching from any thread is fine
/// at the Rust layer; the C action callback hops into main actor before
/// touching `ActionRegistry`. `@unchecked Sendable` because the OpaquePointer
/// handle isn't Sendable per se but ownership stays single-instance.
final class NesttyEngine: @unchecked Sendable {
    private var handle: OpaquePointer?
    /// Captured at init so the action callback (which has no Swift closure
    /// context) can find its way back to ActionRegistry. Set ONCE by
    /// AppDelegate immediately after construction; read from the C callback
    /// thread. `nonisolated(unsafe)` is the Swift 6 escape hatch for the
    /// "set once, read many" pattern that doesn't need a lock.
    nonisolated(unsafe) var actionRegistry: ActionRegistry?

    init() {
        // nestty_engine_create returns OpaquePointer? directly (Swift's clang
        // importer maps the forward-declared C struct that way). No cast.
        guard let h = nestty_engine_create() else {
            FileHandle.standardError.write(Data("[nestty-engine] nestty_engine_create returned NULL\n".utf8))
            return
        }
        handle = h

        // Register the static C trampoline. user_data is a retained Swift
        // pointer to self — paired with `Unmanaged.passRetained` on init
        // and `takeRetainedValue` on shutdown, so the engine can safely
        // dereference it for the entire lifetime of this instance.
        let userData = Unmanaged.passRetained(self).toOpaque()
        nestty_engine_set_action_callback(
            handle,
            NesttyEngine.actionCallback,
            userData,
        )
    }

    // No deinit. The Rust engine holds a retained pointer to `self`
    // through `Unmanaged.passRetained` (so the C action callback can
    // safely deref user_data). That retain count means deinit only
    // fires AFTER `shutdown()` has run `passUnretained.release()`,
    // which is the explicit teardown path AppDelegate must call.
    // Adding a fallback destroy here would race with the retained
    // pointer and isn't needed in practice — the app only constructs
    // one NesttyEngine and shutdown() is always reached on app quit.

    /// Replace the engine's trigger list. Pass an array of trigger dicts
    /// shaped like the TOML `[[triggers]]` blocks (decoded into JSON-friendly
    /// values). Returns the count of loaded triggers, or nil on JSON
    /// encoding failure.
    @discardableResult
    func setTriggers(_ triggers: [[String: Any]]) -> Int? {
        guard let handle else { return nil }
        guard let data = try? JSONSerialization.data(withJSONObject: triggers),
              let str = String(data: data, encoding: .utf8)
        else {
            FileHandle.standardError.write(Data("[nestty-engine] failed to encode triggers JSON\n".utf8))
            return nil
        }
        return str.withCString { ptr in
            let count = nestty_engine_set_triggers(handle, ptr)
            if count < 0 {
                let err = NesttyFFI.lastError() ?? "<no message>"
                FileHandle.standardError.write(Data("[nestty-engine] setTriggers failed: \(err)\n".utf8))
                return nil
            }
            return Int(count)
        }
    }

    /// Dispatch an event to the engine. Triggers matching the kind/payload
    /// fire their actions synchronously via the C callback (which then
    /// hops to the action registry). Returns # triggers fired.
    ///
    /// `source` is stamped onto the synthesized `Event` and threads
    /// through the trust boundary that gates Linux's await-promotion
    /// (`nestty_core::trigger::try_promote_or_drop_preflight` rejects
    /// completion-suffixed events whose source is not
    /// `COMPLETION_EVENT_SOURCE`). Defaults to `"macos.eventbus"` —
    /// `ActionRegistry.publishCompletion` overrides with `"nestty.action"`.
    ///
    /// `context` is the Context snapshot used for `{context.X}`
    /// interpolation + condition evaluation. Pass `nil` (or empty dict)
    /// to dispatch without context — interpolation tokens stay literal,
    /// condition references resolve to null. macOS callers should pass
    /// `ContextService.snapshot()` taken AFTER `apply`-ing the current
    /// event to satisfy the apply-before-dispatch ordering Linux uses
    /// in `Pump::pump_all`.
    @discardableResult
    func dispatchEvent(
        kind: String,
        source: String = "macos.eventbus",
        context: [String: Any]? = nil,
        payload: [String: Any],
    ) -> Int {
        guard let handle else { return 0 }
        let payloadStr: String = if let data = try? JSONSerialization.data(withJSONObject: payload),
                                    let s = String(data: data, encoding: .utf8)
        {
            s
        } else {
            "null"
        }
        // Empty context dict short-circuits to nil so we pass NULL to FFI
        // (saves a "{}" round-trip through serde — engine treats both
        // identically but the NULL path is cheaper).
        let contextStr: String? = if let ctx = context, !ctx.isEmpty,
                                     let data = try? JSONSerialization.data(withJSONObject: ctx),
                                     let s = String(data: data, encoding: .utf8)
        {
            s
        } else {
            nil
        }
        return kind.withCString { kindPtr in
            source.withCString { sourcePtr in
                payloadStr.withCString { payloadPtr in
                    let dispatch: (UnsafePointer<CChar>?) -> Int32 = { ctxPtr in
                        nestty_engine_dispatch_event(
                            handle,
                            kindPtr,
                            sourcePtr,
                            ctxPtr,
                            payloadPtr,
                        )
                    }
                    let n: Int32 = if let ctx = contextStr {
                        ctx.withCString { dispatch($0) }
                    } else {
                        dispatch(nil)
                    }
                    return n < 0 ? 0 : Int(n)
                }
            }
        }
    }

    /// Diagnostic — current number of triggers loaded.
    var triggerCount: Int {
        guard let handle else { return 0 }
        let n = nestty_engine_count_triggers(handle)
        return n < 0 ? 0 : Int(n)
    }

    /// Tear down the engine. After this call the instance is unusable.
    /// AppDelegate calls this from `applicationWillTerminate`.
    func shutdown() {
        guard let handle else { return }
        // Clear callback first so any in-flight dispatch can't invoke
        // a stale function pointer between here and destroy.
        nestty_engine_set_action_callback(
            handle,
            nil,
            nil,
        )
        // Reclaim the retained self pointer so ARC can finalize this
        // instance after AppDelegate drops its reference.
        Unmanaged<NesttyEngine>.passUnretained(self).release()
        nestty_engine_destroy(handle)
        self.handle = nil
    }

    // MARK: - C callback trampoline

    /// `@convention(c)` so the function pointer matches the C signature.
    /// Cannot capture context, hence `user_data` carries the NesttyEngine
    /// instance pointer.
    ///
    /// Engine calls this from whatever thread invoked dispatchEvent.
    /// We hop to the main actor before touching ActionRegistry (which is
    /// `@MainActor`-isolated) or any other Swift state.
    private static let actionCallback: nestty_action_callback = { userData, actionPtr, paramsPtr in
        guard let userData, let actionPtr, let paramsPtr else { return }
        // Take a borrowed reference; the engine owns the retained pointer
        // and only releases it via NesttyEngine.shutdown.
        let unmanaged = Unmanaged<NesttyEngine>.fromOpaque(userData)
        let engine = unmanaged.takeUnretainedValue()
        let actionName = String(cString: actionPtr)
        let paramsJson = String(cString: paramsPtr)

        // Decode params back into [String: Any] so ActionRegistry handlers
        // can use the dict shape they expect from socket dispatch.
        let params: [String: Any] = if let data = paramsJson.data(using: .utf8),
                                       let dict = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any]
        {
            dict
        } else {
            [:]
        }

        // Fire-and-forget dispatch. We deliberately don't await the result —
        // this matches the `{queued: true}` return from the Rust side and
        // keeps the engine's dispatch loop non-blocking. ActionRegistry is
        // @MainActor-isolated, so we hop via Task { @MainActor in ... }.
        let registryRef = engine.actionRegistry
        Task { @MainActor in
            guard let registry = registryRef else {
                FileHandle.standardError.write(Data("[nestty-engine] action callback fired but no ActionRegistry attached: \(actionName)\n".utf8))
                return
            }
            let dispatched = registry.tryDispatch(actionName, params: params) { _ in
                // Fire-and-forget — discard the action's own completion result.
                // Triggers don't currently consume completion data on macOS;
                // when await semantics land we'll plumb this back into the
                // Rust engine via a separate FFI call.
            }
            if !dispatched {
                FileHandle.standardError.write(Data("[nestty-engine] trigger fired \(actionName) but no handler registered (registry has: \(registry.names()))\n".utf8))
            }
        }
    }
}
