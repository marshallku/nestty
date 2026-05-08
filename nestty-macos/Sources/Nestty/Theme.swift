import AppKit

struct RGBColor {
    let r: UInt8
    let g: UInt8
    let b: UInt8

    init(hex: String) {
        let h = hex.trimmingCharacters(in: CharacterSet(charactersIn: "#"))
        let padded = h.count >= 6 ? h : String(repeating: "0", count: 6 - h.count) + h
        r = UInt8(strtoul(String(padded.prefix(2)), nil, 16))
        g = UInt8(strtoul(String(padded.dropFirst(2).prefix(2)), nil, 16))
        b = UInt8(strtoul(String(padded.dropFirst(4).prefix(2)), nil, 16))
    }

    var nsColor: NSColor {
        NSColor(
            red: CGFloat(r) / 255.0,
            green: CGFloat(g) / 255.0,
            blue: CGFloat(b) / 255.0,
            alpha: 1.0,
        )
    }
}

/// Mirrors nestty-core/theme.rs Theme struct exactly.
struct NesttyTheme {
    let name: String
    let foreground: RGBColor
    let background: RGBColor
    /// 16-color ANSI palette (8 normal + 8 bright)
    let palette: [RGBColor]

    // UI semantic colors
    let surface0: RGBColor // Darker bg (tab bar, panels)
    let surface1: RGBColor // Hover bg
    let surface2: RGBColor // Active/selected bg
    let overlay0: RGBColor // Borders, separators
    let text: RGBColor // Primary text (active tabs)
    let subtext0: RGBColor // Dim text (inactive tabs)
    let subtext1: RGBColor // Hover text
    let accent: RGBColor // Focus rings, active indicators
    let red: RGBColor // Destructive/error (close hover)

    static func byName(_ name: String) -> NesttyTheme? {
        switch name {
        case "catppuccin-mocha": .catppuccinMocha
        case "catppuccin-latte": .catppuccinLatte
        case "catppuccin-frappe": .catppuccinFrappe
        case "catppuccin-macchiato": .catppuccinMacchiato
        case "dracula": .dracula
        case "nord": .nord
        case "tokyo-night": .tokyoNight
        case "gruvbox-dark": .gruvboxDark
        case "one-dark": .oneDark
        case "solarized-dark": .solarizedDark
        default: nil
        }
    }

    static var catppuccinMocha: NesttyTheme {
        NesttyTheme(
            name: "catppuccin-mocha",
            foreground: RGBColor(hex: "#cdd6f4"),
            background: RGBColor(hex: "#1e1e2e"),
            palette: [
                "#45475a", "#f38ba8", "#a6e3a1", "#f9e2af", "#89b4fa", "#f5c2e7", "#94e2d5", "#bac2de",
                "#585b70", "#f38ba8", "#a6e3a1", "#f9e2af", "#89b4fa", "#f5c2e7", "#94e2d5", "#a6adc8",
            ].map { RGBColor(hex: $0) },
            surface0: RGBColor(hex: "#181825"),
            surface1: RGBColor(hex: "#262637"),
            surface2: RGBColor(hex: "#313244"),
            overlay0: RGBColor(hex: "#45475a"),
            text: RGBColor(hex: "#cdd6f4"),
            subtext0: RGBColor(hex: "#6c7086"),
            subtext1: RGBColor(hex: "#bac2de"),
            accent: RGBColor(hex: "#89b4fa"),
            red: RGBColor(hex: "#f38ba8"),
        )
    }

    static var catppuccinLatte: NesttyTheme {
        NesttyTheme(
            name: "catppuccin-latte",
            foreground: RGBColor(hex: "#4c4f69"),
            background: RGBColor(hex: "#eff1f5"),
            palette: [
                "#5c5f77", "#d20f39", "#40a02b", "#df8e1d", "#1e66f5", "#ea76cb", "#179299", "#acb0be",
                "#6c6f85", "#d20f39", "#40a02b", "#df8e1d", "#1e66f5", "#ea76cb", "#179299", "#bcc0cc",
            ].map { RGBColor(hex: $0) },
            surface0: RGBColor(hex: "#ccd0da"),
            surface1: RGBColor(hex: "#dce0e8"),
            surface2: RGBColor(hex: "#bcc0cc"),
            overlay0: RGBColor(hex: "#9ca0b0"),
            text: RGBColor(hex: "#4c4f69"),
            subtext0: RGBColor(hex: "#6c6f85"),
            subtext1: RGBColor(hex: "#5c5f77"),
            accent: RGBColor(hex: "#1e66f5"),
            red: RGBColor(hex: "#d20f39"),
        )
    }

    static var catppuccinFrappe: NesttyTheme {
        NesttyTheme(
            name: "catppuccin-frappe",
            foreground: RGBColor(hex: "#c6d0f5"),
            background: RGBColor(hex: "#303446"),
            palette: [
                "#51576d", "#e78284", "#a6d189", "#e5c890", "#8caaee", "#f4b8e4", "#81c8be", "#b5bfe2",
                "#626880", "#e78284", "#a6d189", "#e5c890", "#8caaee", "#f4b8e4", "#81c8be", "#a5adce",
            ].map { RGBColor(hex: $0) },
            surface0: RGBColor(hex: "#292c3c"),
            surface1: RGBColor(hex: "#353849"),
            surface2: RGBColor(hex: "#414559"),
            overlay0: RGBColor(hex: "#51576d"),
            text: RGBColor(hex: "#c6d0f5"),
            subtext0: RGBColor(hex: "#737994"),
            subtext1: RGBColor(hex: "#b5bfe2"),
            accent: RGBColor(hex: "#8caaee"),
            red: RGBColor(hex: "#e78284"),
        )
    }

    static var catppuccinMacchiato: NesttyTheme {
        NesttyTheme(
            name: "catppuccin-macchiato",
            foreground: RGBColor(hex: "#cad3f5"),
            background: RGBColor(hex: "#24273a"),
            palette: [
                "#494d64", "#ed8796", "#a6da95", "#eed49f", "#8aadf4", "#f5bde6", "#8bd5ca", "#b8c0e0",
                "#5b6078", "#ed8796", "#a6da95", "#eed49f", "#8aadf4", "#f5bde6", "#8bd5ca", "#a5adcb",
            ].map { RGBColor(hex: $0) },
            surface0: RGBColor(hex: "#1e2030"),
            surface1: RGBColor(hex: "#2e3248"),
            surface2: RGBColor(hex: "#363a4f"),
            overlay0: RGBColor(hex: "#494d64"),
            text: RGBColor(hex: "#cad3f5"),
            subtext0: RGBColor(hex: "#6e738d"),
            subtext1: RGBColor(hex: "#b8c0e0"),
            accent: RGBColor(hex: "#8aadf4"),
            red: RGBColor(hex: "#ed8796"),
        )
    }

    static var dracula: NesttyTheme {
        NesttyTheme(
            name: "dracula",
            foreground: RGBColor(hex: "#f8f8f2"),
            background: RGBColor(hex: "#282a36"),
            palette: [
                "#21222c", "#ff5555", "#50fa7b", "#f1fa8c", "#bd93f9", "#ff79c6", "#8be9fd", "#f8f8f2",
                "#6272a4", "#ff6e6e", "#69ff94", "#ffffa5", "#d6acff", "#ff92df", "#a4ffff", "#ffffff",
            ].map { RGBColor(hex: $0) },
            surface0: RGBColor(hex: "#21222c"),
            surface1: RGBColor(hex: "#2d2f3f"),
            surface2: RGBColor(hex: "#44475a"),
            overlay0: RGBColor(hex: "#6272a4"),
            text: RGBColor(hex: "#f8f8f2"),
            subtext0: RGBColor(hex: "#6272a4"),
            subtext1: RGBColor(hex: "#bfbfbf"),
            accent: RGBColor(hex: "#bd93f9"),
            red: RGBColor(hex: "#ff5555"),
        )
    }

    static var nord: NesttyTheme {
        NesttyTheme(
            name: "nord",
            foreground: RGBColor(hex: "#d8dee9"),
            background: RGBColor(hex: "#2e3440"),
            palette: [
                "#3b4252", "#bf616a", "#a3be8c", "#ebcb8b", "#81a1c1", "#b48ead", "#88c0d0", "#e5e9f0",
                "#4c566a", "#bf616a", "#a3be8c", "#ebcb8b", "#81a1c1", "#b48ead", "#8fbcbb", "#eceff4",
            ].map { RGBColor(hex: $0) },
            surface0: RGBColor(hex: "#272c36"),
            surface1: RGBColor(hex: "#353b49"),
            surface2: RGBColor(hex: "#3b4252"),
            overlay0: RGBColor(hex: "#4c566a"),
            text: RGBColor(hex: "#d8dee9"),
            subtext0: RGBColor(hex: "#7b88a1"),
            subtext1: RGBColor(hex: "#e5e9f0"),
            accent: RGBColor(hex: "#88c0d0"),
            red: RGBColor(hex: "#bf616a"),
        )
    }

    static var tokyoNight: NesttyTheme {
        NesttyTheme(
            name: "tokyo-night",
            foreground: RGBColor(hex: "#a9b1d6"),
            background: RGBColor(hex: "#1a1b26"),
            palette: [
                "#32344a", "#f7768e", "#9ece6a", "#e0af68", "#7aa2f7", "#ad8ee6", "#449dab", "#787c99",
                "#444b6a", "#ff7a93", "#b9f27c", "#ff9e64", "#7da6ff", "#bb9af7", "#0db9d7", "#acb0d0",
            ].map { RGBColor(hex: $0) },
            surface0: RGBColor(hex: "#16161e"),
            surface1: RGBColor(hex: "#1f2335"),
            surface2: RGBColor(hex: "#292e42"),
            overlay0: RGBColor(hex: "#3b4261"),
            text: RGBColor(hex: "#a9b1d6"),
            subtext0: RGBColor(hex: "#565f89"),
            subtext1: RGBColor(hex: "#787c99"),
            accent: RGBColor(hex: "#7aa2f7"),
            red: RGBColor(hex: "#f7768e"),
        )
    }

    static var gruvboxDark: NesttyTheme {
        NesttyTheme(
            name: "gruvbox-dark",
            foreground: RGBColor(hex: "#ebdbb2"),
            background: RGBColor(hex: "#282828"),
            palette: [
                "#282828", "#cc241d", "#98971a", "#d79921", "#458588", "#b16286", "#689d6a", "#a89984",
                "#928374", "#fb4934", "#b8bb26", "#fabd2f", "#83a598", "#d3869b", "#8ec07c", "#ebdbb2",
            ].map { RGBColor(hex: $0) },
            surface0: RGBColor(hex: "#1d2021"),
            surface1: RGBColor(hex: "#32302f"),
            surface2: RGBColor(hex: "#3c3836"),
            overlay0: RGBColor(hex: "#504945"),
            text: RGBColor(hex: "#ebdbb2"),
            subtext0: RGBColor(hex: "#928374"),
            subtext1: RGBColor(hex: "#a89984"),
            accent: RGBColor(hex: "#83a598"),
            red: RGBColor(hex: "#fb4934"),
        )
    }

    static var oneDark: NesttyTheme {
        NesttyTheme(
            name: "one-dark",
            foreground: RGBColor(hex: "#abb2bf"),
            background: RGBColor(hex: "#282c34"),
            palette: [
                "#282c34", "#e06c75", "#98c379", "#e5c07b", "#61afef", "#c678dd", "#56b6c2", "#abb2bf",
                "#545862", "#e06c75", "#98c379", "#e5c07b", "#61afef", "#c678dd", "#56b6c2", "#c8ccd4",
            ].map { RGBColor(hex: $0) },
            surface0: RGBColor(hex: "#21252b"),
            surface1: RGBColor(hex: "#2c313c"),
            surface2: RGBColor(hex: "#333842"),
            overlay0: RGBColor(hex: "#4b5263"),
            text: RGBColor(hex: "#abb2bf"),
            subtext0: RGBColor(hex: "#636d83"),
            subtext1: RGBColor(hex: "#828997"),
            accent: RGBColor(hex: "#61afef"),
            red: RGBColor(hex: "#e06c75"),
        )
    }

    static var solarizedDark: NesttyTheme {
        NesttyTheme(
            name: "solarized-dark",
            foreground: RGBColor(hex: "#839496"),
            background: RGBColor(hex: "#002b36"),
            palette: [
                "#073642", "#dc322f", "#859900", "#b58900", "#268bd2", "#d33682", "#2aa198", "#eee8d5",
                "#002b36", "#cb4b16", "#586e75", "#657b83", "#839496", "#6c71c4", "#93a1a1", "#fdf6e3",
            ].map { RGBColor(hex: $0) },
            surface0: RGBColor(hex: "#001e26"),
            surface1: RGBColor(hex: "#003847"),
            surface2: RGBColor(hex: "#073642"),
            overlay0: RGBColor(hex: "#586e75"),
            text: RGBColor(hex: "#839496"),
            subtext0: RGBColor(hex: "#657b83"),
            subtext1: RGBColor(hex: "#93a1a1"),
            accent: RGBColor(hex: "#268bd2"),
            red: RGBColor(hex: "#dc322f"),
        )
    }
}
