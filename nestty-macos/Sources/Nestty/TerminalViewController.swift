import AppKit
import SwiftTerm

extension Notification.Name {
    static let terminalTitleChanged = Notification.Name("NesttyTerminalTitleChanged")
}

// MARK: - NesttyTerminalView

/// Wraps LocalProcessTerminalView to:
/// 1. Fix a SwiftTerm bug where processTerminated is never delivered after shell exits.
/// 2. Replace `terminalDelegate` with a proxy that gates OSC 52 clipboard writes
///    (SwiftTerm's built-in `clipboardCopy` is `public` non-`open` and unconditionally
///    writes to `NSPasteboard.general` — we cannot override it directly, so we own the
///    delegate slot).
///
/// Note on PTY output interception: SwiftTerm's `feed(byteArray:)` is an extension
/// method, not overridable, so `terminal.output` events / OSC 133 shell integration
/// cannot be implemented from outside SwiftTerm.
/// `internal` (no explicit modifier) so URLClickHelper can take a typed
/// reference for `getTerminal()` access. Was `private` before Tier 1.5
/// plain-text URL detection landed.
class NesttyTerminalView: LocalProcessTerminalView {
    private var exitMonitor: (any DispatchSourceProcess)?
    /// Strongly retained so SwiftTerm's `weak terminalDelegate` doesn't drop our proxy.
    private var delegateProxy: NesttyTerminalDelegate?

    /// Replace SwiftTerm's self-as-delegate with our policy proxy. Must be called once,
    /// after `super.init`, before any PTY frame can fire `clipboardCopy`.
    func installDelegateProxy(policy: OSC52Policy) {
        let proxy = NesttyTerminalDelegate(host: self, policy: policy)
        delegateProxy = proxy
        terminalDelegate = proxy
    }

    func setOSC52Policy(_ policy: OSC52Policy) {
        delegateProxy?.policy = policy
    }

    func installExitMonitor() {
        let pid = process.shellPid
        guard pid > 0 else { return }
        let src = DispatchSource.makeProcessSource(identifier: pid, eventMask: .exit, queue: .main)
        src.setEventHandler { [weak self, weak src] in
            src?.cancel()
            guard let self else { return }
            processDelegate?.processTerminated(source: self, exitCode: nil)
        }
        exitMonitor = src
        src.activate()
    }

    deinit {
        exitMonitor?.cancel()
    }
}

// MARK: - NesttyTerminalDelegate

/// Proxy that owns SwiftTerm's `terminalDelegate` slot so we can gate `clipboardCopy`
/// (OSC 52). All other methods are forwarded to the host `LocalProcessTerminalView`'s
/// own implementations — those are `public` and callable from outside the module.
///
/// `requestOpenLink`, `bell`, `iTermContent` are intentionally not implemented: their
/// protocol-extension defaults match what `LocalProcessTerminalView` would do, and
/// re-declaring them here would override the defaults with no benefit.
///
/// Tier 1.5 note — OSC 8 hyperlinks (clickable links emitted by programs like
/// `ls --hyperlink`, `gh`, etc.) work on macOS via SwiftTerm's default
/// `requestOpenLink` implementation in `Mac/MacTerminalView.swift:1614`,
/// which calls `NSWorkspace.shared.open(url)`. We don't override it, so click
/// + open already happens for free. Plain-text URL detection (Cmd+click on
/// a bare `https://...` in unstructured output) lives in `URLClickHelper` +
/// `PaneManager.installURLClickMonitor` — handled outside SwiftTerm via an
/// `NSEvent` local monitor because `MacTerminalView.mouseUp` is `public
/// override` (not `open`).
@MainActor
private final class NesttyTerminalDelegate: NSObject, @preconcurrency TerminalViewDelegate {
    weak var host: LocalProcessTerminalView?
    var policy: OSC52Policy

    init(host: LocalProcessTerminalView, policy: OSC52Policy) {
        self.host = host
        self.policy = policy
    }

    func sizeChanged(source: TerminalView, newCols: Int, newRows: Int) {
        host?.sizeChanged(source: source, newCols: newCols, newRows: newRows)
    }

    func setTerminalTitle(source: TerminalView, title: String) {
        host?.setTerminalTitle(source: source, title: title)
    }

    func hostCurrentDirectoryUpdate(source: TerminalView, directory: String?) {
        host?.hostCurrentDirectoryUpdate(source: source, directory: directory)
    }

    func send(source: TerminalView, data: ArraySlice<UInt8>) {
        host?.send(source: source, data: data)
    }

    func scrolled(source: TerminalView, position: Double) {
        host?.scrolled(source: source, position: position)
    }

    func rangeChanged(source: TerminalView, startY: Int, endY: Int) {
        host?.rangeChanged(source: source, startY: startY, endY: endY)
    }

    func clipboardCopy(source _: TerminalView, content: Data) {
        switch policy {
        case .deny:
            let msg = "[nestty] OSC 52 clipboard write blocked (\(content.count) bytes). " +
                "Set [security] osc52 = \"allow\" in config.toml to enable.\n"
            FileHandle.standardError.write(Data(msg.utf8))
        case .allow:
            guard let str = String(bytes: content, encoding: .utf8) else { return }
            let pb = NSPasteboard.general
            pb.clearContents()
            pb.writeObjects([str as NSString])
        }
    }
}

// MARK: - TerminalViewController

@MainActor
class TerminalViewController: NSViewController, NesttyPanel {
    let panelID: String = UUID().uuidString

    private let config: NesttyConfig
    /// Mutable so applyTheme() can update it; clearBackground() uses the live value.
    private var theme: NesttyTheme
    /// `private(set)` so PaneManager's URL click monitor (Tier 1.5 plain-text
    /// extension) can reach the SwiftTerm `Terminal` for buffer reads. Setter
    /// stays private because TerminalViewController owns the view's lifecycle.
    private(set) var terminalView: NesttyTerminalView?
    private var backgroundView: NSImageView?
    private var tintView: NSView?
    private var currentFontSize: CGFloat
    /// Tracks the live font family (may differ from config after hot-reload).
    private var currentFontFamily: String
    /// Base font size from config; updated on hot-reload so zoomReset() uses the
    /// right baseline after the user changes fontSize in the config file.
    private var configFontSize: CGFloat

    private(set) var currentTitle: String = "Terminal"
    private var customTitle: String?
    private var shellStarted = false
    var onProcessTerminated: (() -> Void)?

    /// Set by AppDelegate after EventBus is created.
    weak var eventBus: EventBus?

    /// PR 8 — optional cwd + initial-input passed to `startShell`. Used by
    /// `claude.start` to land the user in a worktree directory and feed
    /// the `tmux new-session` command without relying on a separate
    /// post-spawn `terminal.exec` (which has a startup race against
    /// SwiftTerm's PTY readiness). SwiftTerm's `startProcess` accepts a
    /// `currentDirectory` arg natively; the initial-input string is sent
    /// to the master PTY immediately after spawn — bytes sit in the
    /// kernel buffer until the child shell reads them, so no race.
    private let initialCwd: String?
    private let initialInput: String?

    init(config: NesttyConfig, theme: NesttyTheme, cwd: String? = nil, initialInput: String? = nil) {
        self.config = config
        self.theme = theme
        let baseFontSize = CGFloat(config.fontSize)
        configFontSize = baseFontSize
        currentFontSize = baseFontSize
        currentFontFamily = config.fontFamily
        initialCwd = cwd
        self.initialInput = initialInput
        super.init(nibName: nil, bundle: nil)
    }

    @available(*, unavailable)
    required init?(coder _: NSCoder) {
        fatalError("init(coder:) has not been implemented")
    }

    override func loadView() {
        let frame = NSRect(x: 0, y: 0, width: 1200, height: 800)

        // Container — layer-backed so all subviews composite with alpha blending.
        let container = NSView(frame: frame)
        container.wantsLayer = true

        // Background image view — hidden until a background is set.
        // wantsLayer = true so it participates in the layer compositing chain.
        let bg = NSImageView(frame: container.bounds)
        bg.autoresizingMask = [.width, .height]
        bg.imageScaling = .scaleAxesIndependently
        bg.wantsLayer = true
        bg.isHidden = true
        container.addSubview(bg)
        backgroundView = bg

        // Tint overlay — dark semi-transparent view between image and terminal.
        let tint = NSView(frame: container.bounds)
        tint.autoresizingMask = [.width, .height]
        tint.wantsLayer = true
        tint.isHidden = true
        container.addSubview(tint)
        tintView = tint

        // Terminal view on top.
        // wantsLayer + isOpaque=false lets nativeBackgroundColor=.clear show
        // the layers behind it when a background image is active.
        let tv = NesttyTerminalView(frame: frame)
        tv.autoresizingMask = [.width, .height]
        tv.wantsLayer = true
        tv.layer?.isOpaque = false
        // Install our delegate proxy BEFORE any PTY data can flow, so the very first
        // OSC 52 frame is gated. `loadView` runs before `startShell`, so this ordering
        // is safe.
        tv.installDelegateProxy(policy: config.osc52)
        configureColors(tv)
        configureFont(tv, size: currentFontSize)
        tv.processDelegate = self
        terminalView = tv
        container.addSubview(tv)

        view = container

        // Apply background from config if set
        if let path = config.backgroundPath {
            applyBackground(path: path, tint: config.backgroundTint, opacity: config.backgroundOpacity)
        }
    }

    override func viewDidLoad() {
        super.viewDidLoad()
        // Shell is started explicitly by TabViewController via startShellIfNeeded(),
        // after contentArea.layoutSubtreeIfNeeded() ensures the correct frame.
    }

    override func viewDidAppear() {
        super.viewDidAppear()
        // Layer is guaranteed to exist once the view is in the window.
        // Re-apply background if it was set before the view appeared.
        if backgroundView?.isHidden == false, let image = backgroundView?.image {
            let path = (image.name() ?? "") // fallback: re-apply via stored state
            _ = path // The tint/layer settings are what matter here
            terminalView?.layer?.isOpaque = false
            terminalView?.layer?.backgroundColor = NSColor.clear.cgColor
            terminalView?.needsDisplay = true
        }
    }

    /// Called after the view has been added to the hierarchy
    /// and Auto Layout has been forced to resolve (layoutSubtreeIfNeeded).
    func startIfNeeded() {
        guard !shellStarted else { return }
        shellStarted = true
        startShell()
    }

    // MARK: - Background

    func applyBackground(path: String, tint: Double, opacity: Double) {
        guard let image = NSImage(contentsOfFile: path) else { return }
        backgroundView?.image = image
        // Use alphaValue (not isHidden) to control opacity so applyTheme always
        // sees the background as "active" (isHidden == false) and keeps
        // nativeBackgroundColor = .clear. Hiding via isHidden corrupts SwiftTerm's
        // internal cell buffer: applyTheme fills it with the solid theme color,
        // and setting nativeBackgroundColor back to .clear does not clear cells.
        backgroundView?.alphaValue = CGFloat(opacity)
        backgroundView?.isHidden = false
        tintView?.layer?.backgroundColor = NSColor.black.withAlphaComponent(CGFloat(tint)).cgColor
        tintView?.isHidden = opacity == 0
        // Make terminal layer non-opaque so the image layers composite through.
        // nativeBackgroundColor = .clear tells SwiftTerm not to fill the bg rect.
        terminalView?.layer?.isOpaque = false
        terminalView?.layer?.backgroundColor = NSColor.clear.cgColor
        terminalView?.nativeBackgroundColor = .clear
        terminalView?.needsDisplay = true
    }

    func clearBackground() {
        backgroundView?.image = nil
        backgroundView?.isHidden = true
        tintView?.isHidden = true
        terminalView?.layer?.isOpaque = false // keep layer-backed, just restore color
        terminalView?.layer?.backgroundColor = theme.background.nsColor.cgColor
        terminalView?.nativeBackgroundColor = theme.background.nsColor
        terminalView?.needsDisplay = true
    }

    func setTint(_ alpha: Double) {
        tintView?.layer?.backgroundColor = NSColor.black.withAlphaComponent(CGFloat(alpha)).cgColor
    }

    // MARK: - Hot-reload

    /// Re-apply theme colors to a running terminal (called on config file change).
    func applyTheme(_ newTheme: NesttyTheme) {
        guard let tv = terminalView else { return }
        // Resolve bg color upfront so we set nativeBackgroundColor exactly once,
        // avoiding a potential single-frame flash of the opaque theme color over a
        // background image if SwiftTerm redraws between two assignments.
        // backgroundView.isHidden is always false when a background image is configured
        // (opacity=0 uses alphaValue=0, not isHidden, to avoid corrupting SwiftTerm's
        // cell buffer). Check image != nil to detect "background configured".
        let bgColor: NSColor = (backgroundView?.image != nil) ? .clear : newTheme.background.nsColor
        tv.nativeBackgroundColor = bgColor
        tv.nativeForegroundColor = newTheme.foreground.nsColor
        let ansiColors = newTheme.palette.map { c in
            SwiftTerm.Color(red: UInt16(c.r) * 257, green: UInt16(c.g) * 257, blue: UInt16(c.b) * 257)
        }
        tv.installColors(ansiColors)
        tv.needsDisplay = true
        // Update the stored theme so clearBackground() uses the new color.
        theme = newTheme
    }

    /// Update the OSC 52 clipboard-write policy on a running terminal (config hot-reload).
    func applyOSC52Policy(_ policy: OSC52Policy) {
        terminalView?.setOSC52Policy(policy)
    }

    /// Re-apply font family and base size to a running terminal (called on config hot-reload).
    /// The current zoom level (currentFontSize) is preserved as-is; configFontSize is updated
    /// so that zoomReset() snaps to the new baseline.
    func applyFont(family: String, baseSize: CGFloat) {
        configFontSize = baseSize
        currentFontFamily = family
        guard let tv = terminalView else { return }
        configureFont(tv, size: currentFontSize, family: family)
    }

    // MARK: - Configuration

    private func configureColors(_ tv: LocalProcessTerminalView) {
        tv.nativeBackgroundColor = theme.background.nsColor
        tv.nativeForegroundColor = theme.foreground.nsColor

        let ansiColors = theme.palette.map { c in
            SwiftTerm.Color(red: UInt16(c.r) * 257, green: UInt16(c.g) * 257, blue: UInt16(c.b) * 257)
        }
        tv.installColors(ansiColors)
    }

    private func configureFont(_ tv: LocalProcessTerminalView, size: CGFloat, family: String? = nil) {
        let font = resolveFont(name: family ?? currentFontFamily, size: size)
        tv.font = font
    }

    /// Resolves a font by name using multiple strategies so that Nerd Font family
    /// names (e.g. "JetBrainsMono Nerd Font Mono") are found correctly even when
    /// NSFont(name:) only accepts PostScript names.
    private func resolveFont(name: String, size: CGFloat) -> NSFont {
        // 1. PostScript name / full name lookup (e.g. "JetBrains Mono Regular")
        if let font = NSFont(name: name, size: size) { return font }

        let manager = NSFontManager.shared

        // 2. Exact family-name lookup via NSFontManager
        if let font = regularFont(fromFamily: name, manager: manager, size: size) { return font }

        // 3. Case-insensitive family-name lookup (handles "jetbrainsmono nerd font mono" etc.)
        let lower = name.lowercased()
        for family in manager.availableFontFamilies where family.lowercased() == lower {
            if let font = regularFont(fromFamily: family, manager: manager, size: size) { return font }
        }

        // 4. NSFontDescriptor family lookup (last-resort before system fallback)
        if let font = NSFont(descriptor: NSFontDescriptor().withFamily(name), size: size) { return font }

        return NSFont.monospacedSystemFont(ofSize: size, weight: .regular)
    }

    /// Picks the "Regular" (or closest upright) member of a font family.
    private func regularFont(fromFamily family: String, manager: NSFontManager, size: CGFloat) -> NSFont? {
        guard let members = manager.availableMembers(ofFontFamily: family) else { return nil }
        let preferred = ["Regular", "Book", "Roman", "Medium", "Text"]
        for faceName in preferred {
            if let m = members.first(where: { ($0[1] as? String) == faceName }),
               let psName = m[0] as? String
            {
                return NSFont(name: psName, size: size)
            }
        }
        // Fall back to the first available member in the family
        if let psName = members.first?[0] as? String { return NSFont(name: psName, size: size) }
        return nil
    }

    // MARK: - Shell

    private func startShell() {
        guard let tv = terminalView else { return }
        let pid = ProcessInfo.processInfo.processIdentifier
        let socketPath = "/tmp/nestty-\(pid).sock"

        // Inherit current environment, then append/override our vars
        var env = ProcessInfo.processInfo.environment.map { "\($0.key)=\($0.value)" }
        env.append("TERM=xterm-256color")
        env.append("COLORTERM=truecolor")
        env.append("NESTTY_SOCKET=\(socketPath)")

        tv.startProcess(
            executable: config.shell,
            args: [],
            environment: env,
            execName: nil,
            currentDirectory: initialCwd,
        )
        tv.installExitMonitor()

        // Initial input gets buffered in the master PTY; the child shell
        // reads it once its own startup (rcfiles) completes. No race —
        // PTY input is kernel-buffered. Used by `claude.start` to feed
        // `tmux new-session …` so the user doesn't see a bare shell
        // prompt before tmux takes over.
        if let initialInput {
            tv.send(txt: initialInput)
        }
    }

    // MARK: - Socket Commands (called on main thread by SocketServer)

    /// Send a command + newline to the PTY (terminal.exec)
    func execCommand(_ command: String) {
        terminalView?.send(txt: command + "\n")
    }

    /// Send raw text to the PTY (terminal.feed)
    func feedText(_ text: String) {
        terminalView?.send(txt: text)
    }

    /// Return terminal state: cols, rows, cursor [row, col], title (terminal.state)
    func terminalState() -> [String: Any] {
        guard let tv = terminalView else { return [:] }
        let term = tv.terminal!
        let cursor = term.getCursorLocation()
        return [
            "cols": term.cols,
            "rows": term.rows,
            "cursor": [cursor.y, cursor.x],
            "title": view.window?.title ?? "nestty",
        ]
    }

    /// Return visible screen text (terminal.read)
    func readScreen() -> [String: Any] {
        guard let tv = terminalView else { return [:] }
        let term = tv.terminal!
        var lines: [String] = []
        for row in 0 ..< term.rows {
            guard let line = term.getLine(row: row) else {
                lines.append(String(repeating: " ", count: term.cols))
                continue
            }
            var str = ""
            for col in 0 ..< term.cols {
                let ch = line[col].getCharacter()
                str.append(ch == "\0" ? " " : ch)
            }
            lines.append(str)
        }
        let cursor = term.getCursorLocation()
        return [
            "text": lines.joined(separator: "\n"),
            "cursor": [cursor.y, cursor.x],
            "rows": term.rows,
            "cols": term.cols,
        ]
    }

    /// Return recent scrollback lines (terminal.history).
    /// SwiftTerm exposes scrollback via negative row indices on getLine(row:).
    func history(lines: Int = 100) -> [String: Any] {
        guard let tv = terminalView else { return [:] }
        let term = tv.terminal!
        let cols = term.cols
        var result: [String] = []
        for row in stride(from: -lines, to: 0, by: 1) {
            guard let line = term.getLine(row: row) else {
                result.append(String(repeating: " ", count: cols))
                continue
            }
            var str = ""
            for col in 0 ..< cols {
                let ch = line[col].getCharacter()
                str.append(ch == "\0" ? " " : ch)
            }
            result.append(str)
        }
        return [
            "text": result.joined(separator: "\n"),
            "lines_requested": lines,
            "rows": term.rows,
            "cols": cols,
        ]
    }

    /// Return state + visible screen + recent scrollback (terminal.context).
    func context(historyLines: Int = 50) -> [String: Any] {
        [
            "state": terminalState(),
            "screen": readScreen(),
            "history": history(lines: historyLines),
        ]
    }

    // MARK: - Zoom

    func zoomIn() {
        let newSize = min(currentFontSize + 1, 72)
        setFontSize(newSize)
    }

    func zoomOut() {
        let newSize = max(currentFontSize - 1, 6)
        setFontSize(newSize)
    }

    func zoomReset() {
        setFontSize(configFontSize)
    }

    private func setFontSize(_ size: CGFloat) {
        currentFontSize = size
        guard let tv = terminalView else { return }
        configureFont(tv, size: size)
    }
}

// MARK: - LocalProcessTerminalViewDelegate

extension TerminalViewController: LocalProcessTerminalViewDelegate {
    nonisolated func sizeChanged(source _: LocalProcessTerminalView, newCols _: Int, newRows _: Int) {
        // No-op: terminal handles resize internally
    }

    func setCustomTitle(_ title: String) {
        customTitle = title
        currentTitle = title
        NotificationCenter.default.post(name: .terminalTitleChanged, object: self)
    }

    nonisolated func setTerminalTitle(source _: LocalProcessTerminalView, title: String) {
        let id = panelID
        Task { @MainActor in
            // Custom title (set via tab.rename) suppresses auto-title updates
            guard self.customTitle == nil else { return }
            self.currentTitle = title.isEmpty ? "Terminal" : title
            NotificationCenter.default.post(name: .terminalTitleChanged, object: self)
            eventBus?.broadcast(event: "panel.title_changed", data: ["panel_id": id, "title": self.currentTitle])
        }
    }

    nonisolated func processTerminated(source _: TerminalView, exitCode _: Int32?) {
        let id = panelID
        Task { @MainActor in
            eventBus?.broadcast(event: "panel.exited", data: ["panel_id": id])
            if let cb = self.onProcessTerminated {
                cb()
            } else {
                self.view.window?.close()
            }
        }
    }

    nonisolated func hostCurrentDirectoryUpdate(source _: TerminalView, directory: String?) {
        guard let directory else { return }
        // OSC 7 delivers a file://hostname/path URI; use URL to extract just the path.
        let cwd: String = if let url = URL(string: directory), url.scheme == "file" {
            url.path
        } else {
            directory
        }
        let id = panelID
        Task { @MainActor in
            eventBus?.broadcast(event: "terminal.cwd_changed", data: ["panel_id": id, "cwd": cwd])
        }
    }
}
