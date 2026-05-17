// Phase 0 spike runtime — see docs/macos-renderer-migration-plan.md
// § Phase 0 for what this must prove.
//
// On launch:
//   1. Print versions from BOTH staticlibs (proves §R7 dual-link works)
//   2. Open a window with a desktop wallpaper as NSImageView background
//   3. Draw the fixture snapshot's row over the image using CoreText
//      (proves §D3 ABI + lifetime: borrowed row pointers used during
//      draw, snapshot freed in deinit)
//   4. Draw a separate cursor overlay block at column 14 (proves
//      cursor stays visible against arbitrary image content)
//
// Run `leaks <pid>` after launch to confirm §Phase 0 acceptance #2.

import AppKit
import CNesttyTermSpike

@MainActor
final class SnapshotView: NSView {
    // nonisolated(unsafe) so the deinit (which Swift 6 refuses to
    // promote to @MainActor) can free the Rust snapshot. The Rust
    // side has no thread-affinity for destroy.
    private nonisolated(unsafe) var snapshot: OpaquePointer?
    // Fixed cell metrics for the spike — production renderer will
    // measure from font, but this is enough to prove geometry.
    private let cellWidth: CGFloat = 18
    private let cellHeight: CGFloat = 28
    private let baselineOffset: CGFloat = 6
    /// Cursor sits one column past the fixture row's last run (cols
    /// 0-13 are content; cursor at col 14).
    private let cursorColumn: UInt16 = 14
    /// theme.background equivalent for the spike — Catppuccin Mocha bg.
    private let materializedBg = NSColor(red: 0x1e / 255.0, green: 0x1e / 255.0, blue: 0x2e / 255.0, alpha: 1)
    /// theme.accent equivalent — Catppuccin Mocha blue/lavender.
    private let accentColor = NSColor(red: 0x89 / 255.0, green: 0xb4 / 255.0, blue: 0xfa / 255.0, alpha: 1)

    init() {
        super.init(frame: .zero)
        wantsLayer = true
        layer?.isOpaque = false
        snapshot = nestty_snapshot_create_fixture()
    }

    @available(*, unavailable)
    required init?(coder _: NSCoder) {
        fatalError()
    }

    deinit {
        // snapshot ownership returns to Rust here.
        if let s = snapshot {
            nestty_snapshot_destroy(s)
        }
    }

    override func draw(_: NSRect) {
        guard
            let snap = snapshot,
            let ctx = NSGraphicsContext.current?.cgContext
        else { return }

        // Borrow row 0's runs + utf8. Both pointers live until
        // nestty_snapshot_destroy (called from deinit). Safe to use
        // during this draw call.
        var runsPtr: UnsafePointer<NesttyRun>?
        let runCount = nestty_snapshot_row_runs(snap, 0, &runsPtr)
        var utf8Len: Int = 0
        let utf8Ptr = nestty_snapshot_row_utf8(snap, 0, &utf8Len)
        guard runCount > 0, let runsPtr, let utf8Ptr else { return }

        let rowBuffer = UnsafeBufferPointer<UInt8>(start: utf8Ptr, count: utf8Len)
        let rowBytes = Array(rowBuffer)

        let baseY = bounds.height - cellHeight + baselineOffset

        for i in 0 ..< runCount {
            let run = runsPtr[i]
            let cellsWide = Int(run.end_col - run.start_col)
            let x = CGFloat(run.start_col) * cellWidth
            let cellRect = CGRect(x: x, y: bounds.height - cellHeight,
                                  width: CGFloat(cellsWide) * cellWidth,
                                  height: cellHeight)

            // Materialize default-bg → opaque theme.background, then
            // apply INVERSE swap. This is the Zed pattern (§Phase 3
            // in the plan) that makes reverse-video render visibly
            // against image backgrounds.
            let materializedBgColor = run.bg_rgba == 0
                ? materializedBg.cgColor
                : rgbaCG(run.bg_rgba)
            let cellFg = rgbaCG(run.fg_rgba)
            let isInverse = (run.flags & (1 << 3)) != 0
            let drawBg = isInverse ? cellFg : materializedBgColor
            let drawFg = isInverse ? materializedBgColor : cellFg

            // 1. Opaque cell background — this is what keeps the
            //    cursor/reverse-video visible against an arbitrary
            //    image. Default cells could stay transparent here
            //    (Phase 3's transparent_default_bg config); for the
            //    spike we always materialize so the test row is
            //    legible.
            ctx.setFillColor(drawBg)
            ctx.fill(cellRect)

            // 2. Text glyphs.
            let slice = Array(rowBytes[Int(run.utf8_offset) ..< Int(run.utf8_offset + run.utf8_len)])
            if let str = String(bytes: slice, encoding: .utf8), !str.isEmpty {
                let nsColor: NSColor
                if let c = NSColor(cgColor: drawFg) {
                    nsColor = c
                } else {
                    nsColor = .white
                }
                let attrs: [NSAttributedString.Key: Any] = [
                    .font: NSFont.monospacedSystemFont(ofSize: 18, weight: .regular),
                    .foregroundColor: nsColor,
                ]
                let attr = NSAttributedString(string: str, attributes: attrs)
                let line = CTLineCreateWithAttributedString(attr)
                ctx.textPosition = CGPoint(x: x, y: baseY)
                CTLineDraw(line, ctx)
            }

            // 3. Underline (if requested) — uses run-supplied color
            //    override or falls back to fg.
            if run.underline_style != 0 {
                let underlineColor = run.underline_color_rgba == 0
                    ? drawFg
                    : rgbaCG(run.underline_color_rgba)
                ctx.setStrokeColor(underlineColor)
                ctx.setLineWidth(1.5)
                ctx.beginPath()
                let underlineY = bounds.height - cellHeight + 3
                ctx.move(to: CGPoint(x: cellRect.minX, y: underlineY))
                ctx.addLine(to: CGPoint(x: cellRect.maxX, y: underlineY))
                ctx.strokePath()
            }
        }

        // Cursor block at column 14 — separate draw path. Opaque
        // accent fill over a materialized-bg backdrop so it stays
        // visible regardless of what image pixel sits underneath.
        let cursorRect = CGRect(x: CGFloat(cursorColumn) * cellWidth,
                                y: bounds.height - cellHeight,
                                width: cellWidth, height: cellHeight)
        ctx.setFillColor(materializedBg.cgColor)
        ctx.fill(cursorRect)
        ctx.setFillColor(accentColor.cgColor)
        ctx.fill(cursorRect)
    }

    private func rgbaCG(_ packed: UInt32) -> CGColor {
        let r = CGFloat((packed >> 24) & 0xff) / 255.0
        let g = CGFloat((packed >> 16) & 0xff) / 255.0
        let b = CGFloat((packed >> 8) & 0xff) / 255.0
        let a = CGFloat(packed & 0xff) / 255.0
        return CGColor(red: r, green: g, blue: b, alpha: a)
    }
}

@MainActor
final class SpikeAppDelegate: NSObject, NSApplicationDelegate {
    var window: NSWindow?

    func applicationDidFinishLaunching(_: Notification) {
        // §R7 proof — both staticlibs linked, both symbols callable.
        let termVersion = String(cString: nestty_spike_version())
        let ffiVersion = String(cString: nestty_ffi_version())
        FileHandle.standardError.write(Data("[spike] term lib: \(termVersion)\n".utf8))
        FileHandle.standardError.write(Data("[spike] ffi lib:  \(ffiVersion)\n".utf8))
        FileHandle.standardError.write(Data("[spike] dual-staticlib linking OK\n".utf8))

        let win = NSWindow(
            contentRect: NSRect(x: 100, y: 100, width: 800, height: 200),
            styleMask: [.titled, .closable, .miniaturizable, .resizable],
            backing: .buffered,
            defer: false,
        )
        win.title = "Phase 0 spike — cursor + reverse-video + ZWJ + wide CJK over image"

        let container = NSView(frame: win.contentLayoutRect)
        container.autoresizingMask = [.width, .height]
        container.wantsLayer = true

        // Image background. The wallpaper choice is arbitrary; what
        // matters is that pixels behind the cursor cell vary.
        let imageView = NSImageView(frame: container.bounds)
        imageView.autoresizingMask = [.width, .height]
        imageView.imageScaling = .scaleAxesIndependently
        imageView.image = NSImage(contentsOfFile: "/System/Library/Desktop Pictures/Mac Blue.heic")
        container.addSubview(imageView)

        // The cell rendering view sits on top, semi-transparent
        // (default-bg cells transparent except where the renderer
        // materializes them).
        let snapView = SnapshotView()
        snapView.frame = container.bounds
        snapView.autoresizingMask = [.width, .height]
        container.addSubview(snapView)

        win.contentView = container
        win.makeKeyAndOrderFront(nil)
        window = win

        NSApp.activate(ignoringOtherApps: true)
    }
}

let app = NSApplication.shared
let delegate = SpikeAppDelegate()
app.delegate = delegate
app.setActivationPolicy(.regular)
app.run()
