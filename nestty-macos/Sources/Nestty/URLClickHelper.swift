import AppKit
import Foundation
import SwiftTerm

/// Tier 1.5 (plain-text portion) — Cmd+click on a bare `https://...` in
/// terminal output opens it in the default browser.
///
/// SwiftTerm already covers OSC 8 hyperlinks via its built-in `mouseUp` →
/// `requestOpenLink` path. This helper handles the harder case: arbitrary
/// terminal output (e.g. `git push` showing a PR-creation URL) where there's
/// no explicit `\e]8` markup.
///
/// Why a helper + NSEvent monitor instead of subclassing:
/// `MacTerminalView.mouseUp` is `public override`, not `open`, so subclasses
/// outside the SwiftTerm module can't override it. Same situation that drove
/// `NesttyTerminalDelegate` (PR Tier 0.3) and `PaneManager.installClickMonitor`
/// (focus tracking) — we layer on top via `NSEvent.addLocalMonitorForEvents`.
enum URLClickHelper {
    /// Conservative-ish URL match: scheme + `://` + non-whitespace,
    /// terminating at characters that are usually trailing punctuation
    /// (`)`, `]`, `>`, `"`, `,`, `.`). This isn't RFC-compliant — terminals
    /// historically show messy URLs and we'd rather under-match than open
    /// the wrong target. Linux's VTE uses a similar heuristic.
    static let urlRegex: NSRegularExpression = // swiftlint:disable:next force_try
        try! NSRegularExpression(
            pattern: #"\bhttps?://[^\s<>"\)\]\,]+"#,
            options: [.caseInsensitive],
        )

    /// Given a mouse event in window coords and a terminal view, find the
    /// plain-text URL at the click position (if any). Returns nil when the
    /// click isn't over any matching text — caller should let the event
    /// continue to SwiftTerm's own `mouseUp` so OSC 8 still works.
    @MainActor
    static func findURL(at event: NSEvent, in terminalView: NesttyTerminalView) -> URL? {
        let location = terminalView.convert(event.locationInWindow, from: nil)
        guard terminalView.bounds.contains(location) else { return nil }

        let terminal = terminalView.getTerminal()
        let cols = terminal.cols
        let rows = terminal.rows
        guard cols > 0, rows > 0 else { return nil }

        // Cell size derived from view bounds + grid dimensions. SwiftTerm's
        // own `cellDimension` field is non-public so we reverse-engineer
        // the same value here. Off-by-half-a-cell at edges is acceptable
        // because the regex match expands across whole tokens — a click
        // anywhere inside a URL still hits the right substring.
        let cellHeight = terminalView.bounds.height / CGFloat(rows)
        let cellWidth = terminalView.bounds.width / CGFloat(cols)
        guard cellHeight > 0, cellWidth > 0 else { return nil }

        // NSView is bottom-up; SwiftTerm row 0 is at the top of the visible
        // viewport. Convert by inverting y against view height.
        let yFromTop = terminalView.bounds.height - location.y
        let row = max(0, min(rows - 1, Int(yFromTop / cellHeight)))
        let col = max(0, min(cols - 1, Int(location.x / cellWidth)))

        // Read the clicked row's text via the public `getText(start:end:)`
        // API. This honors wide-character cells and trailing-whitespace
        // trimming — the same wire shape as terminal selection copy.
        let lineText = terminal.getText(
            start: Position(col: 0, row: row),
            end: Position(col: cols - 1, row: row),
        )
        guard !lineText.isEmpty else { return nil }

        // NSRegularExpression operates on UTF-16 code units (NSRange).
        // For ASCII-heavy URL text the column index lines up with the
        // NSString index, so range.contains(col) works directly. Wide
        // chars upstream of a URL would shift the offset — accept that
        // mismatch (we'd rather miss occasionally than mis-open).
        let ns = lineText as NSString
        let fullRange = NSRange(location: 0, length: ns.length)
        let matches = urlRegex.matches(in: lineText, options: [], range: fullRange)
        for match in matches where match.range.contains(col) {
            let candidate = ns.substring(with: match.range)
            // Drop a single trailing punctuation character that the regex
            // didn't catch (URLs often end mid-sentence with a `.` or `!`).
            // Conservative: only strip the very last char and only if it's
            // in this set, so legitimate fragments aren't mangled.
            let trimmed = trimTrailingPunctuation(candidate)
            if let url = URL(string: trimmed) {
                return url
            }
        }
        return nil
    }

    private static let trailingPunct: Set<Character> = [".", ",", ";", "!", "?", ":"]

    private static func trimTrailingPunctuation(_ s: String) -> String {
        guard let last = s.last, trailingPunct.contains(last) else { return s }
        return String(s.dropLast())
    }
}
