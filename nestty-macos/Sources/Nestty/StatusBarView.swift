import AppKit
import Foundation

/// Tier 4.2 — Waybar-style status bar that mirrors `nestty-linux/src/statusbar.rs`
/// in shape:
///
/// - 3 zones (left/center/right) laid out horizontally
/// - per-module `NSTextField` label, sorted by `order` within zone
/// - each module runs a shell command on a `DispatchSourceTimer`
/// - stdout parsed as either plain text or JSON `{text, tooltip}`
///
/// macOS-specific simplifications vs Linux:
///
/// - No CSS hot-reload. Theme colors applied once at view-build time. If the
///   user changes themes the bar will pick up new colors on `applyTheme`
///   (called by AppDelegate's hot-reload path; not yet wired).
/// - Position support is `bottom` only. Linux supports both — top requires
///   reshuffling around the tab bar layout we already do in `TabViewController`.
///   Defer until somebody asks.
/// - Module CSS class (`module.class`) ignored. Linux uses GTK CSS to scope
///   per-module styling; we don't have a clean macOS equivalent without
///   reaching for `NSAttributedString` per module. Module authors that want
///   color cues today should JSON-emit the text with markup or unicode
///   indicators instead.
@MainActor
final class StatusBarView: NSView {
    private let leftStack = NSStackView()
    private let centerStack = NSStackView()
    private let rightStack = NSStackView()
    private var runners: [StatusModuleRunner] = []
    /// Map from `<plugin>.<module>` → label so future event-driven updates
    /// (push from plugin instead of poll) can target a specific module.
    /// Not used yet; kept so the API doesn't have to change later.
    private var labels: [String: NSTextField] = [:]

    /// Backing store for `isHidden` so `statusbar.show/hide/toggle` reads
    /// stay consistent with what we set, even if AppKit's accessor races
    /// across animation states.
    private(set) var isShown: Bool = true

    private let theme: NesttyTheme

    init(theme: NesttyTheme) {
        self.theme = theme
        super.init(frame: .zero)
        translatesAutoresizingMaskIntoConstraints = false
        wantsLayer = true
        layer?.backgroundColor = theme.surface0.nsColor.cgColor
        // 1px top edge so the bar visibly separates from the content above
        // even when the surface0/background contrast is low (Catppuccin
        // Mocha they're nearly the same shade).
        let separator = NSView()
        separator.translatesAutoresizingMaskIntoConstraints = false
        separator.wantsLayer = true
        separator.layer?.backgroundColor = theme.overlay0.nsColor.cgColor
        addSubview(separator)
        NSLayoutConstraint.activate([
            separator.topAnchor.constraint(equalTo: topAnchor),
            separator.leadingAnchor.constraint(equalTo: leadingAnchor),
            separator.trailingAnchor.constraint(equalTo: trailingAnchor),
            separator.heightAnchor.constraint(equalToConstant: 1),
        ])

        for stack in [leftStack, centerStack, rightStack] {
            stack.orientation = .horizontal
            stack.spacing = 12
            stack.translatesAutoresizingMaskIntoConstraints = false
            addSubview(stack)
        }

        // Three zones laid out in a row: left flush-left, center centered,
        // right flush-right. CenterX anchor on the center stack pins it
        // even as the side stacks grow/shrink with content.
        NSLayoutConstraint.activate([
            leftStack.leadingAnchor.constraint(equalTo: leadingAnchor, constant: 12),
            leftStack.centerYAnchor.constraint(equalTo: centerYAnchor),
            centerStack.centerXAnchor.constraint(equalTo: centerXAnchor),
            centerStack.centerYAnchor.constraint(equalTo: centerYAnchor),
            rightStack.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -12),
            rightStack.centerYAnchor.constraint(equalTo: centerYAnchor),
        ])
    }

    @available(*, unavailable)
    required init?(coder _: NSCoder) {
        fatalError()
    }

    /// Build module labels + start their runners. Called once at app launch
    /// from `TabViewController.loadView`. Each `LoadedPluginManifest` may
    /// contribute zero or more modules.
    func loadModules(_ plugins: [LoadedPluginManifest], socketPath: String) {
        // Group + sort within each zone by `order` so manifest ordering
        // doesn't dictate display position. Linux does the same.
        var byZone: [String: [(plugin: LoadedPluginManifest, module: PluginModuleDef)]] = [
            "left": [], "center": [], "right": [],
        ]
        for plugin in plugins {
            for module in plugin.manifest.modules {
                let zone = ["left", "center", "right"].contains(module.position) ? module.position : "right"
                byZone[zone, default: []].append((plugin, module))
            }
        }
        for (_, _) in byZone {
            // No-op for empty zones; sort in-place by order.
        }
        for zone in ["left", "center", "right"] {
            let entries = byZone[zone, default: []].sorted { $0.module.order < $1.module.order }
            let stack = stackForZone(zone)
            for (plugin, module) in entries {
                let label = NSTextField(labelWithString: "...")
                label.textColor = theme.text.nsColor
                label.font = .systemFont(ofSize: 12)
                label.alignment = .center
                stack.addArrangedSubview(label)
                let key = "\(plugin.manifest.plugin.name).\(module.name)"
                labels[key] = label
                let runner = StatusModuleRunner(
                    label: label,
                    pluginDir: plugin.dir,
                    moduleName: key,
                    exec: module.exec,
                    interval: module.interval,
                    socketPath: socketPath,
                )
                runner.start()
                runners.append(runner)
            }
        }
        let total = runners.count
        if total > 0 {
            FileHandle.standardError.write(Data("[nestty] statusbar: \(total) module(s) loaded\n".utf8))
        }
    }

    /// Stop every running module timer. Idempotent. Called from
    /// `applicationWillTerminate` so we don't leave child processes orphaned
    /// for the brief window between quit and process exit.
    func shutdown() {
        for r in runners {
            r.stop()
        }
        runners.removeAll()
    }

    /// `statusbar.show/hide/toggle` socket commands route through this.
    /// Returns the post-call visibility state.
    @discardableResult
    func setShown(_ shown: Bool) -> Bool {
        isShown = shown
        isHidden = !shown
        return shown
    }

    private func stackForZone(_ zone: String) -> NSStackView {
        switch zone {
        case "left": leftStack
        case "center": centerStack
        default: rightStack
        }
    }
}
