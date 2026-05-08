import AppKit
@preconcurrency import WebKit

// MARK: - WebViewController

@MainActor
final class WebViewController: NSViewController, NesttyPanel {
    let panelID: String = UUID().uuidString

    private(set) var webView: WKWebView!
    private(set) var currentTitle: String = "Web"
    private var startURL: URL?
    private var started = false

    private var urlField: NSTextField!
    private var backButton: NSButton!
    private var forwardButton: NSButton!
    private var reloadButton: NSButton!
    private var observations: [NSKeyValueObservation] = []

    /// Set by AppDelegate after EventBus is created.
    weak var eventBus: EventBus?

    init(url: URL? = nil) {
        startURL = url
        super.init(nibName: nil, bundle: nil)
    }

    @available(*, unavailable)
    required init?(coder _: NSCoder) {
        fatalError()
    }

    override func loadView() {
        let config = WKWebViewConfiguration()
        // Enable Safari Web Inspector (right-click → Inspect Element)
        config.preferences.setValue(true, forKey: "developerExtrasEnabled")
        let wv = WKWebView(frame: .zero, configuration: config)
        wv.navigationDelegate = self
        wv.translatesAutoresizingMaskIntoConstraints = false
        webView = wv

        let back = makeToolbarButton(symbol: "chevron.left", tooltip: "Back", action: #selector(backTapped))
        let forward = makeToolbarButton(symbol: "chevron.right", tooltip: "Forward", action: #selector(forwardTapped))
        let reload = makeToolbarButton(symbol: "arrow.clockwise", tooltip: "Reload", action: #selector(reloadTapped))
        let devtools = makeToolbarButton(symbol: "wrench.and.screwdriver", tooltip: "DevTools", action: #selector(devtoolsTapped))
        back.isEnabled = false
        forward.isEnabled = false
        backButton = back
        forwardButton = forward
        reloadButton = reload

        let field = NSTextField()
        field.placeholderString = "Enter URL or search…"
        field.bezelStyle = .roundedBezel
        field.font = .systemFont(ofSize: 12)
        field.usesSingleLineMode = true
        field.lineBreakMode = .byTruncatingTail
        field.cell?.sendsActionOnEndEditing = false
        field.target = self
        field.action = #selector(urlFieldSubmit(_:))
        field.translatesAutoresizingMaskIntoConstraints = false
        if let url = startURL { field.stringValue = url.absoluteString }
        urlField = field

        let toolbar = NSStackView(views: [back, forward, reload, field, devtools])
        toolbar.orientation = .horizontal
        toolbar.spacing = 4
        toolbar.edgeInsets = NSEdgeInsets(top: 4, left: 8, bottom: 4, right: 8)
        toolbar.translatesAutoresizingMaskIntoConstraints = false

        let container = NSView()
        container.addSubview(toolbar)
        container.addSubview(wv)

        NSLayoutConstraint.activate([
            toolbar.topAnchor.constraint(equalTo: container.topAnchor),
            toolbar.leadingAnchor.constraint(equalTo: container.leadingAnchor),
            toolbar.trailingAnchor.constraint(equalTo: container.trailingAnchor),
            wv.topAnchor.constraint(equalTo: toolbar.bottomAnchor),
            wv.leadingAnchor.constraint(equalTo: container.leadingAnchor),
            wv.trailingAnchor.constraint(equalTo: container.trailingAnchor),
            wv.bottomAnchor.constraint(equalTo: container.bottomAnchor),
        ])

        view = container

        observations = [
            wv.observe(\.canGoBack, options: [.new, .initial]) { [weak self] wv, _ in
                Task { @MainActor in self?.backButton?.isEnabled = wv.canGoBack }
            },
            wv.observe(\.canGoForward, options: [.new, .initial]) { [weak self] wv, _ in
                Task { @MainActor in self?.forwardButton?.isEnabled = wv.canGoForward }
            },
            wv.observe(\.url, options: [.new]) { [weak self] wv, _ in
                Task { @MainActor in self?.syncURLField(wv.url) }
            },
        ]
    }

    override func viewDidAppear() {
        super.viewDidAppear()
        if startURL == nil, urlField?.stringValue.isEmpty == true {
            view.window?.makeFirstResponder(urlField)
        }
    }

    private func makeToolbarButton(symbol: String, tooltip: String, action: Selector) -> NSButton {
        let btn = NSButton()
        btn.image = NSImage(systemSymbolName: symbol, accessibilityDescription: tooltip)
        btn.bezelStyle = .regularSquare
        btn.isBordered = false
        btn.imageScaling = .scaleProportionallyDown
        btn.toolTip = tooltip
        btn.target = self
        btn.action = action
        btn.translatesAutoresizingMaskIntoConstraints = false
        btn.widthAnchor.constraint(equalToConstant: 24).isActive = true
        btn.heightAnchor.constraint(equalToConstant: 24).isActive = true
        return btn
    }

    private func syncURLField(_ url: URL?) {
        guard let urlField else { return }
        let s = url?.absoluteString ?? ""
        guard !s.isEmpty, s != "about:blank" else { return }
        // Don't clobber what the user is currently typing.
        if view.window?.firstResponder === urlField.currentEditor() { return }
        urlField.stringValue = s
    }

    @objc private func backTapped() {
        goBack()
    }

    @objc private func forwardTapped() {
        goForward()
    }

    @objc private func reloadTapped() {
        reload()
    }

    @objc private func devtoolsTapped() {
        toggleDevTools()
    }

    @objc private func urlFieldSubmit(_ sender: NSTextField) {
        let text = sender.stringValue.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !text.isEmpty else { return }
        navigate(to: text)
        view.window?.makeFirstResponder(webView)
    }

    // MARK: - NesttyPanel

    func startIfNeeded() {
        guard !started else { return }
        started = true
        if let url = startURL {
            webView.load(URLRequest(url: url))
        } else {
            loadBlankPage()
        }
    }

    /// Background operations are no-ops for WebView panels.
    func applyBackground(path _: String, tint _: Double, opacity _: Double) {}
    func clearBackground() {}
    func setTint(_: Double) {}

    // MARK: - Navigation

    func navigate(to urlString: String) {
        let finalString: String = if urlString.hasPrefix("http://") || urlString.hasPrefix("https://") || urlString.hasPrefix("file://") {
            urlString
        } else {
            "https://" + urlString
        }
        guard let url = URL(string: finalString) else { return }
        webView.load(URLRequest(url: url))
    }

    func goBack() {
        if webView.canGoBack { webView.goBack() }
    }

    func goForward() {
        if webView.canGoForward { webView.goForward() }
    }

    func reload() {
        webView.reload()
    }

    func executeJS(_ script: String, completion: @escaping (Any?, Error?) -> Void) {
        // WKWebView's completionHandler is @Sendable in the Swift 6 SDK; the socket
        // command chain that ultimately owns `completion` is not @Sendable-typed yet.
        // Box the callback so the @Sendable closure literal we pass in only captures
        // a Sendable wrapper. WebKit invokes the callback on the main thread, so the
        // unchecked-sendable bridge is sound.
        let box = SendableBox(completion)
        webView.evaluateJavaScript(script) { result, error in
            box.value(result, error)
        }
    }

    func getContent(completion: @escaping (String) -> Void) {
        let box = SendableBox(completion)
        webView.evaluateJavaScript("document.documentElement.outerHTML") { result, _ in
            box.value(result as? String ?? "")
        }
    }

    // MARK: - State

    func toggleDevTools() {
        // Enables right-click → "Inspect Element" via Safari Web Inspector.
        // developerExtrasEnabled is already set in loadView(); this re-applies it
        // in case the caller wants to toggle the state at runtime.
        let current = webView.configuration.preferences.value(forKey: "developerExtrasEnabled") as? Bool ?? false
        webView.configuration.preferences.setValue(!current, forKey: "developerExtrasEnabled")
    }

    var currentURL: String {
        webView.url?.absoluteString ?? ""
    }

    var canGoBack: Bool {
        webView.canGoBack
    }

    var canGoForward: Bool {
        webView.canGoForward
    }

    var isLoading: Bool {
        webView.isLoading
    }

    // MARK: - Private

    private func loadBlankPage() {
        let html = """
        <html>
        <body style="background:#1e1e2e;color:#cdd6f4;font-family:system-ui;
                     display:flex;align-items:center;justify-content:center;
                     height:100vh;margin:0">
          <p style="opacity:0.4">Open a URL to get started</p>
        </body>
        </html>
        """
        webView.loadHTMLString(html, baseURL: nil)
    }
}

// MARK: - WKNavigationDelegate

extension WebViewController: WKNavigationDelegate {
    nonisolated func webView(_ webView: WKWebView, didFinish _: WKNavigation!) {
        Task { @MainActor in
            let title = webView.title
            let host = webView.url?.host
            self.currentTitle = (title?.isEmpty == false ? title! : host) ?? "Web"
            NotificationCenter.default.post(name: .terminalTitleChanged, object: self)
            let id = self.panelID
            eventBus?.broadcast(event: "webview.loaded", data: ["panel_id": id])
            eventBus?.broadcast(event: "webview.title_changed", data: ["panel_id": id, "title": self.currentTitle])
            eventBus?.broadcast(event: "panel.title_changed", data: ["panel_id": id, "title": self.currentTitle])
        }
    }

    nonisolated func webView(_ webView: WKWebView, didCommit _: WKNavigation!) {
        Task { @MainActor in
            let urlStr = webView.url?.absoluteString ?? ""
            let id = self.panelID
            eventBus?.broadcast(event: "webview.navigated", data: ["panel_id": id, "url": urlStr])
        }
    }
}

// SendableBox lives in Sources/Nestty/SendableBox.swift now — same need shows
// up in PluginSupervisor (PR 3), so the type was hoisted out of this file.
