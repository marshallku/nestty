use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::trigger::Trigger;

fn default_shell() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
}

fn default_font_family() -> String {
    "JetBrainsMono Nerd Font Mono".to_string()
}

fn default_font_size() -> u32 {
    14
}

fn default_tint() -> f64 {
    0.85
}

fn default_tint_color() -> String {
    "#1e1e2e".to_string()
}

fn default_opacity() -> f64 {
    0.95
}

fn default_tab_position() -> String {
    "top".to_string()
}

fn default_tab_width() -> u32 {
    120
}

fn default_theme() -> String {
    "catppuccin-mocha".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalConfig {
    #[serde(default = "default_shell")]
    pub shell: String,

    #[serde(default = "default_font_family")]
    pub font_family: String,

    #[serde(default = "default_font_size")]
    pub font_size: u32,
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self {
            shell: default_shell(),
            font_family: default_font_family(),
            font_size: default_font_size(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackgroundConfig {
    #[serde(default)]
    pub image: Option<String>,

    #[serde(default = "default_tint")]
    pub tint: f64,

    #[serde(default = "default_tint_color")]
    pub tint_color: String,

    #[serde(default = "default_opacity")]
    pub opacity: f64,
}

impl Default for BackgroundConfig {
    fn default() -> Self {
        Self {
            image: None,
            tint: default_tint(),
            tint_color: default_tint_color(),
            opacity: default_opacity(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThemeConfig {
    #[serde(default = "default_theme")]
    pub name: String,
}

impl Default for ThemeConfig {
    fn default() -> Self {
        Self {
            name: default_theme(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TabsConfig {
    /// Tab bar position: "top", "bottom", "left", "right"
    #[serde(default = "default_tab_position")]
    pub position: String,
    /// Width of vertical tabs in pixels (left/right position)
    #[serde(default = "default_tab_width")]
    pub width: u32,
    /// Whether the tab bar starts collapsed (icon-only). Default: true
    #[serde(default = "default_true")]
    pub collapsed: bool,
}

fn default_true() -> bool {
    true
}

impl Default for TabsConfig {
    fn default() -> Self {
        Self {
            position: default_tab_position(),
            width: default_tab_width(),
            collapsed: true,
        }
    }
}

fn default_statusbar_height() -> u32 {
    28
}

fn default_statusbar_position() -> String {
    "bottom".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusBarConfig {
    /// Whether the status bar is enabled. Default: true
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Position: "top" or "bottom". Default: "bottom"
    #[serde(default = "default_statusbar_position")]
    pub position: String,
    /// Height in pixels. Default: 28
    #[serde(default = "default_statusbar_height")]
    pub height: u32,
}

impl Default for StatusBarConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            position: default_statusbar_position(),
            height: default_statusbar_height(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct KeybindingsConfig {
    /// Key combo → command mapping, e.g. "ctrl+shift+g" = "spawn:~/script.sh --arg"
    #[serde(flatten)]
    pub map: HashMap<String, String>,
}

/// Parsed keybinding ready for matching
#[derive(Debug, Clone)]
pub struct ParsedKeybinding {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub key: String,
    pub command: String,
}

impl KeybindingsConfig {
    /// Parse all keybinding entries into structured form
    pub fn parse(&self) -> Vec<ParsedKeybinding> {
        self.map
            .iter()
            .filter_map(|(combo, cmd)| Self::parse_one(combo, cmd))
            .collect()
    }

    fn parse_one(combo: &str, command: &str) -> Option<ParsedKeybinding> {
        let parts: Vec<&str> = combo.split('+').collect();
        if parts.is_empty() {
            return None;
        }

        let mut ctrl = false;
        let mut shift = false;
        let mut alt = false;
        let mut key = None;

        for part in &parts {
            match part.to_lowercase().as_str() {
                "ctrl" | "control" => ctrl = true,
                "shift" => shift = true,
                "alt" => alt = true,
                k => key = Some(k.to_string()),
            }
        }

        Some(ParsedKeybinding {
            ctrl,
            shift,
            alt,
            key: key?,
            command: command.to_string(),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NesttyConfig {
    #[serde(default)]
    pub terminal: TerminalConfig,

    #[serde(default)]
    pub background: BackgroundConfig,

    #[serde(default)]
    pub tabs: TabsConfig,

    #[serde(default)]
    pub theme: ThemeConfig,

    #[serde(default)]
    pub statusbar: StatusBarConfig,

    #[serde(default)]
    pub keybindings: KeybindingsConfig,

    /// Declarative event → action automation. See `docs/workflow-runtime.md`.
    #[serde(default)]
    pub triggers: Vec<Trigger>,
}

impl NesttyConfig {
    pub fn config_path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("/etc"))
            .join("nestty")
            .join("config.toml")
    }

    pub fn load() -> Result<Self> {
        let config_path = Self::config_path();

        if !config_path.exists() {
            return Ok(Self::default());
        }

        let contents = std::fs::read_to_string(&config_path)?;
        let config: NesttyConfig = toml::from_str(&contents)
            .map_err(|e| crate::error::NesttyError::Config(e.to_string()))?;

        Ok(config)
    }

    pub fn write_default() -> Result<PathBuf> {
        let path = Self::config_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let default_config = r##"[terminal]
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
"##;
        std::fs::write(&path, default_config)?;
        Ok(path)
    }
}
