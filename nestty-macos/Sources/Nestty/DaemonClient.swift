import Darwin
import Foundation

/// Swift mirror of `nestty-linux/src/gui_client.rs`. Connects Nestty.app to
/// `nesttyd` over the runtime-dir Unix socket, registers the GUI, sends
/// `gui.register` / receives ack, services daemon `_ping` heartbeats, and
/// exposes `forward(method:params:completion:)` so `ActionRegistry`'s fallback
/// handler can ship plugin/registry RPCs to the daemon.
///
/// Wire shapes (verified against `nestty-core/src/protocol.rs`):
/// - Outbound `Request`: `{"id", "method", "params", "target_client_id"?}`
/// - Inbound `Response`: `{"id", "ok", "result"?, "error"?}`
/// - Inbound `Invoke` (daemon → GUI): `{"id", "invoke", "params"}`
///   - `invoke == "_ping"`: heartbeat. Reply with `{"id", "ok": true, "result": params}`
///   - other invokes: PR3 territory; PR2 logs + ignores. Daemon may time out
///     in-flight invokes for those — accept this limitation until PR3 lands
///     full Invoke routing through `handleCommand`.
/// - Inbound `Event`: `{"type", "data", "source"?}` — PR4a republishes through bridge.
///   PR2 logs + ignores.
///
/// Threading:
/// - One background thread runs the reconnect+reader loop (`run`).
/// - One background thread per connection drains the writer queue.
/// - State (`connected`, `generation`, `pending`) protected by `stateLock` (NSLock).
/// - `isConnected` is a cheap synchronous read for `ActionRegistry` fallback.
/// - Forward completions can fire from reader thread, timeout thread, or
///   disconnect-drain on the run thread. Callers must tolerate any thread.
///
/// Generation gating mirrors Linux's `gui_client.rs` `Arc<AtomicU64>` pattern
/// (`eb2e58d`). Each connection bumps `generation`; pending forwards capture
/// `gen` at submit time, and stale replies after reconnect are dropped.
///
/// `@unchecked Sendable`: state mutations are guarded by `stateLock` (NSLock).
/// We expose `isConnected` as a synchronous read for `ActionRegistry` fallback,
/// which an `actor` would force into async territory. NSLock matches Linux's
/// std::sync::Mutex usage in gui_client.rs.
final class DaemonClient: @unchecked Sendable {
    // MARK: - Public surface

    let capabilities: [String]
    let socketURL: URL

    /// Cheap, lock-protected. ActionRegistry fallback hits this on every
    /// dispatched method, so it must not allocate or await.
    var isConnected: Bool {
        stateLock.lock(); defer { stateLock.unlock() }
        return connected
    }

    /// Capture last register ack so callers (PR4b host_triggers cut-over,
    /// status display) can read negotiated daemon flags. nil until first
    /// successful register.
    private(set) var lastAck: RegisterAck?

    init(socket: URL, capabilities: [String]) {
        socketURL = socket
        self.capabilities = capabilities
    }

    /// Start the background reconnect loop. Idempotent — second call is a
    /// no-op. Returns immediately; connection happens asynchronously.
    func start() {
        stateLock.lock()
        guard !started else { stateLock.unlock(); return }
        started = true
        stateLock.unlock()

        Thread.detachNewThread { [weak self] in
            self?.runForever()
        }
    }

    /// Forward an RPC to the daemon. Completion fires from any thread.
    /// On disconnect or per-request timeout, completion receives an `RPCError`.
    /// **Silent** — does NOT trigger ActionRegistry's completion fan-out
    /// (`<method>.completed` / `.failed`); daemon-side ActionRegistry handles
    /// that already, so re-publishing here would double-fire.
    ///
    /// PR3: GUI-owned methods are now safe to forward — daemon will Invoke
    /// them back, the new `invokeHandler` routes through `handleCommand`,
    /// and the Response flows back through `handleResponse` here.
    func forward(method: String, params: [String: Any], completion: @escaping (Any?) -> Void) {
        stateLock.lock()
        guard connected, let writerCh = currentWriter else {
            stateLock.unlock()
            completion(RPCError(code: "daemon_unavailable", message: "daemon not connected"))
            return
        }
        let id = UUID().uuidString
        let gen = generation
        let pendingEntry = Pending(generation: gen, completion: completion)
        pending[id] = pendingEntry
        stateLock.unlock()

        // Schedule timeout — fires off a serial queue so it doesn't compete
        // with the reader thread.
        timeoutQueue.asyncAfter(deadline: .now() + Self.requestTimeout) { [weak self] in
            self?.timeoutPending(id: id, gen: gen)
        }

        let req: [String: Any] = ["id": id, "method": method, "params": params]
        guard let line = encodeJSONLine(req) else {
            // JSON encoding failure → drop pending + report
            stateLock.lock()
            pending.removeValue(forKey: id)
            stateLock.unlock()
            completion(RPCError(code: "internal_error", message: "encode forward request: \(method)"))
            return
        }
        if !writerCh.send(line) {
            // Writer queue full or already shut down. Drop pending so caller
            // sees a fast `daemon_unavailable` instead of a misleading 30s
            // timeout for a request the daemon never received (codex review C2).
            stateLock.lock()
            pending.removeValue(forKey: id)
            stateLock.unlock()
            completion(RPCError(code: "daemon_unavailable", message: "writer queue overflow or shutdown (forward of \(method))"))
        }
    }

    // MARK: - Internal state

    /// `actor`-free because we already need synchronous `isConnected` reads
    /// from any thread (ActionRegistry fallback). NSLock matches Linux's
    /// std::sync::Mutex usage in gui_client.rs.
    private let stateLock = NSLock()
    private var started = false
    private var connected = false
    private var generation: UInt64 = 0
    private var pending: [String: Pending] = [:]
    private var currentWriter: WriterChannel?

    private let timeoutQueue = DispatchQueue(label: "nestty.daemonclient.timeout")

    /// Per-pending entry. Captures generation so a delayed timeout fired
    /// after the connection it belonged to has been recycled does not
    /// double-complete a fresh request that happened to reuse the id.
    /// (UUID collision is astronomically unlikely, but the gen+id pair
    /// is the safe key.)
    private struct Pending {
        let generation: UInt64
        let completion: (Any?) -> Void
    }

    // MARK: - Inbound Invoke handling (PR3)

    /// Set by AppDelegate during launch (BEFORE `start()` so the first inbound
    /// Invoke isn't dropped). Closure dispatches the daemon's invoke through
    /// `AppDelegate.handleCommand` with `allowFallback: false`
    /// (so daemon→GUI→daemon recursion is impossible) and
    /// `silentCompletion: true` (so the local registry doesn't republish a
    /// `<method>.completed` event that the daemon will publish itself —
    /// PR4b will bridge that back via `_bus.publish`). The `reply` closure
    /// writes the JSON Response line to our writer.
    /// `@MainActor @Sendable`: closure body executes on main actor (so it
    /// can call `AppDelegate.handleCommand` synchronously without a hop),
    /// while the closure ref itself is Sendable (we store it on this
    /// non-actor class). The wrapping Task in `admitInvoke` ensures the
    /// invocation always lands on main.
    typealias InvokeHandler = @MainActor @Sendable (_ id: String, _ method: String, _ params: [String: Any], _ reply: @Sendable @escaping (String) -> Void) -> Void
    var invokeHandler: InvokeHandler?

    /// Bounded admission for inbound Invokes — mirrors Linux's
    /// `gui_client.rs` `POOL_QUEUE = 32`. Saturation → immediate `overloaded`
    /// Response so the daemon's pending invoke completes fast instead of
    /// waiting for its own 5s/120s timeout. Stale-generation invokes (queued
    /// before a disconnect) also get an explicit `overloaded` response (codex
    /// round 1 I1, round 2 confirmed).
    private static let invokeQueueCap = 32
    private var invokeInFlight = 0 // guarded by stateLock

    // MARK: - Constants

    /// Frame cap mirrors `nestty-daemon::socket::read_line_capped` — 1 MiB
    /// including the trailing newline (codex PR3 round 1 C3 + round 3 C1).
    /// Reads beyond this immediately close the connection.
    private static let maxFrameBytes = 1024 * 1024

    private static let backoffMin: TimeInterval = 0.1
    private static let backoffMax: TimeInterval = 5.0
    /// Per-forward request timeout (caller waiting on `forward(...)`). Daemon
    /// owns the daemon-side method timeout (`gui_registry.rs::method_invoke_timeout`,
    /// 5s default / 120s for webview/claude.start). Our 30s is the GUI-side
    /// safety belt for the forward path; if the daemon never replies in time
    /// we fail-complete the caller.
    private static let requestTimeout: TimeInterval = 30.0
    /// Long safety net for inbound Invoke handler completion. Daemon's own
    /// timeout fires sooner (5s/120s); the slot release here only protects
    /// against a leaked slot if a handler closure never completes. 125s
    /// matches Linux's `pump_timeout`. Daemon-side pending entry has already
    /// failed by then — if our handler does eventually call back, the writer
    /// `.send` will return false (channel shut down on reconnect) and the
    /// reply is silently lost. Acceptable: daemon already gave up.
    private static let invokeHandlerSafetyTimeout: TimeInterval = 125.0

    // MARK: - Run loop

    private func runForever() {
        var backoff = Self.backoffMin
        while true {
            if let session = openConnection() {
                backoff = Self.backoffMin
                runReader(session: session)
                // runReader returns when connection closed/erred.
                handleDisconnect()
            } else {
                // Connection failed. Try auto-spawn ONCE per backoff cycle.
                // If spawn succeeds, immediate retry; if not, sleep + retry.
                if AutoSpawn.ensureRunning() {
                    log("auto-spawn succeeded — retrying connect immediately")
                    backoff = Self.backoffMin
                    continue
                }
                Thread.sleep(forTimeInterval: backoff)
                backoff = min(backoff * 2, Self.backoffMax)
            }
        }
    }

    private func openConnection() -> Session? {
        guard let fd = connectUnix(path: socketURL.path(percentEncoded: false)) else {
            return nil
        }
        // Register
        guard let ack = registerOn(fd: fd) else {
            close(fd)
            return nil
        }

        // Build writer channel + thread
        let writer = WriterChannel(fd: fd)
        writer.start()

        stateLock.lock()
        generation &+= 1
        connected = true
        currentWriter = writer
        lastAck = ack
        stateLock.unlock()
        log("registered with nesttyd: ack=\(ack)")

        return Session(fd: fd, writer: writer)
    }

    private func registerOn(fd: Int32) -> RegisterAck? {
        let reqId = UUID().uuidString
        let payload: [String: Any] = [
            "id": reqId,
            "method": "gui.register",
            "params": [
                "window_id": UUID().uuidString,
                "capabilities": capabilities,
                "want_primary": true,
                "version": "0.1.0",
                "protocol_version": 1,
                "gui_env": [:] as [String: String], // PR5 will populate per gui_registry.rs whitelist
            ],
        ]
        guard let line = encodeJSONLine(payload) else { return nil }
        if !writeLine(fd: fd, line: line) { return nil }

        // Read ack synchronously (one line, blocking up to 5s)
        var tv = timeval(tv_sec: 5, tv_usec: 0)
        _ = setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, socklen_t(MemoryLayout<timeval>.size))
        guard let ackLine = readLine(fd: fd) else {
            log("register ack: no response")
            return nil
        }
        guard let json = try? JSONSerialization.jsonObject(with: Data(ackLine.utf8)) as? [String: Any] else {
            log("register ack: malformed JSON: \(ackLine.prefix(200))")
            return nil
        }
        if let ackId = json["id"] as? String, ackId != reqId {
            log("register ack id mismatch: expected \(reqId), got \(ackId)")
            return nil
        }
        if (json["ok"] as? Bool) != true {
            log("register rejected: \(json["error"] ?? "<no error>")")
            return nil
        }
        let result = (json["result"] as? [String: Any]) ?? [:]
        let hostTriggers = (result["host_triggers"] as? Bool) ?? false
        let clientId = (result["client_id"] as? String) ?? ""
        let primary = (result["primary"] as? Bool) ?? false
        let daemonVer = (result["daemon_version"] as? String) ?? ""
        let proto = (result["protocol_version"] as? Int) ?? 0
        // Restore blocking semantics for the reader loop
        var tv2 = timeval(tv_sec: 0, tv_usec: 0)
        _ = setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv2, socklen_t(MemoryLayout<timeval>.size))
        return RegisterAck(
            clientId: clientId,
            primary: primary,
            daemonVersion: daemonVer,
            protocolVersion: proto,
            hostTriggers: hostTriggers,
        )
    }

    private func runReader(session: Session) {
        let fd = session.fd
        var buf = Data()
        let chunkSize = 8192
        var chunk = [UInt8](repeating: 0, count: chunkSize)
        outer: while true {
            let n = chunk.withUnsafeMutableBufferPointer { bp -> Int in
                Darwin.read(fd, bp.baseAddress, bp.count)
            }
            if n <= 0 {
                if n < 0 { log("reader read error: \(String(cString: strerror(errno)))") }
                break outer
            }
            buf.append(chunk, count: n)
            // Line-split with frame cap. Mirrors `nestty-daemon::socket::
            // read_line_capped` (1 MiB frame INCLUDING newline — codex round
            // 3 C1 off-by-one fix). Two checks:
            //   1. Frame length to next newline > cap → reject (close).
            //   2. No newline AND buffer total > cap → reject (would never
            //      assemble a valid frame).
            // Only the offending frame closes the connection; small frames
            // already in the buffer parse normally up to the offender.
            while true {
                if let nlIdx = buf.firstIndex(of: 0x0A) {
                    let frameBytes = nlIdx + 1
                    if frameBytes > Self.maxFrameBytes {
                        log("frame cap exceeded (\(frameBytes) > \(Self.maxFrameBytes)) — closing connection")
                        break outer
                    }
                    let lineData = buf.prefix(nlIdx)
                    buf.removeSubrange(0 ... nlIdx)
                    guard let line = String(data: lineData, encoding: .utf8), !line.trimmingCharacters(in: .whitespaces).isEmpty else { continue }
                    handleInboundLine(line, writer: session.writer)
                } else {
                    if buf.count > Self.maxFrameBytes {
                        log("partial frame exceeds cap (\(buf.count) > \(Self.maxFrameBytes)) — closing connection")
                        break outer
                    }
                    break // need more bytes
                }
            }
        }
        session.writer.shutdown()
        // Close the connection fd so we don't leak one descriptor per
        // disconnect (codex review C1). `registerOn` failure already closes
        // the fd, so by here we own a confirmed-open socket.
        close(fd)
    }

    private func handleInboundLine(_ line: String, writer: WriterChannel) {
        guard let value = try? JSONSerialization.jsonObject(with: Data(line.utf8)) as? [String: Any] else {
            log("malformed line: \(line.prefix(200))")
            return
        }
        if let invokeMethod = value["invoke"] as? String {
            if invokeMethod == "_ping" {
                replyToPing(value: value, writer: writer)
            } else {
                guard let id = value["id"] as? String else {
                    log("Invoke missing id: method=\(invokeMethod) — dropping")
                    return
                }
                let params = (value["params"] as? [String: Any]) ?? [:]
                admitInvoke(id: id, method: invokeMethod, params: params)
            }
            return
        }
        if value["ok"] != nil, let id = value["id"] as? String {
            handleResponse(id: id, value: value)
            return
        }
        if value["type"] != nil {
            // PR4a: republish to local EventBus with fresh bridge_id.
            log("ignoring Event (PR4a): type=\(value["type"] ?? "?")")
            return
        }
        log("ignoring unknown line: \(line.prefix(200))")
    }

    private func replyToPing(value: [String: Any], writer: WriterChannel) {
        guard let id = value["id"] as? String else { return }
        let params = value["params"] ?? [:]
        let response: [String: Any] = ["id": id, "ok": true, "result": params]
        guard let line = encodeJSONLine(response) else { return }
        writer.sendControl(line) // priority lane — bypasses bounded forward queue
    }

    /// Admit an inbound Invoke through a bounded gate, then dispatch on the
    /// main actor. Mirrors Linux `gui_client.rs` `POOL_QUEUE = 32` semantics:
    /// saturation responds `overloaded` immediately; stale-generation invokes
    /// (admitted before a disconnect) also respond `overloaded` so the daemon
    /// completes its pending entry fast instead of waiting for its own
    /// timeout (codex round 2 confirmed Linux writes overloaded on stale).
    ///
    /// **Slot lifetime** (codex round 2 C1, round 3 C2): the admission slot
    /// holds for the *response*, not the Task body. Async handlers
    /// (`webview.execute_js`, `claude.start`, `webview.screenshot`) call
    /// completion later than the Task closure returns. A shared `SlotGuard`
    /// ensures every terminal path — response, missing handler, encode
    /// failure, stale-gen, safety-net timeout — releases the slot exactly
    /// once.
    private func admitInvoke(id: String, method: String, params: [String: Any]) {
        stateLock.lock()
        let admittedGen = generation
        let writerSnapshot = currentWriter
        if invokeInFlight >= Self.invokeQueueCap {
            stateLock.unlock()
            sendInvokeError(id: id, code: "overloaded", message: "GUI invoke queue saturated", writer: writerSnapshot)
            return
        }
        invokeInFlight += 1
        stateLock.unlock()

        let slot = SlotGuard()
        let releaseSlot: @Sendable () -> Void = { [weak self] in
            guard slot.releaseOnce() else { return }
            self?.stateLock.lock()
            if let s = self { s.invokeInFlight = max(0, s.invokeInFlight - 1) }
            self?.stateLock.unlock()
        }

        // Long safety net so a never-completing handler can't leak slots
        // forever (codex C1). Daemon's own timeout fires sooner; if it
        // does fire after, writer.send returns false (channel shut down)
        // and the late reply is silently lost — daemon already gave up.
        timeoutQueue.asyncAfter(deadline: .now() + Self.invokeHandlerSafetyTimeout) {
            releaseSlot()
        }

        // Bridge non-Sendable params + writer ref across the @MainActor hop
        // via SendableBox (existing project pattern — see SendableBox.swift).
        // The race the type system warns about can't occur: params is
        // produced once on the reader thread, consumed once on main, never
        // shared. Same for the writer reference — admitInvoke owns the
        // captured snapshot for this one Invoke.
        let paramsBox = SendableBox(params)
        let writerBox = SendableBox(writerSnapshot)
        Task { @MainActor [weak self] in
            guard let self else { releaseSlot(); return }
            let curGen: UInt64 = {
                self.stateLock.lock(); defer { self.stateLock.unlock() }
                return self.generation
            }()
            if curGen != admittedGen {
                // Connection recycled while we waited for the main actor.
                // Daemon's pending entry already failed; tell daemon
                // explicitly so it doesn't wait for its own timeout.
                sendInvokeError(id: id, code: "overloaded", message: "GUI generation moved before dispatch", writer: writerBox.value)
                releaseSlot()
                return
            }
            guard let handler = invokeHandler else {
                sendInvokeError(id: id, code: "internal_error", message: "DaemonClient invokeHandler not set", writer: writerBox.value)
                releaseSlot()
                return
            }
            handler(id, method, paramsBox.value) { responseLine in
                _ = writerBox.value?.send(responseLine)
                releaseSlot()
            }
        }
    }

    /// Build + send an error Response for failures inside the Invoke admission
    /// path. `writer` may be nil if the connection died between admission and
    /// the failure observation; in that case the daemon already failed its
    /// pending entry on disconnect, so silent drop is acceptable.
    private func sendInvokeError(id: String, code: String, message: String, writer: WriterChannel?) {
        guard let writer else { return }
        let resp: [String: Any] = [
            "id": id,
            "ok": false,
            "error": ["code": code, "message": message],
        ]
        guard let line = encodeJSONLine(resp) else { return }
        _ = writer.send(line)
    }

    private func handleResponse(id: String, value: [String: Any]) {
        stateLock.lock()
        guard let entry = pending.removeValue(forKey: id) else {
            stateLock.unlock()
            // Late reply or stale generation drop — not an error
            return
        }
        let curGen = generation
        stateLock.unlock()

        if entry.generation != curGen {
            // Stale generation — connection was recycled before this reply
            // arrived. Caller already got daemon_unavailable on disconnect.
            return
        }

        let ok = (value["ok"] as? Bool) ?? false
        if ok {
            entry.completion(value["result"] ?? [:])
        } else if let err = value["error"] as? [String: Any] {
            let code = (err["code"] as? String) ?? "unknown"
            let msg = (err["message"] as? String) ?? ""
            entry.completion(RPCError(code: code, message: msg))
        } else {
            entry.completion(RPCError(code: "malformed_response", message: "no ok field"))
        }
    }

    private func handleDisconnect() {
        // Drain pending: each completion gets daemon_unavailable so callers
        // unblock immediately instead of waiting for per-request timeout.
        // Bump generation FIRST so any stale reply that races us in is
        // dropped by handleResponse's gen check.
        stateLock.lock()
        connected = false
        generation &+= 1
        let drained = pending
        pending.removeAll()
        currentWriter?.shutdown()
        currentWriter = nil
        stateLock.unlock()

        for (_, entry) in drained {
            entry.completion(RPCError(code: "daemon_unavailable", message: "daemon disconnected mid-flight"))
        }
        log("disconnected — drained \(drained.count) pending forward(s)")
    }

    private func timeoutPending(id: String, gen: UInt64) {
        stateLock.lock()
        guard let entry = pending[id], entry.generation == gen else {
            stateLock.unlock()
            return
        }
        pending.removeValue(forKey: id)
        stateLock.unlock()
        entry.completion(RPCError(code: "timeout", message: "daemon did not respond in \(Self.requestTimeout)s"))
    }

    // MARK: - Socket helpers (synchronous, blocking)

    private func connectUnix(path: String) -> Int32? {
        let fd = socket(AF_UNIX, SOCK_STREAM, 0)
        guard fd >= 0 else { return nil }
        var addr = sockaddr_un()
        addr.sun_family = sa_family_t(AF_UNIX)
        let cstr = path.utf8CString
        if cstr.count > MemoryLayout.size(ofValue: addr.sun_path) {
            close(fd); return nil
        }
        withUnsafeMutablePointer(to: &addr.sun_path) { p in
            p.withMemoryRebound(to: CChar.self, capacity: cstr.count) { dst in
                _ = cstr.withUnsafeBufferPointer { src in
                    memcpy(dst, src.baseAddress, cstr.count)
                }
            }
        }
        let rc = withUnsafePointer(to: &addr) { ptr -> Int32 in
            ptr.withMemoryRebound(to: sockaddr.self, capacity: 1) { sa in
                Darwin.connect(fd, sa, socklen_t(MemoryLayout<sockaddr_un>.size))
            }
        }
        if rc != 0 { close(fd); return nil }
        return fd
    }

    private func writeLine(fd: Int32, line: String) -> Bool {
        let withNl = line.hasSuffix("\n") ? line : line + "\n"
        let bytes = Array(withNl.utf8)
        var written = 0
        while written < bytes.count {
            let n = bytes.withUnsafeBufferPointer { bp -> Int in
                Darwin.write(fd, bp.baseAddress!.advanced(by: written), bp.count - written)
            }
            if n <= 0 { return false }
            written += n
        }
        return true
    }

    /// Single-line blocking read. Used by `registerOn` for the synchronous
    /// register-ack handshake. **Frame-capped** at `maxFrameBytes` mirroring
    /// `nestty-daemon::socket::read_line_capped` and the runReader loop —
    /// a malicious or runaway daemon can't OOM Nestty.app via the register
    /// channel either (codex round 1 I2 — register ack capped on Linux too).
    /// Returns nil on EOF, error, or oversized frame.
    ///
    /// **Cap semantic** (codex PR3 cross-review C3): `maxFrameBytes` includes
    /// the trailing newline (matching the runReader loop's `nlIdx + 1` check
    /// and Linux's `read_line_capped`). After appending each byte, if the
    /// buffer length equals or exceeds `maxFrameBytes` AND the just-read
    /// byte was NOT the newline, the next byte would push over cap → reject.
    private func readLine(fd: Int32) -> String? {
        var buf = Data()
        var byte: UInt8 = 0
        while true {
            // Reject when accepting another non-newline byte would push the
            // (payload + newline) frame past cap. `buf.count + 1 > cap`
            // catches the case where buf already has cap-1 bytes — adding
            // a newline next would land at exactly cap (acceptable), but
            // adding any non-newline byte would push past.
            if buf.count >= Self.maxFrameBytes {
                log("registerOn: read frame exceeded cap (\(buf.count) >= \(Self.maxFrameBytes)) — aborting register")
                return nil
            }
            let n = withUnsafeMutablePointer(to: &byte) { p -> Int in
                Darwin.read(fd, p, 1)
            }
            if n <= 0 { return nil }
            if byte == 0x0A { break }
            buf.append(byte)
        }
        return String(data: buf, encoding: .utf8)
    }

    private func encodeJSONLine(_ obj: [String: Any]) -> String? {
        guard let data = try? JSONSerialization.data(withJSONObject: obj, options: []),
              let s = String(data: data, encoding: .utf8) else { return nil }
        return s + "\n"
    }

    private func log(_ msg: String) {
        FileHandle.standardError.write(Data("[nestty-daemonclient] \(msg)\n".utf8))
    }
}

// MARK: - Supporting types

/// Captured fields from the daemon's `gui.register` ack.
struct RegisterAck {
    let clientId: String
    let primary: Bool
    let daemonVersion: String
    let protocolVersion: Int
    let hostTriggers: Bool
}

private struct Session {
    let fd: Int32
    let writer: WriterChannel
}

/// Idempotent one-shot release flag. `releaseOnce()` returns `true` exactly
/// once across concurrent calls; subsequent calls return `false`. Used by
/// `admitInvoke` to ensure each Invoke admission slot is decremented exactly
/// once across the racing terminal paths (handler completion, safety-timeout,
/// error early-out — codex round 3 C2).
///
/// NSLock-backed (codex Q1: don't add Swift Atomics for one guard).
final class SlotGuard: @unchecked Sendable {
    private let lock = NSLock()
    private var released = false

    func releaseOnce() -> Bool {
        lock.lock(); defer { lock.unlock() }
        if released { return false }
        released = true
        return true
    }
}

/// Bounded normal-forward queue + unbounded control queue (for `_ping` replies
/// that must not be starved behind 256 forwards). Mirrors Linux's worker-
/// admission split where heartbeat replies skip the pool (round 2 Q2).
///
/// `@unchecked Sendable`: NSCondition's underlying mutex serializes all
/// mutations to `control`/`normal`/`stopped`. The drain thread is the sole
/// owner of `fd` writes.
final class WriterChannel: @unchecked Sendable {
    private let fd: Int32
    private let cond: NSCondition
    private var control: [String] = []
    private var normal: [String] = []
    private var stopped = false
    private static let normalCap = 256

    init(fd: Int32) {
        self.fd = fd
        cond = NSCondition()
    }

    func start() {
        Thread.detachNewThread { [weak self] in
            self?.runDrain()
        }
    }

    /// Normal forward (bounded). Returns `false` on overflow OR when the
    /// channel has already been shut down — caller (DaemonClient.forward)
    /// must immediately fail-complete + drop the corresponding pending
    /// entry. Returning false instead of silently dropping the oldest
    /// frame prevents the "request never sent but caller times out 30s
    /// later" foot-gun (codex review C2). Hard-bounded queue is still
    /// the right back-pressure for a wedged daemon — but the caller
    /// gets to surface the failure synchronously.
    func send(_ line: String) -> Bool {
        cond.lock()
        defer { cond.unlock() }
        if stopped { return false }
        if normal.count >= Self.normalCap { return false }
        normal.append(line)
        cond.signal()
        return true
    }

    /// Control frames (heartbeat replies). Tiny in practice (one `_ping` per
    /// daemon heartbeat interval), so unbounded is safe.
    func sendControl(_ line: String) {
        cond.lock()
        control.append(line)
        cond.signal()
        cond.unlock()
    }

    func shutdown() {
        cond.lock()
        stopped = true
        cond.signal()
        cond.unlock()
    }

    private func runDrain() {
        while true {
            cond.lock()
            while !stopped, control.isEmpty, normal.isEmpty {
                cond.wait()
            }
            if stopped, control.isEmpty, normal.isEmpty {
                cond.unlock()
                return
            }
            let next: String? = if !control.isEmpty {
                control.removeFirst()
            } else if !normal.isEmpty {
                normal.removeFirst()
            } else {
                nil
            }
            cond.unlock()

            guard let line = next else { continue }
            let withNl = line.hasSuffix("\n") ? line : line + "\n"
            let bytes = Array(withNl.utf8)
            var written = 0
            var failed = false
            while written < bytes.count {
                let n = bytes.withUnsafeBufferPointer { bp -> Int in
                    Darwin.write(fd, bp.baseAddress!.advanced(by: written), bp.count - written)
                }
                if n <= 0 { failed = true; break }
                written += n
            }
            if failed {
                // Reader thread will observe disconnect and trigger drain;
                // we just stop the writer.
                return
            }
        }
    }
}
