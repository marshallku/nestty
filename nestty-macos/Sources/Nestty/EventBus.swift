import Foundation

// Event wire format (matches Linux nestty-core protocol):
// {"event": "terminal.output", "data": {"panel_id": "...", "text": "..."}}

// MARK: - EventBus

/// Broadcast hub for all nestty events.
/// Subscribers hold an EventChannel that buffers events until the socket thread reads them.
final class EventBus: @unchecked Sendable {
    private let lock = NSLock()
    private var channels: [EventChannel] = []

    /// Optional fan-out hook fired BEFORE channel broadcast, on the
    /// caller's thread. Set by AppDelegate to forward every event into
    /// the trigger engine (PR 5c) without making EventBus aware of
    /// NesttyEngine. Closure must be cheap — runs synchronously on the
    /// broadcast call's thread.
    ///
    /// `source` mirrors `nestty_core::event_bus::Event.source` — used by
    /// the trigger engine's `try_promote_or_drop_preflight` to gate
    /// await-chain promotion (only events stamped
    /// `nestty_core::action_registry::COMPLETION_EVENT_SOURCE` =
    /// `"nestty.action"` advance an await state machine). PR 7 plumbs
    /// the field through so registry-synthesized completion events
    /// satisfy the same trust boundary as on Linux.
    nonisolated(unsafe) var onBroadcast: (@Sendable (
        _ kind: String,
        _ source: String,
        _ data: [String: Any],
    ) -> Void)?

    func subscribe() -> EventChannel {
        let ch = EventChannel()
        lock.withLock { channels.append(ch) }
        return ch
    }

    /// Broadcast an event to all live subscribers. Dead subscribers are pruned.
    ///
    /// `source` defaults to `"macos.eventbus"` — match the historical
    /// stamp the FFI used pre-PR-7. `ActionRegistry.publishCompletion`
    /// passes `"nestty.action"` for registry-synthesized completion
    /// events; other broadcast sites should leave the default.
    func broadcast(event: String, source: String = "macos.eventbus", data: [String: Any] = [:]) {
        // Fire the trigger-engine hook first — keeps the engine in
        // the same logical "tick" as the channel publish so a trigger
        // that itself broadcasts (chained workflows) gets its event
        // ordered immediately after the original.
        onBroadcast?(event, source, data)

        let payload: [String: Any] = ["event": event, "data": data]
        guard
            let jsonData = try? JSONSerialization.data(withJSONObject: payload),
            let json = String(data: jsonData, encoding: .utf8)
        else { return }

        lock.withLock {
            channels.removeAll { !$0.send(json) }
        }
    }
}

// MARK: - EventChannel

/// Single-subscriber FIFO queue. The socket thread blocks on `receive()`
/// while the main thread pushes events via `send(_:)`.
final class EventChannel: @unchecked Sendable {
    private var queue: [String] = []
    private let sema = DispatchSemaphore(value: 0)
    private let lock = NSLock()
    private var closed = false

    /// Returns false if the channel is already closed (subscriber disconnected).
    func send(_ event: String) -> Bool {
        lock.lock()
        guard !closed else { lock.unlock(); return false }
        queue.append(event)
        lock.unlock()
        sema.signal()
        return true
    }

    /// Blocks until an event is available. Returns nil when the channel is closed.
    func receive() -> String? {
        sema.wait()
        return lock.withLock {
            if closed, queue.isEmpty { return nil }
            return queue.isEmpty ? nil : queue.removeFirst()
        }
    }

    func close() {
        lock.withLock { closed = true }
        sema.signal()
    }
}

// MARK: - NSLock convenience

private extension NSLock {
    @discardableResult
    func withLock<T>(_ body: () -> T) -> T {
        lock(); defer { unlock() }
        return body()
    }
}
