import Foundation

/// Watches the nestty config file for changes using kqueue via DispatchSource.
///
/// Handles the "write via rename" pattern used by most text editors (vim, nano, etc.):
/// editors write to a temp file then rename it over the original, so we watch
/// for both `.write` and `.rename`/`.delete` events.
///
/// On rename/delete the original fd becomes stale; the watcher automatically
/// reopens the file and installs a fresh source after a short delay.
///
/// If the config file does not exist when `start()` is called (fresh install),
/// the watcher retries every 2 seconds until the file appears.
///
/// All methods MUST be called on the main queue. The DispatchSource handler and
/// asyncAfter closures both target the main queue, so all state access is serialised.
/// `@unchecked Sendable` silences the Swift 6 cross-actor sending diagnostic;
/// the single-queue invariant provides the actual safety guarantee.
final class ConfigWatcher: @unchecked Sendable {
    private nonisolated(unsafe) var source: (any DispatchSourceFileSystemObject)?
    private nonisolated(unsafe) var debounceWork: DispatchWorkItem?
    private nonisolated(unsafe) var retryWork: DispatchWorkItem?

    /// Called on the main queue after a config change is detected.
    var onChange: (() -> Void)?

    /// Resolved (symlink-expanded) URL so kqueue watches the real inode.
    private let url: URL

    init(url: URL) {
        // Resolve symlinks so kqueue watches the target file, not the symlink inode.
        // Common with dotfile managers that symlink ~/.config into a managed repo.
        self.url = url.resolvingSymlinksInPath()
    }

    // MARK: - Public

    func start() {
        openAndWatch()
    }

    func stop() {
        retryWork?.cancel()
        retryWork = nil
        debounceWork?.cancel()
        debounceWork = nil
        source?.cancel()
        source = nil
        // fd is closed in the source's cancelHandler
    }

    // MARK: - Private

    private func openAndWatch() {
        let fd = open(url.path, O_EVTONLY)
        guard fd != -1 else {
            // Config file doesn't exist yet (e.g. fresh install before --init-config).
            // Retry every 2 s until the file appears.
            scheduleRetry()
            return
        }

        // File found — cancel any pending retry.
        retryWork?.cancel()
        retryWork = nil

        let src = DispatchSource.makeFileSystemObjectSource(
            fileDescriptor: fd,
            eventMask: [.write, .rename, .delete],
            queue: .main,
        )

        src.setEventHandler { [weak self, weak src] in
            guard let self else { return }
            let flags = src?.data ?? []
            scheduleReload()
            // rename/delete means the fd is now stale — restart to track the new file.
            if flags.contains(.rename) || flags.contains(.delete) {
                restartAfterDelay()
            }
        }

        src.setCancelHandler { [fd] in
            close(fd)
        }

        source = src
        src.resume()
    }

    private func scheduleReload() {
        debounceWork?.cancel()
        let work = DispatchWorkItem { [weak self] in self?.onChange?() }
        debounceWork = work
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.2, execute: work)
    }

    private func restartAfterDelay() {
        // Cancel only the source (the fd is stale after rename/delete).
        // Do NOT cancel debounceWork — it was scheduled before restartAfterDelay and
        // the file content is already final by the time kqueue fires.
        // We re-open the new file AFTER the debounce fires so we don't fight the
        // editor's atomic-write sequence.
        source?.cancel()
        source = nil
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.5) { [weak self] in
            self?.openAndWatch()
        }
    }

    private func scheduleRetry() {
        retryWork?.cancel()
        let work = DispatchWorkItem { [weak self] in self?.openAndWatch() }
        retryWork = work
        DispatchQueue.main.asyncAfter(deadline: .now() + 2.0, execute: work)
    }
}
