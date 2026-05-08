import AppKit
import Foundation

/// Tier 4.2 — runs one status-bar module's `exec` shell command on a repeating
/// timer and pushes the parsed output into its label.
///
/// Threading:
/// - Timer fires on a serial DispatchQueue per module so a slow-running
///   command on one module can't stall the others.
/// - Process completion handler (`terminationHandler`) reads stdout off the
///   timer queue, parses, then hops to main to update the label.
///
/// Output protocol (matches `nestty-linux/src/statusbar.rs::parse_output`):
/// - JSON `{"text": "...", "tooltip": "..."}` — both fields optional, text
///   defaults to the raw stdout if missing.
/// - Plain text — used as-is.
/// `@unchecked Sendable` because the timer fires on a background queue while
/// the label is touched from main; we route label writes through
/// `DispatchQueue.main.async` so the cross-actor hop is explicit. The mutable
/// `stopped` and `timer` fields are guarded by the same serial queue that
/// owns `runOnce`, plus `start`/`stop` are only called from main during
/// view setup + teardown.
final class StatusModuleRunner: @unchecked Sendable {
    private nonisolated(unsafe) weak var label: NSTextField?
    private let pluginDir: URL
    private let moduleName: String
    private let exec: String
    private let interval: Int
    private let socketPath: String
    private let queue: DispatchQueue
    private var timer: DispatchSourceTimer?
    private nonisolated(unsafe) var stopped = false

    init(
        label: NSTextField,
        pluginDir: URL,
        moduleName: String,
        exec: String,
        interval: Int,
        socketPath: String,
    ) {
        self.label = label
        self.pluginDir = pluginDir
        self.moduleName = moduleName
        self.exec = exec
        self.interval = max(1, interval)
        self.socketPath = socketPath
        // Serial queue per module so terminationHandler completion can't
        // race with the next timer tick on the same module.
        queue = DispatchQueue(label: "nestty.statusbar.\(moduleName)", qos: .utility)
    }

    func start() {
        let t = DispatchSource.makeTimerSource(queue: queue)
        t.schedule(deadline: .now(), repeating: .seconds(interval))
        t.setEventHandler { [weak self] in
            self?.runOnce()
        }
        t.resume()
        timer = t
    }

    func stop() {
        stopped = true
        timer?.cancel()
        timer = nil
    }

    /// One iteration: spawn `sh -c <exec>`, capture stdout, parse, update label.
    /// Runs on the module's serial queue. Process spawn errors fall through
    /// to a stderr log + label kept at last value (no flicker on transient
    /// failures). Long-running commands that exceed `interval` will queue up
    /// the next tick on the same queue — by design (we won't run two copies
    /// of a slow command concurrently for the same module).
    private func runOnce() {
        if stopped { return }
        let process = Process()
        process.executableURL = URL(fileURLWithPath: "/bin/sh")
        process.arguments = ["-c", exec]
        process.currentDirectoryURL = pluginDir
        var env = ProcessInfo.processInfo.environment
        env["NESTTY_SOCKET"] = socketPath
        env["NESTTY_PLUGIN_DIR"] = pluginDir.path
        process.environment = env

        let stdoutPipe = Pipe()
        let stderrPipe = Pipe()
        process.standardOutput = stdoutPipe
        process.standardError = stderrPipe

        do {
            try process.run()
        } catch {
            FileHandle.standardError.write(Data("[nestty] statusbar \(moduleName) spawn failed: \(error)\n".utf8))
            return
        }

        // Block this serial queue until the child exits. Process bookkeeping
        // is cheap; the timer interval is what controls actual throughput.
        process.waitUntilExit()

        let outData = stdoutPipe.fileHandleForReading.readDataToEndOfFile()
        let errData = stderrPipe.fileHandleForReading.readDataToEndOfFile()
        if process.terminationStatus != 0 {
            let errStr = String(data: errData, encoding: .utf8) ?? "<binary>"
            FileHandle.standardError.write(Data("[nestty] statusbar \(moduleName) exec error: \(errStr)\n".utf8))
            return
        }

        let raw = String(data: outData, encoding: .utf8) ?? ""
        let (text, tooltip) = Self.parseOutput(raw)
        let labelBox = SendableBox(label)
        DispatchQueue.main.async {
            guard let label = labelBox.value else { return }
            label.stringValue = text
            label.toolTip = tooltip
        }
    }

    /// `{text, tooltip}` JSON or plain text. `serde_json::Value` parsing
    /// matches `nestty-linux/src/statusbar.rs::parse_output` byte-for-byte:
    /// trim whitespace, attempt JSON only when the trimmed string looks
    /// like an object (starts with `{`).
    static func parseOutput(_ raw: String) -> (String, String?) {
        let trimmed = raw.trimmingCharacters(in: .whitespacesAndNewlines)
        if trimmed.hasPrefix("{"),
           let data = trimmed.data(using: .utf8),
           let dict = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any]
        {
            let text = (dict["text"] as? String) ?? trimmed
            let tooltip = dict["tooltip"] as? String
            return (text, tooltip)
        }
        return (trimmed, nil)
    }
}
