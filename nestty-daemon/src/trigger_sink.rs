use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc;

use nestty_core::action_registry::{ActionRegistry, ActionResult, internal_error, invalid_params};
use nestty_core::protocol::{Request, Response};
use nestty_core::trigger::TriggerSink;
use serde_json::{Value, json};

use crate::socket::SocketCommand;

/// Trigger-only methods. Reserved by `ServiceSupervisor` so a plugin
/// manifest can't register them — the unix socket is reachable from any
/// user process, and arbitrary process spawn from socket would be a
/// trust break.
pub const TRIGGER_ONLY_RESERVED_METHODS: &[&str] = &["system.spawn"];

/// Tries `ActionRegistry` first, falls through to `socket::dispatch`
/// for legacy match-arm actions. All three paths (sync registry,
/// blocking registry worker, legacy fallthrough) log `[nestty] trigger
/// ...` on failure so misconfigured triggers stay visible.
pub struct LiveTriggerSink {
    registry: Arc<ActionRegistry>,
    dispatch_tx: mpsc::Sender<SocketCommand>,
    reply_tx: mpsc::Sender<Response>,
}

impl LiveTriggerSink {
    pub fn new(registry: Arc<ActionRegistry>, dispatch_tx: mpsc::Sender<SocketCommand>) -> Self {
        let (reply_tx, reply_rx) = mpsc::channel::<Response>();
        // Consumer thread: logs any fallthrough reply that came back with
        // ok=false. Lives until all `reply_tx` clones drop (i.e. the sink is
        // gone AND every queued SocketCommand has been processed).
        std::thread::spawn(move || {
            while let Ok(resp) = reply_rx.recv() {
                if resp.ok {
                    continue;
                }
                let (code, msg) = resp
                    .error
                    .map(|e| (e.code, e.message))
                    .unwrap_or_else(|| ("unknown".into(), String::new()));
                eprintln!(
                    "[nestty] trigger fallthrough id={} failed: {}: {}",
                    resp.id, code, msg
                );
            }
        });
        Self {
            registry,
            dispatch_tx,
            reply_tx,
        }
    }
}

impl LiveTriggerSink {
    /// Trigger-only fire-and-forget exec. Reachable ONLY from
    /// `[[triggers]]` config (same trust surface as `[keybindings]
    /// spawn:`) — `nestctl call system.spawn` returns `unknown_method`.
    ///
    /// Param shape: `{ argv: ["program", "arg1", ...] }`. argv is NOT
    /// auto-wrapped in a shell, so each `{event.*}`/`{context.*}` lands
    /// as a literal argv element where metacharacters can't be re-parsed
    /// — `; rm -rf ~` in a payload field stays inert. Users opting into
    /// `["sh", "-c", "<string>"]` lose that guarantee and own auditing
    /// every value spliced into the shell string. Children reaped on a
    /// worker; non-zero exits log.
    fn handle_system_spawn(params: Value) -> ActionResult {
        let argv = params
            .get("argv")
            .and_then(|v| v.as_array())
            .ok_or_else(|| invalid_params("system.spawn: argv must be a non-empty string array"))?;
        if argv.is_empty() {
            return Err(invalid_params("system.spawn: argv must not be empty"));
        }
        let argv_strs: Vec<String> = argv
            .iter()
            .map(|v| {
                v.as_str().map(String::from).ok_or_else(|| {
                    invalid_params("system.spawn: argv elements must all be strings")
                })
            })
            .collect::<Result<_, _>>()?;
        if argv_strs[0].is_empty() {
            return Err(invalid_params(
                "system.spawn: argv[0] (program name) must not be an empty string",
            ));
        }

        let program = argv_strs[0].clone();
        let mut child = Command::new(&program)
            .args(&argv_strs[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .spawn()
            .map_err(|e| {
                internal_error(format!("system.spawn: failed to exec {program:?}: {e}"))
            })?;

        let pid = child.id();
        eprintln!("[nestty] trigger system.spawn pid={pid} argv={argv_strs:?}");

        // Reap on a worker thread so the child doesn't become a
        // zombie. Trigger-spawned processes are fire-and-forget; we
        // discard the exit status on success and log on non-zero so a
        // misconfigured cure command is visible.
        let argv_log = argv_strs;
        std::thread::spawn(move || match child.wait() {
            Ok(status) if !status.success() => {
                eprintln!(
                    "[nestty] trigger system.spawn pid={pid} argv={argv_log:?} exited {status}"
                );
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!("[nestty] trigger system.spawn pid={pid} wait failed: {e}");
            }
        });

        Ok(json!({ "queued": true, "pid": pid }))
    }
}

impl TriggerSink for LiveTriggerSink {
    fn dispatch_action(&self, action: &str, params: Value) -> ActionResult {
        // `system.spawn` is intercepted before the registry check —
        // not registered, not in socket::dispatch's match arm — so the
        // unix socket can't reach it. See `handle_system_spawn` for
        // rationale.
        if action == "system.spawn" {
            return Self::handle_system_spawn(params);
        }
        if self.registry.has(action) {
            // Branch on blocking flag so we preserve the prior
            // synchronous error semantics for sync registry actions.
            // The TriggerEngine increments `fired` only on `Ok` and
            // log::warn's on `Err`; collapsing every registry call to
            // `Ok(queued)` would silently re-classify sync failures
            // as successful queueing.
            if self.registry.is_blocking(action) {
                // Worker-thread path: callback fires from worker after
                // we've returned `Ok(queued)` to the engine, so the
                // engine never sees the underlying error — log here
                // directly so misconfigured blocking actions stay
                // visible.
                let action_owned = action.to_string();
                self.registry.try_dispatch(
                    action,
                    params,
                    Box::new(move |result| {
                        if let Err(err) = result {
                            eprintln!(
                                "[nestty] trigger registry id={} (blocking) failed: {}: {}",
                                action_owned, err.code, err.message
                            );
                        }
                    }),
                );
                return Ok(json!({ "queued": true }));
            }
            // Sync path: propagate the ActionResult so the engine
            // increments `fired` only on Ok and logs Err.
            // `try_invoke` runs inline regardless of the blocking flag —
            // already guarded against blocking above.
            return self
                .registry
                .try_invoke(action, params)
                .expect("registry.has() just returned true");
        }
        // Fall through to legacy `socket::dispatch`. The reply channel is
        // shared with the consumer thread spawned in `new()` — that thread
        // surfaces any non-ok response to logs.
        let cmd = SocketCommand {
            request: Request::new(
                format!("trg-{}", uuid::Uuid::new_v4()),
                action.to_string(),
                params,
            ),
            reply: self.reply_tx.clone(),
            silent_completion: false,
        };
        self.dispatch_tx
            .send(cmd)
            .map_err(|e| internal_error(format!("trigger redispatch failed: {e}")))?;
        Ok(json!({ "queued": true }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nestty_core::action_registry::invalid_params;

    fn mk_sink_with_registry() -> (
        Arc<ActionRegistry>,
        LiveTriggerSink,
        mpsc::Receiver<SocketCommand>,
    ) {
        let registry = Arc::new(ActionRegistry::new());
        let (tx, rx) = mpsc::channel::<SocketCommand>();
        let sink = LiveTriggerSink::new(registry.clone(), tx);
        (registry, sink, rx)
    }

    #[test]
    fn sync_registry_action_returns_actual_result_not_queued() {
        let (registry, sink, _rx) = mk_sink_with_registry();
        registry.register("sync.ok", |_| Ok(json!("real-value")));
        let r = sink.dispatch_action("sync.ok", json!({})).unwrap();
        assert_eq!(r, json!("real-value"));
    }

    #[test]
    fn sync_registry_action_propagates_err_so_engine_logs_it() {
        let (registry, sink, _rx) = mk_sink_with_registry();
        registry.register("sync.fail", |_| Err(invalid_params("bad")));
        let err = sink.dispatch_action("sync.fail", json!({})).unwrap_err();
        assert_eq!(err.code, "invalid_params");
        assert_eq!(err.message, "bad");
    }

    #[test]
    fn blocking_registry_action_returns_queued_immediately() {
        let (registry, sink, _rx) = mk_sink_with_registry();
        registry.register_blocking("slow.ok", |_| {
            std::thread::sleep(std::time::Duration::from_millis(50));
            Ok(json!("eventual"))
        });
        let start = std::time::Instant::now();
        let r = sink.dispatch_action("slow.ok", json!({})).unwrap();
        assert!(
            start.elapsed() < std::time::Duration::from_millis(40),
            "dispatch_action returned in {:?}, expected <40ms",
            start.elapsed()
        );
        assert_eq!(r, json!({"queued": true}));
    }

    #[test]
    fn unknown_action_falls_through_to_socket_dispatch() {
        let (_registry, sink, rx) = mk_sink_with_registry();
        let r = sink
            .dispatch_action("legacy.thing", json!({"x": 1}))
            .unwrap();
        assert_eq!(r, json!({"queued": true}));
        // The fallthrough must have queued one SocketCommand on the
        // dispatch channel.
        let cmd = rx.try_recv().expect("expected one queued legacy command");
        assert_eq!(cmd.request.method, "legacy.thing");
        assert_eq!(cmd.request.params, json!({"x": 1}));
    }

    #[test]
    fn system_spawn_rejects_missing_argv() {
        let (_r, sink, _rx) = mk_sink_with_registry();
        let err = sink.dispatch_action("system.spawn", json!({})).unwrap_err();
        assert_eq!(err.code, "invalid_params");
        assert!(err.message.contains("argv"));
    }

    #[test]
    fn system_spawn_rejects_non_array_argv() {
        let (_r, sink, _rx) = mk_sink_with_registry();
        let err = sink
            .dispatch_action("system.spawn", json!({ "argv": "true" }))
            .unwrap_err();
        assert_eq!(err.code, "invalid_params");
    }

    #[test]
    fn system_spawn_rejects_empty_argv() {
        let (_r, sink, _rx) = mk_sink_with_registry();
        let err = sink
            .dispatch_action("system.spawn", json!({ "argv": [] }))
            .unwrap_err();
        assert_eq!(err.code, "invalid_params");
        assert!(err.message.contains("must not be empty"));
    }

    #[test]
    fn system_spawn_rejects_empty_program_name() {
        let (_r, sink, _rx) = mk_sink_with_registry();
        let err = sink
            .dispatch_action("system.spawn", json!({ "argv": ["", "--flag"] }))
            .unwrap_err();
        assert_eq!(err.code, "invalid_params");
        assert!(err.message.contains("program name"));
    }

    #[test]
    fn system_spawn_rejects_non_string_argv_element() {
        let (_r, sink, _rx) = mk_sink_with_registry();
        let err = sink
            .dispatch_action("system.spawn", json!({ "argv": ["true", 123] }))
            .unwrap_err();
        assert_eq!(err.code, "invalid_params");
        assert!(err.message.contains("strings"));
    }

    #[test]
    fn system_spawn_executes_program_and_reports_pid() {
        let (_r, sink, rx) = mk_sink_with_registry();
        // `true` exits 0; intent is to assert the spawn path returns
        // Ok with a pid AND does NOT fall through to socket::dispatch
        // (rx empty). Linux ships `/bin/true`; macOS ships only
        // `/usr/bin/true` (no `/bin/true` symlink), so probe both.
        let true_path = if std::path::Path::new("/bin/true").exists() {
            "/bin/true"
        } else {
            "/usr/bin/true"
        };
        let r = sink
            .dispatch_action("system.spawn", json!({ "argv": [true_path] }))
            .unwrap();
        assert_eq!(r["queued"], json!(true));
        assert!(r["pid"].as_u64().unwrap() > 0);
        assert!(
            rx.try_recv().is_err(),
            "system.spawn must not fall through to socket dispatch"
        );
    }

    #[test]
    fn system_spawn_reports_exec_failure() {
        let (_r, sink, _rx) = mk_sink_with_registry();
        let err = sink
            .dispatch_action(
                "system.spawn",
                json!({ "argv": ["/nonexistent/binary/that/does/not/exist"] }),
            )
            .unwrap_err();
        assert_eq!(err.code, "internal_error");
        assert!(err.message.contains("failed to exec"));
    }

    #[test]
    fn system_spawn_is_not_registered_in_action_registry() {
        // Trust-boundary regression guard: `system.spawn` MUST NOT be
        // reachable through the registry, otherwise `nestctl call
        // system.spawn` (which goes registry-first via socket::dispatch)
        // would arbitrary-spawn from any process holding NESTTY_SOCKET.
        let (registry, _sink, _rx) = mk_sink_with_registry();
        assert!(
            !registry.has("system.spawn"),
            "system.spawn must remain trigger-only — not in ActionRegistry"
        );
    }
}
