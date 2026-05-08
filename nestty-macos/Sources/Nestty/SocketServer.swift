import Darwin
import Foundation

/// Sentinel completion value: handlers that need to surface a JSON-RPC error
/// envelope (`{ok:false, error:{code, message}}`) instead of a success result
/// pass an instance of this struct to their `completion` closure. SocketServer
/// detects it in `dispatch()` and emits the proper error frame.
///
/// Why a sentinel rather than a typed `Result`-style completion: the existing
/// command handler signature is `(_ value: Any?) -> Void` and is used by ~30
/// handlers. Sending an `RPCError` through the same channel keeps the
/// signature stable while letting webview commands report `not_found` /
/// `wrong_panel_type` / `invalid_params` matching the Linux `socket.rs` shape.
struct RPCError: Error {
    let code: String
    let message: String
}

/// Unix socket server that accepts nestctl connections.
/// Mirrors the role of socket.rs in nestty-linux.
///
/// Threading model:
///   - Accept loop runs on a dedicated Thread (POSIX blocking)
///   - Each client gets its own Thread
///   - Command handler is dispatched via DispatchQueue.main.async
///   - Socket thread blocks on a DispatchSemaphore until the handler calls its completion
///   - Async commands (e.g. webview.execute_js) call completion from within a WKWebView callback
final class SocketServer: @unchecked Sendable {
    private let socketPath: String
    private var serverFd: Int32 = -1

    /// Called on the main thread to dispatch a command.
    /// Must call `completion` with the result (or nil for unknown method) — may be called
    /// asynchronously (e.g. after a WKWebView JS evaluation completes).
    var commandHandler: ((_ method: String, _ params: [String: Any], _ completion: @escaping (Any?) -> Void) -> Void)?

    /// Event bus for streaming events to subscribed clients.
    var eventBus: EventBus?

    init() {
        let pid = ProcessInfo.processInfo.processIdentifier
        socketPath = "/tmp/nestty-\(pid).sock"
    }

    var path: String {
        socketPath
    }

    func start() {
        unlink(socketPath)

        serverFd = socket(AF_UNIX, SOCK_STREAM, 0)
        guard serverFd >= 0 else {
            print("[nestty] socket create failed: \(String(cString: strerror(errno)))")
            return
        }

        var addr = sockaddr_un()
        addr.sun_family = sa_family_t(AF_UNIX)
        withUnsafeMutableBytes(of: &addr.sun_path) { buf in
            let count = min(socketPath.utf8.count, buf.count - 1)
            buf.copyBytes(from: socketPath.utf8.prefix(count))
            buf[count] = 0
        }

        let addrSize = socklen_t(MemoryLayout<sockaddr_un>.size)
        let bound = withUnsafePointer(to: &addr) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                bind(serverFd, $0, addrSize)
            }
        }

        guard bound == 0 else {
            print("[nestty] socket bind failed: \(String(cString: strerror(errno)))")
            close(serverFd)
            serverFd = -1
            return
        }

        listen(serverFd, 5)
        print("[nestty] socket listening at \(socketPath)")

        let fd = serverFd
        Thread.detachNewThread { [weak self] in self?.acceptLoop(serverFd: fd) }
    }

    func stop() {
        if serverFd >= 0 {
            close(serverFd)
            serverFd = -1
        }
        unlink(socketPath)
    }

    // MARK: - Private

    private func acceptLoop(serverFd: Int32) {
        while true {
            let clientFd = accept(serverFd, nil, nil)
            if clientFd < 0 { break }
            Thread.detachNewThread { [weak self] in self?.handleClient(clientFd) }
        }
    }

    private func handleClient(_ fd: Int32) {
        defer { close(fd) }

        var buffer = Data()
        var chunk = [UInt8](repeating: 0, count: 4096)

        while true {
            let n = read(fd, &chunk, chunk.count)
            if n <= 0 { break }
            buffer.append(contentsOf: chunk[..<n])

            while let nlIdx = buffer.firstIndex(of: UInt8(ascii: "\n")) {
                let line = Data(buffer[..<nlIdx])
                buffer = Data(buffer[buffer.index(after: nlIdx)...])

                // event.subscribe: stay connected and stream events
                if let json = try? JSONSerialization.jsonObject(with: line) as? [String: Any],
                   let id = json["id"] as? String,
                   (json["method"] as? String) == "event.subscribe"
                {
                    var resp = success(id: id, result: ["status": "subscribed"])
                    resp.append(UInt8(ascii: "\n"))
                    _ = resp.withUnsafeBytes { write(fd, $0.baseAddress!, $0.count) }
                    streamEvents(fd: fd)
                    return
                }

                var response = dispatch(line)
                response.append(UInt8(ascii: "\n"))
                _ = response.withUnsafeBytes { write(fd, $0.baseAddress!, $0.count) }
            }
        }
    }

    private func streamEvents(fd: Int32) {
        guard let bus = eventBus else { return }
        let channel = bus.subscribe()
        defer { channel.close() }
        while let event = channel.receive() {
            let data = Data((event + "\n").utf8)
            let result = data.withUnsafeBytes { write(fd, $0.baseAddress!, $0.count) }
            if result <= 0 { break }
        }
    }

    /// Box for passing the handler result from main-actor context to socket thread.
    /// @unchecked Sendable is safe here: access is serialized by the semaphore.
    private final class ResultBox: @unchecked Sendable {
        var value: Any?
    }

    private func dispatch(_ data: Data) -> Data {
        guard
            let json = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
            let id = json["id"] as? String,
            let method = json["method"] as? String
        else {
            return error(id: "?", code: "invalid_request", message: "malformed JSON")
        }

        let params = json["params"] as? [String: Any] ?? [:]

        // Dispatch to main thread asynchronously; block socket thread on semaphore.
        // Supports both sync commands (completion called immediately) and async
        // commands like webview.execute_js (completion called from WKWebView callback).
        let sema = DispatchSemaphore(value: 0)
        let box = ResultBox()

        DispatchQueue.main.async {
            guard let handler = self.commandHandler else {
                sema.signal()
                return
            }
            handler(method, params) { value in
                box.value = value
                sema.signal()
            }
        }

        sema.wait()

        if let err = box.value as? RPCError {
            return error(id: id, code: err.code, message: err.message)
        }
        if let result = box.value {
            return success(id: id, result: result)
        } else {
            return error(id: id, code: "unknown_method", message: "unknown: \(method)")
        }
    }

    private func success(id: String, result: Any) -> Data {
        let dict: [String: Any] = ["id": id, "ok": true, "result": result]
        return (try? JSONSerialization.data(withJSONObject: dict)) ?? Data()
    }

    private func error(id: String, code: String, message: String) -> Data {
        let dict: [String: Any] = [
            "id": id,
            "ok": false,
            "error": ["code": code, "message": message],
        ]
        return (try? JSONSerialization.data(withJSONObject: dict)) ?? Data()
    }
}
