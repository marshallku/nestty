import Foundation

/// Centralized path utility — mirrors `nestty-core::paths::daemon_socket_path()`
/// so Swift and Rust agree on where the daemon socket lives.
///
/// macOS runtime dir: `~/Library/Caches/nestty/`. The Rust side already uses
/// the same root (verified `nestty-core/src/paths.rs:20`); duplicating here
/// avoids adding an FFI call just to read a constant path.
enum NesttyPaths {
    /// `~/Library/Caches/nestty`. Created on demand by daemon (perms 0700)
    /// or by Swift helpers (FileLock, etc).
    static func runtimeDir() -> URL {
        let home = FileManager.default.homeDirectoryForCurrentUser
        return home.appending(path: "Library/Caches/nestty")
    }

    /// Daemon Unix socket path. Mirrors `nestty-core::paths::daemon_socket_path()`:
    ///
    /// 1. `NESTTY_SOCKET` env override wins UNLESS it points at a legacy
    ///    per-instance GUI socket shape (`/tmp/nestty-{PID}.sock` or
    ///    `<runtime_dir>/gui-{PID}.sock`). A child shell launched from
    ///    Nestty.app inherits `NESTTY_SOCKET` set to the per-GUI socket
    ///    so `nestctl` calls reach the originating window — but the GUI
    ///    socket doesn't speak the daemon wire protocol, so falling
    ///    through to the well-known daemon path keeps daemon clients
    ///    correctly routed when launched from such a child shell.
    /// 2. Otherwise: `<runtime_dir>/socket`.
    ///
    /// **Why not just hard-code the default**: `AutoSpawn` inherits this
    /// process's env when forking `nesttyd`, so `nesttyd` would bind whatever
    /// `NESTTY_SOCKET` points to. If the Swift client hard-coded the default,
    /// it would wait for connect on a path the daemon never bound. Codex PR2
    /// review C1 (INTENT-MISMATCH).
    static func daemonSocket() -> URL {
        if let override = ProcessInfo.processInfo.environment["NESTTY_SOCKET"], !override.isEmpty {
            let url = URL(fileURLWithPath: override)
            if !isLegacyPerInstanceSocket(url) {
                return url
            }
            // Fall through — child shell inherited GUI socket env.
        }
        return runtimeDir().appending(path: "socket")
    }

    /// True for `/tmp/nestty-{PID}.sock` (older builds) or
    /// `<runtime_dir>/gui-{PID}.sock` (current). Mirror of
    /// `nestty-core::paths::is_legacy_per_instance_socket`.
    static func isLegacyPerInstanceSocket(_ url: URL) -> Bool {
        let s = url.path(percentEncoded: false)
        // /tmp/nestty-{digits}.sock
        if let middle = s.stripPrefix("/tmp/nestty-")?.stripSuffix(".sock"),
           !middle.isEmpty, middle.allSatisfy(\.isASCIIDigit)
        {
            return true
        }
        // <runtime_dir>/gui-{digits}.sock
        let runtime = runtimeDir().path(percentEncoded: false)
        if let trimmed = s.stripPrefix(runtime + "/"),
           let middle = trimmed.stripPrefix("gui-")?.stripSuffix(".sock"),
           !middle.isEmpty, middle.allSatisfy(\.isASCIIDigit)
        {
            return true
        }
        return false
    }

    /// Single-flight lock for nesttyd auto-spawn.
    static func spawnLock() -> URL {
        runtimeDir().appending(path: ".spawn.lock")
    }

    /// Ensure the runtime dir exists with restrictive perms. Idempotent.
    static func ensureRuntimeDir() throws {
        let dir = runtimeDir()
        try FileManager.default.createDirectory(
            at: dir,
            withIntermediateDirectories: true,
            attributes: [.posixPermissions: 0o700],
        )
    }
}

private extension String {
    func stripPrefix(_ prefix: String) -> String? {
        guard hasPrefix(prefix) else { return nil }
        return String(dropFirst(prefix.count))
    }

    func stripSuffix(_ suffix: String) -> String? {
        guard hasSuffix(suffix) else { return nil }
        return String(dropLast(suffix.count))
    }
}

private extension Character {
    var isASCIIDigit: Bool { isASCII && isNumber }
}
