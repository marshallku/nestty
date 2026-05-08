import Foundation

/// macOS service plugin supervisor. Discovers manifests, spawns services
/// (eager or lazy depending on activation), runs the init handshake, registers
/// `provides[]` actions in `ActionRegistry`, routes `action.invoke` over
/// stdio, fans `event.publish` notifications onto `EventBus` so the trigger
/// engine sees them, sends `shutdown` on app quit.
///
/// Activation supported:
///
/// - `onStartup` (PR 3) — eager spawn at app launch. Required for plugins
///   that must publish events from boot (echo heartbeat, calendar poller).
/// - `onAction:<glob>` (PR 6a) — register placeholder handlers per
///   `provides[]`; spawn on first matching action call via `LazyEntry.ensure`.
///   Spawn runs off main thread so the 5s init-handshake ceiling can't
///   freeze the UI. Concurrent first-callers serialize behind the entry's
///   lock.
///
/// Still missing:
///
/// - **`onEvent:<glob>`** activation. Eager spawn defeats the point (plugin
///   should only run when matching events arrive); needs the trigger engine
///   to drive a "spawn on event match" path. Skipped with log today.
/// - **Restart-on-crash policy.** A crashed plugin stays dead for the
///   lifetime of this nestty process. Hot-reload of the config doesn't
///   recreate `LazyEntry` either, so an entry in `.failed` stays failed.
/// - **Outbound `event.dispatch`.** Plugins that subscribe to bus events
///   (`subscribes = [...]`) don't yet receive forwarded events.
/// - **`provides[]` conflict resolution** across plugins.
/// - **Process-group ownership + signal handlers + crash-recovery PID
///   file** (codex I5). We rely today on stdin-EOF cascade + `shutdown`
///   notification + `process.terminate()`.
///
/// Threading model:
///
/// - `discoverAndStart` runs on `@MainActor` (called from AppDelegate).
/// - Each `PluginProcess` owns a dedicated reader thread that reads NDJSON
///   from stdout, looks up the response by id in a lock-protected pending
///   dict, and bounces the completion back to `DispatchQueue.main.async`.
/// - Stdin writes happen from the calling thread (main actor for action
///   dispatch, init handshake thread for the initial frame). `FileHandle.write`
///   is thread-safe per Apple docs and our writes are atomic per JSON
///   line so interleaving across threads is fine in practice.
@MainActor
final class PluginSupervisor {
    private let registry: ActionRegistry
    let eventBus: EventBus
    /// Eagerly-spawned (`onStartup`) processes. Lazy processes are owned by
    /// LazyEntry instances after first spawn — collected separately at
    /// shutdown time.
    private var processes: [PluginProcess] = []
    /// Lazy `onAction:<glob>` entries keyed by the service identifier so we
    /// can iterate them at shutdown.
    private var lazyEntries: [String: LazyEntry] = [:]

    init(registry: ActionRegistry, eventBus: EventBus) {
        self.registry = registry
        self.eventBus = eventBus
    }

    /// Discover every manifest, spawn services with `activation = onStartup`,
    /// run init handshake, register provided actions in the registry. Idempotent
    /// only if called once per process — safe to call multiple times only after
    /// `shutdown()` has reset state.
    func discoverAndStart() {
        let manifests = PluginManifestStore.discover()
        for loaded in manifests {
            for service in loaded.manifest.services {
                // Activation handling:
                //
                // - `onStartup` → spawn eagerly. Plugins that need to publish
                //   events from boot (slack, calendar, echo heartbeat) need
                //   to be alive before the first user interaction.
                // - `onAction:<glob>` → register placeholder handlers per
                //   provides[]; spawn on first call. Saves cold-start cost
                //   for plugins that may not be invoked every session
                //   (git, llm, kb-when-it-lands).
                // - `onEvent:<glob>` → skip with log. Eager spawn defeats
                //   the point (plugin should only run when matching events
                //   arrive) and the trigger engine doesn't yet drive
                //   spawn-on-event. Future work.
                let activation = service.activation
                if activation == "onStartup" {
                    spawnEager(loaded: loaded, service: service)
                } else if activation.hasPrefix("onAction:") {
                    registerLazy(loaded: loaded, service: service)
                } else if activation.hasPrefix("onEvent:") {
                    let msg = "[nestty-plugin] \(loaded.manifest.plugin.name)/\(service.name): activation \(activation) needs event-driven spawn — skipping (TODO)\n"
                    FileHandle.standardError.write(Data(msg.utf8))
                } else {
                    let msg = "[nestty-plugin] \(loaded.manifest.plugin.name)/\(service.name): unknown activation '\(activation)' — skipping\n"
                    FileHandle.standardError.write(Data(msg.utf8))
                }
            }
        }
    }

    private func spawnEager(loaded: LoadedPluginManifest, service: PluginServiceDef) {
        guard let proc = PluginProcess.spawn(loaded: loaded, service: service) else {
            return
        }
        proc.eventBus = eventBus
        processes.append(proc)
        for actionName in proc.provides {
            registry.register(actionName) { [weak proc, actionName] params, completion in
                guard let proc else {
                    completion(RPCError(
                        code: "plugin_unavailable",
                        message: "plugin providing \(actionName) is no longer running",
                    ))
                    return
                }
                proc.invoke(action: actionName, params: params, completion: completion)
            }
        }
    }

    /// Register placeholder handlers for each `provides[]` entry without
    /// spawning the plugin yet. First call to any of these handlers triggers
    /// `LazyEntry.ensure` which kicks the spawn off-main and serializes
    /// concurrent first-callers behind a lock. Once the plugin is up the
    /// same handler stays in place; ensure() is a fast lock + early-return
    /// on subsequent calls.
    private func registerLazy(loaded: LoadedPluginManifest, service: PluginServiceDef) {
        let key = "\(loaded.manifest.plugin.name)/\(service.name)"
        let entry = LazyEntry(loaded: loaded, service: service, eventBus: eventBus)
        lazyEntries[key] = entry
        for actionName in service.provides {
            registry.register(actionName) { [weak entry, actionName] params, completion in
                guard let entry else {
                    completion(RPCError(
                        code: "plugin_unavailable",
                        message: "lazy plugin entry for \(actionName) is gone",
                    ))
                    return
                }
                entry.ensure { result in
                    switch result {
                    case let .failure(err):
                        completion(err)
                    case let .success(proc):
                        proc.invoke(action: actionName, params: params, completion: completion)
                    }
                }
            }
        }
    }

    /// Send `shutdown` notification to every plugin and wait briefly before
    /// force-terminating. Called from `applicationWillTerminate`.
    func shutdown() {
        for p in processes {
            p.shutdown()
        }
        processes.removeAll()
        for entry in lazyEntries.values {
            entry.shutdown()
        }
        lazyEntries.removeAll()
    }
}

// MARK: - LazyEntry

/// Tracks a `onAction:<glob>` plugin's spawn lifecycle. State machine:
///
/// - `.notStarted` — initial. First `ensure` call moves to `.spawning` and
///   kicks off the spawn worker.
/// - `.spawning` — a background thread is running `PluginProcess.spawn`
///   right now. New `ensure` callers append their completion to `waiters`
///   and block (well, store-and-return — completion fires async when
///   spawn finishes).
/// - `.ready(proc)` — plugin alive. `ensure` returns the cached proc
///   immediately.
/// - `.failed(err)` — spawn failed and we're not retrying. `ensure`
///   returns the cached error so callers see a consistent message instead
///   of attempting to spawn again per-call (cheap to retry, but log spam
///   from a permanently-broken plugin gets old fast). Hot-reload (PR 5c
///   `setTriggers` reapply) is the user's escape hatch — we don't
///   currently re-create LazyEntry on config reload, so a `failed` entry
///   stays failed for the lifetime of this nestty process.
final class LazyEntry: @unchecked Sendable {
    private let loaded: LoadedPluginManifest
    private let service: PluginServiceDef
    private weak var eventBus: EventBus?

    private let lock = NSLock()
    private var state: State = .notStarted
    private var waiters: [(Result<PluginProcess, RPCError>) -> Void] = []

    enum State {
        case notStarted
        case spawning
        case ready(PluginProcess)
        case failed(RPCError)
    }

    init(loaded: LoadedPluginManifest, service: PluginServiceDef, eventBus: EventBus) {
        self.loaded = loaded
        self.service = service
        self.eventBus = eventBus
    }

    /// Resolve the proc synchronously if `.ready`, otherwise kick off (or
    /// join an in-flight) spawn and call completion when ready. Completion
    /// fires on the spawn worker thread for the first caller, or directly
    /// on the calling thread for the cached-state cases.
    func ensure(completion: @escaping (Result<PluginProcess, RPCError>) -> Void) {
        lock.lock()
        switch state {
        case let .ready(proc):
            lock.unlock()
            completion(.success(proc))
        case let .failed(err):
            lock.unlock()
            completion(.failure(err))
        case .spawning:
            waiters.append(completion)
            lock.unlock()
        case .notStarted:
            state = .spawning
            waiters.append(completion)
            lock.unlock()
            // Off-main background spawn. We avoid main actor here because
            // spawn includes a 5s init-handshake semaphore wait — blocking
            // main for that long during a plugin first-call would freeze UI.
            DispatchQueue.global(qos: .userInitiated).async { [weak self] in
                self?.runSpawn()
            }
        }
    }

    private func runSpawn() {
        let pluginName = loaded.manifest.plugin.name
        let serviceName = service.name
        FileHandle.standardError.write(Data("[nestty-plugin] \(pluginName)/\(serviceName): lazy spawn (first call)\n".utf8))

        let result: Result<PluginProcess, RPCError>
        if let proc = PluginProcess.spawn(loaded: loaded, service: service) {
            // Hook event bus same as eager path so plugin event.publish
            // notifications still flow into the trigger engine.
            proc.eventBus = eventBus
            result = .success(proc)
        } else {
            result = .failure(RPCError(
                code: "spawn_failed",
                message: "lazy spawn failed for \(pluginName)/\(serviceName) — see [nestty-plugin] stderr for details",
            ))
        }

        lock.lock()
        switch result {
        case let .success(proc):
            state = .ready(proc)
        case let .failure(err):
            state = .failed(err)
        }
        let toFire = waiters
        waiters.removeAll()
        lock.unlock()

        for waiter in toFire {
            waiter(result)
        }
    }

    /// Tear down the spawned process if any. Called from supervisor.shutdown.
    func shutdown() {
        lock.lock()
        let toShutdown: PluginProcess? = if case let .ready(proc) = state {
            proc
        } else {
            nil
        }
        // Move state to a terminal so any future ensure() reports cleanly
        // rather than racing with an in-progress shutdown.
        state = .failed(RPCError(code: "supervisor_shutdown", message: "supervisor shutdown"))
        let strandedWaiters = waiters
        waiters.removeAll()
        lock.unlock()

        toShutdown?.shutdown()
        for waiter in strandedWaiters {
            waiter(.failure(RPCError(code: "supervisor_shutdown", message: "supervisor shutdown before lazy spawn completed")))
        }
    }
}

// MARK: - PluginProcess

/// Wraps a single supervised plugin subprocess: stdio pipes, reader thread,
/// pending-request dict, and the methods to send action frames.
///
/// The class is `@unchecked Sendable` because reader threads access the
/// pending dict from off-main; access is serialized through `lock` and
/// completions always bounce back to main actor before the host's closure
/// fires, so the unsafety is contained to the dict mutation.
final class PluginProcess: @unchecked Sendable {
    let pluginName: String
    private(set) var provides: [String] = []
    /// Set by the supervisor before spawn returns. Plugin notifications
    /// of kind `event.publish` get fanned onto this bus so the trigger
    /// engine (and any other subscriber) sees them. Optional so unit
    /// tests / dry-runs can construct a process without an event bus.
    weak var eventBus: EventBus?

    private let process: Process
    private let stdinHandle: FileHandle
    private let stdoutHandle: FileHandle
    private let stderrHandle: FileHandle

    /// Pending request completions keyed by frame id. Reader thread pops
    /// entries when responses arrive. NSLock is fine here — contention is
    /// trivial (one writer per dispatch, one reader per response).
    private let lock = NSLock()
    private var pending: [String: (Any?) -> Void] = [:]

    private init(
        name: String,
        process: Process,
        stdin: FileHandle,
        stdout: FileHandle,
        stderr: FileHandle,
    ) {
        pluginName = name
        self.process = process
        stdinHandle = stdin
        stdoutHandle = stdout
        stderrHandle = stderr
    }

    /// Spawn the plugin binary, run the `initialize` handshake, send the
    /// `initialized` notification. Returns nil on any failure (binary
    /// missing, spawn error, init timeout, init error response, missing
    /// provides[]) — caller logs the failure and skips this service.
    static func spawn(
        loaded: LoadedPluginManifest,
        service: PluginServiceDef,
    ) -> PluginProcess? {
        let pluginName = loaded.manifest.plugin.name
        guard let execURL = resolveExecutable(service.exec, in: loaded.dir) else {
            log("[nestty-plugin] \(pluginName)/\(service.name): exec '\(service.exec)' not found in plugin dir or PATH")
            return nil
        }

        let process = Process()
        process.executableURL = execURL
        process.arguments = service.args
        let stdin = Pipe()
        let stdout = Pipe()
        let stderr = Pipe()
        process.standardInput = stdin
        process.standardOutput = stdout
        process.standardError = stderr

        do {
            try process.run()
        } catch {
            log("[nestty-plugin] \(pluginName)/\(service.name): spawn failed: \(error)")
            return nil
        }

        let pp = PluginProcess(
            name: pluginName,
            process: process,
            stdin: stdin.fileHandleForWriting,
            stdout: stdout.fileHandleForReading,
            stderr: stderr.fileHandleForReading,
        )
        pp.startReaderThread()
        pp.startStderrLoggerThread()

        // Init handshake. Use a class-bound "init box" to ferry the result
        // out of the reader thread's @Sendable closure without tripping the
        // strict-concurrency checker on captured `var`s.
        let initBox = InitBox()
        let initId = "init-\(UUID().uuidString)"
        pp.lock.lock()
        pp.pending[initId] = { [initBox] response in
            initBox.resolve(response)
        }
        pp.lock.unlock()

        pp.sendFrame([
            "id": initId,
            "method": "initialize",
            "params": ["protocol_version": 1],
        ])

        // 5s ceiling matches what feels reasonable for echo. Production
        // plugins (Slack/Calendar) doing OAuth on first run may want
        // longer; revisit when they land.
        guard initBox.semaphore.wait(timeout: .now() + .seconds(5)) == .success else {
            log("[nestty-plugin] \(pluginName)/\(service.name): init timeout")
            pp.shutdown()
            return nil
        }

        switch initBox.outcome {
        case .none:
            log("[nestty-plugin] \(pluginName)/\(service.name): init returned without populating outcome (programmer error)")
            pp.shutdown()
            return nil
        case let .failure(err):
            log("[nestty-plugin] \(pluginName)/\(service.name): init failed: \(err)")
            pp.shutdown()
            return nil
        case let .success(result):
            // Validate provides[] subset matches the manifest declaration.
            // Linux is strict: runtime subset OK, runtime superset rejected.
            // We mirror that — a plugin that claims more at runtime than the
            // manifest promised could shadow another plugin's actions.
            let runtimeProvides = result["provides"] as? [String] ?? []
            let manifestProvides = Set(service.provides)
            let unexpected = runtimeProvides.filter { !manifestProvides.contains($0) }
            if !unexpected.isEmpty {
                log("[nestty-plugin] \(pluginName)/\(service.name): runtime provides \(unexpected) not in manifest \(service.provides) — rejecting")
                pp.shutdown()
                return nil
            }
            pp.provides = runtimeProvides
        }

        // Notification (no id) — plugin starts emitting events from this point.
        pp.sendFrame(["method": "initialized"])

        log("[nestty-plugin] \(pluginName)/\(service.name): ready (provides: \(pp.provides.joined(separator: ", ")))")
        return pp
    }

    /// Send an `action.invoke` request, store completion keyed by id, return.
    /// Reader thread fires the completion when the response arrives. Completion
    /// receives either the plugin's `result` Any value or an `RPCError` for
    /// `ok: false` responses.
    func invoke(
        action: String,
        params: [String: Any],
        completion: @escaping (Any?) -> Void,
    ) {
        let id = UUID().uuidString
        lock.lock()
        pending[id] = completion
        lock.unlock()

        sendFrame([
            "id": id,
            "method": "action.invoke",
            "params": [
                "name": action,
                "params": params,
            ],
        ])
    }

    /// Send shutdown notification, wait briefly, then terminate. Idempotent.
    func shutdown() {
        if process.isRunning {
            sendFrame(["method": "shutdown"])
            // Echo's shutdown is `std::process::exit(0)` so it goes away fast.
            // 200ms ceiling is plenty; longer plugins get terminated cleanly
            // by the next branch.
            Thread.sleep(forTimeInterval: 0.2)
        }
        if process.isRunning {
            process.terminate()
        }
        // Fail any still-pending requests so callers don't hang forever.
        lock.lock()
        let stranded = pending
        pending.removeAll()
        lock.unlock()
        for completion in stranded.values {
            // Direct invocation — same reasoning as `handleLine`. Completions
            // here are leaf closures from SocketServer that just signal
            // a semaphore; no actor isolation required.
            completion(RPCError(
                code: "plugin_shutdown",
                message: "plugin shut down before responding",
            ))
        }
    }

    // MARK: - Internal IO

    private func sendFrame(_ frame: [String: Any]) {
        guard let data = try? JSONSerialization.data(withJSONObject: frame),
              let str = String(data: data, encoding: .utf8)
        else {
            log("[nestty-plugin] \(pluginName): failed to serialize frame")
            return
        }
        var line = Data(str.utf8)
        line.append(UInt8(ascii: "\n"))
        do {
            try stdinHandle.write(contentsOf: line)
        } catch {
            log("[nestty-plugin] \(pluginName): stdin write failed: \(error)")
        }
    }

    private func startReaderThread() {
        Thread.detachNewThread { [weak self] in
            self?.readerLoop()
        }
    }

    private func readerLoop() {
        var buffer = Data()
        while true {
            let chunk = stdoutHandle.availableData
            if chunk.isEmpty { break } // EOF — plugin exited
            buffer.append(chunk)
            while let nl = buffer.firstIndex(of: UInt8(ascii: "\n")) {
                let lineData = buffer.subdata(in: 0 ..< nl)
                buffer = buffer.subdata(in: (nl + 1) ..< buffer.count)
                handleLine(lineData)
            }
        }
        log("[nestty-plugin] \(pluginName): stdout EOF")
    }

    private func startStderrLoggerThread() {
        Thread.detachNewThread { [weak self] in
            self?.stderrLoop()
        }
    }

    private func stderrLoop() {
        // Forward plugin stderr to host stderr verbatim, prefixed with the
        // plugin name so multiplexed plugins don't lose attribution.
        var buffer = Data()
        while true {
            let chunk = stderrHandle.availableData
            if chunk.isEmpty { break }
            buffer.append(chunk)
            while let nl = buffer.firstIndex(of: UInt8(ascii: "\n")) {
                let lineData = buffer.subdata(in: 0 ..< nl)
                buffer = buffer.subdata(in: (nl + 1) ..< buffer.count)
                if let line = String(data: lineData, encoding: .utf8) {
                    PluginProcess.log("[nestty-plugin] \(pluginName) stderr: \(line)")
                }
            }
        }
    }

    private func handleLine(_ data: Data) {
        guard let parsed = try? JSONSerialization.jsonObject(with: data),
              let obj = parsed as? [String: Any]
        else {
            log("[nestty-plugin] \(pluginName): skipping non-JSON line")
            return
        }

        // Response to a request we sent (has id + ok).
        if let id = obj["id"] as? String {
            lock.lock()
            let completion = pending.removeValue(forKey: id)
            lock.unlock()
            guard let completion else {
                log("[nestty-plugin] \(pluginName): response for unknown id \(id)")
                return
            }
            // Fire the completion inline on the reader thread. We deliberately
            // do NOT hop to main actor here because:
            //   1. The init-handshake completion below is invoked while the
            //      main thread is parked on `initBox.semaphore.wait()` — a
            //      `DispatchQueue.main.async` bounce would deadlock instantly.
            //   2. The action.invoke completions all funnel back into
            //      SocketServer's leaf closure (`box.value = result; sema.signal()`)
            //      which is `@unchecked Sendable` and tolerates any thread.
            //   3. Any future completion that DOES need main actor work can
            //      bounce inside its own body — pushing the policy to the
            //      callee instead of forcing it on every plugin response.
            let payload = decodeResponse(obj)
            completion(payload)
            return
        }

        // Notification from plugin (no id). PR 5c: event.publish frames
        // get fanned onto EventBus so the trigger engine (and any other
        // subscriber) can observe the event. AppDelegate wires
        // eventBus.onBroadcast → NesttyEngine.dispatchEvent so the chain
        // closes plugin → eventBus → engine → callback → ActionRegistry.
        if let method = obj["method"] as? String {
            switch method {
            case "event.publish":
                if let params = obj["params"] as? [String: Any],
                   let kind = params["kind"] as? String
                {
                    let payload = (params["payload"] as? [String: Any]) ?? [:]
                    eventBus?.broadcast(event: kind, data: payload)
                }
            default:
                log("[nestty-plugin] \(pluginName): unknown notification \(method)")
            }
            return
        }

        log("[nestty-plugin] \(pluginName): malformed frame (no id, no method)")
    }

    /// Convert a plugin response frame into either the success result (Any?)
    /// or an `RPCError` so the registry completion gets the right thing.
    private func decodeResponse(_ obj: [String: Any]) -> Any? {
        if let ok = obj["ok"] as? Bool, ok {
            return obj["result"]
        }
        if let err = obj["error"] as? [String: Any] {
            let code = (err["code"] as? String) ?? "plugin_error"
            let message = (err["message"] as? String) ?? "plugin returned error without message"
            return RPCError(code: code, message: message)
        }
        return RPCError(code: "malformed_response", message: "plugin response missing ok/error")
    }

    // MARK: - Helpers

    private static func resolveExecutable(_ name: String, in dir: URL) -> URL? {
        let direct = dir.appendingPathComponent(name)
        if FileManager.default.isExecutableFile(atPath: direct.path) {
            return direct
        }
        // PATH lookup mirrors Linux's behavior. `which` is the simplest
        // correct implementation; we don't shell out to /usr/bin/which
        // because that re-evaluates against $PATH at process spawn time.
        let path = ProcessInfo.processInfo.environment["PATH"] ?? ""
        for entry in path.split(separator: ":") {
            let candidate = URL(fileURLWithPath: String(entry)).appendingPathComponent(name)
            if FileManager.default.isExecutableFile(atPath: candidate.path) {
                return candidate
            }
        }
        return nil
    }

    fileprivate static func log(_ message: String) {
        FileHandle.standardError.write(Data((message + "\n").utf8))
    }

    private func log(_ message: String) {
        PluginProcess.log(message)
    }
}

// MARK: - InitBox

/// Class-bound carrier for the init handshake result. Lets us ferry data out
/// of the reader-thread `@Sendable` closure without tripping Swift 6 strict
/// concurrency on captured `var`s. Single-use — `resolve` signals the
/// semaphore exactly once.
private final class InitBox: @unchecked Sendable {
    enum Outcome {
        case success([String: Any])
        case failure(String)
    }

    let semaphore = DispatchSemaphore(value: 0)
    var outcome: Outcome?

    func resolve(_ response: Any?) {
        defer { semaphore.signal() }
        if let err = response as? RPCError {
            outcome = .failure("\(err.code): \(err.message)")
        } else if let dict = response as? [String: Any] {
            outcome = .success(dict)
        } else {
            outcome = .failure("init response was neither RPCError nor [String: Any]: \(String(describing: response))")
        }
    }
}
