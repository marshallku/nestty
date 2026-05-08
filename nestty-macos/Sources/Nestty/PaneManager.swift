import AppKit

/// NSSplitView subclass that distributes all subviews equally on the first resize pass.
/// Works for any number of subviews (N panes → each gets 1/N of available space).
/// After the initial layout the user can freely drag dividers to any position.
///
/// Using NSSplitViewDelegate.splitView(_:resizeSubviewsWithOldSize:) rather than
/// layout() because NSSplitView sets subview frames via resizeSubviews, which runs
/// *before* layout(). By the time layout() fires, the (wrong) frames are already
/// committed. The delegate method intercepts at exactly the right moment.
private class EqualSplitView: NSSplitView, NSSplitViewDelegate {
    private var initialSizeSet = false

    override init(frame: NSRect) {
        super.init(frame: frame)
        delegate = self
    }

    @available(*, unavailable)
    required init?(coder _: NSCoder) {
        fatalError()
    }

    func splitView(_ splitView: NSSplitView, resizeSubviewsWithOldSize _: NSSize) {
        let total = isVertical ? splitView.frame.width : splitView.frame.height
        guard total > 0, splitView.subviews.count >= 2 else {
            splitView.adjustSubviews()
            return
        }

        if initialSizeSet {
            // After initial sizing: let NSSplitView handle normal proportional resize.
            splitView.adjustSubviews()
            return
        }
        initialSizeSet = true

        let n = splitView.subviews.count
        let eachSize = (total - dividerThickness * CGFloat(n - 1)) / CGFloat(n)
        if isVertical {
            var x: CGFloat = 0
            for sub in splitView.subviews {
                sub.frame = NSRect(x: x, y: 0, width: eachSize, height: splitView.frame.height)
                x += eachSize + dividerThickness
            }
        } else {
            var y: CGFloat = 0
            for sub in splitView.subviews {
                sub.frame = NSRect(x: 0, y: y, width: splitView.frame.width, height: eachSize)
                y += eachSize + dividerThickness
            }
        }
    }
}

enum InitialPanel {
    case terminal
    /// PR 8 — terminal seeded with a specific cwd and/or initial PTY
    /// input. Used by `claude.start` to land the user in a worktree
    /// directory and feed the `tmux new-session` command. Separate
    /// case (rather than associated values on `.terminal`) so existing
    /// `.terminal` callers stay unchanged.
    case terminalSeed(cwd: String?, initialInput: String?)
    case webview(url: URL?)
    /// Tier 4.1 — pre-constructed plugin panel. Caller (TabViewController)
    /// builds the PluginPanelController itself because it needs the registry
    /// + event bus references; PaneManager just embeds it.
    case pluginPanel(any NesttyPanel)
}

/// Manages the split-pane tree for a single tab.
/// TabViewController embeds `containerView` once; PaneManager rebuilds its
/// contents on every split/close using fresh NSSplitView instances.
@MainActor
final class PaneManager {
    /// Mutable so split-spawned panes after a config hot-reload pick up the new values
    /// (theme/font/security). `applyTheme` / `applyFont` / `applyOSC52Policy` already
    /// fan out to existing panes; updating the snapshot here keeps new splits in step.
    private var config: NesttyConfig
    private var theme: NesttyTheme

    private(set) var root: SplitNode
    private(set) var activePane: any NesttyPanel

    /// Stable container — TabViewController pins this to contentArea once and never re-embeds.
    let containerView: NSView

    var onLastPaneClosed: (() -> Void)?
    var onActivePaneChanged: (() -> Void)?

    /// Propagated from AppDelegate so all panels can emit events.
    weak var eventBus: EventBus? {
        didSet { propagateEventBus() }
    }

    private nonisolated(unsafe) var clickMonitor: Any?
    /// Tier 1.5 (plain-text part) — separate `.leftMouseUp` monitor that
    /// intercepts cmd+click for URL opening. Distinct from `clickMonitor`
    /// (which is `.leftMouseDown` for pane focus) so each handler stays
    /// single-purpose.
    private nonisolated(unsafe) var urlClickMonitor: Any?
    /// Tracks the fill constraints added to containerView so they can be
    /// deactivated before the next rebuild.
    private var rootConstraints: [NSLayoutConstraint] = []

    // MARK: - Init

    init(config: NesttyConfig, theme: NesttyTheme, initialPanel: InitialPanel = .terminal) {
        self.config = config
        self.theme = theme

        let panel: any NesttyPanel = switch initialPanel {
        case .terminal:
            TerminalViewController(config: config, theme: theme)
        case let .terminalSeed(cwd, initialInput):
            TerminalViewController(config: config, theme: theme, cwd: cwd, initialInput: initialInput)
        case let .webview(url):
            WebViewController(url: url)
        case let .pluginPanel(p):
            p
        }

        root = .leaf(panel)
        activePane = panel

        containerView = NSView()
        containerView.translatesAutoresizingMaskIntoConstraints = false

        wirePanel(panel)
        rebuildViewHierarchy()
        installClickMonitor()
        installURLClickMonitor()
    }

    deinit {
        if let m = clickMonitor { NSEvent.removeMonitor(m) }
        if let m = urlClickMonitor { NSEvent.removeMonitor(m) }
    }

    // MARK: - Public API

    func splitActive(orientation: SplitOrientation) {
        let newTermVC = TerminalViewController(config: config, theme: theme)
        assignEventBus(to: newTermVC)
        wirePanel(newTermVC)

        root = root.splitting(activePane, with: .leaf(newTermVC), orientation: orientation)

        rebuildViewHierarchy()

        setActive(newTermVC)
        newTermVC.startIfNeeded()
        newTermVC.view.window?.makeFirstResponder(newTermVC.view)
    }

    func splitActiveWithWebView(url: URL? = nil, orientation: SplitOrientation = .horizontal) {
        let webVC = WebViewController(url: url)
        assignEventBus(to: webVC)
        wirePanel(webVC)

        root = root.splitting(activePane, with: .leaf(webVC), orientation: orientation)

        rebuildViewHierarchy()

        setActive(webVC)
        webVC.startIfNeeded()
        webVC.view.window?.makeFirstResponder(webVC.view)
    }

    /// Tier 4.1 — split with a pre-built plugin panel. Caller assembles the
    /// PluginPanelController (registry + eventBus deps) and hands us the
    /// NesttyPanel to embed; PaneManager doesn't reach into AppDelegate state.
    func splitActiveWithPluginPanel(_ panel: any NesttyPanel, orientation: SplitOrientation = .horizontal) {
        assignEventBus(to: panel)
        wirePanel(panel)

        root = root.splitting(activePane, with: .leaf(panel), orientation: orientation)

        rebuildViewHierarchy()

        setActive(panel)
        panel.startIfNeeded()
        panel.view.window?.makeFirstResponder(panel.view)
    }

    func closeActive() {
        let closing = activePane
        guard let newRoot = root.removing(closing) else {
            closing.view.removeFromSuperview()
            closing.removeFromParent()
            onLastPaneClosed?()
            return
        }

        root = newRoot
        closing.view.removeFromSuperview()
        closing.removeFromParent()
        rebuildViewHierarchy()

        let next = root.allLeaves().first!
        setActive(next)
        next.view.window?.makeFirstResponder(next.view)
    }

    func setActive(_ panel: any NesttyPanel) {
        activePane = panel
        onActivePaneChanged?()
        eventBus?.broadcast(event: "panel.focused", data: ["panel_id": panel.panelID])
    }

    private func propagateEventBus() {
        allPanels().forEach { assignEventBus(to: $0) }
    }

    private func assignEventBus(to panel: any NesttyPanel) {
        if let t = panel as? TerminalViewController { t.eventBus = eventBus }
        if let w = panel as? WebViewController { w.eventBus = eventBus }
    }

    func allPanels() -> [any NesttyPanel] {
        root.allLeaves()
    }

    /// Tier 1.1 — pane focus navigation. Cycle the active pane forward (`+1`)
    /// or backward (`-1`) over the DFS order of leaves under `root`. Wraps
    /// at both ends. No-op when the tab has only one pane. Used by the
    /// Cmd+Shift+] / Cmd+Shift+[ menu items in `AppDelegate`.
    func focusNextPane(direction: Int = 1) {
        let leaves = root.allLeaves()
        guard leaves.count > 1 else { return }
        let currentIdx = leaves.firstIndex { ObjectIdentifier($0 as AnyObject) == ObjectIdentifier(activePane as AnyObject) }
        guard let idx = currentIdx else { return }
        let count = leaves.count
        // Modulo handles both directions including negative wrap.
        let nextIdx = ((idx + direction) % count + count) % count
        let next = leaves[nextIdx]
        setActive(next)
        next.view.window?.makeFirstResponder(next.view)
    }

    func allTerminals() -> [TerminalViewController] {
        root.allLeaves().compactMap { $0 as? TerminalViewController }
    }

    func activeTerminal() -> TerminalViewController? {
        activePane as? TerminalViewController
    }

    func activeWebView() -> WebViewController? {
        activePane as? WebViewController
    }

    func setCustomTitle(_ title: String) {
        (activePane as? TerminalViewController)?.setCustomTitle(title)
    }

    func applyBackground(path: String, tint: Double, opacity: Double) {
        allPanels().forEach { $0.applyBackground(path: path, tint: tint, opacity: opacity) }
    }

    func clearBackground() {
        allPanels().forEach { $0.clearBackground() }
    }

    func setTint(_ alpha: Double) {
        allPanels().forEach { $0.setTint(alpha) }
    }

    /// Single hot-reload entry: snapshot the new config/theme so split-spawned panes
    /// pick them up, then fan out to existing terminals.
    func applyConfig(_ newConfig: NesttyConfig, theme newTheme: NesttyTheme) {
        config = newConfig
        theme = newTheme
        for term in allTerminals() {
            term.applyTheme(newTheme)
            term.applyFont(family: newConfig.fontFamily, baseSize: CGFloat(newConfig.fontSize))
            term.applyOSC52Policy(newConfig.osc52)
        }
    }

    // MARK: - View Hierarchy

    /// Rebuilds the entire view hierarchy from the SplitNode tree.
    /// This is called on every split/close, creating fresh EqualSplitViews each time.
    private func rebuildViewHierarchy() {
        NSLayoutConstraint.deactivate(rootConstraints)
        rootConstraints = []
        containerView.subviews.forEach { $0.removeFromSuperview() }

        let rootView = buildView(from: root)
        rootView.translatesAutoresizingMaskIntoConstraints = false
        containerView.addSubview(rootView)

        let constraints = [
            rootView.topAnchor.constraint(equalTo: containerView.topAnchor),
            rootView.leadingAnchor.constraint(equalTo: containerView.leadingAnchor),
            rootView.trailingAnchor.constraint(equalTo: containerView.trailingAnchor),
            rootView.bottomAnchor.constraint(equalTo: containerView.bottomAnchor),
        ]
        NSLayoutConstraint.activate(constraints)
        rootConstraints = constraints
    }

    /// Recursively builds the view tree. NSSplitView manages subview sizing,
    /// so direct children use translatesAutoresizingMaskIntoConstraints = true.
    private func buildView(from node: SplitNode) -> NSView {
        switch node {
        case let .leaf(panel):
            panel.view.translatesAutoresizingMaskIntoConstraints = true
            panel.view.autoresizingMask = [.width, .height]
            return panel.view

        case let .branch(orientation, children):
            let sv = EqualSplitView()
            sv.isVertical = (orientation == .horizontal)
            sv.dividerStyle = .thin
            for child in children {
                sv.addSubview(buildView(from: child))
            }
            return sv
        }
    }

    // MARK: - Focus Monitor

    private func installClickMonitor() {
        clickMonitor = NSEvent.addLocalMonitorForEvents(matching: .leftMouseDown) { [weak self] event in
            guard let self else { return event }
            let leaves = root.allLeaves()
            guard leaves.count > 1 else { return event }
            for panel in leaves {
                let view = panel.view
                let locationInView = view.convert(event.locationInWindow, from: nil)
                if view.bounds.contains(locationInView) {
                    setActive(panel)
                    break
                }
            }
            return event
        }
    }

    /// Tier 1.5 — Cmd+click anywhere in a terminal pane: detect a plain-text
    /// URL at the click position via `URLClickHelper` and open it. Returning
    /// `nil` consumes the event so SwiftTerm's own `mouseUp` doesn't also
    /// see it (which would be harmless — its OSC 8 path looks at a payload
    /// we don't have on plain text — but consuming keeps the responder
    /// chain tidy). Returning the event when no URL matches is intentional:
    /// SwiftTerm still gets to handle OSC 8 hyperlinks via its built-in
    /// `requestOpenLink` flow.
    private func installURLClickMonitor() {
        urlClickMonitor = NSEvent.addLocalMonitorForEvents(matching: .leftMouseUp) { [weak self] event in
            guard let self else { return event }
            guard event.modifierFlags.contains(.command) else { return event }
            for panel in root.allLeaves() {
                guard let termVC = panel as? TerminalViewController,
                      let terminalView = termVC.terminalView
                else { continue }
                let locInTerm = terminalView.convert(event.locationInWindow, from: nil)
                guard terminalView.bounds.contains(locInTerm) else { continue }
                if let url = URLClickHelper.findURL(at: event, in: terminalView) {
                    NSWorkspace.shared.open(url)
                    return nil
                }
            }
            return event
        }
    }

    // MARK: - Panel Wiring

    private func wirePanel(_ panel: any NesttyPanel) {
        if let termVC = panel as? TerminalViewController {
            termVC.onProcessTerminated = { [weak self, weak termVC] in
                guard let self, let termVC else { return }
                if ObjectIdentifier(termVC) == ObjectIdentifier(activePane) {
                    closeActive()
                } else {
                    guard let newRoot = root.removing(termVC) else {
                        onLastPaneClosed?(); return
                    }
                    termVC.view.removeFromSuperview()
                    termVC.removeFromParent()
                    root = newRoot
                    rebuildViewHierarchy()
                }
            }
        }
    }
}
