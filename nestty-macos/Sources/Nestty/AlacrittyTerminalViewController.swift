import AppKit
import Foundation

/// Phase 3.1 scaffold for the alacritty_terminal-backed terminal pane
/// (see docs/macos-renderer-migration-plan.md). Conforms to
/// `NesttyPanel` so PaneManager / SplitNode / socket commands treat it
/// identically to `TerminalViewController`. PTY + grid rendering land
/// in Phase 3.2; this stage just proves the factory branch reaches a
/// distinct, visible view.
///
/// Visual scaffold: dark-gray fill + a label printing the FFI
/// version string + the panel UUID — enough to confirm the renderer
/// flag is honored when comparing side-by-side with a swiftterm tab.
@MainActor
final class AlacrittyTerminalViewController: NSViewController, NesttyPanel {
    let panelID: String = UUID().uuidString
    private(set) var currentTitle: String = "Terminal (alacritty)"

    private let config: NesttyConfig
    private let theme: NesttyTheme
    private let initialCwd: String?
    private let initialInput: String?

    /// Lifecycle of the Rust-side `nestty-term` handle. Created in
    /// `startIfNeeded` so `loadView`'s container is in the hierarchy
    /// before the PTY spawns (matching the SwiftTerm path's
    /// pattern). Phase 3.1: handle is created but not yet rendered.
    private var termHandle: NesttyTermFFI.Handle?
    private var shellStarted = false

    init(config: NesttyConfig, theme: NesttyTheme, cwd: String? = nil, initialInput: String? = nil) {
        self.config = config
        self.theme = theme
        initialCwd = cwd
        self.initialInput = initialInput
        super.init(nibName: nil, bundle: nil)
    }

    @available(*, unavailable)
    required init?(coder _: NSCoder) {
        fatalError("init(coder:) has not been implemented")
    }

    override func loadView() {
        let container = NSView(frame: NSRect(x: 0, y: 0, width: 1200, height: 800))
        container.wantsLayer = true
        container.layer?.backgroundColor = theme.background.nsColor.cgColor

        // Placeholder banner — temporary visual cue for Phase 3.1. The
        // CTLine-based render path replaces this in Phase 3.2.
        let banner = NSTextField(labelWithString: bannerText())
        banner.font = .monospacedSystemFont(ofSize: 11, weight: .regular)
        banner.textColor = theme.subtext0.nsColor
        banner.translatesAutoresizingMaskIntoConstraints = false
        banner.alignment = .center
        container.addSubview(banner)
        NSLayoutConstraint.activate([
            banner.centerXAnchor.constraint(equalTo: container.centerXAnchor),
            banner.centerYAnchor.constraint(equalTo: container.centerYAnchor),
        ])
        view = container
    }

    private func bannerText() -> String {
        "alacritty renderer scaffold — \(NesttyTermFFI.version())\npanel \(panelID)"
    }

    func startIfNeeded() {
        guard !shellStarted else { return }
        shellStarted = true
        // Spawn the PTY immediately so subsequent phases have a live
        // shell to render. Output is discarded until Phase 3.2 wires
        // the renderer; resize is honored once the view has a frame
        // (geometry → cell math lands with the renderer).
        let cols = UInt16(80)
        let rows = UInt16(24)
        termHandle = NesttyTermFFI.Handle(
            cols: cols,
            rows: rows,
            shell: initialCwd != nil ? config.shell : nil,
            cwd: initialCwd,
        )
        if let initialInput {
            termHandle?.input(Array(initialInput.utf8))
        }
    }

    // MARK: - NesttyPanel — background + tint

    func applyBackground(path _: String, tint _: Double, opacity _: Double) {
        // Phase 3.5 will materialize the Zed pattern (opaque per-cell
        // bg + image layer below). No-op until then so swapping
        // backends on a config with `[background]` doesn't crash.
    }

    func clearBackground() {
        // See applyBackground.
    }

    func setTint(_: Double) {
        // See applyBackground.
    }
}
