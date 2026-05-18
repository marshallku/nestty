import AppKit
import CNesttyTerm
import Foundation

/// Phase 3.2 — `nestty-term` (alacritty_terminal-backed) pane with
/// CoreText cell rendering. Conforms to `NesttyPanel` so PaneManager
/// / SplitNode / socket commands treat it identically to
/// `TerminalViewController`. See
/// docs/macos-renderer-migration-plan.md for the staged scope.
///
/// What ships in this slice:
///
/// - PTY spawn (`NesttyTermFFI.Handle`) — already lands in 3.1
/// - CoreText cell draw — snapshot → row-by-row attributed strings
///   built from each run's borrowed utf8 + CTLine + CTLineDraw
/// - Periodic refresh (Timer at ~30 Hz) — Phase 3.6 will replace with
///   damage-tracked CADisplayLink
/// - Keyboard input — printable chars + the common control bytes
///   shells need to function (Return, Backspace, Tab, Esc, arrows)
///
/// Deferred:
///
/// - Cursor render (Phase 3.3)
/// - ANSI palette + inverse video (Phase 3.4)
/// - Image background + Zed-pattern materialize (Phase 3.5)
/// - Damage tracking + selection + IME + ligatures + automation
///   parity (Phases 3.6, 4, 5, 6, 7)
@MainActor
final class AlacrittyTerminalViewController: NSViewController, NesttyPanel {
    let panelID: String = UUID().uuidString
    private(set) var currentTitle: String = "Terminal (alacritty)"

    private let config: NesttyConfig
    private var theme: NesttyTheme
    private let initialCwd: String?
    private let initialInput: String?

    private var termHandle: NesttyTermFFI.Handle?
    private var renderView: AlacrittyRenderView?
    private var backgroundView: NSImageView?
    private var tintView: NSView?
    private var shellStarted = false

    /// Focus target for `panel.focusTarget` — callers like PaneManager
    /// that activate a pane (`makeFirstResponder`) need the renderView,
    /// not the layout container.
    var focusTarget: NSView {
        renderView ?? view
    }

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

    /// Layered view hierarchy mirroring `TerminalViewController`:
    ///   container (plain NSView)
    ///   ├─ backgroundView (NSImageView, hidden until image set)
    ///   ├─ tintView (NSView with dark overlay layer)
    ///   └─ renderView (AlacrittyRenderView, transparent layer when image active)
    ///
    /// Focus contract: external callers that target `panel.view` (the
    /// container) get a silent no-op because the container's default
    /// `acceptsFirstResponder` is false. The render view becomes
    /// first responder via `startIfNeeded`'s explicit
    /// `makeFirstResponder(render)` call, via user mouse clicks (the
    /// `mouseDown` override re-asserts focus), and via the
    /// activate-on-tab-switch path going through PaneManager. This
    /// mirrors what SwiftTerm's `TerminalViewController` does — the
    /// container is just a layout host, not a focus participant.
    override func loadView() {
        let frame = NSRect(x: 0, y: 0, width: 1200, height: 800)
        let container = NSView(frame: frame)
        container.wantsLayer = true

        let bg = NSImageView(frame: container.bounds)
        bg.autoresizingMask = [.width, .height]
        bg.imageScaling = .scaleAxesIndependently
        bg.wantsLayer = true
        bg.isHidden = true
        container.addSubview(bg)
        backgroundView = bg

        let tint = NSView(frame: container.bounds)
        tint.autoresizingMask = [.width, .height]
        tint.wantsLayer = true
        tint.isHidden = true
        container.addSubview(tint)
        tintView = tint

        let render = AlacrittyRenderView(
            theme: theme,
            font: resolveFont(family: config.fontFamily, size: CGFloat(config.fontSize)),
            transparentDefaultBg: config.transparentDefaultBg,
            osc52Policy: config.osc52,
        )
        render.frame = container.bounds
        render.autoresizingMask = [.width, .height]
        container.addSubview(render)
        renderView = render

        view = container

        // Apply background from config if set. Runs before viewDidAppear,
        // which is fine: NSImageView accepts an image even off-screen, and
        // we re-snap layer state in applyBackground itself.
        if let path = config.backgroundPath {
            applyBackground(path: path, tint: config.backgroundTint, opacity: config.backgroundOpacity)
        }
    }

    override func viewDidLayout() {
        super.viewDidLayout()
        // Compute terminal grid size from view bounds + cell metrics so
        // the shell sees a winsize matching what we'll actually draw.
        guard let render = renderView else { return }
        let (cols, rows) = render.computeGrid()
        termHandle?.resize(cols: cols, rows: rows)
    }

    func startIfNeeded() {
        guard !shellStarted else { return }
        shellStarted = true
        let (cols, rows) = renderView?.computeGrid() ?? (80, 24)
        termHandle = NesttyTermFFI.Handle(
            cols: cols,
            rows: rows,
            shell: initialCwd != nil ? config.shell : nil,
            cwd: initialCwd,
        )
        if let initialInput {
            termHandle?.input(Array(initialInput.utf8))
        }
        renderView?.bind(handle: termHandle)
        // Target the render view explicitly. The container forwards too
        // (belt and braces for callers that already have a reference to
        // `panel.view`), but going direct skips the second hop.
        if let render = renderView {
            view.window?.makeFirstResponder(render)
        }
    }

    /// Config hot-reload: flip the OSC 52 policy on the live render
    /// view so already-open alacritty panes start honoring the new
    /// `[security] osc52` setting without needing to be recreated.
    func applyOSC52Policy(_ policy: OSC52Policy) {
        renderView?.setOSC52Policy(policy)
    }

    /// Config hot-reload: swap the theme on a running pane. Mirrors
    /// `TerminalViewController.applyTheme` so `PaneManager.applyConfig`
    /// can fan out theme changes uniformly across both backends.
    func applyTheme(_ newTheme: NesttyTheme) {
        theme = newTheme
        renderView?.setTheme(newTheme)
    }

    /// Config hot-reload: swap the font family/size on a running pane.
    /// When the new face changes cell metrics, also resize the term
    /// grid so the PTY's winsize matches what the renderer will draw.
    /// (We don't preserve an independent zoom level the way SwiftTerm
    /// does — alacritty path has no Cmd+/- yet, so baseSize IS the
    /// active size.)
    func applyFont(family: String, baseSize: CGFloat) {
        let newFont = resolveFont(family: family, size: baseSize)
        guard let render = renderView else { return }
        let metricsChanged = render.setFont(newFont)
        if metricsChanged {
            let (cols, rows) = render.computeGrid()
            termHandle?.resize(cols: cols, rows: rows)
        }
    }

    // MARK: - NesttyPanel — background

    /// Wire an image background + tint overlay. The render view's layer
    /// goes transparent so the image layer underneath composites
    /// through. `transparent_default_bg` config decides whether default
    /// cells fill opaquely on top (image hidden behind text area, cursor
    /// always visible) or stay transparent (image visible through blank
    /// cells, cursor visibility depends on accent vs image contrast).
    func applyBackground(path: String, tint: Double, opacity: Double) {
        guard let image = NSImage(contentsOfFile: path) else { return }
        backgroundView?.image = image
        backgroundView?.alphaValue = CGFloat(opacity)
        backgroundView?.isHidden = false
        tintView?.layer?.backgroundColor = NSColor.black.withAlphaComponent(CGFloat(tint)).cgColor
        tintView?.isHidden = opacity == 0
        renderView?.setImageBackgroundActive(true)
        renderView?.needsDisplay = true
    }

    func clearBackground() {
        backgroundView?.image = nil
        backgroundView?.isHidden = true
        tintView?.isHidden = true
        renderView?.setImageBackgroundActive(false)
        renderView?.needsDisplay = true
    }

    func setTint(_ alpha: Double) {
        tintView?.layer?.backgroundColor = NSColor.black.withAlphaComponent(CGFloat(alpha)).cgColor
    }

    // MARK: - Font

    /// Mirrors `TerminalViewController.resolveFont` — PostScript name
    /// → family lookup → case-insensitive fallback → monospaced
    /// system. Trimmed to the cases we need for the alacritty path.
    private func resolveFont(family: String, size: CGFloat) -> NSFont {
        if let font = NSFont(name: family, size: size) { return font }
        let manager = NSFontManager.shared
        if let font = manager.font(withFamily: family, traits: [], weight: 5, size: size) {
            return font
        }
        let lower = family.lowercased()
        for fam in manager.availableFontFamilies where fam.lowercased() == lower {
            if let font = manager.font(withFamily: fam, traits: [], weight: 5, size: size) {
                return font
            }
        }
        return .monospacedSystemFont(ofSize: size, weight: .regular)
    }
}

// MARK: - Render view

/// Custom NSView that draws the terminal grid via CoreText. Snapshots
/// are taken under the `nestty-term` handle's `FairMutex`; the lock is
/// dropped before `setNeedsDisplay` so AppKit's redraw doesn't block
/// the PTY reader thread.
///
/// Coordinate system is **flipped** (origin top-left, y down) so row 0
/// renders at the top of the view — matching the terminal convention
/// and keeping cell math straightforward.
@MainActor
private final class AlacrittyRenderView: NSView, @preconcurrency NSTextInputClient {
    /// `var` so `setTheme(_:)` can hot-swap on config reload. Draw
    /// paths read it directly each frame, so a swap takes effect on
    /// the next paint without further plumbing.
    private var theme: NesttyTheme
    private var font: NSFont
    private var boldFont: NSFont
    private var italicFont: NSFont
    private var boldItalicFont: NSFont
    private(set) var cellWidth: CGFloat = 0
    private(set) var cellHeight: CGFloat = 0
    private var ascent: CGFloat = 0

    /// Cached CGColor for the 16-color ANSI palette + xterm 256
    /// extension. Indices 0-15 from `theme.palette` (so theme changes
    /// reflect the right color); 16-231 from the 6×6×6 cube; 232-255
    /// from the grayscale ramp. `var` so `setTheme(_:)` can rebuild it
    /// on hot-reload — the 256-entry rebuild is cheap.
    private var paletteCache: [CGColor]

    private weak var termHandle: NesttyTermFFI.Handle?
    /// CADisplayLink fires once per display refresh (typically 60 Hz,
    /// up to ProMotion's 120 Hz). Replaces the Timer-driven 30 Hz
    /// poll: aligned to vsync (no tearing, no half-frame draws), and
    /// the per-tick `takeDamage` gate means an idle terminal does
    /// zero work between key presses or PTY output bursts.
    ///
    /// `nonisolated(unsafe)` so deinit (Swift 6 nonisolated) can
    /// invalidate without crossing the main-actor barrier — same
    /// pattern as the previous `refreshTimer`.
    private nonisolated(unsafe) var vsyncLink: CADisplayLink?

    /// Cached snapshot for the most recent paint. Refreshed only when
    /// `nestty_term_take_damage` reports the grid changed.
    private var snapshotCache: NesttyTermFFI.Snapshot?

    /// User opt-in: when true AND an image background is active, default
    /// (sentinel-zero) cells render without a bg fill so the image shows
    /// through. Independent of the controller's bg state because the
    /// flag is set at init from the live config; the
    /// `imageBackgroundActive` runtime flag (set/cleared as the user
    /// applies or clears the background) AND-gates the actual behavior
    /// — no image, no transparency, regardless of the user pref.
    private let transparentDefaultBg: Bool
    private var imageBackgroundActive = false

    /// OSC 52 policy from config. `.deny` (default) drops the request
    /// with a stderr warning; `.allow` writes to NSPasteboard.general.
    /// `var` so config hot-reload can flip it without re-creating the
    /// pane — matches `TerminalViewController.applyOSC52Policy`.
    private var osc52Policy: OSC52Policy

    /// Setter for the controller to forward `applyConfig` updates.
    func setOSC52Policy(_ policy: OSC52Policy) {
        osc52Policy = policy
    }

    /// Cursor-blink state. Honored only when the TUI/shell actually
    /// asks for it via DECSCUSR (`cursor.blink == 1` on the snapshot).
    /// When idle with blink on, the display-link callback forces a
    /// redraw every `blinkInterval` even when `takeDamage` says
    /// nothing changed — that's 2 redraws/sec, acceptable cost.
    private var blinkVisible = true
    private var lastBlinkToggle = Date.distantPast
    private let blinkInterval: TimeInterval = 0.5

    /// Trackpad pixel deltas accumulate here between `scrollWheel`
    /// events so a slow swipe (each tick fractional sub-cell) still
    /// eventually produces a whole-cell scroll. Mouse-wheel devices
    /// (`hasPreciseScrollingDeltas == false`) bypass this accumulator
    /// — their per-notch delta is already line-count-shaped.
    private var accumulatedScrollDelta: CGFloat = 0

    /// IME composition state. While the user is composing (Korean
    /// 2-Set, Japanese kana → kanji, Pinyin, …) the system delivers
    /// `setMarkedText` with the in-progress string; nothing flows to
    /// the PTY until the IME commits via `insertText`. We paint the
    /// marked text as an overlay at the cursor cell so the user can
    /// see what they're composing without it ever touching the
    /// terminal buffer.
    ///
    /// `markedSelectedRange` is the IME-highlighted sub-range inside
    /// the marked text (e.g. the active syllable on a multi-syllable
    /// composition). Drawn with a stronger underline.
    private var markedText: String?
    private var markedSelectedRange: NSRange = .init(location: 0, length: 0)

    init(theme: NesttyTheme, font: NSFont, transparentDefaultBg: Bool, osc52Policy: OSC52Policy) {
        self.theme = theme
        self.font = font
        boldFont = Self.deriveTrait(font, mask: .boldFontMask)
        italicFont = Self.deriveTrait(font, mask: .italicFontMask)
        boldItalicFont = Self.deriveTrait(font, mask: [.boldFontMask, .italicFontMask])
        paletteCache = Self.buildPalette(theme: theme)
        self.transparentDefaultBg = transparentDefaultBg
        self.osc52Policy = osc52Policy
        super.init(frame: .zero)
        wantsLayer = true
        layer?.backgroundColor = theme.background.nsColor.cgColor
        recomputeCellMetrics()
        // CADisplayLink can't be created until the view has a window
        // (the link binds to the display showing the view). Hooked up
        // in `viewDidMoveToWindow`.
    }

    /// Hot-reload: swap the theme and rebuild the palette cache.
    /// Draw paths read `theme` and `paletteCache` directly each frame,
    /// so the next paint picks up the new colors. Also touches the
    /// layer bg so a theme change while no image is active flips the
    /// underlying clear-vs-themed layer (matches what
    /// `setImageBackgroundActive` does for the image-on path).
    func setTheme(_ newTheme: NesttyTheme) {
        theme = newTheme
        paletteCache = Self.buildPalette(theme: newTheme)
        if !imageBackgroundActive {
            layer?.backgroundColor = newTheme.background.nsColor.cgColor
        }
        needsDisplay = true
    }

    /// Hot-reload: swap the font (regular face) and rebuild the bold /
    /// italic / bold-italic derivatives and cell metrics. Returns true
    /// when the cell size actually changed so the caller can resize
    /// the term grid to match — without that, the PTY keeps sending
    /// content sized to the old grid.
    @discardableResult
    func setFont(_ newFont: NSFont) -> Bool {
        font = newFont
        boldFont = Self.deriveTrait(newFont, mask: .boldFontMask)
        italicFont = Self.deriveTrait(newFont, mask: .italicFontMask)
        boldItalicFont = Self.deriveTrait(newFont, mask: [.boldFontMask, .italicFontMask])
        let oldW = cellWidth
        let oldH = cellHeight
        recomputeCellMetrics()
        needsDisplay = true
        return cellWidth != oldW || cellHeight != oldH
    }

    /// Called by the controller when `applyBackground` / `clearBackground`
    /// flips the layered-view state. Toggles the layer-bg clear/opaque
    /// AND the bounds-fill skip — both are needed: layer-bg covers the
    /// image even without per-cell draw, and the bounds fill would
    /// re-cover it inside `draw(_:)`.
    func setImageBackgroundActive(_ active: Bool) {
        imageBackgroundActive = active
        layer?.backgroundColor = active
            ? NSColor.clear.cgColor
            : theme.background.nsColor.cgColor
        needsDisplay = true
    }

    /// Apply font traits via NSFontManager, falling back to the regular
    /// face if the family doesn't ship the requested variant (common
    /// for monospace fonts that lack an italic — synthesized italics
    /// are visually awkward, so we just don't slant).
    private static func deriveTrait(_ regular: NSFont, mask: NSFontTraitMask) -> NSFont {
        let mgr = NSFontManager.shared
        if let variant = mgr.convert(regular, toHaveTrait: mask) as NSFont? {
            return variant
        }
        return regular
    }

    /// 256-color ANSI table, computed once at view init. Indices 0-15
    /// follow theme.palette so a theme change re-derives the right
    /// brand colors; 16-231 are the canonical xterm 6×6×6 cube;
    /// 232-255 are the 24-step grayscale ramp.
    private static func buildPalette(theme: NesttyTheme) -> [CGColor] {
        var out: [CGColor] = []
        out.reserveCapacity(256)
        for c in theme.palette {
            out.append(c.nsColor.cgColor)
        }
        // Defensive padding if a theme ships fewer than 16 palette
        // entries — black for the missing slots so a stray index
        // doesn't crash.
        while out.count < 16 {
            out.append(CGColor(red: 0, green: 0, blue: 0, alpha: 1))
        }
        // 6×6×6 RGB cube (216 colors).
        let cubeLevels: [CGFloat] = [0, 95, 135, 175, 215, 255].map { $0 / 255.0 }
        for r in 0 ..< 6 {
            for g in 0 ..< 6 {
                for b in 0 ..< 6 {
                    out.append(CGColor(red: cubeLevels[r], green: cubeLevels[g], blue: cubeLevels[b], alpha: 1))
                }
            }
        }
        // 24-step grayscale.
        for i in 0 ..< 24 {
            let v = CGFloat(8 + i * 10) / 255.0
            out.append(CGColor(red: v, green: v, blue: v, alpha: 1))
        }
        return out
    }

    /// Decode the fg/bg encoding from `nestty-term::color_to_rgba`.
    /// High byte is a tag: 0x00=default, 0x01=indexed (low byte holds
    /// the index), 0xFF=direct RGB in the low 24 bits. Tagged because
    /// the old "alpha byte = 0 means indexed" trick collided with RGB
    /// colors that have R=0 (skyblue, pure green) — those silently
    /// fell into the indexed path and rendered as grayscale.
    private func resolveColor(_ packed: UInt32, defaultColor: CGColor) -> CGColor {
        let tag = (packed >> 24) & 0xFF
        switch tag {
        case 0x00:
            return defaultColor
        case 0x01:
            let idx = Int(packed & 0xFF)
            return idx < paletteCache.count ? paletteCache[idx] : defaultColor
        case 0xFF:
            let r = CGFloat((packed >> 16) & 0xFF) / 255.0
            let g = CGFloat((packed >> 8) & 0xFF) / 255.0
            let b = CGFloat(packed & 0xFF) / 255.0
            return CGColor(red: r, green: g, blue: b, alpha: 1.0)
        default:
            return defaultColor
        }
    }

    @available(*, unavailable)
    required init?(coder _: NSCoder) {
        fatalError()
    }

    deinit {
        vsyncLink?.invalidate()
        NotificationCenter.default.removeObserver(self)
    }

    /// Bind the display link once the view has a window. AppKit calls
    /// this with `nil` window when the view is removed, so we tear
    /// down too — no leaked link firing into a detached view. We also
    /// observe key-window transitions so the cursor block can flip
    /// between filled (focused) and hollow (blurred) without waiting
    /// for unrelated terminal damage.
    override func viewDidMoveToWindow() {
        super.viewDidMoveToWindow()
        let center = NotificationCenter.default
        guard let win = window else {
            vsyncLink?.invalidate()
            vsyncLink = nil
            center.removeObserver(self)
            return
        }
        if vsyncLink == nil {
            // `displayLink(target:selector:)` is NSView's vsync-link
            // factory (macOS 14+). Property name above is `vsyncLink`
            // to avoid shadowing the method with our stored link.
            let link = displayLink(target: self, selector: #selector(displayLinkFired(_:)))
            link.add(to: .current, forMode: .common)
            vsyncLink = link
        }
        center.removeObserver(self)
        center.addObserver(self, selector: #selector(windowFocusChanged(_:)),
                           name: NSWindow.didBecomeKeyNotification, object: win)
        center.addObserver(self, selector: #selector(windowFocusChanged(_:)),
                           name: NSWindow.didResignKeyNotification, object: win)
    }

    @objc private func windowFocusChanged(_: Notification) {
        // Cursor draw depends on `window?.isKeyWindow`; force the next
        // paint to pick the new focus state up. The damage gate stays
        // safe (no snapshot churn) — we just invalidate the cached
        // bitmap so AppKit re-runs `draw(_:)` with the cached snapshot.
        needsDisplay = true
    }

    override var isFlipped: Bool {
        true
    }

    override var acceptsFirstResponder: Bool {
        true
    }

    func bind(handle: NesttyTermFFI.Handle?) {
        termHandle = handle
    }

    func computeGrid() -> (cols: UInt16, rows: UInt16) {
        let w = max(1, Int(bounds.width / cellWidth))
        let h = max(1, Int(bounds.height / cellHeight))
        return (UInt16(min(w, Int(UInt16.max))), UInt16(min(h, Int(UInt16.max))))
    }

    private func recomputeCellMetrics() {
        // Monospaced advance: measure the wide-but-canonical "M".
        // Falls back to font.maximumAdvancement if measurement fails
        // (shouldn't on a real monospaced face).
        let attrs: [NSAttributedString.Key: Any] = [.font: font]
        let m = NSAttributedString(string: "M", attributes: attrs)
        cellWidth = ceil(m.size().width)
        if cellWidth <= 0 { cellWidth = ceil(font.maximumAdvancement.width) }
        ascent = ceil(font.ascender)
        let descent = ceil(abs(font.descender))
        let leading = ceil(font.leading)
        cellHeight = ascent + descent + leading
        if cellHeight <= 0 { cellHeight = 16 }
    }

    /// Display-link callback. Runs on the main runloop at vsync. Gated
    /// on `takeDamage` so an idle terminal pays only the FFI bool
    /// query (sub-microsecond) instead of a full snapshot + redraw.
    /// When a TUI-driven blinking cursor is active, an additional
    /// 2 Hz tick forces a redraw to advance the blink phase. Also
    /// drains the OSC 52 clipboard-request queue so paste requests
    /// from inside the terminal flow through the policy gate.
    @objc private func displayLinkFired(_: CADisplayLink) {
        guard let handle = termHandle else { return }
        drainClipboardRequests(handle)
        let damaged = handle.takeDamage()
        let blinkPhaseChanged = advanceBlinkPhase()
        guard damaged || blinkPhaseChanged else { return }
        if damaged {
            snapshotCache = handle.snapshot()
        }
        needsDisplay = true
    }

    /// Apply the user's OSC 52 policy to any pending clipboard write
    /// request. `.allow` writes through to NSPasteboard.general;
    /// `.deny` (the secure default) drops with a stderr warning so a
    /// rogue program in the terminal can't silently overwrite the
    /// user's clipboard. Matches the SwiftTerm path's behavior.
    private func drainClipboardRequests(_ handle: NesttyTermFFI.Handle) {
        guard let text = handle.takeClipboardRequest() else { return }
        switch osc52Policy {
        case .allow:
            let pb = NSPasteboard.general
            pb.declareTypes([.string], owner: nil)
            pb.setString(text, forType: .string)
        case .deny:
            let msg = "[nestty] OSC 52 clipboard write blocked (\(text.utf8.count) bytes). "
                + "Set `[security] osc52 = \"allow\"` to opt in.\n"
            FileHandle.standardError.write(Data(msg.utf8))
        }
    }

    /// Toggle the cursor visibility once per `blinkInterval` whenever
    /// the most recent snapshot reports `cursor.blink == 1`. Restores
    /// the cursor to visible if a previously-blinking TUI handed back
    /// to a steady cursor — otherwise the cursor could stick off.
    private func advanceBlinkPhase() -> Bool {
        let cursorBlink = snapshotCache?.cursor.blink ?? 0
        if cursorBlink == 1 {
            let now = Date()
            if now.timeIntervalSince(lastBlinkToggle) >= blinkInterval {
                blinkVisible.toggle()
                lastBlinkToggle = now
                return true
            }
            return false
        }
        if !blinkVisible {
            blinkVisible = true
            return true
        }
        return false
    }

    override func draw(_: NSRect) {
        guard let snap = snapshotCache,
              let ctx = NSGraphicsContext.current?.cgContext
        else { return }

        // Fill the bounds with theme background unless the user opted
        // into transparent default cells AND a background image is
        // active. In that case the image layer underneath shows through
        // blank cells; cells with explicit ANSI bg or reverse-video
        // still materialize opaque in `drawRow` (Zed pattern), so
        // reverse-video stays legible against the image.
        if !(transparentDefaultBg && imageBackgroundActive) {
            ctx.setFillColor(theme.background.nsColor.cgColor)
            ctx.fill(bounds)
        }

        // CTLineDraw uses CoreGraphics-native y-up glyph orientation.
        // Our view is `isFlipped = true` (so row 0 is at the top
        // visually) — without this textMatrix flip the glyphs render
        // upside-down + mirrored against the flipped CTM. Save/restore
        // the prior state so we don't leak the flip into non-text
        // drawing later.
        ctx.saveGState()
        ctx.textMatrix = CGAffineTransform(scaleX: 1, y: -1)
        defer { ctx.restoreGState() }

        let snapRows = snap.rows
        for row in 0 ..< snapRows {
            let runs = snap.rowRuns(row)
            let utf8 = snap.rowUtf8(row)
            guard runs.count > 0, utf8.count > 0 else { continue }
            drawRow(row: row, runs: runs, utf8: utf8, ctx: ctx)
        }

        // Cursor on top of the glyph layer: nvim/htop/etc. paint the
        // cursor cell with their own highlight group (CursorLine,
        // Cursor) which previously covered an early-drawn cursor. For
        // block style we then re-render the cell glyph in
        // theme.background so the character under the cursor stays
        // readable (xterm/iTerm2/Terminal.app convention).
        drawCursor(snap: snap, ctx: ctx)

        // Selection highlight last so it tints OVER the text instead
        // of getting covered by per-cell bg fills. theme.surface2 at
        // ~0.4 alpha keeps the underlying text legible while clearly
        // marking the range.
        paintSelection(snap.selection, ctx: ctx)

        // IME preedit overlay (Korean / Japanese / Chinese composition).
        // Paints OVER everything else at the cursor cell — what the
        // user is composing, before any of it touches the PTY.
        paintMarkedText(snap.cursor, ctx: ctx)
    }

    /// Paint the in-progress IME composition at the cursor cell.
    /// Fills the underlying cells with theme.background opaque so the
    /// preedit is legible regardless of what was there before, then
    /// draws the marked string with an underline (single line for the
    /// whole composition; the IME-highlighted sub-range gets a thicker
    /// double underline).
    private func paintMarkedText(_ cursor: NesttyCursor, ctx: CGContext) {
        guard let marked = markedText, !marked.isEmpty,
              cellWidth > 0, cellHeight > 0 else { return }

        let baseAttrs: [NSAttributedString.Key: Any] = [
            .font: font,
            .foregroundColor: NSColor(cgColor: theme.foreground.nsColor.cgColor) ?? .white,
            .underlineStyle: NSUnderlineStyle.single.rawValue,
            .underlineColor: NSColor(cgColor: theme.accent.nsColor.cgColor) ?? .yellow,
        ]
        let attr = NSMutableAttributedString(string: marked, attributes: baseAttrs)
        // Thicker double underline on the IME-highlighted sub-range
        // so the user can see which syllable / kana is "active" in a
        // multi-segment composition.
        if markedSelectedRange.length > 0,
           markedSelectedRange.location + markedSelectedRange.length <= (marked as NSString).length
        {
            attr.addAttribute(
                .underlineStyle,
                value: NSUnderlineStyle([.double, .thick]).rawValue,
                range: markedSelectedRange,
            )
        }

        let line = CTLineCreateWithAttributedString(attr)
        // Typographic width tells us how many cells the preedit covers;
        // round up to a whole cell so the bg fill aligns with the grid.
        var ascentT: CGFloat = 0
        var descentT: CGFloat = 0
        var leadingT: CGFloat = 0
        let width = CGFloat(CTLineGetTypographicBounds(line, &ascentT, &descentT, &leadingT))
        let cellsCovered = max(1, Int(ceil(width / cellWidth)))
        let pxWidth = CGFloat(cellsCovered) * cellWidth

        let x = CGFloat(cursor.col) * cellWidth
        let y = CGFloat(cursor.row) * cellHeight
        ctx.setFillColor(theme.background.nsColor.cgColor)
        ctx.fill(CGRect(x: x, y: y, width: pxWidth, height: cellHeight))

        // CTLineDraw needs the text matrix flip that the main row loop
        // already applied; we're inside its scope (the defer-restore
        // hasn't fired yet) so `textPosition` + draw is correct.
        ctx.textPosition = CGPoint(x: x, y: y + ascent)
        CTLineDraw(line, ctx)
    }

    /// Paint a translucent `theme.surface2` overlay across the cells
    /// covered by the active selection. `end_row` / `end_col` are
    /// inclusive per alacritty's `SelectionRange` convention — paint
    /// `end_col - start_col + 1` cells on the end-row.
    private func paintSelection(_ sel: NesttySelectionRange, ctx: CGContext) {
        guard sel.present == 1, cellWidth > 0, cellHeight > 0 else { return }
        let color = theme.surface2.nsColor.withAlphaComponent(0.45).cgColor
        ctx.setFillColor(color)

        let startRow = Int(sel.start_row)
        let endRow = Int(sel.end_row)
        let startCol = Int(sel.start_col)
        let endCol = Int(sel.end_col)
        let cols = max(1, Int(bounds.width / cellWidth))
        let lastCol = cols - 1

        for row in startRow ... endRow {
            // Single-row selection: only the start_col..=end_col span.
            // Multi-row: start_row covers start_col..=lastCol, end_row
            // covers 0..=end_col, intermediate rows cover the full width.
            let firstCol: Int
            let finalCol: Int
            if startRow == endRow {
                firstCol = startCol
                finalCol = endCol
            } else if row == startRow {
                firstCol = startCol
                finalCol = lastCol
            } else if row == endRow {
                firstCol = 0
                finalCol = endCol
            } else {
                firstCol = 0
                finalCol = lastCol
            }
            guard firstCol <= finalCol else { continue }
            let x = CGFloat(firstCol) * cellWidth
            let w = CGFloat(finalCol - firstCol + 1) * cellWidth
            let y = CGFloat(row) * cellHeight
            ctx.fill(CGRect(x: x, y: y, width: w, height: cellHeight))
        }
    }

    /// Cursor render. Style 0 = hidden (skip). Block (1) fills the
    /// whole cell, then re-renders the cell glyph in theme.background
    /// so the character under the cursor stays legible. Beam (2) is a
    /// 2-px vertical bar at the cell's leading edge. Underline (3)
    /// is a 2-px horizontal bar at the cell's bottom. When the window
    /// isn't key (e.g. user switched apps), block style draws as a
    /// hollow outline — Terminal.app + iTerm2 do the same.
    private func drawCursor(snap: NesttyTermFFI.Snapshot, ctx: CGContext) {
        let cursor = snap.cursor
        guard cursor.style != 0,
              cellWidth > 0, cellHeight > 0,
              // Honor TUI-requested blink: skip the draw on the OFF
              // phase so the cursor actually disappears between
              // `blinkInterval` ticks. Steady cursors (`blink == 0`)
              // ignore `blinkVisible` entirely.
              cursor.blink == 0 || blinkVisible
        else { return }
        let x = CGFloat(cursor.col) * cellWidth
        let y = CGFloat(cursor.row) * cellHeight
        let cell = CGRect(x: x, y: y, width: cellWidth, height: cellHeight)
        let isKey = window?.isKeyWindow ?? false
        let color = theme.accent.nsColor.cgColor

        switch cursor.style {
        case 1: // block
            if isKey {
                ctx.setFillColor(color)
                ctx.fill(cell)
                redrawCursorGlyph(snap: snap, ctx: ctx)
            } else {
                ctx.setStrokeColor(color)
                ctx.setLineWidth(1)
                // Stroke is centered on the path; inset by half so
                // it stays inside the cell rect.
                ctx.stroke(cell.insetBy(dx: 0.5, dy: 0.5))
            }
        case 2: // beam (bar)
            let barWidth: CGFloat = 2
            let rect = CGRect(x: x, y: y, width: barWidth, height: cellHeight)
            ctx.setFillColor(color)
            ctx.fill(rect)
        case 3: // underline
            let barHeight: CGFloat = 2
            let rect = CGRect(x: x, y: y + cellHeight - barHeight, width: cellWidth, height: barHeight)
            ctx.setFillColor(color)
            ctx.fill(rect)
        default:
            break
        }
    }

    /// Paint the glyph at the cursor cell using theme.background as
    /// the foreground color, so it stands out against the accent
    /// block underneath. Honors bold/italic flags on the underlying
    /// run so styled text under the cursor still reads correctly.
    private func redrawCursorGlyph(snap: NesttyTermFFI.Snapshot, ctx: CGContext) {
        let cursor = snap.cursor
        let runs = snap.rowRuns(cursor.row)
        let utf8 = snap.rowUtf8(cursor.row)

        // Runs are emitted per cell (or per wide-cell-pair), so the
        // cursor sits inside exactly one run. Wide chars: cursor lands
        // on the leading half, so start_col == cursor.col still holds.
        var hit: NesttyRun?
        for i in 0 ..< runs.count {
            let r = runs[i]
            if r.start_col <= cursor.col, cursor.col < r.end_col {
                hit = r
                break
            }
        }
        guard let run = hit else { return }

        let len = Int(run.utf8_len)
        let offset = Int(run.utf8_offset)
        guard offset + len <= utf8.count else { return }

        // Pick the byte range and draw position. Three shapes:
        //   1. Aggregated uniform-ASCII run (multi-cell, all same byte):
        //      take exactly one byte and draw at the cursor cell's x.
        //      Drawing the full run would overpaint adjacent cells.
        //   2. Wide char (WIDE_LEADING flag, 2-cell span): draw the
        //      full utf8 at the run's start (= cursor.col for the
        //      leading half).
        //   3. Single cell, possibly with combining marks: draw the
        //      full utf8 at the run's start (= cursor.col).
        let flagBold: UInt16 = 1 << 0
        let flagItalic: UInt16 = 1 << 1
        let flagWideLeading: UInt16 = 1 << 7
        let runSpan = run.end_col - run.start_col
        let isWide = run.flags & flagWideLeading != 0
        let isAggregatedUniform = !isWide && runSpan > 1

        let drawBytes: UnsafeBufferPointer<UInt8>
        let drawX: CGFloat
        if isAggregatedUniform {
            // Every byte in the run is the same ASCII char by
            // construction (see walk_row in nestty-term).
            drawBytes = UnsafeBufferPointer(rebasing: utf8[offset ..< offset + 1])
            drawX = CGFloat(cursor.col) * cellWidth
        } else {
            drawBytes = UnsafeBufferPointer(rebasing: utf8[offset ..< offset + len])
            drawX = CGFloat(run.start_col) * cellWidth
        }
        guard
            let str = String(bytes: drawBytes, encoding: .utf8),
            !str.isEmpty,
            str != " "
        else { return }

        let isBold = run.flags & flagBold != 0
        let isItalic = run.flags & flagItalic != 0
        let runFont: NSFont = switch (isBold, isItalic) {
        case (true, true): boldItalicFont
        case (true, false): boldFont
        case (false, true): italicFont
        case (false, false): font
        }

        let attrs: [NSAttributedString.Key: Any] = [
            .font: runFont,
            .foregroundColor: NSColor(cgColor: theme.background.nsColor.cgColor) ?? .black,
        ]
        let attr = NSAttributedString(string: str, attributes: attrs)
        let line = CTLineCreateWithAttributedString(attr)
        let baselineY = CGFloat(cursor.row) * cellHeight + ascent
        ctx.textPosition = CGPoint(x: drawX, y: baselineY)
        CTLineDraw(line, ctx)
    }

    private func drawRow(
        row: UInt16,
        runs: UnsafeBufferPointer<NesttyRun>,
        utf8: UnsafeBufferPointer<UInt8>,
        ctx: CGContext,
    ) {
        // Baseline in flipped coords: top of row + ascent.
        let baselineY = CGFloat(row) * cellHeight + ascent
        let defaultFg = theme.foreground.nsColor.cgColor
        let defaultBg = theme.background.nsColor.cgColor

        // Flag bits mirror nestty_term::flags (see
        // nestty-term/src/lib.rs). Kept as Swift constants to avoid
        // a third source of truth.
        let flagBold: UInt16 = 1 << 0
        let flagItalic: UInt16 = 1 << 1
        let flagInverse: UInt16 = 1 << 3
        let flagDim: UInt16 = 1 << 4
        let flagStrike: UInt16 = 1 << 5

        let transparentMode = transparentDefaultBg && imageBackgroundActive

        for i in 0 ..< runs.count {
            let run = runs[i]

            // Provenance — was this cell's bg from the default sentinel
            // (`run.bg_rgba == 0`)? We need to know BEFORE resolving so
            // we can decide whether transparent mode applies. Equality
            // check on the resolved color is not enough: an explicit
            // ANSI bg that happens to equal theme.background should
            // still paint (it's a real intent), and a real default cell
            // should NOT paint in transparent mode even though its
            // resolved color matches theme.bg.
            let bgIsDefault = run.bg_rgba == 0
            let isInverse = run.flags & flagInverse != 0

            // Resolve colors then apply inverse swap. Default-bg
            // materializes to theme.background BEFORE the swap (Zed
            // pattern from §Phase 3 in the plan — reverse-video over
            // transparent bg would render invisibly without it).
            var fg = resolveColor(run.fg_rgba, defaultColor: defaultFg)
            var bg = resolveColor(run.bg_rgba, defaultColor: defaultBg)
            if isInverse {
                swap(&fg, &bg)
            }
            // Dim → reduce fg alpha. ANSI spec is intentionally vague
            // here; ~65% is the conventional value across emulators.
            if run.flags & flagDim != 0, let dimmed = fg.copy(alpha: 0.65) {
                fg = dimmed
            }

            let x = CGFloat(run.start_col) * cellWidth
            let cellsWide = CGFloat(run.end_col - run.start_col)
            let cellRect = CGRect(x: x, y: CGFloat(row) * cellHeight,
                                  width: cellsWide * cellWidth, height: cellHeight)

            // Per-cell bg fill — overrides the global bounds fill.
            //   Opaque mode: skip when the resolved bg equals theme.bg
            //     (the bounds fill already covered it).
            //   Transparent mode: skip only when the cell came from the
            //     default sentinel AND is not inverse — those are the
            //     only cells we let the image bleed through. Inverse +
            //     default-bg is opaque theme.fg after swap and must
            //     still paint.
            let skipFill = transparentMode
                ? (bgIsDefault && !isInverse)
                : cgColorsApproxEqual(bg, defaultBg)
            if !skipFill {
                ctx.setFillColor(bg)
                ctx.fill(cellRect)
            }

            // Text. Empty/whitespace skipped to save a CTLine alloc.
            let len = Int(run.utf8_len)
            let offset = Int(run.utf8_offset)
            guard offset + len <= utf8.count else { continue }
            guard
                let str = String(bytes: UnsafeBufferPointer(rebasing: utf8[offset ..< offset + len]), encoding: .utf8),
                !str.isEmpty
            else { continue }

            let isBold = run.flags & flagBold != 0
            let isItalic = run.flags & flagItalic != 0
            let runFont: NSFont = switch (isBold, isItalic) {
            case (true, true): boldItalicFont
            case (true, false): boldFont
            case (false, true): italicFont
            case (false, false): font
            }
            var attrs: [NSAttributedString.Key: Any] = [
                .font: runFont,
                .foregroundColor: NSColor(cgColor: fg) ?? .white,
            ]
            // underline_style: 0=none, others=show single (richer
            // double/curly/dotted/dashed decoding will land alongside
            // the dirty-line refinement when we extend the FFI).
            if run.underline_style != 0 {
                attrs[.underlineStyle] = NSUnderlineStyle.single.rawValue
                let ulColor = run.underline_color_rgba == 0
                    ? fg
                    : resolveColor(run.underline_color_rgba, defaultColor: fg)
                attrs[.underlineColor] = NSColor(cgColor: ulColor) ?? .white
            }
            if run.flags & flagStrike != 0 {
                attrs[.strikethroughStyle] = NSUnderlineStyle.single.rawValue
                attrs[.strikethroughColor] = NSColor(cgColor: fg) ?? .white
            }
            let attr = NSAttributedString(string: str, attributes: attrs)
            let line = CTLineCreateWithAttributedString(attr)
            ctx.textPosition = CGPoint(x: x, y: baselineY)
            CTLineDraw(line, ctx)
        }
    }

    /// Cheap component-wise equality for the "is this cell's bg the
    /// same as the bounds fill we already did" early-out. Falls back
    /// to ObjectIdentifier when components aren't comparable (mixed
    /// color spaces).
    private func cgColorsApproxEqual(_ a: CGColor, _ b: CGColor) -> Bool {
        guard let ac = a.components, let bc = b.components, ac.count == bc.count else { return false }
        for (x, y) in zip(ac, bc) where abs(x - y) > 0.001 {
            return false
        }
        return true
    }

    // MARK: - Mouse selection

    /// Convert a window-coord mouse event into a grid (row, col, side)
    /// triple, clamping out-of-bounds drag positions so the FFI sees
    /// a valid `UInt16`. AppKit fires mouseDragged with coordinates
    /// outside the view bounds when the user drags past the edge —
    /// that's normal and should clamp to the nearest visible cell.
    private func gridLocation(for event: NSEvent) -> (row: UInt16, col: UInt16, side: NesttyTermFFI.Handle.CellSide)? {
        guard cellWidth > 0, cellHeight > 0 else { return nil }
        let local = convert(event.locationInWindow, from: nil)
        let maxCol = max(0, Int(bounds.width / cellWidth) - 1)
        let maxRow = max(0, Int(bounds.height / cellHeight) - 1)
        let col = min(maxCol, max(0, Int(local.x / cellWidth)))
        let row = min(maxRow, max(0, Int(local.y / cellHeight)))
        let xInCell = max(0, local.x - CGFloat(col) * cellWidth)
        let side: NesttyTermFFI.Handle.CellSide = xInCell < cellWidth / 2 ? .left : .right
        return (UInt16(clamping: row), UInt16(clamping: col), side)
    }

    /// 1-click → simple drag selection, 2 → semantic (word), 3+ →
    /// lines. Matches the iTerm2 / Terminal.app convention.
    private func selectionKind(for event: NSEvent) -> NesttyTermFFI.Handle.SelectionKind {
        switch event.clickCount {
        case 2: .word
        case let n where n >= 3: .line
        default: .simple
        }
    }

    /// When a TUI has any mouse-reporting mode on (`vim` with
    /// `set mouse=a`, `less`, `htop`, …), plain drag goes to the TUI
    /// — Shift held overrides so the user can still grab text. The
    /// renderer doesn't *forward* mouse events to the PTY yet
    /// (deferred to a future phase), so plain drag in mouse-mode
    /// apps is a no-op until forwarding lands.
    private func shouldHandleAsSelection(_ event: NSEvent) -> Bool {
        if event.modifierFlags.contains(.shift) { return true }
        return !(termHandle?.mouseModeActive ?? false)
    }

    override func mouseDown(with event: NSEvent) {
        // Always take first responder on click, even if we're going to
        // bail out for mouse-mode TUI handling. An unfocused alacritty
        // pane needs to become focusable on click regardless of whether
        // the click also starts a selection — otherwise the subsequent
        // Cmd+C / keyboard interaction has no responder target.
        window?.makeFirstResponder(self)

        // Cmd+click takes priority over selection: try OSC 8 hyperlink
        // first, fall back to plain-text URL regex on the clicked row.
        // Matches iTerm2 / Terminal.app / SwiftTerm path behavior.
        if event.modifierFlags.contains(.command) {
            if openURLAtClick(event) {
                return
            }
            // No URL at that point — fall through to normal mouseDown
            // so the user gets a selection start instead of nothing.
        }

        guard shouldHandleAsSelection(event) else {
            super.mouseDown(with: event)
            return
        }
        guard let (row, col, side) = gridLocation(for: event), let h = termHandle else {
            super.mouseDown(with: event)
            return
        }
        h.selectionStart(row: row, col: col, side: side, kind: selectionKind(for: event))
        needsDisplay = true
    }

    /// Resolve the URL at a Cmd+click point and hand it to NSWorkspace.
    /// Returns true when a URL was opened (so mouseDown can short-
    /// circuit). Checks OSC 8 first via the snapshot's hyperlink table,
    /// then falls back to URLClickHelper's plain-text regex.
    private func openURLAtClick(_ event: NSEvent) -> Bool {
        guard let snap = snapshotCache,
              let (row, col, _) = gridLocation(for: event)
        else { return false }

        // OSC 8: walk the row's runs for one whose hyperlink_id !=0 and
        // whose `start_col..<end_col` covers the clicked column.
        let runs = snap.rowRuns(row)
        for i in 0 ..< runs.count {
            let r = runs[i]
            if r.hyperlink_id != 0, col >= r.start_col, col < r.end_col,
               let uri = snap.hyperlinkURI(r.hyperlink_id),
               let url = URL(string: uri)
            {
                NSWorkspace.shared.open(url)
                return true
            }
        }

        // Plain text: decode the row's utf8 and find a regex match
        // containing the clicked column. NSRegularExpression operates
        // on UTF-16 units; ASCII-dominant URL text lines up with the
        // column index, so range.contains(col) works for the common
        // case. Wide chars upstream shift the offset — accept that
        // mismatch (URLClickHelper takes the same trade-off).
        let utf8 = snap.rowUtf8(row)
        guard utf8.count > 0,
              let lineText = String(bytes: UnsafeBufferPointer(start: utf8.baseAddress, count: utf8.count), encoding: .utf8)
        else { return false }

        let ns = lineText as NSString
        let fullRange = NSRange(location: 0, length: ns.length)
        let matches = URLClickHelper.urlRegex.matches(in: lineText, options: [], range: fullRange)
        for match in matches where match.range.contains(Int(col)) {
            let candidate = ns.substring(with: match.range)
            let trimmed = URLClickHelper.trimURLTrailingPunctuation(candidate)
            if let url = URL(string: trimmed) {
                NSWorkspace.shared.open(url)
                return true
            }
        }
        return false
    }

    override func mouseDragged(with event: NSEvent) {
        guard shouldHandleAsSelection(event) else {
            super.mouseDragged(with: event)
            return
        }
        guard let (row, col, side) = gridLocation(for: event), let h = termHandle else {
            return
        }
        h.selectionUpdate(row: row, col: col, side: side)
        needsDisplay = true
    }

    // MARK: - Scrolling

    /// Mouse wheel / trackpad scroll. Maps NSEvent's `scrollingDeltaY`
    /// into an integer line count and tells alacritty's grid to shift
    /// `display_offset`. Positive deltaY ("natural" scroll: fingers
    /// down on trackpad, or wheel back on a mouse) brings older content
    /// into view; the FFI's `scrollLines(positive)` does the same.
    override func scrollWheel(with event: NSEvent) {
        guard let h = termHandle, cellHeight > 0 else {
            super.scrollWheel(with: event)
            return
        }
        let dy = event.scrollingDeltaY
        let lines: Int
        if event.hasPreciseScrollingDeltas {
            // Trackpad — fractional pixel deltas. Accumulate so slow
            // swipes don't round to zero on every tick.
            accumulatedScrollDelta += dy
            let whole = (accumulatedScrollDelta / cellHeight).rounded(.towardZero)
            lines = Int(whole)
            accumulatedScrollDelta -= whole * cellHeight
        } else {
            // Mouse wheel — `scrollingDeltaY` is roughly line-count
            // shaped already (≈ 1 per notch on most devices). No
            // accumulator needed; rounding toward zero matches the
            // direction of partial deltas.
            lines = Int(dy.rounded(.towardZero))
        }
        if lines != 0 {
            h.scrollLines(Int32(lines))
            // No needsDisplay here — the vsync displayLink will see
            // the state-hash change on its next tick (≤16ms) and
            // schedule the redraw with a FRESH snapshot. Marking
            // dirty inline caused a double-render per scroll event:
            // AppKit drew the stale snapshotCache, then vsync drew
            // again with the post-scroll snapshot.
        }
    }

    /// Bring the view back to the live bottom. Called before sending
    /// user input to the PTY (typing) — convention is that any input
    /// dismisses the scrolled-back view so the user sees what they
    /// just typed land. PTY-side output (which arrives without a key
    /// press) leaves the scrolled state alone.
    private func scrollToBottomOnInput() {
        termHandle?.scrollToBottom()
    }

    // MARK: - Clipboard / Edit responder actions

    /// Standard responder action; fires for Cmd+C via the Edit menu
    /// key equivalent (which AppKit dispatches through the responder
    /// chain BEFORE keyDown ever runs). No selection → no-op so the
    /// chain continues to the next handler (matches Terminal.app).
    /// Not `override`-marked because NSResponder's cut/copy/paste are
    /// informal actions in Swift's bridging — they exist as Objective-C
    /// methods but aren't declared as overridable on NSView in Swift.
    /// `@objc` is enough to put them on the responder chain.
    @objc func copy(_: Any?) {
        guard let text = termHandle?.selectionString(), !text.isEmpty else { return }
        let pb = NSPasteboard.general
        pb.declareTypes([.string], owner: nil)
        pb.setString(text, forType: .string)
    }

    @objc func paste(_: Any?) {
        guard let text = NSPasteboard.general.string(forType: .string), !text.isEmpty else { return }
        sendPaste(text)
    }

    @objc override func selectAll(_: Any?) {
        termHandle?.selectionAll()
        needsDisplay = true
    }

    /// Cmd+V dispatch. Wraps the pasted bytes in bracketed-paste
    /// markers (`\e[200~ … \e[201~`) when the program enabled
    /// `\e[?2004h` — that's how zsh, vim's `set paste`, and modern
    /// shells distinguish pasted bytes from typed bytes.
    private func sendPaste(_ text: String) {
        guard let h = termHandle else { return }
        scrollToBottomOnInput()
        let bytes = Array(text.utf8)
        if h.bracketedPasteActive {
            h.input([0x1B, 0x5B, 0x32, 0x30, 0x30, 0x7E]) // ESC [ 2 0 0 ~
            h.input(bytes)
            h.input([0x1B, 0x5B, 0x32, 0x30, 0x31, 0x7E]) // ESC [ 2 0 1 ~
        } else {
            h.input(bytes)
        }
    }

    // MARK: - Keyboard

    /// Route keyDown through `interpretKeyEvents` so the system IME
    /// (Korean 2-Set, Japanese, …) sees the keystrokes. Without this,
    /// IME-active keystrokes don't deliver committed text back via
    /// `event.characters` and Korean/Japanese input silently drops.
    /// Preedit-text rendering during composition is still Phase 6;
    /// this slice just lets COMMITTED IME text flow into the PTY.
    ///
    /// Ctrl-letter combinations bypass IME entirely because shells +
    /// TUIs rely on them as raw control bytes (Ctrl+C = 0x03 → SIGINT,
    /// Ctrl+D = 0x04 → EOF). Cmd-modified keys go to the responder
    /// chain (menu shortcuts, clipboard) by calling super.
    override func keyDown(with event: NSEvent) {
        let mods = event.modifierFlags.intersection(.deviceIndependentFlagsMask)
        // Scroll navigation: Cmd+Up/Down (line), Cmd+Home/End (top/
        // bottom), Shift+PageUp/PageDown (page). These DON'T forward
        // to the PTY — they're host-side viewport controls.
        if handleScrollKey(event, mods: mods) {
            return
        }
        if mods.contains(.command) {
            // Menu key equivalents already fired in performKeyEquivalent
            // before keyDown, so anything left here is a Cmd combo with
            // no menu binding. Map the readline-style line-edit ones
            // (Cmd+←/→/⌫/⌦) before falling through — super.keyDown on
            // these would just beep.
            if let bytes = commandKeyBytes(forKeyCode: event.keyCode) {
                scrollToBottomOnInput()
                termHandle?.input(bytes)
                return
            }
            super.keyDown(with: event)
            return
        }
        if mods == .control, let bytes = controlBytes(for: event) {
            // User typed → jump back to bottom so the keypress lands
            // visibly. Matches Terminal.app / iTerm2 behavior.
            scrollToBottomOnInput()
            termHandle?.input(bytes)
            return
        }
        interpretKeyEvents([event])
    }

    /// macOS virtual key codes for the keys we own as scroll shortcuts.
    /// Using `keyCode` instead of `characters` so IME-active keystrokes
    /// (Korean Caps-Lock toggle, Japanese Eisu mode, …) don't shadow
    /// the shortcuts when no character is delivered.
    private enum KeyCode {
        static let up: UInt16 = 126
        static let down: UInt16 = 125
        static let left: UInt16 = 123
        static let right: UInt16 = 124
        static let home: UInt16 = 115
        static let end: UInt16 = 119
        static let pageUp: UInt16 = 116
        static let pageDown: UInt16 = 121
        static let delete: UInt16 = 51
        static let forwardDelete: UInt16 = 117
    }

    /// Intercept Cmd / Shift-modified scroll keys before they reach
    /// the PTY. Returns true when the key was consumed as a scroll
    /// gesture; caller short-circuits in that case.
    private func handleScrollKey(_ event: NSEvent, mods: NSEvent.ModifierFlags) -> Bool {
        guard let h = termHandle else { return false }
        let kc = event.keyCode
        // Same pattern as scrollWheel: skip needsDisplay because the
        // vsync displayLink will pick up the state-hash change within
        // one frame and trigger a draw with a fresh snapshot. Marking
        // dirty inline caused a stale draw before the vsync redraw.
        if mods.contains(.command) {
            switch kc {
            case KeyCode.up: h.scrollLines(1); return true
            case KeyCode.down: h.scrollLines(-1); return true
            case KeyCode.home: h.scrollToTop(); return true
            case KeyCode.end: h.scrollToBottom(); return true
            default: break
            }
        }
        if mods.contains(.shift) {
            switch kc {
            case KeyCode.pageUp: h.scrollPageUp(); return true
            case KeyCode.pageDown: h.scrollPageDown(); return true
            default: break
            }
        }
        return false
    }

    /// Cmd+arrow / Cmd+delete line-edit shortcuts. iTerm2/Terminal.app
    /// convention: map to the equivalent readline byte sequences so
    /// shells (bash/zsh) and Insert-mode vim react as users expect.
    /// Lives next to controlBytes/commandBytes for parity, but called
    /// directly from keyDown because Cmd-modified events never reach
    /// interpretKeyEvents (we route them to super for menu dispatch).
    private func commandKeyBytes(forKeyCode kc: UInt16) -> [UInt8]? {
        switch kc {
        case KeyCode.left: [0x01] // Cmd+← → Ctrl+A (beginning-of-line)
        case KeyCode.right: [0x05] // Cmd+→ → Ctrl+E (end-of-line)
        case KeyCode.delete: [0x15] // Cmd+⌫ → Ctrl+U (unix-line-discard)
        case KeyCode.forwardDelete: [0x0B] // Cmd+⌦ → Ctrl+K (kill-line)
        default: nil
        }
    }

    /// Map Ctrl+letter / Ctrl+@ / Ctrl+[ / Ctrl+\ / Ctrl+] / Ctrl+^
    /// / Ctrl+_ / Ctrl+Space to their canonical control bytes
    /// (0x00–0x1f, 0x7f). Returns nil for combinations not in the
    /// standard ASCII control set so the responder chain can handle
    /// them.
    private func controlBytes(for event: NSEvent) -> [UInt8]? {
        guard
            let chars = event.charactersIgnoringModifiers?.lowercased(),
            let scalar = chars.unicodeScalars.first
        else { return nil }
        let v = scalar.value
        // a-z → 0x01-0x1a
        if (0x61 ... 0x7A).contains(v) { return [UInt8(v - 0x60)] }
        switch v {
        case 0x20: return [0x00] // Ctrl+Space → NUL
        case 0x40: return [0x00] // Ctrl+@ → NUL
        case 0x5B: return [0x1B] // Ctrl+[ → ESC
        case 0x5C: return [0x1C] // Ctrl+\
        case 0x5D: return [0x1D] // Ctrl+]
        case 0x5E: return [0x1E] // Ctrl+^
        case 0x5F: return [0x1F] // Ctrl+_
        case 0x3F: return [0x7F] // Ctrl+? → DEL
        default: return nil
        }
    }

    // NSTextInputClient — IME routes commits through `insertText` and
    // special keys through `doCommand`. Phase 6 will flesh out the
    // marked-text path so preedit characters render in-cell during
    // composition.

    func insertText(_ string: Any, replacementRange _: NSRange) {
        let text: String
        if let s = string as? String { text = s }
        else if let a = string as? NSAttributedString { text = a.string }
        else { return }
        // IME commit: the system normally calls unmarkText() before
        // delivering the committed string, but some IMEs (and some
        // commit paths) skip that. Clear here too so the preedit
        // overlay doesn't linger after the bytes land in the PTY.
        if markedText != nil {
            markedText = nil
            needsDisplay = true
        }
        guard !text.isEmpty else { return }
        scrollToBottomOnInput()
        termHandle?.input(Array(text.utf8))
    }

    override func doCommand(by selector: Selector) {
        if let bytes = commandBytes(for: selector) {
            scrollToBottomOnInput()
            termHandle?.input(bytes)
        }
        // Unmapped selectors fall on the floor — better than calling
        // super which would try to interpret them as text editing on
        // a view that has no document model.
    }

    /// Selectors AppKit's text-input system synthesizes for keys that
    /// aren't plain printable characters. Mapped to the byte sequences
    /// a VT100-ish terminal expects.
    private func commandBytes(for selector: Selector) -> [UInt8]? {
        switch selector {
        case #selector(NSStandardKeyBindingResponding.insertNewline(_:)):
            [0x0D]
        case #selector(NSStandardKeyBindingResponding.insertTab(_:)):
            [0x09]
        case #selector(NSStandardKeyBindingResponding.deleteBackward(_:)):
            [0x7F]
        case #selector(NSStandardKeyBindingResponding.deleteForward(_:)):
            [0x1B, 0x5B, 0x33, 0x7E] // ESC [ 3 ~
        case #selector(NSStandardKeyBindingResponding.cancelOperation(_:)):
            [0x1B]
        case #selector(NSStandardKeyBindingResponding.moveLeft(_:)):
            [0x1B, 0x5B, 0x44]
        case #selector(NSStandardKeyBindingResponding.moveRight(_:)):
            [0x1B, 0x5B, 0x43]
        case #selector(NSStandardKeyBindingResponding.moveUp(_:)):
            [0x1B, 0x5B, 0x41]
        case #selector(NSStandardKeyBindingResponding.moveDown(_:)):
            [0x1B, 0x5B, 0x42]
        // Option+←/→ — readline backward-word / forward-word.
        case #selector(NSStandardKeyBindingResponding.moveWordLeft(_:)):
            [0x1B, 0x62] // ESC b
        case #selector(NSStandardKeyBindingResponding.moveWordRight(_:)):
            [0x1B, 0x66] // ESC f
        // Option+⌫/⌦ — readline backward-kill-word / kill-word.
        // ESC+DEL is the meta-backspace sequence bash/zsh bind out of
        // the box; raw Ctrl+W (0x17) would also delete word but ignores
        // the readline word-boundary config.
        case #selector(NSStandardKeyBindingResponding.deleteWordBackward(_:)):
            [0x1B, 0x7F]
        case #selector(NSStandardKeyBindingResponding.deleteWordForward(_:)):
            [0x1B, 0x64] // ESC d
        // Defensive: if a custom DefaultKeyBinding.dict or a third-party
        // text-input plugin synthesizes these line-edit selectors, route
        // them too. Normal flow hits commandKeyBytes() in keyDown first.
        case #selector(NSStandardKeyBindingResponding.moveToBeginningOfLine(_:)):
            [0x01]
        case #selector(NSStandardKeyBindingResponding.moveToEndOfLine(_:)):
            [0x05]
        case #selector(NSStandardKeyBindingResponding.deleteToBeginningOfLine(_:)):
            [0x15]
        case #selector(NSStandardKeyBindingResponding.deleteToEndOfLine(_:)):
            [0x0B]
        default:
            nil
        }
    }

    // IME preedit support. NSTextInputClient hands us the in-progress
    // composition via setMarkedText; we store it and paint it as an
    // overlay at the cursor cell in `draw(_:)`. Nothing flows to the
    // PTY until the IME calls `insertText` with the committed string.

    func setMarkedText(_ string: Any, selectedRange: NSRange, replacementRange _: NSRange) {
        let text: String = if let s = string as? String { s }
        else if let a = string as? NSAttributedString { a.string }
        else { "" }
        if text.isEmpty {
            markedText = nil
        } else {
            markedText = text
            // Clamp the IME-highlighted sub-range to the actual length
            // — some IMEs (and dictation) send ranges that extend past
            // the marked string. Drawing with an out-of-range index
            // would crash CoreText.
            let utf16Count = (text as NSString).length
            let loc = max(0, min(selectedRange.location, utf16Count))
            let len = max(0, min(selectedRange.length, utf16Count - loc))
            markedSelectedRange = NSRange(location: loc, length: len)
        }
        needsDisplay = true
    }

    func unmarkText() {
        guard markedText != nil else { return }
        markedText = nil
        markedSelectedRange = NSRange(location: 0, length: 0)
        needsDisplay = true
    }

    /// IMEs query this to know where the caret sits inside the
    /// "document." We don't have a real text buffer, so report a
    /// zero-length range at the start — Korean / Japanese IMEs
    /// accept this and key off `markedRange` + `firstRect` instead.
    /// Returning NSNotFound here breaks several IMEs (no input).
    func selectedRange() -> NSRange {
        NSRange(location: 0, length: 0)
    }

    func markedRange() -> NSRange {
        guard let text = markedText else {
            return NSRange(location: NSNotFound, length: 0)
        }
        return NSRange(location: 0, length: (text as NSString).length)
    }

    func hasMarkedText() -> Bool {
        markedText != nil
    }

    /// We don't expose the terminal buffer to the IME (it'd be
    /// awkward to map cell coordinates to NSRange offsets). Returning
    /// nil is fine — used mostly by accessibility / dictation paths
    /// that gracefully degrade.
    func attributedSubstring(forProposedRange _: NSRange, actualRange _: NSRangePointer?) -> NSAttributedString? {
        nil
    }

    /// Minimal set of attribute keys the IME can include in marked
    /// text. We honor underline via our own painting and ignore
    /// segment styles — sufficient for Korean/Japanese/Chinese IMEs.
    func validAttributesForMarkedText() -> [NSAttributedString.Key] {
        [.underlineStyle, .underlineColor]
    }

    /// Where the IME should anchor its candidate window. Returns the
    /// cursor cell's rect in *screen* coordinates — AppKit's IME
    /// pipeline expects screen-space here, not view or window. Without
    /// this the candidate popup floats at (0, 0) on the main display.
    /// View is `isFlipped == true` but `convert(_:to:)` already
    /// handles the flip between the view's top-left origin and the
    /// window's bottom-left origin — passing local flipped coords
    /// directly is correct (manually inverting y here double-flips
    /// and anchors the candidate window at the mirror row).
    func firstRect(forCharacterRange _: NSRange, actualRange _: NSRangePointer?) -> NSRect {
        guard let snap = snapshotCache, cellWidth > 0, cellHeight > 0 else { return .zero }
        let cursor = snap.cursor
        let cellRect = NSRect(
            x: CGFloat(cursor.col) * cellWidth,
            y: CGFloat(cursor.row) * cellHeight,
            width: cellWidth,
            height: cellHeight,
        )
        guard let win = window else { return .zero }
        let windowRect = convert(cellRect, to: nil)
        return win.convertToScreen(windowRect)
    }

    /// Hit-test for clicking into a preedit composition — we don't
    /// support it, but returning a deterministic NSNotFound keeps
    /// the IME from probing further.
    func characterIndex(for _: NSPoint) -> Int {
        NSNotFound
    }
}
