mod client;
mod commands;
mod plugin_cmds;
mod update;

use clap::Parser;
use commands::{Cli, Command, EventCommand, UpdateCommand};

fn main() {
    let cli = Cli::parse();

    // Handle update commands locally (no socket needed)
    if let Command::Update(cmd) = &cli.command {
        match cmd {
            UpdateCommand::Check => update::check_update(),
            UpdateCommand::Apply { version } => update::apply_update(version.clone()),
        }
        return;
    }

    // `nestctl event publish` bypasses the generic `discover_socket`
    // (which prefers GUI per-instance sockets) and connects directly
    // to the daemon — `events.publish` is daemon-only. Done BEFORE
    // `socket_path` resolution so an invalid JSON payload fails
    // without ever touching a socket (the generic resolver probes
    // NESTTY_SOCKET + discover_socket as a side effect).
    if let Command::Event(EventCommand::Publish { kind, payload }) = &cli.command {
        std::process::exit(dispatch_publish(kind, payload.as_deref(), cli.json));
    }

    let socket_path = cli.socket.clone().unwrap_or_else(|| {
        std::env::var("NESTTY_SOCKET")
            .ok()
            .filter(|p| std::os::unix::net::UnixStream::connect(p).is_ok())
            .unwrap_or_else(|| discover_socket().unwrap_or_else(|| "/tmp/nestty.sock".to_string()))
    });

    if std::env::var("NESTTY_DEBUG_SOCKET").is_ok() {
        eprintln!("[nestctl] using socket: {socket_path}");
    }

    // Event subscribe is a long-lived streaming connection
    if matches!(&cli.command, Command::Event(EventCommand::Subscribe)) {
        match client::subscribe(&socket_path) {
            Ok(()) => {}
            Err(e) => {
                eprintln!("Failed to subscribe: {e}");
                std::process::exit(1);
            }
        }
        return;
    }

    // Phase 19.1: per-plugin ergonomic wrappers own their dispatch
    // (preflight id resolution + custom human renderer), bypassing
    // the generic `cli.method() / cli.params()` path.
    if let Command::Todo(cmd) = &cli.command {
        std::process::exit(plugin_cmds::todo::dispatch(cmd, &socket_path, cli.json));
    }
    if let Command::Git(cmd) = &cli.command {
        std::process::exit(plugin_cmds::git::dispatch(cmd, &socket_path, cli.json));
    }
    if let Command::Bookmark(cmd) = &cli.command {
        std::process::exit(plugin_cmds::bookmark::dispatch(cmd, &socket_path, cli.json));
    }
    if let Command::Jira(cmd) = &cli.command {
        std::process::exit(plugin_cmds::jira::dispatch(cmd, &socket_path, cli.json));
    }
    if let Command::Slack(cmd) = &cli.command {
        std::process::exit(plugin_cmds::slack::dispatch(cmd, &socket_path, cli.json));
    }
    if let Command::Calendar(cmd) = &cli.command {
        std::process::exit(plugin_cmds::calendar::dispatch(cmd, &socket_path, cli.json));
    }
    // Phase 19.2 context aggregator. Bypass to the new dispatcher
    // unless the user is explicitly asking for the raw legacy shape
    // (`--json` without `--full`) — that path stays on the generic
    // `cli.method() / cli.params()` flow so any script piping the
    // bare snapshot keeps working.
    if let Command::Context { full } = &cli.command
        && (!cli.json || *full)
    {
        std::process::exit(plugin_cmds::context::dispatch(&socket_path, cli.json));
    }

    let result = client::send_command(&socket_path, &cli.method(), cli.params());

    match result {
        Ok(response) => {
            if response.ok {
                if let Some(result) = response.result {
                    if cli.json {
                        println!("{}", serde_json::to_string_pretty(&result).unwrap());
                    } else {
                        print_result(&result);
                    }
                }
            } else if let Some(err) = response.error {
                eprintln!("Error [{}]: {}", err.code, err.message);
                std::process::exit(1);
            }
        }
        Err(e) => {
            eprintln!("Failed to connect: {e}");
            std::process::exit(1);
        }
    }
}

/// Three-tier discovery in strict preference order:
/// 1. **Hardened GUI sockets** under `<runtime_dir>/gui-{PID}.sock` —
///    parent dir verified owner-only.
/// 2. **Legacy GUI sockets** under `/tmp/nestty-{PID}.sock` — kept so
///    a still-running pre-5b.4 nestty stays addressable.
/// 3. **Daemon well-known path** `<runtime_dir>/socket` — last resort.
///
/// Within each tier, newest mtime wins. The tier boundary is HARD: a
/// freshly-touched legacy socket cannot preempt a hardened one. This
/// matters because tier 2's parent (`/tmp`) is world-writable while
/// tier 1's is owner-only.
fn discover_socket() -> Option<String> {
    let runtime_dir = nestty_core::paths::runtime_dir();
    let trusted = nestty_core::paths::is_trusted_dir(&runtime_dir);

    if trusted && let Some(hit) = best_connectable_with_prefix(&runtime_dir, "gui-", ".sock") {
        return Some(hit);
    }
    if let Some(hit) =
        best_connectable_with_prefix(std::path::Path::new("/tmp"), "nestty-", ".sock")
    {
        return Some(hit);
    }
    if trusted {
        let well_known = runtime_dir.join("socket");
        if std::os::unix::net::UnixStream::connect(&well_known).is_ok() {
            return Some(well_known.to_string_lossy().to_string());
        }
    }
    None
}

fn best_connectable_with_prefix(
    dir: &std::path::Path,
    prefix: &str,
    suffix: &str,
) -> Option<String> {
    let mut candidates: Vec<std::path::PathBuf> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .flatten()
            .filter(|e| {
                let n = e.file_name();
                let s = n.to_string_lossy();
                s.starts_with(prefix) && s.ends_with(suffix)
            })
            .map(|e| e.path())
            .collect(),
        Err(_) => return None,
    };
    candidates.sort_by(|a, b| {
        let ta = a.metadata().and_then(|m| m.modified()).ok();
        let tb = b.metadata().and_then(|m| m.modified()).ok();
        tb.cmp(&ta)
    });
    for path in candidates {
        if std::os::unix::net::UnixStream::connect(&path).is_ok() {
            return Some(path.to_string_lossy().to_string());
        }
    }
    None
}

fn dispatch_publish(kind: &str, payload: Option<&str>, json: bool) -> i32 {
    // Local JSON parse so a malformed payload fails before opening the
    // daemon socket. Defaults to `{}` when omitted.
    let payload_value: serde_json::Value = match payload {
        Some(raw) => match serde_json::from_str(raw) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("Error [invalid_argument]: payload is not valid JSON: {e}");
                return 1;
            }
        },
        None => serde_json::json!({}),
    };
    let Some(socket_path) = nestty_core::paths::daemon_socket_path() else {
        eprintln!(
            "Error [no_daemon]: daemon socket path is untrusted or runtime dir missing; is nesttyd running?"
        );
        return 1;
    };
    let params = serde_json::json!({
        "kind": kind,
        "payload": payload_value,
    });
    match client::send_command(&socket_path.to_string_lossy(), "events.publish", params) {
        Ok(response) => {
            if response.ok {
                if let Some(result) = response.result {
                    if json {
                        println!("{}", serde_json::to_string_pretty(&result).unwrap());
                    } else {
                        print_result(&result);
                    }
                }
                0
            } else if let Some(err) = response.error {
                eprintln!("Error [{}]: {}", err.code, err.message);
                1
            } else {
                eprintln!("Error: response indicated failure but had no error body");
                1
            }
        }
        Err(e) => {
            eprintln!("Failed to connect: {e}");
            1
        }
    }
}

fn print_result(value: &serde_json::Value) {
    match value {
        serde_json::Value::String(s) => println!("{s}"),
        serde_json::Value::Array(arr) => {
            for item in arr {
                println!("{}", serde_json::to_string(item).unwrap());
            }
        }
        other => println!("{}", serde_json::to_string_pretty(other).unwrap()),
    }
}
