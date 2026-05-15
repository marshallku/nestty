import Darwin
import Foundation

/// Single-flight `nesttyd` auto-spawn helper. Mirrors the intent (not the
/// thread model) of Linux's manual-`nesttyd &` UX, but adds a "GUI tries to
/// auto-spawn so the user doesn't have to remember" convenience.
///
/// Flow (called by DaemonClient when first connect attempt fails):
///   1. Acquire `~/Library/Caches/nestty/.spawn.lock` via `flock(LOCK_EX|LOCK_NB)`.
///      - Lock acquired → proceed to (2).
///      - `EWOULDBLOCK` → another process is spawning. Sleep 50ms; caller retries connect.
///   2. Live socket probe (1s budget, 100ms per attempt × 10): attempt connect to
///      `~/Library/Caches/nestty/socket`. If it succeeds, another process won the race
///      and bound between our first attempt and acquiring the lock — release lock, return.
///   3. Locate `nesttyd` binary (PATH first, then `~/.cargo/bin/nesttyd`).
///   4. Detached spawn via `/bin/sh -c "nohup nesttyd >/dev/null 2>&1 &"` so the child
///      survives Nestty.app exit (no SIGHUP cascade) and we don't need `posix_spawn`
///      ceremony.
///   5. Protocol-level probe: connect + send `system.ping`, wait ≤500ms for `{ok:true}`.
///      Repeat up to 6 attempts (3s total budget per codex round 2 I4 — manifest
///      discovery + command registration happen before bind, so daemon may take
///      a moment to listen).
///   6. Release lock.
///
/// **No** stub when nesttyd binary missing — return failure, DaemonClient stays in
/// disconnected state, ActionRegistry fallback returns `daemon_unavailable` (matches
/// user-decided narrower fallback contract).
///
/// **No** nestctl auto-spawn (codex round 2 C2). Linux nestctl doesn't auto-spawn
/// either; this helper is Nestty.app-only.
enum AutoSpawn {
    /// Try to bring up `nesttyd`. Returns true if the daemon socket is now
    /// alive AND responsive to `system.ping`. Returns false on any failure
    /// (binary missing, fork failed, no ping response within budget).
    /// Caller (DaemonClient) interprets false as "stay disconnected, retry
    /// connect via reconnect loop".
    static func ensureRunning() -> Bool {
        do {
            try NesttyPaths.ensureRuntimeDir()
        } catch {
            log("ensureRuntimeDir: \(error)")
            return false
        }

        let lock: FileLock
        do {
            lock = try FileLock(path: NesttyPaths.spawnLock())
        } catch {
            log("open spawn lock: \(error)")
            return false
        }

        do {
            let acquired = try lock.tryAcquire()
            if !acquired {
                // Another process holds the lock. Don't wait inside the
                // lock; caller's reconnect loop will retry connect, and
                // by then either (a) the other spawner finished and
                // socket is alive, or (b) it failed and the lock is free
                // for us next iteration.
                log("spawn lock held by another process — caller should retry connect")
                return false
            }
        } catch {
            log("flock acquire: \(error)")
            return false
        }
        defer { lock.release() }

        // Step 2: live socket probe — someone may have bound between our
        // first failed connect and acquiring the lock.
        if probeSocket(timeout: 1.0) {
            log("daemon socket alive at lock-acquire (race winner) — skipping spawn")
            return true
        }

        // Step 3-4: locate + spawn.
        guard let nesttydPath = locateBinary() else {
            log("nesttyd binary not found in PATH or ~/.cargo/bin — install via `cargo install --path nestty-daemon`")
            return false
        }
        if !spawnDetached(path: nesttydPath) {
            return false
        }

        // Step 5: protocol-level probe with 3s overall budget.
        return waitForPing(budget: 3.0, perAttempt: 0.5)
    }

    // MARK: - Helpers

    private static func locateBinary() -> URL? {
        let env = ProcessInfo.processInfo.environment
        let pathString = env["PATH"] ?? "/usr/local/bin:/usr/bin:/bin"
        var dirs = pathString.split(separator: ":").map { String($0) }
        // Cargo's default bin dir — common for Rust users on macOS, not always on PATH
        // when launching a `.app` from Finder/Dock.
        let cargoBin = NSHomeDirectory() + "/.cargo/bin"
        if !dirs.contains(cargoBin) { dirs.append(cargoBin) }
        let fm = FileManager.default
        for dir in dirs {
            let candidate = URL(fileURLWithPath: dir).appending(path: "nesttyd")
            if fm.isExecutableFile(atPath: candidate.path) { return candidate }
        }
        return nil
    }

    /// Detached spawn so the child outlives Nestty.app. `nohup` suppresses
    /// SIGHUP, `&` backgrounds. stdout/stderr to /dev/null because
    /// Nestty.app's stderr is not a TTY the user reads.
    ///
    /// **NESTTY_SOCKET handling** (codex round 2 C1): `nesttyd` honors the
    /// inherited `NESTTY_SOCKET` to choose its bind path. `NesttyPaths.daemonSocket()`
    /// deliberately ignores legacy per-GUI-socket overrides (so launching
    /// Nestty.app from a child shell still routes the client to the daemon).
    /// To keep the daemon's bind path in sync with the client's connect
    /// path, we explicitly set `NESTTY_SOCKET` on the child to the *resolved*
    /// daemon socket path. Without this, a child shell launched from
    /// Nestty.app inherits a per-GUI socket env, the daemon binds there,
    /// the Swift client probes `~/Library/Caches/nestty/socket`, and the
    /// two never meet.
    private static func spawnDetached(path: URL) -> Bool {
        let proc = Process()
        proc.executableURL = URL(fileURLWithPath: "/bin/sh")
        let escaped = path.path(percentEncoded: false).replacingOccurrences(of: "'", with: "'\\''")
        proc.arguments = ["-c", "nohup '\(escaped)' >/dev/null 2>&1 &"]
        var env = ProcessInfo.processInfo.environment
        env["NESTTY_SOCKET"] = NesttyPaths.daemonSocket().path(percentEncoded: false)
        proc.environment = env
        do {
            try proc.run()
        } catch {
            log("spawn fork: \(error)")
            return false
        }
        proc.waitUntilExit()
        return proc.terminationStatus == 0
    }

    /// Try a Unix socket connect. No protocol traffic — just verifies the
    /// listener is bound. Used for the race-winner check at step 2.
    /// **Closes** the probe fd before returning success so we don't leak
    /// (codex round-2 review I1).
    private static func probeSocket(timeout: TimeInterval) -> Bool {
        let deadline = Date().addingTimeInterval(timeout)
        while Date() < deadline {
            if let fd = connectOnce() {
                close(fd)
                return true
            }
            Thread.sleep(forTimeInterval: 0.1)
        }
        return false
    }

    /// Protocol probe: connect, send `{"id": "<uuid>", "method": "system.ping"}`,
    /// read response line, verify `ok=true`. Caller controls retry budget.
    private static func waitForPing(budget: TimeInterval, perAttempt: TimeInterval) -> Bool {
        let deadline = Date().addingTimeInterval(budget)
        var attempt = 0
        while Date() < deadline {
            attempt += 1
            if pingOnce(timeout: perAttempt) {
                log("daemon ack'd system.ping on attempt \(attempt)")
                return true
            }
            Thread.sleep(forTimeInterval: 0.2)
        }
        log("daemon did not ack system.ping within \(budget)s — auto-spawn FAILED, stays disconnected")
        return false
    }

    private static func connectOnce() -> Int32? {
        let fd = socket(AF_UNIX, SOCK_STREAM, 0)
        guard fd >= 0 else { return nil }
        var addr = sockaddr_un()
        addr.sun_family = sa_family_t(AF_UNIX)
        let path = NesttyPaths.daemonSocket().path(percentEncoded: false)
        let cstr = path.utf8CString
        if cstr.count > MemoryLayout.size(ofValue: addr.sun_path) {
            close(fd)
            return nil
        }
        withUnsafeMutablePointer(to: &addr.sun_path) { p in
            p.withMemoryRebound(to: CChar.self, capacity: cstr.count) { dst in
                _ = cstr.withUnsafeBufferPointer { src in
                    memcpy(dst, src.baseAddress, cstr.count)
                }
            }
        }
        let rc = withUnsafePointer(to: &addr) { addrPtr -> Int32 in
            addrPtr.withMemoryRebound(to: sockaddr.self, capacity: 1) { sa in
                Darwin.connect(fd, sa, socklen_t(MemoryLayout<sockaddr_un>.size))
            }
        }
        if rc != 0 { close(fd); return nil }
        return fd
    }

    private static func pingOnce(timeout _: TimeInterval) -> Bool {
        guard let fd = connectOnce() else { return false }
        defer { close(fd) }
        let id = UUID().uuidString
        let req = "{\"id\":\"\(id)\",\"method\":\"system.ping\",\"params\":{}}\n"
        var bytes = Array(req.utf8)
        let sent = bytes.withUnsafeMutableBufferPointer { buf -> Int in
            Darwin.write(fd, buf.baseAddress, buf.count)
        }
        if sent <= 0 { return false }

        // Read up to 4KB of response, line-buffered. Use SO_RCVTIMEO so a
        // wedged daemon doesn't block forever; perAttempt budget enforced
        // by caller's loop.
        var tv = timeval(tv_sec: 0, tv_usec: 500_000) // 500ms
        _ = setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, socklen_t(MemoryLayout<timeval>.size))

        var buf = [UInt8](repeating: 0, count: 4096)
        let n = buf.withUnsafeMutableBufferPointer { bp -> Int in
            Darwin.read(fd, bp.baseAddress, bp.count)
        }
        if n <= 0 { return false }
        let line = String(decoding: buf.prefix(n), as: UTF8.self)
        // Minimal parse: presence of `"ok":true` is enough; full JSON parse
        // is overkill for a probe.
        return line.contains("\"ok\":true") && line.contains(id)
    }

    private static func log(_ msg: String) {
        FileHandle.standardError.write(Data("[nestty-autospawn] \(msg)\n".utf8))
    }
}
