import AppKit
import Foundation

@main
struct NesttyApp {
    static func main() {
        // CLI flag handling — must run BEFORE NSApplication.shared so
        // `nestty --config-path` (and friends) print and exit instead
        // of accidentally launching the GUI. Mirrors nestty-linux's
        // src/main.rs surface so users get the same commands across
        // platforms.
        let args = CommandLine.arguments
        if handleCLIFlags(args) {
            return
        }

        let app = NSApplication.shared
        let delegate = AppDelegate()
        app.delegate = delegate
        app.run()
    }

    /// Returns true when a flag was consumed and the process should
    /// exit (caller `return`s from `main`). Returns false when no
    /// recognized flag is present — the caller proceeds to launch
    /// the GUI as usual.
    private static func handleCLIFlags(_ args: [String]) -> Bool {
        if args.contains("--version") || args.contains("-V") {
            // Bundle short version matches the SwiftPM package version
            // baked into Info.plist at build time. Fallback to a
            // placeholder so a debug/dev launch without Info.plist
            // still prints something useful.
            let v = Bundle.main.infoDictionary?["CFBundleShortVersionString"] as? String ?? "dev"
            print("nestty \(v)")
            return true
        }

        if args.contains("--config-path") {
            print(NesttyConfig.configPath().path)
            return true
        }

        if args.contains("--init-config") {
            return runInitConfig()
        }

        if args.contains("--help") || args.contains("-h") {
            printUsage()
            return true
        }

        return false
    }

    /// Write the default config TOML to `configPath()` if no file
    /// exists yet. Matches `nestty_core::NesttyConfig::write_default`'s
    /// contract — refuses to overwrite an existing file so a user
    /// can't lose customizations by typing the wrong flag.
    private static func runInitConfig() -> Bool {
        let target = NesttyConfig.configPath()
        let fm = FileManager.default

        if fm.fileExists(atPath: target.path) {
            print("Config already exists at: \(target.path)")
            return true
        }

        do {
            try fm.createDirectory(
                at: target.deletingLastPathComponent(),
                withIntermediateDirectories: true,
            )
            // Swift `"""…"""` strips the newline before the closing
            // delimiter; nestty-core's `write_default` includes a
            // trailing `\n`, so append one to keep the two files
            // byte-identical (the only thing diff would otherwise
            // complain about).
            try (defaultConfigTOML + "\n").write(to: target, atomically: true, encoding: .utf8)
            print("Config written to: \(target.path)")
        } catch {
            FileHandle.standardError.write(
                Data("Failed to write config: \(error.localizedDescription)\n".utf8),
            )
            exit(1)
        }
        return true
    }

    private static func printUsage() {
        let usage = """
        nestty — cross-platform terminal emulator

        Usage:
          nestty                 Launch the GUI app.
          nestty --version       Print the version.
          nestty --config-path   Print the config file path.
          nestty --init-config   Write the default config if none exists.
          nestty --help          Show this message.

        Companion tools:
          nestctl                CLI for a running nestty/daemon (sockets, panels, plugins).
          nesttyd                Background daemon (status bar, triggers, plugin runtime).
        """
        print(usage)
    }

    /// Verbatim copy of nestty-core's `write_default` template. Kept
    /// in sync by hand — drift means macOS init writes a different
    /// file than Linux. If this grows past a screen, switch to an
    /// FFI call into `NesttyConfig::write_default()`.
    private static let defaultConfigTOML = """
    [terminal]
    # shell = "/bin/zsh"
    font_family = "JetBrainsMono Nerd Font Mono"
    font_size = 14

    [background]
    # image = "/path/to/wallpaper.jpg"
    # tint = 0.85
    # tint_color = "#1e1e2e"
    # opacity = 0.95

    [tabs]
    # position = "top"  # top, bottom, left, right
    # width = 120       # vertical tab width in pixels (left/right)
    # collapsed = true  # start with tab bar collapsed (icon-only)

    [theme]
    # Available: catppuccin-mocha, catppuccin-latte, catppuccin-frappe, catppuccin-macchiato,
    #            dracula, nord, tokyo-night, gruvbox-dark, one-dark, solarized-dark
    name = "catppuccin-mocha"

    [statusbar]
    # enabled = true       # Show/hide the status bar
    # position = "bottom"  # "top" or "bottom"
    # height = 28          # Height in pixels

    [keybindings]
    # Map key combos to shell commands (spawn:) — runs in background
    # "ctrl+shift+g" = "spawn:~/my-script.sh --next"
    # "ctrl+shift+m" = "spawn:~/my-script.sh --toggle"

    # [[triggers]]
    # name = "log-cwd"
    # action = "system.log"
    # # Interpolation tokens: {event.<payload-key>} reaches into the event's
    # # JSON payload; if missing there, falls back to {event.kind|source|timestamp_ms}.
    # params = { message = "[{event.timestamp_ms}] cwd: {event.cwd}" }
    # [triggers.when]
    # event_kind = "terminal.cwd_changed"
    """
}
