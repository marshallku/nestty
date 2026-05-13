//! Daemon-client mode for nestty-linux: connects to `nesttyd`, advertises
//! GUI capabilities via `gui.register`, and forwards inbound `Invoke`
//! requests through the existing dispatch pump.
//!
//! Off by default. Enable with `NESTTY_DAEMON_CLIENT=1`.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::thread;
use std::time::Duration;

use nestty_core::protocol::{Invoke, Request, Response};
use nestty_core::thread_pool::Cancelable;
use serde_json::Value;

use crate::socket::SocketCommand;

const PROTOCOL_VERSION: u32 = nestty_core::protocol::PROTOCOL_VERSION;

/// Daemon→GUI invoke workers are mostly blocked on the GTK reply channel
/// (`recv_timeout(125s)`), not on CPU. The size cap exists to stop a
/// runaway plugin/webview burst from accumulating an unbounded number of
/// idle waiters, not to maximize throughput. 8/32 is large enough for
/// realistic concurrent webview + plugin command load and small enough
/// to make pathological bursts surface as `overloaded` quickly.
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

/// Reconnect backoff bounds.
const BACKOFF_INITIAL: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Spawns the reconnect loop. Each iteration connects, registers, and
/// handles Invokes until the daemon connection drops; then waits a
/// backoff interval and retries. Backoff resets to 1s after a successful
/// register.
pub fn spawn(dispatch_tx: Sender<SocketCommand>) {
    thread::Builder::new()
        .name("nestty-gui-client".into())
        .spawn(move || {
            // Process-lifetime pool. Sharing across reconnects keeps
            // ThreadPool::Drop (which drains the queue and joins workers,
            // each potentially holding a 125s reply timeout) off the
            // reconnect path.
            //
            // `generation` invalidates queued jobs across a disconnect:
            // each `run()` invocation bumps it, and every `GuiInvokeJob`
            // captures the generation it was admitted under. A worker
            // that picks up a stale job (admitted while connection N was
            // alive, executed after connection N+1 took over) writes an
            // overloaded response back through its now-dead writer_tx
            // instead of dispatching the side-effecting method.
            let pool = nestty_core::thread_pool::ThreadPool::new(POOL_WORKERS, POOL_QUEUE);
            let generation = Arc::new(AtomicU64::new(0));
            reconnect_loop(dispatch_tx, pool, generation);
        })
        .expect("spawn nestty-gui-client");
}

fn reconnect_loop(
    dispatch_tx: Sender<SocketCommand>,
    pool: std::sync::Arc<nestty_core::thread_pool::ThreadPool>,
    generation: Arc<AtomicU64>,
) {
    let mut backoff = BACKOFF_INITIAL;
    loop {
        let socket_path = nestty_core::paths::socket_path();
        let registered = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        match run(
            &socket_path.to_string_lossy(),
            dispatch_tx.clone(),
            pool.clone(),
            generation.clone(),
            registered.clone(),
        ) {
            Ok(()) => eprintln!("[nestty] gui_client disconnected from daemon"),
            Err(e) => eprintln!("[nestty] gui_client error: {e}"),
        }
        eprintln!("[nestty] gui_client reconnect in {:?}", backoff);
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
) -> Result<(), String> {
    // Bump generation on entry AND on every exit (Drop guard). The exit
    // bump is the critical one: between EOF and the next `run()` call
    // the reconnect_loop sleeps for backoff, and without immediate
    // invalidation a queued stale job could still pass the generation
    // check during that window and side-effect through the GTK pump.
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
    await_register_ack(&mut reader, &register_id)?;
    registered.store(true, std::sync::atomic::Ordering::SeqCst);

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
            // _ping handled inline on the reader thread — cheap echo,
            // no GTK round-trip, no pool slot consumed. Keeps heartbeat
            // responsive under burst load that saturates the pool.
            if method == "_ping" {
                if let Err(e) = handle_ping(value, &writer_tx) {
                    log::warn!("[nestty] gui_client ping reply: {e}");
                }
                continue;
            }
            // Slow path: bounded pool. Saturation → cancel inline,
            // which writes back an `overloaded` Response so daemon's
            // GuiClient::invoke unblocks immediately.
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
            // Response to our gui.register or a heartbeat reply later.
            log::debug!("[nestty] gui_client response: {value}");
        } else if value.get("type").is_some() {
            // Auto-subscribed Event stream — not consumed yet.
            log::trace!("[nestty] gui_client event: {value}");
        } else {
            log::debug!("[nestty] gui_client ignoring: {line:.200}");
        }
    }
    Ok(())
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

fn await_register_ack(reader: &mut BufReader<UnixStream>, register_id: &str) -> Result<(), String> {
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
    log::info!(
        "[nestty] gui_client registered with nesttyd: {}",
        resp.result.unwrap_or_default()
    );
    Ok(())
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
    /// Bumped by every `run()` invocation. A job with `admitted_gen`
    /// less than the current value comes from a dropped connection and
    /// must NOT dispatch — `tab.new`, `terminal.exec`, `webview.*`, etc.
    /// would otherwise side-effect after the daemon already failed the
    /// pending invoke via `fail_all_pending`.
    generation: Arc<AtomicU64>,
    admitted_gen: u64,
}

impl GuiInvokeJob {
    fn write_overloaded(value: &Value, writer_tx: &Sender<String>) {
        // Best-effort id extraction. If parsing fails the daemon falls
        // back to its 5/120s gui_timeout — not ideal, but cancel/stale
        // paths must never panic.
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
        // Generation check: if the connection we were admitted under is
        // gone, treat as stale and write overloaded back through the
        // (now-dead) writer_tx. Critical — without this, queued jobs
        // would side-effect through the GTK dispatcher after disconnect.
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
    // Larger than the daemon's max per-method timeout (120s) so the
    // daemon's outer timeout fires first; this is the wedged-pump safety net.
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
