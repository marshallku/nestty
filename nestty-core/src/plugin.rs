use std::path::PathBuf;

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct PluginManifest {
    pub plugin: PluginMeta,
    #[serde(default)]
    pub panels: Vec<PluginPanelDef>,
    #[serde(default)]
    pub commands: Vec<PluginCommandDef>,
    #[serde(default)]
    pub modules: Vec<PluginModuleDef>,
    #[serde(default)]
    pub services: Vec<PluginServiceDef>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PluginMeta {
    pub name: String,
    pub title: String,
    pub version: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PluginPanelDef {
    pub name: String,
    pub title: String,
    pub file: String,
    pub icon: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PluginCommandDef {
    pub name: String,
    pub exec: String,
    pub description: Option<String>,
}

fn default_module_position() -> String {
    "right".to_string()
}

fn default_module_order() -> i32 {
    50
}

fn default_module_interval() -> u64 {
    10
}

#[derive(Debug, Clone, Deserialize)]
pub struct PluginModuleDef {
    pub name: String,
    /// Shell command. stdout becomes module text — or, if it's JSON with a
    /// `text` field, that value is used (with optional `tooltip`).
    pub exec: String,
    /// Execution interval in seconds
    #[serde(default = "default_module_interval")]
    pub interval: u64,
    /// Position in the status bar: "left", "center", "right"
    #[serde(default = "default_module_position")]
    pub position: String,
    /// Sort order within position section (lower = first)
    #[serde(default = "default_module_order")]
    pub order: i32,
    /// CSS class name applied to this module's container element
    #[serde(default)]
    pub class: Option<String>,
}

/// Supervised stdio-RPC subprocess. `provides`/`subscribes` are the
/// pre-spawn conflict-resolution + init-reply-subset check inputs;
/// see `service_supervisor`'s module preamble.
#[derive(Debug, Clone, Deserialize)]
pub struct PluginServiceDef {
    /// Service identifier within the plugin (a single plugin may host
    /// multiple services, though one is the common case).
    pub name: String,
    /// Path or PATH-resolvable binary launched by the supervisor. Resolved
    /// against the plugin directory if relative.
    pub exec: String,
    /// Optional argv tail passed to `exec`. Useful for shared binaries that
    /// dispatch on a subcommand.
    #[serde(default)]
    pub args: Vec<String>,
    /// When the supervisor should spawn the process. Parsed from a string
    /// like `"onStartup"`, `"onAction:kb.*"`, `"onEvent:slack.*"`.
    #[serde(
        default = "default_activation",
        deserialize_with = "deserialize_activation"
    )]
    pub activation: Activation,
    /// Restart behavior for unexpected exits. Defaults to `on-crash`.
    #[serde(default = "default_restart", deserialize_with = "deserialize_restart")]
    pub restart: RestartPolicy,
    /// Pre-spawn conflict resolution: lexical-name winner takes a
    /// contested action; loser skips just that entry.
    #[serde(default)]
    pub provides: Vec<String>,
    /// Bus event-kind globs the service wants forwarded via
    /// `event.dispatch`. Same asymmetric validation rule as `provides`.
    #[serde(default)]
    pub subscribes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Activation {
    OnStartup,
    OnAction(String),
    OnEvent(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartPolicy {
    OnCrash,
    Always,
    Never,
}

fn default_activation() -> Activation {
    Activation::OnStartup
}

fn default_restart() -> RestartPolicy {
    RestartPolicy::OnCrash
}

fn deserialize_activation<'de, D>(de: D) -> Result<Activation, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = String::deserialize(de)?;
    parse_activation(&s).map_err(serde::de::Error::custom)
}

fn deserialize_restart<'de, D>(de: D) -> Result<RestartPolicy, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = String::deserialize(de)?;
    parse_restart(&s).map_err(serde::de::Error::custom)
}

pub fn parse_activation(raw: &str) -> Result<Activation, String> {
    let trimmed = raw.trim();
    if trimmed == "onStartup" {
        return Ok(Activation::OnStartup);
    }
    if let Some(glob) = trimmed.strip_prefix("onAction:") {
        let glob = glob.trim();
        if glob.is_empty() {
            return Err(format!("activation '{raw}' has empty action glob"));
        }
        return Ok(Activation::OnAction(glob.to_string()));
    }
    if let Some(glob) = trimmed.strip_prefix("onEvent:") {
        let glob = glob.trim();
        if glob.is_empty() {
            return Err(format!("activation '{raw}' has empty event glob"));
        }
        return Ok(Activation::OnEvent(glob.to_string()));
    }
    Err(format!(
        "unknown activation '{raw}'; expected onStartup | onAction:<glob> | onEvent:<glob>"
    ))
}

pub fn parse_restart(raw: &str) -> Result<RestartPolicy, String> {
    match raw.trim() {
        "on-crash" => Ok(RestartPolicy::OnCrash),
        "always" => Ok(RestartPolicy::Always),
        "never" => Ok(RestartPolicy::Never),
        other => Err(format!(
            "unknown restart policy '{other}'; expected on-crash | always | never"
        )),
    }
}

#[derive(Debug, Clone)]
pub struct LoadedPlugin {
    pub manifest: PluginManifest,
    pub dir: PathBuf,
}

pub fn plugin_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("~/.config"))
        .join("nestty")
        .join("plugins")
}

pub fn discover_plugins() -> Vec<LoadedPlugin> {
    let dir = plugin_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };

    let mut plugins = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let manifest_path = path.join("plugin.toml");
        if !manifest_path.exists() {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&manifest_path) else {
            eprintln!("[nestty] failed to read {}", manifest_path.display());
            continue;
        };
        match toml::from_str::<PluginManifest>(&content) {
            Ok(manifest) => {
                plugins.push(LoadedPlugin {
                    manifest,
                    dir: path,
                });
            }
            Err(e) => {
                eprintln!("[nestty] failed to parse {}: {e}", manifest_path.display());
            }
        }
    }
    plugins
}

/// Discovery + canonical sort: by `manifest.plugin.name` then by `dir` as
/// the stable tiebreaker for duplicates. Use this everywhere a plugin
/// name needs to resolve to a single manifest, so daemon dispatch and
/// GUI panel/command resolution pick the same winner under duplicate
/// names.
pub fn discover_sorted_plugins() -> Vec<LoadedPlugin> {
    let mut plugins = discover_plugins();
    plugins.sort_by(|a, b| {
        a.manifest
            .plugin
            .name
            .cmp(&b.manifest.plugin.name)
            .then_with(|| a.dir.cmp(&b.dir))
    });
    plugins
}

/// On a `discover_sorted_plugins` slice, returns the duplicate winner
/// for `name` (sorted-last entry). Callers that resolve a plugin by
/// name (panel open, command dispatch, module exec) must consume a
/// sorted slice for results to agree across daemon and GUI.
pub fn resolve_by_name<'a>(plugins: &'a [LoadedPlugin], name: &str) -> Option<&'a LoadedPlugin> {
    plugins
        .iter()
        .rev()
        .find(|p| p.manifest.plugin.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_loaded(name: &str, dir: &str) -> LoadedPlugin {
        LoadedPlugin {
            manifest: PluginManifest {
                plugin: PluginMeta {
                    name: name.into(),
                    title: name.into(),
                    version: "0".into(),
                    description: None,
                },
                panels: Vec::new(),
                commands: Vec::new(),
                modules: Vec::new(),
                services: Vec::new(),
            },
            dir: std::path::PathBuf::from(dir),
        }
    }

    #[test]
    fn resolve_by_name_returns_sorted_last_entry_under_duplicates() {
        // Caller is expected to pass a sorted slice (same shape as
        // `discover_sorted_plugins`'s output). The last entry with a
        // matching name wins — matches daemon dispatch's HashMap
        // last-write-wins ordering.
        let plugins = vec![
            mk_loaded("dup", "/plugins/a"),
            mk_loaded("dup", "/plugins/b"),
            mk_loaded("other", "/plugins/c"),
        ];
        let winner = resolve_by_name(&plugins, "dup").unwrap();
        assert_eq!(winner.dir.to_string_lossy(), "/plugins/b");
        assert!(resolve_by_name(&plugins, "missing").is_none());
    }

    #[test]
    fn parse_activation_onstartup() {
        assert_eq!(
            parse_activation("onStartup").unwrap(),
            Activation::OnStartup
        );
    }

    #[test]
    fn parse_activation_onaction_with_glob() {
        assert_eq!(
            parse_activation("onAction:kb.*").unwrap(),
            Activation::OnAction("kb.*".into())
        );
    }

    #[test]
    fn parse_activation_onevent_with_glob() {
        assert_eq!(
            parse_activation("onEvent:slack.*").unwrap(),
            Activation::OnEvent("slack.*".into())
        );
    }

    #[test]
    fn parse_activation_rejects_empty_glob() {
        assert!(parse_activation("onAction:").is_err());
        assert!(parse_activation("onEvent:").is_err());
    }

    #[test]
    fn parse_activation_rejects_unknown() {
        assert!(parse_activation("onWeirdo").is_err());
    }

    #[test]
    fn parse_restart_known_policies() {
        assert_eq!(parse_restart("on-crash").unwrap(), RestartPolicy::OnCrash);
        assert_eq!(parse_restart("always").unwrap(), RestartPolicy::Always);
        assert_eq!(parse_restart("never").unwrap(), RestartPolicy::Never);
    }

    #[test]
    fn parse_restart_rejects_unknown() {
        assert!(parse_restart("respawn").is_err());
    }

    #[test]
    fn manifest_with_services_section() {
        let toml_src = r#"
            [plugin]
            name = "kb"
            title = "Knowledge Base"
            version = "1.0.0"

            [[services]]
            name = "main"
            exec = "nestty-plugin-kb"
            activation = "onAction:kb.*"
            restart = "on-crash"
            provides = ["kb.search", "kb.read"]
            subscribes = []
        "#;
        let m: PluginManifest = toml::from_str(toml_src).unwrap();
        assert_eq!(m.services.len(), 1);
        let s = &m.services[0];
        assert_eq!(s.name, "main");
        assert_eq!(s.exec, "nestty-plugin-kb");
        assert_eq!(s.activation, Activation::OnAction("kb.*".into()));
        assert_eq!(s.restart, RestartPolicy::OnCrash);
        assert_eq!(s.provides, vec!["kb.search".to_string(), "kb.read".into()]);
        assert!(s.subscribes.is_empty());
    }

    #[test]
    fn manifest_service_defaults() {
        let toml_src = r#"
            [plugin]
            name = "echo"
            title = "Echo"
            version = "0.1.0"

            [[services]]
            name = "main"
            exec = "nestty-plugin-echo"
        "#;
        let m: PluginManifest = toml::from_str(toml_src).unwrap();
        let s = &m.services[0];
        assert_eq!(s.activation, Activation::OnStartup);
        assert_eq!(s.restart, RestartPolicy::OnCrash);
        assert!(s.provides.is_empty());
        assert!(s.subscribes.is_empty());
        assert!(s.args.is_empty());
    }

    #[test]
    fn manifest_without_services_section() {
        let toml_src = r#"
            [plugin]
            name = "panel-only"
            title = "Panel"
            version = "0.1.0"
        "#;
        let m: PluginManifest = toml::from_str(toml_src).unwrap();
        assert!(m.services.is_empty());
    }
}
