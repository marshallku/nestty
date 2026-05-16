import AppKit
import Foundation

/// Cmd+Shift+P modal palette mirroring Linux's Ctrl+Shift+P
/// (`nestty-linux/src/command_palette.rs`, commit f8a77c8). Macro shape:
/// `NSSearchField` over a filtered `NSTableView`, Enter dispatches the
/// selected action through `AppDelegate.handleCommand` so registered +
/// legacy + daemon-fallback methods all reach the same pump.
///
/// **AppKit lifetime**: `NSTableView` data source / delegate are unowned
/// references. The closing window cannot also own the controller — the
/// controller must be retained externally for the entire sheet lifetime.
/// `AppDelegate` holds `commandPaletteController` while the sheet is
/// attached and clears it from the `endSheet` completion. Without that
/// the table goes blank or stops responding the moment `open` returns.
///
/// **v1 limitations** (mirror of `docs/decisions.md` decision 29):
///
/// - **Empty params only.** Actions that need params (`terminal.exec`,
///   `webview.navigate`, `tab.rename`, …) surface their normal
///   `invalid_params` error to stderr. v2 can wire a second-stage form.
/// - **Focus restore on Esc/Cancel only.** Post-dispatch leaves focus to
///   the action — `tab.close` removes the captured pane, `tab.new` /
///   `webview.open` / split commands intentionally change the active
///   view, so restoring the pre-palette responder would either crash
///   (gone view) or fight the new focus.
@MainActor
final class CommandPaletteController: NSObject, NSTableViewDataSource, NSTableViewDelegate, NSSearchFieldDelegate {
    private let palette: NSWindow
    private let parentWindow: NSWindow
    private let actions: [String]
    private var filtered: [String]
    private weak var search: NSSearchField?
    private weak var table: NSTableView?
    private weak var restoreFocus: NSResponder?
    private var localKeyMonitor: Any?
    private let dispatch: (String) -> Void
    private let onClose: () -> Void

    /// Empty-param dispatch on these would silently destroy live work.
    /// Mirrors `command_palette::DESTRUCTIVE_ACTIONS` on Linux.
    static let destructiveActions: Set<String> = ["tab.close"]

    init(
        parentWindow: NSWindow,
        actions: [String],
        restoreFocus: NSResponder?,
        dispatch: @escaping (String) -> Void,
        onClose: @escaping () -> Void,
    ) {
        let panel = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 520, height: 420),
            styleMask: [.titled],
            backing: .buffered,
            defer: false,
        )
        panel.title = "Command palette"
        panel.isReleasedWhenClosed = false
        palette = panel
        self.parentWindow = parentWindow
        self.actions = actions
        filtered = actions
        self.restoreFocus = restoreFocus
        self.dispatch = dispatch
        self.onClose = onClose
        super.init()
        buildContent()
    }

    /// Open as window-modal sheet. Caller owns the controller's lifetime —
    /// hold the reference until `onClose` fires.
    func open() {
        installLocalKeyMonitor()
        parentWindow.beginSheet(palette) { [weak self] _ in
            self?.onClose()
        }
        // `beginSheet` makes `palette` key but does not focus the
        // search field automatically.
        palette.makeFirstResponder(search)
    }

    private func buildContent() {
        let content = NSView(frame: palette.contentRect(forFrameRect: palette.frame))
        content.autoresizingMask = [.width, .height]

        let searchField = NSSearchField(frame: .zero)
        searchField.translatesAutoresizingMaskIntoConstraints = false
        searchField.placeholderString = "Type to filter actions…"
        searchField.delegate = self
        searchField.target = self
        searchField.action = #selector(searchEnter(_:))
        content.addSubview(searchField)
        search = searchField

        let scroll = NSScrollView(frame: .zero)
        scroll.translatesAutoresizingMaskIntoConstraints = false
        scroll.hasVerticalScroller = true
        scroll.borderType = .bezelBorder

        let tableView = NSTableView(frame: .zero)
        tableView.headerView = nil
        tableView.allowsMultipleSelection = false
        tableView.allowsEmptySelection = false
        tableView.usesAlternatingRowBackgroundColors = false
        tableView.rowHeight = 22
        tableView.target = self
        tableView.doubleAction = #selector(tableDoubleClick(_:))

        let column = NSTableColumn(identifier: NSUserInterfaceItemIdentifier("name"))
        column.title = "Action"
        column.resizingMask = [.autoresizingMask]
        tableView.addTableColumn(column)

        tableView.dataSource = self
        tableView.delegate = self
        scroll.documentView = tableView
        content.addSubview(scroll)
        table = tableView

        NSLayoutConstraint.activate([
            searchField.topAnchor.constraint(equalTo: content.topAnchor, constant: 8),
            searchField.leadingAnchor.constraint(equalTo: content.leadingAnchor, constant: 8),
            searchField.trailingAnchor.constraint(equalTo: content.trailingAnchor, constant: -8),
            scroll.topAnchor.constraint(equalTo: searchField.bottomAnchor, constant: 6),
            scroll.leadingAnchor.constraint(equalTo: content.leadingAnchor, constant: 8),
            scroll.trailingAnchor.constraint(equalTo: content.trailingAnchor, constant: -8),
            scroll.bottomAnchor.constraint(equalTo: content.bottomAnchor, constant: -8),
        ])

        palette.contentView = content
        selectFirstRow()
    }

    /// Up/Down + Esc handled at panel scope so NSSearchField keeps focus
    /// while the user navigates rows. AppKit doesn't expose a closure-based
    /// key handler on the panel itself; a local NSEvent monitor scoped to
    /// the controller's lifetime is the cleanest path.
    private func installLocalKeyMonitor() {
        localKeyMonitor = NSEvent.addLocalMonitorForEvents(matching: .keyDown) { [weak self] event in
            guard
                let self,
                event.window === self.palette
            else { return event }
            switch event.keyCode {
            case 53: // Escape
                cancel()
                return nil
            case 125: // Down
                moveSelection(by: 1)
                return nil
            case 126: // Up
                moveSelection(by: -1)
                return nil
            default:
                return event
            }
        }
    }

    private func uninstallLocalKeyMonitor() {
        if let token = localKeyMonitor {
            NSEvent.removeMonitor(token)
            localKeyMonitor = nil
        }
    }

    private func cancel() {
        closeSheet { [weak self] in
            self?.restorePreviousFocus()
        }
    }

    private func closeSheet(then: (() -> Void)? = nil) {
        uninstallLocalKeyMonitor()
        parentWindow.endSheet(palette)
        then?()
    }

    private func restorePreviousFocus() {
        guard let restore = restoreFocus else { return }
        parentWindow.makeFirstResponder(restore)
    }

    private func selectFirstRow() {
        guard let table else { return }
        if !filtered.isEmpty {
            table.selectRowIndexes(IndexSet(integer: 0), byExtendingSelection: false)
        }
    }

    private func moveSelection(by delta: Int) {
        guard let table else { return }
        if filtered.isEmpty { return }
        let current = table.selectedRow
        let base = current >= 0 ? current : 0
        let next = max(0, min(filtered.count - 1, base + delta))
        table.selectRowIndexes(IndexSet(integer: next), byExtendingSelection: false)
        table.scrollRowToVisible(next)
    }

    @objc private func searchEnter(_: Any) {
        activateSelected()
    }

    @objc private func tableDoubleClick(_: Any) {
        activateSelected()
    }

    private func activateSelected() {
        guard let table, table.selectedRow >= 0, table.selectedRow < filtered.count else { return }
        let action = filtered[table.selectedRow]
        if Self.destructiveActions.contains(action) {
            confirmThenDispatch(action: action)
        } else {
            closeSheet()
            dispatch(action)
        }
    }

    /// Cancel is the default + cancel button so a stray Enter (after the
    /// palette's Enter closes the sheet) won't fall through into the
    /// destructive action. Mirrors Linux's `confirm_then_dispatch`
    /// alert dialog button ordering.
    private func confirmThenDispatch(action: String) {
        // Capture strongly BEFORE closeSheet — endSheet completion clears
        // AppDelegate's controller ivar (the only strong owner), so a
        // `[weak self]` closure here would no-op on Confirm and skip
        // focus restore on Cancel by the time the alert dismisses.
        let dispatch = dispatch
        let restoreFocus = restoreFocus
        let parentWindow = parentWindow
        closeSheet()
        let alert = NSAlert()
        alert.messageText = "Confirm action: \(action)"
        alert.informativeText = "This is a destructive action. Cancel and re-run if unintended."
        let cancelButton = alert.addButton(withTitle: "Cancel")
        let confirmButton = alert.addButton(withTitle: "Confirm")
        cancelButton.keyEquivalent = "\r"
        confirmButton.keyEquivalent = ""
        alert.beginSheetModal(for: parentWindow) { response in
            if response == .alertSecondButtonReturn {
                dispatch(action)
            } else if let restoreFocus {
                parentWindow.makeFirstResponder(restoreFocus)
            }
        }
    }

    // MARK: - NSTableViewDataSource

    func numberOfRows(in _: NSTableView) -> Int {
        filtered.count
    }

    func tableView(_: NSTableView, objectValueFor _: NSTableColumn?, row: Int) -> Any? {
        filtered[row]
    }

    // MARK: - NSSearchFieldDelegate

    func controlTextDidChange(_ obj: Notification) {
        guard let field = obj.object as? NSSearchField else { return }
        applyFilter(field.stringValue)
    }

    private func applyFilter(_ query: String) {
        filtered = CommandPalette.filterActions(actions, query: query)
        table?.reloadData()
        selectFirstRow()
    }
}

/// Stateless filter helpers + action-surface assembly. Matches Linux
/// `command_palette::filter_actions` semantics: case-insensitive substring,
/// whitespace-trimmed; empty query returns every entry.
enum CommandPalette {
    /// macOS-side handlers in `AppDelegate.handleCommand`. Hand-kept in
    /// sync with the switch arms — `LEGACY_DISPATCH_METHODS` in
    /// `nestty-daemon::socket` is similar but not identical (daemon list
    /// omits `tab.switch`, `pane.focus_next`, `pane.focus_prev`,
    /// `terminal.shell_precmd`, `terminal.shell_preexec`, `webview.state`).
    /// Source-of-truth: case arms in `AppDelegate.swift` `handleCommand`.
    static let macOSLegacyMethods: [String] = [
        "agent.approve",
        "background.clear",
        "background.next",
        "background.set",
        "background.set_tint",
        "background.toggle",
        "pane.focus_next",
        "pane.focus_prev",
        "plugin.open",
        "session.info",
        "session.list",
        "split.horizontal",
        "split.vertical",
        "statusbar.hide",
        "statusbar.show",
        "statusbar.toggle",
        "system.ping",
        "tab.close",
        "tab.info",
        "tab.list",
        "tab.new",
        "tab.rename",
        "tab.switch",
        "tabs.toggle_bar",
        "terminal.context",
        "terminal.exec",
        "terminal.feed",
        "terminal.history",
        "terminal.read",
        "terminal.shell_precmd",
        "terminal.shell_preexec",
        "terminal.state",
        "webview.back",
        "webview.click",
        "webview.devtools",
        "webview.execute_js",
        "webview.fill",
        "webview.forward",
        "webview.get_content",
        "webview.get_styles",
        "webview.navigate",
        "webview.open",
        "webview.page_info",
        "webview.query",
        "webview.query_all",
        "webview.reload",
        "webview.screenshot",
        "webview.scroll",
        "webview.state",
    ]

    @MainActor
    static func collectActions(registry: ActionRegistry) -> [String] {
        var seen = Set<String>()
        var out: [String] = []
        for name in registry.names() where seen.insert(name).inserted {
            out.append(name)
        }
        for name in macOSLegacyMethods where seen.insert(name).inserted {
            out.append(name)
        }
        return out.sorted()
    }

    static func filterActions(_ entries: [String], query: String) -> [String] {
        let q = query.trimmingCharacters(in: .whitespaces).lowercased()
        if q.isEmpty { return entries }
        return entries.filter { $0.lowercased().contains(q) }
    }
}
