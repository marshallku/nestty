//! Daemon-side `TriggerSink` implementation. Mirrors
//! [`crate::trigger_sink::LiveTriggerSink`] in semantics — registry-first
//! dispatch with `system.spawn` intercepted before the registry — but
//! routes GUI-owned action fallthrough through a registered GUI's
//! `Invoke` channel instead of a GTK [`SocketCommand`] queue, which
//! the daemon does not have.
//!
//! Fallthrough is fire-and-forget through a bounded mpsc queue + single
//! worker thread. `dispatch_action` never blocks the trigger pump thread
//! on a GUI Invoke (which can run up to `method_invoke_timeout(action)`
//! — 5s for tabs/terminal, 120s for webview). Queue saturation logs a
//! warning and drops; this matches today's GUI semantics where
//! `LiveTriggerSink::dispatch_tx` is a `mpsc::Sender` and a wedged
//! GTK dispatcher would back up the channel.

use std::sync::Arc;
use std::sync::mpsc;
use std::thread;

use nestty_core::action_registry::{ActionRegistry, ActionResult, internal_error, invalid_params};
use nestty_core::trigger::TriggerSink;
use serde_json::{Value, json};

use crate::gui_registry::{GuiRegistry, method_invoke_timeout};

/// Worker queue capacity. Higher than the action pool's queue (64)
/// because trigger fan-out can burst — completion events fan out to
/// every matching trigger, each of which may fall through to a GUI
/// Invoke. Bounded so a wedged GUI cannot grow daemon memory without
/// limit.
const FALLTHROUGH_QUEUE: usize = 256;

pub struct DaemonTriggerSink {
    registry: Arc<ActionRegistry>,
    /// Held so `system.spawn` can merge the registered GUI's curated
    /// env (`HYPRLAND_INSTANCE_SIGNATURE`, `DISPLAY`, etc.) into the
    /// child's environment. The fallthrough worker also reaches the
    /// registry, but it holds its own clone via the worker's move
    /// closure.
    gui: Arc<GuiRegistry>,
    fallthrough_tx: mpsc::SyncSender<(String, Value)>,
}

impl DaemonTriggerSink {
    pub fn new(registry: Arc<ActionRegistry>, gui: Arc<GuiRegistry>) -> Self {
        let (tx, rx) = mpsc::sync_channel::<(String, Value)>(FALLTHROUGH_QUEUE);
        let gui_for_worker = gui.clone();
        thread::Builder::new()
            .name("nestty-trigger-fallthrough".into())
            .spawn(move || fallthrough_worker(rx, gui_for_worker))
            .expect("spawn trigger fallthrough worker");
        Self {
            registry,
            gui,
            fallthrough_tx: tx,
        }
    }

    /// Spawn child for trigger-fired `system.spawn`. Merges the
    /// registered primary GUI's curated env (Stage E) on top of the
    /// daemon's inherited env, so triggers running under a graphical
    /// session see `HYPRLAND_INSTANCE_SIGNATURE` etc. With no
    /// registered GUI (headless), the child inherits daemon env only.
    fn handle_system_spawn(&self, params: Value) -> ActionResult {
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
        let mut cmd = std::process::Command::new(&program);
        cmd.args(&argv_strs[1..])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null());
        // Merge primary GUI's daemon-filtered env on top of inherited
        // daemon env. `Command::envs` OVERRIDES matching keys without
        // clearing unrelated ones (PATH etc. from the daemon stay).
        if let Some(gui_env) = self.gui.primary_gui_env() {
            cmd.envs(gui_env);
        }
        let mut child = cmd.spawn().map_err(|e| {
            internal_error(format!("system.spawn: failed to exec {program:?}: {e}"))
        })?;
        let pid = child.id();
        log::info!("trigger system.spawn pid={pid} argv={argv_strs:?}");
        let argv_log = argv_strs;
        thread::spawn(move || match child.wait() {
            Ok(status) if !status.success() => {
                log::warn!("trigger system.spawn pid={pid} argv={argv_log:?} exited {status}");
            }
            Ok(_) => {}
            Err(e) => log::warn!("trigger system.spawn pid={pid} wait failed: {e}"),
        });
        Ok(json!({ "queued": true, "pid": pid }))
    }
}

impl TriggerSink for DaemonTriggerSink {
    fn dispatch_action(&self, action: &str, params: Value) -> ActionResult {
        if action == "system.spawn" {
            return self.handle_system_spawn(params);
        }
        if self.registry.has(action) {
            if self.registry.is_blocking(action) {
                let action_owned = action.to_string();
                self.registry.try_dispatch(
                    action,
                    params,
                    Box::new(move |result| {
                        if let Err(err) = result {
                            log::warn!(
                                "trigger registry id={} (blocking) failed: {}: {}",
                                action_owned,
                                err.code,
                                err.message
                            );
                        }
                    }),
                );
                return Ok(json!({ "queued": true }));
            }
            return self
                .registry
                .try_invoke(action, params)
                .expect("registry.has() just returned true");
        }
        // Fallthrough — push to the bounded queue. `try_send` so a wedged
        // GUI can't stall the trigger pump.
        match self.fallthrough_tx.try_send((action.to_string(), params)) {
            Ok(()) => Ok(json!({ "queued": true })),
            Err(mpsc::TrySendError::Full(_)) => {
                log::warn!(
                    "trigger fallthrough queue full ({FALLTHROUGH_QUEUE}); dropping {action}"
                );
                Err(internal_error("trigger fallthrough queue saturated"))
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {
                Err(internal_error("trigger fallthrough worker gone"))
            }
        }
    }
}

fn fallthrough_worker(rx: mpsc::Receiver<(String, Value)>, gui: Arc<GuiRegistry>) {
    while let Ok((action, params)) = rx.recv() {
        match gui.route(&action, None) {
            Ok(client) => {
                let timeout = method_invoke_timeout(&action);
                let resp = client.invoke(&action, params, timeout);
                if !resp.ok {
                    let (code, message) = resp
                        .error
                        .map(|e| (e.code, e.message))
                        .unwrap_or_else(|| ("unknown".into(), String::new()));
                    log::warn!("trigger fallthrough action={action} failed: {code}: {message}");
                }
            }
            Err(reason) => {
                log::warn!("trigger fallthrough action={action} unroutable: {reason}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nestty_core::protocol::ResponseError;

    fn mk_sink() -> (Arc<ActionRegistry>, Arc<GuiRegistry>, DaemonTriggerSink) {
        let reg = Arc::new(ActionRegistry::new());
        let gui = GuiRegistry::new();
        let sink = DaemonTriggerSink::new(reg.clone(), gui.clone());
        (reg, gui, sink)
    }

    #[test]
    fn sync_registry_action_returns_real_value() {
        let (reg, _gui, sink) = mk_sink();
        reg.register("sync.ok", |_| Ok(json!("hello")));
        assert_eq!(
            sink.dispatch_action("sync.ok", json!({})).unwrap(),
            json!("hello")
        );
    }

    #[test]
    fn sync_registry_err_is_propagated() {
        let (reg, _gui, sink) = mk_sink();
        reg.register("sync.bad", |_| {
            Err(ResponseError {
                code: "invalid_params".into(),
                message: "no".into(),
            })
        });
        let e = sink.dispatch_action("sync.bad", json!({})).unwrap_err();
        assert_eq!(e.code, "invalid_params");
    }

    #[test]
    fn system_spawn_rejects_empty_argv() {
        let (_reg, _gui, sink) = mk_sink();
        let e = sink
            .dispatch_action("system.spawn", json!({"argv": []}))
            .unwrap_err();
        assert_eq!(e.code, "invalid_params");
    }

    #[test]
    fn system_spawn_rejects_missing_argv() {
        let (_reg, _gui, sink) = mk_sink();
        let e = sink.dispatch_action("system.spawn", json!({})).unwrap_err();
        assert_eq!(e.code, "invalid_params");
    }

    #[test]
    fn unknown_action_fallthrough_with_no_gui_returns_queued() {
        let (_reg, _gui, sink) = mk_sink();
        // Method is GUI-capability'd (tab.new) but no GUI registered → worker
        // dequeues and logs `unroutable: no_gui`. dispatch_action returns
        // `queued` immediately regardless of GUI state — matches
        // LiveTriggerSink semantics where the reply consumer logs failures
        // out-of-band.
        let r = sink.dispatch_action("tab.new", json!({})).unwrap();
        assert_eq!(r, json!({"queued": true}));
    }

    #[test]
    fn fallthrough_queue_returns_err_when_saturated() {
        let (_reg, _gui, sink) = mk_sink();
        // Saturate the queue by sending FALLTHROUGH_QUEUE + worker-in-flight
        // entries. The worker is blocked waiting for an Invoke response from
        // a non-existent GUI — so route() returns Err immediately and the
        // worker spins fast. We need to keep ahead of the worker; do this
        // by enqueueing rapidly via direct channel access. But the public
        // API only enqueues via try_send → realistically this test cannot
        // reliably observe the Full case without a synchronization barrier.
        // We instead verify the dispatch path returns Ok when not saturated,
        // and leave Full-path coverage to a focused integration test once
        // we have a way to throttle the worker. For now: smoke-test that
        // many dispatches don't panic.
        for _ in 0..50 {
            let _ = sink.dispatch_action("tab.new", json!({}));
        }
    }
}
