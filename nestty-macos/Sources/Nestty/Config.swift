import Foundation
import TOMLKit

/// Policy for OSC 52 clipboard writes from the PTY.
///
/// Background: SwiftTerm's `LocalProcessTerminalView` writes to `NSPasteboard.general`
/// unconditionally on OSC 52. That lets any program in the terminal silently overwrite
/// the user's clipboard. We intercept by replacing `terminalDelegate` with a proxy
/// that consults this policy. Default is `deny`; matches VTE's hardened default on
/// Linux (VTE has OSC 52 disabled unless explicitly opted in).
enum OSC52Policy: String, Decodable {
    case deny
    case allow
}

/// Where the tab bar sits relative to the content area. Linux supports
/// `top`/`bottom`/`left`/`right`; macOS implements top/bottom now and
/// defers vertical orientation (left/right) — they require a separate
/// layout pass that's not worth doing until somebody asks for it.
enum TabsPosition: String, Decodable {
    case top
    case bottom

    /// Decode permissively: an unrecognized value (e.g. user wrote
    /// `position = "left"` as on Linux) falls back to `.top` with a
    /// stderr warning rather than crashing.
    static func parse(_ raw: String) -> TabsPosition {
        if let p = TabsPosition(rawValue: raw.lowercased()) { return p }
        let msg = "[nestty] [tabs] position '\(raw)' not yet supported on macOS — using 'top'\n"
        FileHandle.standardError.write(Data(msg.utf8))
        return .top
    }
}

/// `[statusbar]` config: enable + position + height. Same shape as Linux.
/// Position support is limited to `bottom` on macOS today (top deferred —
/// requires layout reshuffle around tab bar position).
struct StatusBarConfig {
    let enabled: Bool
    let position: String
    let height: Int

    static let defaults = StatusBarConfig(enabled: true, position: "bottom", height: 28)
}

/// Selects which terminal-emulation core renders a pane.
/// `alacritty` is the current default after Phase 3-6 reached feature
/// parity with the SwiftTerm path on the slice that matters for daily
/// use (cells/colors/cursor/image bg/transparency/damage gating,
/// selection + clipboard + OSC 52/8 + URL click, scrollback +
/// mouse wheel + Cmd nav, IME preedit, font/theme hot-reload).
/// `swiftterm` is retained as an explicit fallback while the
/// alacritty path dogfoods — drop it via Phase 10b once we're
/// confident nothing daily falls back. Read once per pane at
/// construction time — flipping the config requires a restart
/// (or a new tab) for the change to apply.
enum RendererBackend: String {
    case swiftterm
    case alacritty

    static func parse(_ raw: String?) -> RendererBackend {
        switch raw?.lowercased() {
        case "swiftterm": .swiftterm
        // Default to alacritty — an unspecified or unknown value
        // gets the active production backend rather than the legacy
        // fallback. Users who explicitly want SwiftTerm set
        // `[renderer] backend = "swiftterm"`.
        default: .alacritty
        }
    }
}

struct NesttyConfig {
    let shell: String
    let fontFamily: String
    let fontSize: Int
    let themeName: String
    let backgroundPath: String?
    let backgroundTint: Double
    /// Opacity of the background image layer itself (0.0 = invisible, 1.0 = fully visible).
    /// Distinct from `backgroundTint`, which darkens the image via an overlay.
    let backgroundOpacity: Double
    let osc52: OSC52Policy
    /// `[renderer] backend = "swiftterm" | "alacritty"`. Defaults to
    /// `alacritty` after Phase 10a; SwiftTerm stays as a one-flag-flip
    /// fallback during dogfooding. Per-pane: each new tab/split reads
    /// the live config value at construction time (changing it doesn't
    /// affect already-open panes — they keep whichever backend they
    /// were spawned with).
    let rendererBackend: RendererBackend
    /// `[renderer] transparent_default_bg = true` makes default-bg cells
    /// transparent on the alacritty backend, so a configured background
    /// image shows through blank cells. Off by default — cursor
    /// visibility against image backgrounds wins over aesthetic
    /// transparency. Cells with explicit ANSI bg colors and reverse-video
    /// cells still materialize opaquely (Zed pattern). No-op on the
    /// SwiftTerm backend.
    let transparentDefaultBg: Bool
    /// Tier 1.4 — `[tabs] position` (top/bottom). left/right deferred.
    let tabsPosition: TabsPosition
    /// Tier 4.2 — `[statusbar]` config (enabled/position/height). Modules
    /// themselves come from plugin manifests' `[[modules]]` declarations.
    let statusBar: StatusBarConfig
    /// Tier 1.2 — `[keybindings]` flat dict: combo string → command string.
    /// Compiled to `Keybindings.Binding` at AppDelegate init time and
    /// matched in the NSEvent local monitor. Empty when no `[keybindings]`
    /// section is present.
    let keybindings: [String: String]
    /// PR 5c — raw `[[triggers]]` array from config.toml, walked from the
    /// TOMLKit table tree into JSON-friendly `[[String: Any]]` so it can be
    /// JSON-encoded and shipped to the Rust trigger engine via FFI. We don't
    /// type each trigger statically because the schema allows arbitrary
    /// nested values under `params` / `when.payload_match` / `await.payload_match`,
    /// and the Rust side already has the canonical Deserialize impl.
    let triggers: [[String: Any]]

    /// `$XDG_CONFIG_HOME/nestty/config.toml`, else `~/.config/nestty/
    /// config.toml`. Mirrors `nestty_core::config::NesttyConfig::
    /// config_path()` so Swift renderer, Rust daemon, and nestctl all
    /// agree on the canonical location.
    static func configPath() -> URL {
        let env = ProcessInfo.processInfo.environment["XDG_CONFIG_HOME"]
        let base: URL = if let env, !env.isEmpty {
            URL(fileURLWithPath: env)
        } else {
            FileManager.default.homeDirectoryForCurrentUser
                .appendingPathComponent(".config")
        }
        return base
            .appendingPathComponent("nestty")
            .appendingPathComponent("config.toml")
    }

    static func load() -> NesttyConfig {
        let configURL = configPath()
        guard let contents = try? String(contentsOf: configURL, encoding: .utf8) else {
            return .defaults
        }
        return parse(contents)
    }

    /// Decode a TOML config string into NesttyConfig. Falls back to `.defaults` if the
    /// document is malformed; the parse error is written to stderr so the user can
    /// fix it. Unknown sections (e.g. `[[triggers]]`, `[keybindings]`, `[statusbar]`
    /// from the Linux schema) are tolerated — we only decode the fields the macOS
    /// app currently uses, and the rest stay intact for future parity work.
    static func parse(_ contents: String) -> NesttyConfig {
        let decoder = TOMLDecoder()
        let raw: RawConfig
        do {
            raw = try decoder.decode(RawConfig.self, from: contents)
        } catch {
            let msg = "[nestty] config.toml parse failed: \(error.localizedDescription) — using defaults\n"
            FileHandle.standardError.write(Data(msg.utf8))
            return .defaults
        }

        let defaults = NesttyConfig.defaults
        let bgImage = raw.background?.path ?? raw.background?.image
        let bgPath: String? = if let bgImage, !bgImage.isEmpty { expandTilde(bgImage) } else { nil }

        return NesttyConfig(
            shell: raw.terminal?.shell ?? defaults.shell,
            fontFamily: raw.terminal?.fontFamily ?? defaults.fontFamily,
            fontSize: raw.terminal?.fontSize ?? defaults.fontSize,
            themeName: raw.theme?.name ?? defaults.themeName,
            backgroundPath: bgPath,
            backgroundTint: clamp01(raw.background?.tint ?? defaults.backgroundTint),
            backgroundOpacity: clamp01(raw.background?.opacity ?? defaults.backgroundOpacity),
            osc52: raw.security?.osc52 ?? defaults.osc52,
            rendererBackend: RendererBackend.parse(raw.renderer?.backend),
            transparentDefaultBg: raw.renderer?.transparentDefaultBg ?? defaults.transparentDefaultBg,
            tabsPosition: raw.tabs?.position.map(TabsPosition.parse) ?? defaults.tabsPosition,
            statusBar: StatusBarConfig(
                enabled: raw.statusbar?.enabled ?? defaults.statusBar.enabled,
                position: raw.statusbar?.position ?? defaults.statusBar.position,
                height: raw.statusbar?.height ?? defaults.statusBar.height,
            ),
            keybindings: parseKeybindings(from: contents),
            triggers: parseTriggersArray(from: contents),
        )
    }

    /// JSON-friendly trigger list ready to ship through `NesttyEngine.setTriggers`.
    /// Just exposes the parsed `[[triggers]]` array; kept as a static helper
    /// (rather than an instance method) so AppDelegate doesn't have to know
    /// the encoding rules.
    static func triggersJSON(from config: NesttyConfig) -> [[String: Any]] {
        config.triggers
    }

    static var defaults: NesttyConfig {
        NesttyConfig(
            shell: ProcessInfo.processInfo.environment["SHELL"] ?? "/bin/zsh",
            fontFamily: "JetBrains Mono",
            fontSize: 14,
            themeName: "catppuccin-mocha",
            backgroundPath: nil,
            backgroundTint: 0.6,
            backgroundOpacity: 1.0,
            osc52: .deny,
            rendererBackend: .alacritty,
            transparentDefaultBg: false,
            tabsPosition: .top,
            statusBar: .defaults,
            keybindings: [:],
            triggers: [],
        )
    }

    private static func clamp01(_ d: Double) -> Double {
        max(0, min(1, d))
    }

    private static func expandTilde(_ path: String) -> String {
        guard path.hasPrefix("~") else { return path }
        let home = FileManager.default.homeDirectoryForCurrentUser.path
        return home + path.dropFirst()
    }

    /// Walk the TOML `[[triggers]]` array into JSON-friendly `[[String: Any]]`.
    /// We can't use a plain Decodable struct because trigger entries contain
    /// arbitrary nested values (`params`, `payload_match`) that we don't want
    /// to type statically — Rust's `serde_json::Value` round-trips the same
    /// tree losslessly. Walks via `TOMLTable` opaque API so the values flow
    /// straight into `JSONSerialization`-compatible types.
    private static func parseTriggersArray(from contents: String) -> [[String: Any]] {
        guard let table = try? TOMLTable(string: contents),
              let arr = table["triggers"]?.array
        else {
            return []
        }
        var result: [[String: Any]] = []
        for value in arr {
            if let dict = tomlValueToDict(value) {
                result.append(dict)
            }
        }
        return result
    }

    private static func tomlValueToDict(_ v: TOMLValueConvertible) -> [String: Any]? {
        guard let table = v.table else { return nil }
        var dict: [String: Any] = [:]
        for key in table.keys {
            if let val = table[key], let any = tomlValueToAny(val) {
                dict[key] = any
            }
        }
        return dict
    }

    /// Walk the `[keybindings]` table into a `[combo: command]` dict. Same
    /// rationale as triggers — the schema is a flat string-to-string dict
    /// and we don't want a separate Decodable struct just for that.
    private static func parseKeybindings(from contents: String) -> [String: String] {
        guard let table = try? TOMLTable(string: contents),
              let kb = table["keybindings"]?.table
        else {
            return [:]
        }
        var dict: [String: String] = [:]
        for key in kb.keys {
            if let val = kb[key], let s = val.string {
                dict[key] = s
            }
        }
        return dict
    }

    private static func tomlValueToAny(_ v: TOMLValueConvertible) -> Any? {
        // Order matters: check leaf types before composites because TOMLValue
        // may report multiple accessors as non-nil for ambiguous cases.
        if let s = v.string { return s }
        if let i = v.int { return i }
        if let d = v.double { return d }
        if let b = v.bool { return b }
        if let arr = v.array {
            return arr.compactMap(tomlValueToAny)
        }
        if let table = v.table {
            var d: [String: Any] = [:]
            for key in table.keys {
                if let val = table[key], let any = tomlValueToAny(val) {
                    d[key] = any
                }
            }
            return d
        }
        return nil
    }
}

// MARK: - Decodable shadow types

/// TOML shape for the macOS-relevant subset of the shared config schema. Sections
/// we don't decode yet (`[tabs]`, `[statusbar]`, `[keybindings]`, `[[triggers]]`)
/// are silently dropped — TOML decoding ignores unknown keys at the top level, so
/// users can keep their full Linux-shape config and the macOS app just picks out
/// what it understands. TOMLKit 0.6 has no `keyDecodingStrategy`, so snake_case
/// keys need explicit `CodingKeys`.
private struct RawConfig: Decodable {
    var terminal: TerminalSection?
    var theme: ThemeSection?
    var background: BackgroundSection?
    var security: SecuritySection?
    var renderer: RendererSection?
    var tabs: TabsSection?
    var statusbar: StatusBarSection?
}

private struct RendererSection: Decodable {
    var backend: String?
    var transparentDefaultBg: Bool?

    enum CodingKeys: String, CodingKey {
        case backend
        case transparentDefaultBg = "transparent_default_bg"
    }
}

private struct StatusBarSection: Decodable {
    var enabled: Bool?
    var position: String?
    var height: Int?
}

private struct TabsSection: Decodable {
    var position: String?
    // Linux schema also has `width` and `collapsed`; not consumed on macOS yet.
}

private struct TerminalSection: Decodable {
    var shell: String?
    var fontFamily: String?
    var fontSize: Int?

    enum CodingKeys: String, CodingKey {
        case shell
        case fontFamily = "font_family"
        case fontSize = "font_size"
    }
}

private struct ThemeSection: Decodable {
    var name: String?
}

private struct BackgroundSection: Decodable {
    var path: String?
    var image: String?
    var tint: Double?
    var opacity: Double?
}

private struct SecuritySection: Decodable {
    var osc52: OSC52Policy?
}
