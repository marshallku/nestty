import AppKit
import Foundation
@preconcurrency import WebKit

/// Tier 4.1 — host-rendered plugin panel. Mirrors `nestty-linux/src/plugin_panel.rs`:
/// loads the plugin's `panel.html` in a WKWebView, injects a `window.nestty` JS
/// API at document-start so panel code can call back into nestty via a Promise-based
/// bridge AND subscribe to the EventBus.
///
/// JS contract (must match Linux byte-for-byte so plugin authors can ship one
/// `panel.html` for both platforms):
///
/// ```js
/// window.nestty = {
///     panel: { id, name, plugin },
///     async call(method, params = {}) -> result, // throws Error{code, message}
///     on(type, callback),
///     off(type, callback),
///     // _handleEvent(type, data) — internal, called by host
/// }
/// ```
///
/// Bridge transport: `WKScriptMessageHandlerWithReply` (macOS 11+; we target
/// macOS 14). Each `window.nestty.call(...)` becomes a `postMessage(JSON)` →
/// `replyHandler(jsonStr, errorMsg)` round-trip. We dispatch through
/// `ActionRegistry.tryDispatch`, so plugin actions and the same first-party
/// surface that `nestctl call` reaches are both available from panel JS.
///
/// Event delivery: subscribe to `EventBus`, and on each event push
/// `nestty._handleEvent(type, data)` into the webview via `evaluateJavaScript`.
/// Same pattern as Linux's `event_bus.subscribe("*")` + glib timer poll, but
/// we use the SwiftEventBus subscriber API which already runs on a worker
/// thread that we hop to main from.
@MainActor
final class PluginPanelController: NSViewController, NesttyPanel {
    let panelID: String = UUID().uuidString
    private(set) var currentTitle: String

    private let pluginName: String
    private let panelName: String
    private let panelFileURL: URL
    private weak var registry: ActionRegistry?
    private weak var eventBus: EventBus?

    private var webView: WKWebView!
    private var started = false
    private var eventChannel: EventChannel?
    /// Guards the bridge handler against retain cycles (WKWebView's content
    /// controller holds the handler strongly; we hand it a weak proxy).
    private var bridgeHandler: BridgeHandler?

    init(
        plugin: LoadedPluginManifest,
        panelDef: PluginPanelDef,
        registry: ActionRegistry,
        eventBus: EventBus,
    ) {
        pluginName = plugin.manifest.plugin.name
        panelName = panelDef.name
        currentTitle = panelDef.title
        panelFileURL = plugin.dir.appendingPathComponent(panelDef.file)
        self.registry = registry
        self.eventBus = eventBus
        super.init(nibName: nil, bundle: nil)
    }

    @available(*, unavailable)
    required init?(coder _: NSCoder) {
        fatalError()
    }

    override func loadView() {
        let config = WKWebViewConfiguration()
        config.preferences.setValue(true, forKey: "developerExtrasEnabled")

        // Bridge handler: receives JSON {method, params}, dispatches via
        // ActionRegistry, replies with JSON {ok, result|error}.
        let handler = BridgeHandler(controller: self)
        bridgeHandler = handler
        config.userContentController.addScriptMessageHandler(
            handler,
            contentWorld: .page,
            name: "nestty",
        )

        // Inject the JS bridge at document start so panel scripts can use
        // window.nestty immediately on first run. Identical contract to Linux's
        // build_bridge_js — keep the two in sync if either side evolves.
        let bridgeJS = Self.buildBridgeJS(
            pluginName: pluginName,
            panelName: panelName,
            panelID: panelID,
        )
        let userScript = WKUserScript(
            source: bridgeJS,
            injectionTime: .atDocumentStart,
            forMainFrameOnly: false,
        )
        config.userContentController.addUserScript(userScript)

        let wv = WKWebView(frame: .zero, configuration: config)
        wv.translatesAutoresizingMaskIntoConstraints = false
        // Transparent so window-level background image shows through (matches
        // Linux's `set_background_color(rgba(0, 0, 0, 0))`). Plugins that
        // want an opaque card style apply background CSS to inner elements.
        wv.setValue(false, forKey: "drawsBackground")
        webView = wv
        view = wv
    }

    func startIfNeeded() {
        guard !started else { return }
        started = true

        // Load via loadFileURL so relative resources (CSS, JS, images alongside
        // panel.html) resolve against the plugin directory. Allow read access
        // to the plugin dir too — by default loadFileURL grants only the file
        // itself, blocking sibling assets.
        webView.loadFileURL(
            panelFileURL,
            allowingReadAccessTo: panelFileURL.deletingLastPathComponent(),
        )

        startEventForwarding()
    }

    // MARK: - NesttyPanel no-ops

    func applyBackground(path _: String, tint _: Double, opacity _: Double) {}
    func clearBackground() {}
    func setTint(_: Double) {}

    // MARK: - Event forwarding

    /// Subscribe to the bus and forward events into the webview. Mirrors
    /// Linux's `EventBus.subscribe("*")` + JS `nestty._handleEvent` push.
    private func startEventForwarding() {
        guard let bus = eventBus else { return }
        let channel = bus.subscribe()
        eventChannel = channel
        // EventChannel.receive() blocks; run on a background thread, hop
        // to main for the WKWebView call. Same pattern as SocketServer.
        Thread.detachNewThread { [weak self] in
            while let json = channel.receive() {
                // EventBus serializes as `{"event": "<kind>", "data": {...}}`.
                // Plugin contract takes (type, data) separately, so re-parse
                // and project — keeps the wire format symmetric across panels
                // + socket subscribers.
                guard let data = json.data(using: .utf8),
                      let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
                      let type = obj["event"] as? String
                else { continue }
                let payload = obj["data"] ?? NSNull()
                let typeJSON = Self.jsonString(type) ?? "\"\""
                let dataJSON = (try? String(
                    data: JSONSerialization.data(withJSONObject: payload, options: [.fragmentsAllowed]),
                    encoding: .utf8,
                )) ?? "null"
                let js = "if (window.nestty && window.nestty._handleEvent) nestty._handleEvent(\(typeJSON), \(dataJSON))"
                let jsBox = SendableBox(js)
                DispatchQueue.main.async { [weak self] in
                    self?.webView.evaluateJavaScript(jsBox.value, completionHandler: nil)
                }
            }
        }
    }

    // MARK: - Bridge dispatch (called from BridgeHandler on main)

    fileprivate func handleBridgeMessage(_ message: [String: Any], reply: @escaping (String) -> Void) {
        guard let method = message["method"] as? String else {
            reply(Self.errorJSON(code: "parse_error", message: "missing 'method' field"))
            return
        }
        let params = (message["params"] as? [String: Any]) ?? [:]
        guard let registry else {
            reply(Self.errorJSON(code: "internal_error", message: "ActionRegistry gone"))
            return
        }
        let dispatched = registry.tryDispatch(method, params: params) { result in
            // The completion may fire from any thread (plugin reader thread,
            // FFI callback, etc.). reply must be invoked synchronously with a
            // String — but WKScriptMessageReply itself is fine on any thread.
            // We just need to be careful not to capture mutable state.
            if let err = result as? RPCError {
                reply(Self.errorJSON(code: err.code, message: err.message))
            } else if let result {
                reply(Self.successJSON(result: result))
            } else {
                reply(Self.successJSON(result: NSNull()))
            }
        }
        if !dispatched {
            reply(Self.errorJSON(code: "unknown_method", message: "unknown: \(method)"))
        }
    }

    // MARK: - Static helpers

    private static func buildBridgeJS(pluginName: String, panelName: String, panelID: String) -> String {
        let id = jsonString(panelID) ?? "\"\""
        let name = jsonString(panelName) ?? "\"\""
        let plugin = jsonString(pluginName) ?? "\"\""
        return """
        (() => {
            const _listeners = {};
            window.nestty = {
                panel: {
                    id: \(id),
                    name: \(name),
                    plugin: \(plugin),
                },
                async call(method, params = {}) {
                    const resp = await window.webkit.messageHandlers.nestty.postMessage(
                        JSON.stringify({ method, params })
                    );
                    const parsed = JSON.parse(resp);
                    if (!parsed.ok) {
                        const err = new Error(parsed.error?.message || "Unknown error");
                        err.code = parsed.error?.code;
                        throw err;
                    }
                    return parsed.result;
                },
                on(type, callback) {
                    if (!_listeners[type]) _listeners[type] = [];
                    _listeners[type].push(callback);
                },
                off(type, callback) {
                    if (!_listeners[type]) return;
                    _listeners[type] = _listeners[type].filter(cb => cb !== callback);
                },
                _handleEvent(type, data) {
                    const cbs = _listeners[type] || [];
                    for (const cb of cbs) {
                        try { cb(data); } catch (e) { console.error("nestty event handler error:", e); }
                    }
                    const wildcards = _listeners["*"] || [];
                    for (const cb of wildcards) {
                        try { cb(type, data); } catch (e) { console.error("nestty event handler error:", e); }
                    }
                },
            };
        })()
        """
    }

    private nonisolated static func jsonString(_ s: String) -> String? {
        guard let data = try? JSONSerialization.data(withJSONObject: s, options: [.fragmentsAllowed]),
              let str = String(data: data, encoding: .utf8)
        else { return nil }
        return str
    }

    private static func successJSON(result: Any) -> String {
        let dict: [String: Any] = ["ok": true, "result": result]
        guard let data = try? JSONSerialization.data(withJSONObject: dict, options: [.fragmentsAllowed]),
              let str = String(data: data, encoding: .utf8)
        else { return #"{"ok":true,"result":null}"# }
        return str
    }

    private static func errorJSON(code: String, message: String) -> String {
        let dict: [String: Any] = [
            "ok": false,
            "error": ["code": code, "message": message],
        ]
        guard let data = try? JSONSerialization.data(withJSONObject: dict),
              let str = String(data: data, encoding: .utf8)
        else { return #"{"ok":false,"error":{"code":"internal_error","message":"json encode failed"}}"# }
        return str
    }
}

// MARK: - Bridge handler

/// Bridges `window.webkit.messageHandlers.nestty.postMessage(json)` from JS into
/// `PluginPanelController.handleBridgeMessage`. Held weakly so the WKWebView's
/// strong reference doesn't keep the controller alive past panel close.
/// Conforms to `WKScriptMessageHandlerWithReply` (macOS 14 SDK marks the
/// replyHandler as `@MainActor @Sendable`). The class itself is `@MainActor`
/// so the protocol method runs on main and can touch the controller without
/// hopping. We deliberately use `@unchecked Sendable` because the controller
/// reference is weak and only read on main.
private final class BridgeHandler: NSObject, WKScriptMessageHandlerWithReply, @unchecked Sendable {
    weak var controller: PluginPanelController?

    init(controller: PluginPanelController) {
        self.controller = controller
        super.init()
    }

    @MainActor
    func userContentController(
        _: WKUserContentController,
        didReceive message: WKScriptMessage,
        replyHandler: @escaping @MainActor @Sendable (Any?, String?) -> Void,
    ) {
        guard let str = message.body as? String,
              let data = str.data(using: .utf8),
              let dict = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any]
        else {
            replyHandler(nil, "bridge message was not JSON-encoded {method, params}")
            return
        }
        guard let controller else {
            replyHandler(nil, "PluginPanelController gone")
            return
        }
        // handleBridgeMessage's completion can fire from any thread (action
        // handlers in the registry don't promise main-actor invocation).
        // Bounce the reply back onto main where the WKWebView reply handler
        // wants it. SendableBox carries the closure across the @Sendable hop.
        let replyBox = SendableBox(replyHandler)
        controller.handleBridgeMessage(dict) { replyJson in
            DispatchQueue.main.async {
                replyBox.value(replyJson, nil)
            }
        }
    }
}
