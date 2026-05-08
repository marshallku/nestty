import Foundation

/// Tier 1.3 — random wallpaper rotation. Mirrors the Linux semantics in
/// `nestty-linux/src/socket.rs`:
///
/// - **Source list**: a flat text file with one image path per line.
///   macOS-native preferred, XDG-style as fallback for users on shared
///   dotfiles.
/// - **Mode flag**: a one-line file holding `"active"` or `"deactive"`.
///   `next()` returns nothing when deactive — `[background.next]` socket
///   command becomes a no-op so a timer-driven trigger doesn't spam image
///   changes when the user toggled rotation off.
/// - **Selection**: `subsec_nanos % lines.count` — same poor-man's random
///   as Linux. Good enough for a wallpaper roll.
///
/// The wallpapers list is **populated externally** (a personal cron, image
/// scraper, or the `[background] directory` config that's still TODO on
/// both platforms). nestty just reads the file; it doesn't curate.
enum BackgroundRotator {
    /// macOS-native cache path: `~/Library/Caches/nestty/wallpapers.txt`.
    static var primaryListURL: URL {
        FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent("Library")
            .appendingPathComponent("Caches")
            .appendingPathComponent("nestty")
            .appendingPathComponent("wallpapers.txt")
    }

    /// XDG-style fallback: `~/.cache/terminal-wallpapers.txt`. Lets users
    /// running the same dotfiles across Linux + macOS keep one wallpaper
    /// list. macOS-native wins on conflict (checked first).
    static var fallbackListURL: URL {
        FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent(".cache")
            .appendingPathComponent("terminal-wallpapers.txt")
    }

    /// Mode file: `~/Library/Caches/nestty/bg-mode`. Holds `active` or
    /// `deactive`. Missing file = active (default behavior matches Linux).
    static var modeFileURL: URL {
        FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent("Library")
            .appendingPathComponent("Caches")
            .appendingPathComponent("nestty")
            .appendingPathComponent("bg-mode")
    }

    /// True if rotation is currently active. False after `toggle()` flipped
    /// us into `deactive`.
    static var isActive: Bool {
        guard let s = try? String(contentsOf: modeFileURL, encoding: .utf8) else {
            return true
        }
        return s.trimmingCharacters(in: .whitespacesAndNewlines) != "deactive"
    }

    /// Flip the mode bit and persist. Returns the new state.
    @discardableResult
    static func toggle() -> Bool {
        let newActive = !isActive
        let mode = newActive ? "active" : "deactive"
        // Ensure the cache dir exists — a fresh macOS install won't have it.
        try? FileManager.default.createDirectory(
            at: modeFileURL.deletingLastPathComponent(),
            withIntermediateDirectories: true,
        )
        try? mode.write(to: modeFileURL, atomically: true, encoding: .utf8)
        return newActive
    }

    /// Pick a random wallpaper path from the configured list. Returns nil
    /// if no list exists, the list is empty, or rotation is currently
    /// deactive (caller decides whether to surface that as an error or
    /// no-op — `background.next` chooses no-op for trigger-friendliness).
    static func nextRandomImage() -> String? {
        guard let contents = readListContents() else { return nil }
        let lines = contents.split(separator: "\n").map { line in
            line.trimmingCharacters(in: .whitespacesAndNewlines)
        }.filter { !$0.isEmpty }
        guard !lines.isEmpty else { return nil }
        // subsec nanos as cheap entropy (matches Linux's strategy). For a
        // ~10s rotation cadence the time-based jitter is plenty random.
        let now = Date().timeIntervalSince1970
        let nanos = Int((now - now.rounded(.down)) * 1_000_000_000)
        let idx = abs(nanos) % lines.count
        return lines[idx]
    }

    private static func readListContents() -> String? {
        if let s = try? String(contentsOf: primaryListURL, encoding: .utf8) {
            return s
        }
        return try? String(contentsOf: fallbackListURL, encoding: .utf8)
    }
}
