//! `nesttyd` binary entry. Hosts the daemon-side `ActionRegistry`
//! (builtins + plugins via `ServiceSupervisor`) and the GUI registry.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use nestty_core::action_registry::{ActionRegistry, internal_error, invalid_params};
use nestty_core::paths;
use nestty_core::plugin::LoadedPlugin;
use nestty_core::protocol::ResponseError;
use nestty_core::thread_pool::ThreadPool;
use nestty_daemon::plugin_exec::{ShellError, spawn_plugin_shell};
use nestty_daemon::service_supervisor::ServiceSupervisor;
use nestty_daemon::socket::{
    self, DaemonState, LEGACY_DISPATCH_METHODS, SocketPrep, new_event_bus,
};
use nestty_daemon::trigger_sink::TRIGGER_ONLY_RESERVED_METHODS;
use serde_json::json;

/// `plugin.<name>.<cmd>` inherits the supervisor's 120s action_timeout;
/// the inner timeout is below that so the watchdog's kill+reap path
/// always wins the race over the registry's outer 120s recv_timeout.
const PLUGIN_CMD_TIMEOUT: Duration = Duration::from_secs(90);

/// Statusbar modules tick at 10s default. Generous-but-bounded so a
/// runaway module can't pile up across ticks.
const MODULE_RUN_TIMEOUT: Duration = Duration::from_secs(8);

const ENV_E2E_ACTIONS: &str = "NESTTYD_E2E_TEST_ACTIONS";
const ENV_POOL_WORKERS: &str = "NESTTYD_POOL_WORKERS";
const ENV_POOL_QUEUE: &str = "NESTTYD_POOL_QUEUE";

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let socket_path: PathBuf = paths::socket_path();
    log::info!("nesttyd starting; socket={}", socket_path.display());

    match socket::prepare_socket_path(&socket_path) {
        SocketPrep::Fresh => log::debug!("socket path fresh"),
        SocketPrep::StaleCleared => log::info!("removed stale socket file"),
        SocketPrep::InUse => {
            log::error!(
                "socket {} already bound by another nesttyd; refusing to start",
                socket_path.display()
            );
            return ExitCode::from(2);
        }
        SocketPrep::Error(msg) => {
            log::error!("socket prep failed: {msg}");
            return ExitCode::from(1);
        }
        SocketPrep::NotSocket => {
            log::error!(
                "path {} exists but is not a Unix socket; refusing to unlink (set NESTTY_SOCKET to a fresh path)",
                socket_path.display()
            );
            return ExitCode::from(3);
        }
    }

    let event_bus = new_event_bus();
    let pool = build_pool();
    let actions =
        Arc::new(ActionRegistry::with_completion_bus(event_bus.clone())).with_pool(pool.clone());
    let plugins = discover_and_sort_plugins();
    register_builtins(&actions, &plugins);
    register_plugin_commands(&actions, &plugins, &socket_path);
    if env_flag_enabled(ENV_E2E_ACTIONS) {
        register_e2e_actions(&actions);
    }

    // Bind before activating plugins so a bind failure can't orphan
    // eagerly-spawned children.
    let listener = match socket::bind_listener(&socket_path) {
        Ok(l) => l,
        Err(e) => {
            log::error!("bind({}): {e}", socket_path.display());
            return ExitCode::from(1);
        }
    };

    let supervisor_guard: Arc<ServiceSupervisor> =
        activate_supervisor(&actions, &event_bus, &plugins);

    let state = DaemonState::new(actions, event_bus.clone(), plugins, socket_path.clone());

    log::info!("nesttyd listening on {}", socket_path.display());
    socket::run_accept_loop(listener, state);

    // Arc::drop does not call shutdown_all; we must invoke it explicitly
    // for cooperative plugin shutdown before unlinking the socket.
    log::info!("shutting down supervised plugins");
    supervisor_guard.shutdown_all();
    // Explicit pool shutdown breaks any registry↔handler↔supervisor Arc
    // cycle that would otherwise prevent the pool's Drop from running.
    pool.shutdown();

    socket::cleanup_socket(&socket_path);
    log::info!("nesttyd shut down");
    ExitCode::SUCCESS
}

fn build_pool() -> Arc<ThreadPool> {
    let default_workers = std::thread::available_parallelism()
        .map(|n| n.get().saturating_mul(2))
        .unwrap_or(8)
        .clamp(4, 16);
    // Clamp env overrides to a sane band so a typo (e.g. `WORKERS=10000`)
    // can't OS-exhaust the daemon on startup.
    let workers = env_usize(ENV_POOL_WORKERS)
        .unwrap_or(default_workers)
        .clamp(1, 256);
    let queue_cap = env_usize(ENV_POOL_QUEUE).unwrap_or(64).clamp(1, 4096);
    log::info!("action pool: workers={workers} queue_cap={queue_cap}");
    ThreadPool::new(workers, queue_cap)
}

fn env_usize(var: &str) -> Option<usize> {
    let raw = std::env::var(var).ok()?;
    match raw.trim().parse::<usize>() {
        Ok(0) => {
            log::warn!("ignoring {var}={raw} (must be >= 1)");
            None
        }
        Ok(n) => Some(n),
        Err(e) => {
            log::warn!("ignoring {var}={raw}: parse error: {e}");
            None
        }
    }
}

/// Test-only actions enabled via `NESTTYD_E2E_TEST_ACTIONS=1`. Keep these
/// gated so they never appear in normal daemon runs.
fn register_e2e_actions(actions: &Arc<ActionRegistry>) {
    log::warn!("e2e test actions enabled (NESTTYD_E2E_TEST_ACTIONS=1)");
    actions.register_blocking("__test.slow_blocking", |params| {
        let ms = params.get("ms").and_then(|v| v.as_u64()).unwrap_or(200);
        std::thread::sleep(std::time::Duration::from_millis(ms));
        Ok(json!({ "slept_ms": ms }))
    });
}

fn register_plugin_commands(
    actions: &Arc<ActionRegistry>,
    plugins: &Arc<Vec<LoadedPlugin>>,
    socket_path: &Path,
) {
    let socket_str = socket_path.to_string_lossy().into_owned();
    // Iterate only the WINNING entry per unique plugin name (sorted
    // slice → resolve_by_name returns the last-by-dir entry). Without
    // this dedup, a losing duplicate with a command name the winner
    // does NOT have would leak into the dispatch table; only collisions
    // on the same `<name>.<cmd>` method get HashMap-overwritten.
    let mut seen_names: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let winners: Vec<&LoadedPlugin> = plugins
        .iter()
        .rev()
        .filter(|p| seen_names.insert(p.manifest.plugin.name.as_str()))
        .collect();
    for plugin in winners.iter() {
        let plugin_name = plugin.manifest.plugin.name.clone();
        for cmd in &plugin.manifest.commands {
            // A dot in the command name would create a 4+ segment
            // method that breaks `plugin.<name>.<cmd>` parsing for
            // downstream consumers (the trigger engine, the CLI).
            if cmd.name.contains('.') {
                log::warn!(
                    "plugin {} command `{}` contains a dot; skipping registration",
                    plugin_name,
                    cmd.name
                );
                continue;
            }
            let method = format!("plugin.{}.{}", plugin_name, cmd.name);
            let exec = cmd.exec.clone();
            let dir = plugin.dir.clone();
            let socket = socket_str.clone();
            actions.register_blocking(method, move |params| {
                run_plugin_shell(
                    &dir,
                    &exec,
                    &params.to_string(),
                    &socket,
                    PLUGIN_CMD_TIMEOUT,
                )
                .map(parse_plugin_stdout)
                .map_err(map_shell_error)
            });
        }
    }
    let plugins_for_module = plugins.clone();
    let socket_for_module = socket_str;
    actions.register_blocking_silent("_module.run", move |params| {
        let plugin_name = params
            .get("plugin")
            .and_then(|v| v.as_str())
            .ok_or_else(|| invalid_params("missing 'plugin' field"))?;
        let module_name = params
            .get("module")
            .and_then(|v| v.as_str())
            .ok_or_else(|| invalid_params("missing 'module' field"))?;
        // `resolve_by_name` picks the sorted-last (winner) entry,
        // matching `register_plugin_commands`' winners-only set.
        let plugin = nestty_core::plugin::resolve_by_name(&plugins_for_module, plugin_name)
            .ok_or_else(|| ResponseError {
                code: "not_found".into(),
                message: format!("plugin not found: {plugin_name}"),
            })?;
        let module = plugin
            .manifest
            .modules
            .iter()
            .find(|m| m.name == module_name)
            .ok_or_else(|| ResponseError {
                code: "not_found".into(),
                message: format!("module '{module_name}' not in plugin '{plugin_name}'"),
            })?;
        let out = run_plugin_shell(
            &plugin.dir,
            &module.exec,
            "",
            &socket_for_module,
            MODULE_RUN_TIMEOUT,
        )
        .map_err(map_shell_error)?;
        Ok(json!({
            "stdout": out.stdout,
            "exit_code": out.exit_code,
        }))
    });
}

fn run_plugin_shell(
    dir: &Path,
    exec: &str,
    stdin_payload: &str,
    socket_path: &str,
    timeout: Duration,
) -> Result<nestty_daemon::plugin_exec::ShellOutput, ShellError> {
    let mut env = HashMap::new();
    env.insert("NESTTY_SOCKET".into(), socket_path.into());
    env.insert(
        "NESTTY_PLUGIN_DIR".into(),
        dir.to_string_lossy().into_owned(),
    );
    spawn_plugin_shell(dir, exec, stdin_payload.as_bytes(), &env, timeout)
}

/// Mirrors the legacy GUI handler's contract: JSON stdout is returned
/// verbatim; otherwise wrap the trimmed text under `{ "output": ... }`
/// so the caller always receives a JSON object.
fn parse_plugin_stdout(out: nestty_daemon::plugin_exec::ShellOutput) -> serde_json::Value {
    serde_json::from_str::<serde_json::Value>(&out.stdout)
        .unwrap_or_else(|_| json!({ "output": out.stdout.trim() }))
}

fn map_shell_error(err: ShellError) -> ResponseError {
    match err {
        ShellError::NonZero(out) => ResponseError {
            code: "plugin_command_failed".into(),
            message: format!(
                "exit {}: {}",
                out.exit_code,
                out.stderr.trim().lines().next().unwrap_or("")
            ),
        },
        ShellError::Timeout { after, .. } => ResponseError {
            code: "plugin_timeout".into(),
            message: format!("plugin shell did not complete within {after:?}"),
        },
        ShellError::Spawn(msg) | ShellError::Wait(msg) => ResponseError {
            code: "plugin_spawn_failed".into(),
            message: msg,
        },
    }
}

fn register_builtins(
    actions: &Arc<ActionRegistry>,
    plugins: &Arc<Vec<nestty_core::plugin::LoadedPlugin>>,
) {
    actions.register_silent("system.ping", |_| Ok(json!({ "status": "ok" })));
    actions.register("system.log", |params| {
        let msg = params
            .get("message")
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_else(|| params.to_string());
        eprintln!("[system.log] {msg}");
        Ok(json!({}))
    });
    let actions_for_info = actions.clone();
    actions.register_silent("daemon.info", move |_| {
        let stats = actions_for_info.pool_stats();
        serde_json::to_value(serde_json::json!({
            "daemon": "nesttyd",
            "version": env!("CARGO_PKG_VERSION"),
            "host_plugins": true,
            "pool": stats.map(|s| serde_json::json!({
                "workers": s.workers,
                "capacity": s.capacity,
                "active": s.active,
                "queued": s.queued,
            })),
        }))
        .map_err(|e| internal_error(format!("daemon.info serialization failed: {e}")))
    });
    actions.register("theme.list", |_| {
        let themes: Vec<&str> = nestty_core::theme::Theme::list().to_vec();
        // `current` is GUI-state (per-window). Daemon reports null; GUI
        // resolves its own current theme through GUI-owned routing later.
        Ok(json!({ "themes": themes, "current": serde_json::Value::Null }))
    });
    let plugins_for_list = plugins.clone();
    actions.register("plugin.list", move |_| {
        let body: Vec<_> = plugins_for_list
            .iter()
            .map(|p| {
                let m = &p.manifest;
                json!({
                    "name": m.plugin.name,
                    "title": m.plugin.title,
                    "version": m.plugin.version,
                    "description": m.plugin.description,
                    "panels": m.panels.iter().map(|pd| json!({
                        "name": pd.name,
                        "title": pd.title,
                        "file": pd.file,
                        "icon": pd.icon,
                    })).collect::<Vec<_>>(),
                    "commands": m.commands.iter().map(|c| json!({
                        "name": c.name,
                        "exec": c.exec,
                        "description": c.description,
                    })).collect::<Vec<_>>(),
                    "modules": m.modules.iter().map(|md| json!({
                        "name": md.name,
                        "exec": md.exec,
                        "interval": md.interval,
                        "position": md.position,
                        "order": md.order,
                        "class": md.class,
                    })).collect::<Vec<_>>(),
                })
            })
            .collect();
        Ok(json!({ "plugins": body }))
    });
}

/// Accepts `1`, `true`, `yes` (case-insensitive). Everything else,
/// including `0` / `false` / empty / unset, disables.
fn env_flag_enabled(var: &str) -> bool {
    match std::env::var(var) {
        Ok(v) => matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"),
        Err(_) => false,
    }
}

/// `manifest.plugin.name` is the dispatch key for `plugin.<name>.<cmd>`
/// and statusbar `_module.run`. Sort the once-discovered list so two
/// daemons on the same machine register the same set in the same order
/// (deterministic last-write-wins on duplicates). Warns about dupes so
/// the user can fix the manifest.
pub fn discover_and_sort_plugins() -> Arc<Vec<nestty_core::plugin::LoadedPlugin>> {
    let plugins = nestty_core::plugin::discover_sorted_plugins();
    // After sort: equal names are adjacent and ordered by dir.
    // `register_blocking` does last-write-wins on HashMap insertion,
    // so the entry registered LAST (largest dir) is the active one.
    // `nestty_core::plugin::resolve_by_name` picks the same winner.
    let mut prev: Option<&str> = None;
    for p in &plugins {
        let name = p.manifest.plugin.name.as_str();
        if Some(name) == prev {
            log::warn!(
                "duplicate plugin manifest name `{}` at {}; the entry sorted last by dir wins `plugin.{}.<cmd>` resolution",
                name,
                p.dir.display(),
                name
            );
        }
        prev = Some(name);
    }
    log::info!(
        "discovered {} plugin manifest(s); spawning onStartup services",
        plugins.len()
    );
    for p in &plugins {
        log::info!(
            "plugin: {} v{}",
            p.manifest.plugin.name,
            p.manifest.plugin.version
        );
    }
    Arc::new(plugins)
}

fn activate_supervisor(
    actions: &Arc<ActionRegistry>,
    event_bus: &Arc<nestty_core::event_bus::EventBus>,
    plugins: &Arc<Vec<nestty_core::plugin::LoadedPlugin>>,
) -> Arc<ServiceSupervisor> {
    let reserved: Vec<&str> = LEGACY_DISPATCH_METHODS
        .iter()
        .copied()
        .chain(TRIGGER_ONLY_RESERVED_METHODS.iter().copied())
        .collect();
    ServiceSupervisor::new(
        event_bus.clone(),
        actions.clone(),
        plugins,
        env!("CARGO_PKG_VERSION"),
        &reserved,
    )
}
