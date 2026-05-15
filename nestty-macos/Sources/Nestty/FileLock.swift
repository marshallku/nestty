import Darwin
import Foundation

/// Thin Swift wrapper around `lockf(2)` for single-flight gating.
///
/// Used by `AutoSpawn` to ensure only one process forks `nesttyd` when
/// multiple Nestty.app instances launch concurrently with no daemon running.
/// `lockf(F_TLOCK)` semantics: non-blocking exclusive advisory lock on a byte
/// range; for our single-byte file the range and per-fd lock are equivalent
/// to `flock`'s per-fd whole-file lock.
///
/// **Why `lockf` not `flock`**: Swift's Darwin module exports `struct flock`
/// (the byte-range descriptor used by `fcntl`), which collides with the
/// `flock(_:_:)` function's name — Swift resolves the type first and the
/// function call fails to compile. `lockf` has no name collision.
///
/// **Not** an actor — auto-spawn flow is straight-line synchronous, and the
/// fd is owned exclusively by one helper at a time.
final class FileLock {
    private let fd: Int32
    private(set) var holding = false

    /// Open or create the lock file. The file itself is just an inode anchor;
    /// its content is irrelevant. Permissions 0o600 since `~/Library/Caches/`
    /// is per-user.
    init(path: URL) throws {
        let cstr = path.path(percentEncoded: false)
        let opened = open(cstr, O_CREAT | O_RDWR, 0o600)
        if opened < 0 {
            let err = String(cString: strerror(errno))
            throw FileLockError.openFailed(path: cstr, message: err)
        }
        fd = opened
    }

    deinit {
        if holding { _ = Darwin.lockf(fd, F_ULOCK, 0) }
        Darwin.close(fd)
    }

    /// Non-blocking exclusive acquire. Returns `true` on success, `false` if
    /// another process holds it (`EAGAIN` / `EWOULDBLOCK`). Throws on real
    /// errors.
    func tryAcquire() throws -> Bool {
        let rc = Darwin.lockf(fd, F_TLOCK, 0)
        if rc == 0 {
            holding = true
            return true
        }
        if errno == EAGAIN || errno == EACCES {
            // Both are acceptable "held by someone else" returns per POSIX
            return false
        }
        let err = String(cString: strerror(errno))
        throw FileLockError.lockfFailed(message: err)
    }

    /// Release the lock. Idempotent.
    func release() {
        guard holding else { return }
        _ = Darwin.lockf(fd, F_ULOCK, 0)
        holding = false
    }
}

enum FileLockError: Error, CustomStringConvertible {
    case openFailed(path: String, message: String)
    case lockfFailed(message: String)

    var description: String {
        switch self {
        case let .openFailed(path, message): "FileLock open(\(path)): \(message)"
        case let .lockfFailed(message): "FileLock lockf: \(message)"
        }
    }
}
