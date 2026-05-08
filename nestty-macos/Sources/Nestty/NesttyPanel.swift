import AppKit

/// Common interface for all panel types (terminal, webview, …).
/// Both TerminalViewController and WebViewController conform to this.
@MainActor
protocol NesttyPanel: AnyObject {
    /// Stable identifier for this panel (UUID string). Used in event payloads.
    var panelID: String { get }

    /// The root NSView managed by this panel (from NSViewController).
    var view: NSView { get }

    /// Title shown in the tab bar.
    var currentTitle: String { get }

    /// Called once after the panel's view is embedded and layout is resolved.
    func startIfNeeded()

    /// Background image (no-op for panels that don't support it).
    func applyBackground(path: String, tint: Double, opacity: Double)
    func clearBackground()
    func setTint(_ alpha: Double)

    /// NSViewController lifecycle (satisfied automatically by subclasses).
    func removeFromParent()
}
