//! Daemon-client thread: connects to `nesttyd`, advertises GUI
//! capabilities via `gui.register`, and forwards inbound `Invoke`
//! requests through the existing dispatch pump. A missing daemon is
//! benign — the reconnect loop polls quietly with capped backoff.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::thread;
use std::time::Duration;

use nestty_core::event_bus::{Event as BusEvent, EventBus, RecvOutcome, next_bridge_id};
use nestty_core::protocol::{Event as WireEvent, Invoke, Request, Response};
use nestty_core::thread_pool::Cancelable;
use serde_json::Value;

use crate::socket::SocketCommand;

const PROTOCOL_VERSION: u32 = nestty_core::protocol::PROTOCOL_VERSION;

/// GUI bus → daemon bus forwarder kind list. Only events relevant to
/// daemon-side triggers / context cross the bridge; `terminal.output`
/// is excluded because it fires per-keystroke and would saturate the
/// wire without any trigger-facing value.
const GUI_TO_DAEMON_FORWARD_KINDS: &[&str] = &[
    "panel.exited",
    "panel.focused",
    "panel.title_changed",
    "tab.closed",
    "tab.created",
    "tab.renamed",
    "terminal.cwd_changed",
    "terminal.shell_precmd",
    "terminal.shell_preexec",
    "webview.loaded",
    "webview.navigated",
    "webview.title_changed",
    "window.restored",
];

/// How often each forwarder thread checks the shutdown flag when no
/// matching events have arrived. 100 ms is responsive enough for
/// reconnect (~3 cycles per backoff tick) without burning CPU.
const FORWARDER_POLL: Duration = Duration::from_millis(100);

/// Workers spend most of their time waiting on the GTK reply channel,
/// so the cap is concurrency-limiting, not throughput-tuning.
const POOL_WORKERS: usize = 8;
const POOL_QUEUE: usize = 32;

const CAPABILITIES: &[&str] = &[
    "tab",
    "split",
    "terminal",
    "webview",
    "background",
    "statusbar",
    "agent.ui",
    "plugin.open",
    "session",
    "search",
];

const BACKOFF_INITIAL: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

pub fn spawn(
    dispatch_tx: Sender<SocketCommand>,
    event_bus: Arc<EventBus>,
    host_triggers_tx: Sender<bool>,
) {
    thread::Builder::new()
        .name("nestty-gui-client".into())
        .spawn(move || {
            // Pool is process-lifetime: per-reconnect Drop would block
            // up to `pool_queue * 125s` joining slow invoke workers.
            // `generation` invalidates jobs admitted under an older
            // connection so they bail out before mutating GTK state.
            let pool = nestty_core::thread_pool::ThreadPool::new(POOL_WORKERS, POOL_QUEUE);
            let generation = Arc::new(AtomicU64::new(0));
            reconnect_loop(dispatch_tx, pool, generation, event_bus, host_triggers_tx);
        })
        .expect("spawn nestty-gui-client");
}

fn reconnect_loop(
    dispatch_tx: Sender<SocketCommand>,
    pool: std::sync::Arc<nestty_core::thread_pool::ThreadPool>,
    generation: Arc<AtomicU64>,
    event_bus: Arc<EventBus>,
    host_triggers_tx: Sender<bool>,
) {
    let mut backoff = BACKOFF_INITIAL;
    loop {
        // daemon_socket_path filters inherited per-instance NESTTY_SOCKET
        // and refuses untrusted runtime dirs.
        let Some(socket_path) = nestty_core::paths::daemon_socket_path() else {
            log::debug!(
                "gui_client: daemon socket path untrusted; sleeping {:?}",
                backoff
            );
            thread::sleep(backoff);
            backoff = (backoff * 2).min(BACKOFF_MAX);
            continue;
        };
        let registered = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        match run(
            &socket_path.to_string_lossy(),
            dispatch_tx.clone(),
            pool.clone(),
            generation.clone(),
            registered.clone(),
            event_bus.clone(),
            host_triggers_tx.clone(),
        ) {
            // log::debug so a daemon-never-starts run stays silent on
            // stderr — the loop polls at most every 30s anyway, but a
            // line per attempt would still be visible noise. Surface
            // with RUST_LOG=debug if you need to see the cadence.
            Ok(()) => log::debug!("gui_client disconnected from daemon"),
            Err(e) => log::debug!("gui_client error: {e}"),
        }
        log::debug!("gui_client reconnect in {:?}", backoff);
        thread::sleep(backoff);
        // Bump AFTER the sleep so the first retry waits BACKOFF_INITIAL,
        // not 2× it. Reset on success.
        if registered.load(std::sync::atomic::Ordering::SeqCst) {
            backoff = BACKOFF_INITIAL;
        } else {
            backoff = (backoff * 2).min(BACKOFF_MAX);
        }
    }
}

fn run(
    socket_path: &str,
    dispatch_tx: Sender<SocketCommand>,
    pool: std::sync::Arc<nestty_core::thread_pool::ThreadPool>,
    generation: Arc<AtomicU64>,
    registered: std::sync::Arc<std::sync::atomic::AtomicBool>,
    event_bus: Arc<EventBus>,
    host_triggers_tx: Sender<bool>,
) -> Result<(), String> {
    // Exit bump invalidates queued stale jobs IMMEDIATELY on disconnect,
    // not on the next `run()` — otherwise the reconnect backoff sleep
    // is a window where a stale job can still pass the generation check.
    struct GenGuard<'a>(&'a Arc<AtomicU64>);
    impl Drop for GenGuard<'_> {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    let my_gen = generation.fetch_add(1, Ordering::SeqCst).wrapping_add(1);
    let _gen_guard = GenGuard(&generation);
    let stream = UnixStream::connect(socket_path)
        .map_err(|e| format!("connect to nesttyd at {socket_path}: {e}"))?;
    let write_stream = stream
        .try_clone()
        .map_err(|e| format!("clone stream: {e}"))?;

    let (writer_tx, writer_rx) = channel::<String>();
    thread::spawn(move || {
        let mut writer = write_stream;
        while let Ok(line) = writer_rx.recv() {
            if writeln!(writer, "{line}").is_err() {
                return;
            }
        }
    });

    let mut reader = BufReader::new(stream);
    let register_id = register(&writer_tx)?;
    let ack = await_register_ack(&mut reader, &register_id)?;
    registered.store(true, std::sync::atomic::Ordering::SeqCst);

    // Daemon advertises whether IT will dispatch triggers. If true,
    // start the GUI→daemon event forwarder so the daemon's engine
    // sees GUI-published events. Otherwise the GUI's local engine
    // remains authoritative (Stage A semantics) and the forwarder
    // stays dormant.
    let forwarder_stop = Arc::new(AtomicBool::new(false));
    let _forwarder_guard = ForwarderGuard(forwarder_stop.clone());
    if ack.host_triggers {
        start_gui_event_forwarder(event_bus.clone(), writer_tx.clone(), forwarder_stop.clone());
    }

    // Cut-over signal: send AFTER the forwarder subscribes so that
    // events published in the window between forwarder-start and the
    // GTK timer clearing the local engine are caught by the forwarder
    // (and dispatched daemon-side). Sending before would let the timer
    // clear local subscriptions while the forwarder is not yet
    // subscribed — events in that gap would be lost by both engines.
    // The drop guard symmetrically sends `false` on every `run()`
    // exit, so a daemon crash mid-session restores local authority
    // before the reconnect-backoff window opens.
    let _ht_guard = HostTriggersGuard(host_triggers_tx.clone());
    let _ = host_triggers_tx.send(ack.host_triggers);

    for line in reader.lines() {
        let line = line.map_err(|e| format!("read: {e}"))?;
        if line.trim().is_empty() {
            continue;
        }
        let value: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                log::debug!("[nestty] gui_client malformed line: {e}");
                continue;
            }
        };

        if let Some(method) = value.get("invoke").and_then(|v| v.as_str()) {
            // _ping inline so heartbeat stays responsive even when the
            // pool is saturated.
            if method == "_ping" {
                if let Err(e) = handle_ping(value, &writer_tx) {
                    log::warn!("[nestty] gui_client ping reply: {e}");
                }
                continue;
            }
            let job = Box::new(GuiInvokeJob {
                value,
                dispatch_tx: dispatch_tx.clone(),
                writer_tx: writer_tx.clone(),
                generation: generation.clone(),
                admitted_gen: my_gen,
            });
            if let Err(rejected) = pool.try_execute(job) {
                rejected.cancel();
            }
        } else if value.get("ok").is_some() {
            log::debug!("[nestty] gui_client response: {value}");
        } else if value.get("type").is_some() {
            // `source` must round-trip — the local trigger engine's
            // preflight-promotion gates on COMPLETION_EVENT_SOURCE for
            // daemon-hosted plugin completions.
            //
            // Stage B: stamp `bridge_id` on the republish so the
            // GUI→daemon forwarder (Stage C) recognizes this as
            // "already crossed once" and doesn't echo it back.
            match serde_json::from_value::<WireEvent>(value) {
                Ok(wire) => {
                    let source = wire.source.unwrap_or_else(|| "daemon".to_string());
                    event_bus.publish_bridged(
                        BusEvent::new(wire.event_type, source, wire.data),
                        next_bridge_id(),
                    );
                }
                Err(e) => log::debug!("[nestty] gui_client malformed Event: {e}"),
            }
        } else {
            log::debug!("[nestty] gui_client ignoring: {line:.200}");
        }
    }
    Ok(())
}

/// Drop-guard for the forwarder shutdown flag. Mirrors the existing
/// `GenGuard` pattern: flips the AtomicBool on every `run()` exit
/// (success OR error), guaranteeing the spawned forwarder threads
/// observe it within one [`FORWARDER_POLL`] interval and exit. The
/// per-kind `subscribe_unbounded` receivers drop when the threads
/// exit, breaking the bus subscription cleanly.
struct ForwarderGuard(Arc<AtomicBool>);

impl Drop for ForwarderGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// Drop-guard that signals the GTK timer to restore local trigger
/// authority on `run()` exit. Without this, a daemon crash mid-session
/// leaves the local engine empty (set by the prior `host_triggers=true`
/// cut-over) for the entire reconnect-backoff window. The `send` is
/// best-effort: if the receiver is gone (window already shut down),
/// the error is benign.
struct HostTriggersGuard(Sender<bool>);

impl Drop for HostTriggersGuard {
    fn drop(&mut self) {
        let _ = self.0.send(false);
    }
}

/// Spawn one thread per [`GUI_TO_DAEMON_FORWARD_KINDS`] entry. Each
/// thread subscribes unbounded to its kind and forwards GUI-native
/// (non-bridged) events to the daemon via `_bus.publish` Request
/// frames on `writer_tx`. Bridged events (those that crossed the
/// daemon→GUI bridge) are skipped via the `bridge_id` check, so the
/// loop is broken on the GUI side.
fn start_gui_event_forwarder(
    event_bus: Arc<EventBus>,
    writer_tx: Sender<String>,
    stop: Arc<AtomicBool>,
) {
    for kind in GUI_TO_DAEMON_FORWARD_KINDS {
        let rx = event_bus.subscribe_unbounded(*kind);
        let writer_tx = writer_tx.clone();
        let stop = stop.clone();
        let kind = (*kind).to_string();
        let thread_name = format!("nestty-gui-forwarder-{}", kind.replace('.', "-"));
        if let Err(e) = thread::Builder::new()
            .name(thread_name)
            .spawn(move || gui_forwarder_loop(kind, rx, writer_tx, stop))
        {
            log::warn!("[nestty] gui_client: failed to spawn forwarder thread: {e}");
        }
    }
}

fn gui_forwarder_loop(
    kind: String,
    rx: nestty_core::event_bus::EventReceiver,
    writer_tx: Sender<String>,
    stop: Arc<AtomicBool>,
) {
    loop {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        match rx.recv_timeout(FORWARDER_POLL) {
            RecvOutcome::Event(ev) => {
                if ev.bridge_id.is_some() {
                    // Came in via the daemon→GUI bridge — already
                    // crossed once, do not echo back.
                    continue;
                }
                let req = Request::new(
                    uuid::Uuid::new_v4().to_string(),
                    "_bus.publish",
                    serde_json::json!({
                        "kind": ev.kind,
                        "source": ev.source,
                        "timestamp_ms": ev.timestamp_ms,
                        "payload": ev.payload,
                    }),
                );
                let line = match serde_json::to_string(&req) {
                    Ok(s) => s,
                    Err(e) => {
                        log::warn!("[nestty] forwarder({kind}) serialize: {e}");
                        continue;
                    }
                };
                if writer_tx.send(line).is_err() {
                    // Writer thread exited (stream closed) — nothing
                    // more we can do; let the receiver drop on exit.
                    return;
                }
            }
            RecvOutcome::Timeout => continue,
            RecvOutcome::Disconnected => return,
        }
    }
}

fn register(writer_tx: &Sender<String>) -> Result<String, String> {
    let window_id = uuid::Uuid::new_v4().to_string();
    let req_id = uuid::Uuid::new_v4().to_string();
    let req = Request::new(
        &req_id,
        "gui.register",
        serde_json::json!({
            "window_id": window_id,
            "capabilities": CAPABILITIES,
            "want_primary": true,
            "version": env!("CARGO_PKG_VERSION"),
            "protocol_version": PROTOCOL_VERSION,
        }),
    );
    let line = serde_json::to_string(&req).map_err(|e| format!("serialize register: {e}"))?;
    writer_tx
        .send(line)
        .map_err(|_| "writer thread exited before register".to_string())?;
    Ok(req_id)
}

/// Captured fields from the daemon's `gui.register` success ack.
/// Today only `host_triggers` flows back to the caller; future
/// fields (negotiated protocol version, daemon-advertised
/// capabilities) would join here.
#[derive(Debug, Clone, Default)]
struct RegisterAck {
    host_triggers: bool,
}

fn await_register_ack(
    reader: &mut BufReader<UnixStream>,
    register_id: &str,
) -> Result<RegisterAck, String> {
    let mut line = String::new();
    if reader
        .read_line(&mut line)
        .map_err(|e| format!("read register ack: {e}"))?
        == 0
    {
        return Err("daemon closed connection before register ack".into());
    }
    let resp: Response = serde_json::from_str(line.trim())
        .map_err(|e| format!("parse register ack: {e} (line={line:.200})"))?;
    if resp.id != register_id {
        return Err(format!(
            "register ack id mismatch: expected {register_id}, got {}",
            resp.id
        ));
    }
    if !resp.ok {
        let err = resp.error.unwrap_or(nestty_core::protocol::ResponseError {
            code: "unknown".into(),
            message: String::new(),
        });
        return Err(format!("register rejected: {} {}", err.code, err.message));
    }
    let result = resp.result.unwrap_or_default();
    let host_triggers = result
        .get("host_triggers")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    log::info!("[nestty] gui_client registered with nesttyd: {result}");
    Ok(RegisterAck { host_triggers })
}

fn handle_ping(value: Value, writer_tx: &Sender<String>) -> Result<(), String> {
    let inv: Invoke = serde_json::from_value(value).map_err(|e| format!("parse ping: {e}"))?;
    let resp = Response::success(inv.id, inv.params);
    let encoded = serde_json::to_string(&resp).map_err(|e| format!("serialize ping: {e}"))?;
    writer_tx
        .send(encoded)
        .map_err(|_| "writer thread closed".to_string())
}

struct GuiInvokeJob {
    value: Value,
    dispatch_tx: Sender<SocketCommand>,
    writer_tx: Sender<String>,
    /// Connection-generation gate: a worker that picks up a job after
    /// its admitting connection died MUST NOT dispatch side-effecting
    /// methods through GTK — the daemon has already failed the pending
    /// invoke.
    generation: Arc<AtomicU64>,
    admitted_gen: u64,
}

impl GuiInvokeJob {
    fn write_overloaded(value: &Value, writer_tx: &Sender<String>) {
        // Best-effort id extraction — `cancel` MUST NOT panic on
        // malformed input.
        let id = value
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let resp = Response::error(
            id,
            "overloaded",
            "GUI invoke pool saturated; client cannot accept more concurrent invokes",
        );
        match serde_json::to_string(&resp) {
            Ok(encoded) => {
                let _ = writer_tx.send(encoded);
            }
            Err(e) => log::warn!("[nestty] gui_client cancel serialize: {e}"),
        }
    }
}

impl Cancelable for GuiInvokeJob {
    fn run(self: Box<Self>) {
        let this = *self;
        if this.admitted_gen != this.generation.load(Ordering::SeqCst) {
            Self::write_overloaded(&this.value, &this.writer_tx);
            return;
        }
        if let Err(e) = handle_invoke(this.value, &this.dispatch_tx, &this.writer_tx) {
            log::warn!("[nestty] gui_client invoke worker: {e}");
        }
    }

    fn cancel(self: Box<Self>) {
        Self::write_overloaded(&self.value, &self.writer_tx);
    }
}

fn handle_invoke(
    value: Value,
    dispatch_tx: &Sender<SocketCommand>,
    writer_tx: &Sender<String>,
) -> Result<(), String> {
    let inv: Invoke = serde_json::from_value(value).map_err(|e| format!("parse Invoke: {e}"))?;
    let (reply_tx, reply_rx) = channel::<Response>();
    let cmd = SocketCommand {
        request: Request::new(inv.id.clone(), &inv.invoke, inv.params),
        reply: reply_tx,
    };
    if dispatch_tx.send(cmd).is_err() {
        return Err("GTK dispatch channel closed".into());
    }
    // > daemon's 120s outer timeout — wedged-pump safety net only.
    let resp = match reply_rx.recv_timeout(Duration::from_secs(125)) {
        Ok(r) => r,
        Err(_) => Response::error(
            inv.id.clone(),
            "gui_internal_timeout",
            "GTK pump did not reply",
        ),
    };
    let encoded = serde_json::to_string(&resp).map_err(|e| format!("serialize response: {e}"))?;
    writer_tx
        .send(encoded)
        .map_err(|_| "writer thread closed".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::mpsc::RecvTimeoutError;

    fn invoke_value(id: &str, method: &str) -> Value {
        json!({ "id": id, "invoke": method, "params": { "x": 1 } })
    }

    fn mk_job(
        value: Value,
        dispatch_tx: Sender<SocketCommand>,
        writer_tx: Sender<String>,
        generation: Arc<AtomicU64>,
        admitted_gen: u64,
    ) -> Box<GuiInvokeJob> {
        Box::new(GuiInvokeJob {
            value,
            dispatch_tx,
            writer_tx,
            generation,
            admitted_gen,
        })
    }

    #[test]
    fn run_dispatches_and_writes_reply() {
        let (dispatch_tx, dispatch_rx) = channel::<SocketCommand>();
        let (writer_tx, writer_rx) = channel::<String>();
        let dispatcher = thread::spawn(move || {
            let cmd = dispatch_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("dispatch_rx should receive a command");
            cmd.reply
                .send(Response::success(cmd.request.id, json!("ok")))
                .unwrap();
        });
        let generation = Arc::new(AtomicU64::new(1));
        let job = mk_job(
            invoke_value("inv-1", "webview.eval"),
            dispatch_tx,
            writer_tx,
            generation,
            1,
        );
        Cancelable::run(job);
        let line = writer_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("writer_tx should receive reply");
        let resp: Response = serde_json::from_str(&line).unwrap();
        assert_eq!(resp.id, "inv-1");
        assert!(resp.ok);
        dispatcher.join().unwrap();
    }

    #[test]
    fn cancel_writes_overloaded_response() {
        let (dispatch_tx, dispatch_rx) = channel::<SocketCommand>();
        let (writer_tx, writer_rx) = channel::<String>();
        let generation = Arc::new(AtomicU64::new(1));
        let job = mk_job(
            invoke_value("inv-2", "webview.eval"),
            dispatch_tx,
            writer_tx,
            generation,
            1,
        );
        Cancelable::cancel(job);
        match dispatch_rx.recv_timeout(Duration::from_millis(50)) {
            Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => {}
            Ok(_) => panic!("dispatch_rx unexpectedly produced a command"),
        }
        let line = writer_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("writer_tx should receive overloaded response");
        let resp: Response = serde_json::from_str(&line).unwrap();
        assert_eq!(resp.id, "inv-2");
        assert!(!resp.ok);
        assert_eq!(resp.error.unwrap().code, "overloaded");
    }

    #[test]
    fn cancel_with_missing_id_still_replies() {
        let (dispatch_tx, _dispatch_rx) = channel::<SocketCommand>();
        let (writer_tx, writer_rx) = channel::<String>();
        let generation = Arc::new(AtomicU64::new(1));
        let job = mk_job(
            json!({ "invoke": "webview.eval", "params": {} }),
            dispatch_tx,
            writer_tx,
            generation,
            1,
        );
        Cancelable::cancel(job);
        let line = writer_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("writer_tx should receive overloaded response even on malformed input");
        let resp: Response = serde_json::from_str(&line).unwrap();
        assert_eq!(resp.id, "");
        assert_eq!(resp.error.unwrap().code, "overloaded");
    }

    #[test]
    fn forwarder_emits_bus_publish_for_native_event() {
        let bus = Arc::new(EventBus::new());
        let (writer_tx, writer_rx) = channel::<String>();
        let stop = Arc::new(AtomicBool::new(false));
        start_gui_event_forwarder(bus.clone(), writer_tx, stop.clone());
        // Give the per-kind subscriber threads a tick to enter
        // recv_timeout before we publish.
        thread::sleep(Duration::from_millis(50));
        bus.publish(BusEvent::new(
            "panel.focused",
            "test",
            json!({ "panel_id": "p1" }),
        ));
        let line = writer_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("forwarder should emit _bus.publish");
        let req: Request = serde_json::from_str(&line).unwrap();
        assert_eq!(req.method, "_bus.publish");
        assert_eq!(req.params["kind"], json!("panel.focused"));
        assert_eq!(req.params["source"], json!("test"));
        assert_eq!(req.params["payload"]["panel_id"], json!("p1"));
        stop.store(true, Ordering::SeqCst);
    }

    #[test]
    fn forwarder_skips_bridged_event() {
        let bus = Arc::new(EventBus::new());
        let (writer_tx, writer_rx) = channel::<String>();
        let stop = Arc::new(AtomicBool::new(false));
        start_gui_event_forwarder(bus.clone(), writer_tx, stop.clone());
        thread::sleep(Duration::from_millis(50));
        // Publish a BRIDGED event — forwarder must skip.
        bus.publish_bridged(BusEvent::new("panel.focused", "daemon-side", json!({})), 99);
        // 200ms backstop — no Request should appear.
        match writer_rx.recv_timeout(Duration::from_millis(200)) {
            Err(_) => {}
            Ok(line) => panic!("forwarder leaked bridged event: {line}"),
        }
        // Sanity: a native event DOES flow.
        bus.publish(BusEvent::new("panel.focused", "test", json!({})));
        writer_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("native event should flow");
        stop.store(true, Ordering::SeqCst);
    }

    #[test]
    fn forwarder_guard_flips_stop_on_drop() {
        let stop = Arc::new(AtomicBool::new(false));
        {
            let _g = ForwarderGuard(stop.clone());
            assert!(!stop.load(Ordering::SeqCst));
        }
        assert!(
            stop.load(Ordering::SeqCst),
            "ForwarderGuard must flip stop on drop"
        );
    }

    #[test]
    fn stale_generation_skips_dispatch() {
        // Job admitted under generation=1 but current generation=2 →
        // run() must skip handle_invoke (no command on dispatch_tx) and
        // write back an overloaded response.
        let (dispatch_tx, dispatch_rx) = channel::<SocketCommand>();
        let (writer_tx, writer_rx) = channel::<String>();
        let generation = Arc::new(AtomicU64::new(2));
        let job = mk_job(
            invoke_value("inv-stale", "tab.new"),
            dispatch_tx,
            writer_tx,
            generation,
            1,
        );
        Cancelable::run(job);
        match dispatch_rx.recv_timeout(Duration::from_millis(50)) {
            Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => {}
            Ok(_) => panic!("stale job must not dispatch any SocketCommand"),
        }
        let line = writer_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("stale job must still write back a response");
        let resp: Response = serde_json::from_str(&line).unwrap();
        assert_eq!(resp.id, "inv-stale");
        assert_eq!(resp.error.unwrap().code, "overloaded");
    }
}
