use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use gtk4::prelude::*;
use gtk4::{Application, ApplicationWindow, gdk, gio, glib};
use serde_json::json;
use webkit6::prelude::WebViewExt;

use nestty_core::action_registry::{ActionRegistry, internal_error, invalid_params};
use nestty_core::config::NesttyConfig;
use nestty_core::context::ContextService;
use nestty_core::event_bus::{Event as BusEvent, EventBus as CoreEventBus, EventReceiver};
use nestty_core::trigger::{Trigger, TriggerEngine, TriggerSink, covering_patterns};

use nestty_daemon::trigger_sink::LiveTriggerSink;

use crate::background::BackgroundLayer;

use crate::panel::Panel;
use crate::socket;
use crate::statusbar::StatusBar;
use crate::tabs::TabManager;

pub struct NesttyWindow {
    pub window: ApplicationWindow,
    pub tab_manager: Rc<TabManager>,
    #[allow(dead_code)]
    statusbar: Rc<StatusBar>,
    #[allow(dead_code)]
    background: Rc<BackgroundLayer>,
    /// Hidden 1×1 zero-opacity WebView that loads a stub at window
    /// construction time, warming WebKit's auxiliary services
    /// (xdg-desktop-portal activation, bubblewrap sandbox, document-portal
    /// D-Bus handshake) before the user opens their first plugin panel.
    /// Without this, the first plugin panel's `load_uri()` hangs in
    /// WebProcess startup on cold boot. Held to keep `WebContext` alive.
    #[allow(dead_code)]
    prewarm_webview: webkit6::WebView,
}

impl NesttyWindow {
    pub fn new(app: &Application, config: &NesttyConfig) -> Self {
        let window = ApplicationWindow::builder()
            .application(app)
            .title("nestty")
            .default_width(1200)
            .default_height(800)
            .build();

        // Cold-boot WebKit prewarm: kicked off ASAP (before plugins
        // load, before tabs build) so WebKit's host-side daemons —
        // xdg-desktop-portal lazy systemd activation, the bubblewrap
        // sandbox setup path, the session-bus connection to the
        // portal, and the document-portal handshake that mediates
        // file:// access from a sandboxed WebProcess — all finish
        // handshaking while nestty does the rest of its init in
        // parallel. The first plugin panel the user opens then
        // finds those daemons already running and avoids the
        // cold-boot hang where `load_uri()` sits silent until a
        // second nestty process happens to wake them. See commit
        // 78ebdb1 for the diagnostic instrumentation, and the
        // `prewarm_webview` field on `NesttyWindow` for the lifetime
        // contract.
        //
        // The prewarm uses its own `WebContext` so it doesn't share
        // a sandbox / cookie jar with any plugin panel. Note that
        // WebKitGTK process state (NetworkProcess, WebProcess) is
        // per-WebContext, so each plugin panel still cold-spawns
        // its own; what this prewarm warms is the SHARED host-side
        // state (portal daemons, D-Bus name ownership, kernel
        // bubblewrap setup) which is what's suspected of cold-boot
        // hang. Loading a `file://` stub (not `about:blank`) and
        // adding `/tmp` to the sandbox exercises the same code path
        // plugin panels later traverse, including the portal-mediated
        // file read that is the most likely hang site.
        //
        // Cost: one extra WebProcess + 100-byte temp file for the
        // window's lifetime. Temp file is per-pid so concurrent nestty
        // instances don't collide; cleaned up on window destroy.
        let prewarm_path =
            std::env::temp_dir().join(format!("nestty-prewarm-{}.html", std::process::id()));
        // Surface the write failure rather than swallow it — if the
        // temp file isn't there, the file:// load fails silently and
        // the cold-boot hypothesis can't be evaluated next reproduction.
        if let Err(e) = std::fs::write(&prewarm_path, b"<!doctype html><title>p</title>") {
            eprintln!(
                "[nestty] prewarm: failed to write {}: {e} — cold-boot \
                 prewarm degraded to file-not-found",
                prewarm_path.display()
            );
        }
        let prewarm_webview = {
            let ctx = webkit6::WebContext::new();
            ctx.add_path_to_sandbox(std::env::temp_dir(), false);
            let wv = webkit6::WebView::builder().web_context(&ctx).build();
            wv.set_size_request(1, 1);
            wv.set_opacity(0.0);
            wv.set_can_focus(false);
            wv.set_can_target(false);
            wv.load_uri(&format!("file://{}", prewarm_path.display()));
            wv
        };

        // Window-level fallback bg: visible whenever no `BackgroundLayer`
        // image is loaded. The provider handle is reused on hot reload so a
        // theme switch updates this color in lockstep with the rest of the
        // UI; without that the fallback bg sticks at the old theme color
        // because terminals are permanently transparent now.
        let window_css = gtk4::CssProvider::new();
        let theme = nestty_core::theme::Theme::by_name(&config.theme.name).unwrap_or_default();
        update_window_bg_css(&window_css, &theme);
        gtk4::style_context_add_provider_for_display(
            &gtk4::gdk::Display::default().unwrap(),
            &window_css,
            gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );

        let event_bus = socket::new_event_bus();

        // Context Service: live snapshot of "what the user is currently doing."
        // Pumped from the GTK timer below; exposed via the `context.snapshot` action.
        let context = Arc::new(ContextService::new());

        // `with_completion_bus` auto-publishes `<name>.completed/.failed`
        // so chained triggers compose without per-plugin emit code.
        let actions = Arc::new(ActionRegistry::with_completion_bus(event_bus.clone()));
        // High-frequency built-ins are registered "silent" so their
        // completions don't dwarf real workflow events on the bus.
        // system.ping fires from heartbeat probes; context.snapshot
        // fires from anything that wants to see the active panel
        // (potentially every keystroke in agent flows).
        actions.register_silent("system.ping", |_| Ok(json!({ "status": "ok" })));
        actions.register("system.log", |params| {
            // Built-in observable action — useful as a trigger sink. Falls
            // back to the full params JSON when no `message` field is present
            // so the user always sees something even with a misshapen call.
            let msg = params
                .get("message")
                .and_then(|v| v.as_str())
                .map(String::from)
                .unwrap_or_else(|| params.to_string());
            eprintln!("[system.log] {msg}");
            Ok(json!({}))
        });
        {
            let ctx = context.clone();
            actions.register_silent("context.snapshot", move |_| {
                serde_json::to_value(ctx.snapshot())
                    .map_err(|e| internal_error(format!("snapshot serialization failed: {e}")))
            });
        }
        {
            // `event.history` — recent bus events snapshot. Callers
            // pass optional `since_ms` (epoch millis cutoff) and
            // `kind` (event-kind glob, same matcher as
            // `event.subscribe`). The bus owns the ring buffer
            // (default cap 500); this action just projects + filters.
            // Registered silent because its own `.completed` event on
            // the bus would otherwise inflate every history call's
            // result on the next call.
            let bus = event_bus.clone();
            actions.register_silent("event.history", move |params| {
                // Reject malformed types loudly instead of treating
                // them as absent — `event.subscribe` also rejects
                // bad `patterns`, so the surface stays consistent.
                if let Some(v) = params.get("since_ms")
                    && !v.is_null()
                    && v.as_u64().is_none()
                {
                    return Err(invalid_params(
                        "event.history `since_ms` must be a non-negative integer",
                    ));
                }
                if let Some(v) = params.get("kind")
                    && !v.is_null()
                    && v.as_str().is_none()
                {
                    return Err(invalid_params("event.history `kind` must be a string glob"));
                }
                let since_ms = params.get("since_ms").and_then(|v| v.as_u64());
                let kind = params
                    .get("kind")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let events = bus.history(since_ms, kind.as_deref());
                let arr: Vec<serde_json::Value> = events
                    .into_iter()
                    .map(|e| {
                        // Wire shape matches `event.subscribe`'s
                        // `{type, data, source}` (see `Event` in
                        // `nestty-core/src/protocol.rs`) with an added
                        // `timestamp_ms` so the catch-up consumer can
                        // render a per-event clock. Older `event.
                        // subscribe` consumers ignore the extra field.
                        json!({
                            "type": e.kind,
                            "data": e.payload,
                            "source": e.source,
                            "timestamp_ms": e.timestamp_ms,
                        })
                    })
                    .collect();
                Ok(json!({ "events": arr }))
            });
        }
        // `notify.show` — mirror of the daemon-side registration so
        // the GUI's in-process trigger engine reaches the same action
        // when `host_triggers = OFF`. Blocking-silent: subprocess off
        // the dispatch thread, no `.completed` spam.
        {
            let notifier = nestty_core::notifier::platform_notifier();
            actions.register_blocking_silent("notify.show", move |params| {
                let title = match params.get("title").and_then(|v| v.as_str()) {
                    Some(s) if !s.is_empty() => s.to_string(),
                    _ => {
                        return Err(invalid_params(
                            "notify.show requires non-empty `title` string",
                        ));
                    }
                };
                let body = match params.get("body") {
                    Some(v) if v.is_null() => String::new(),
                    None => String::new(),
                    Some(v) => match v.as_str() {
                        Some(s) => s.to_string(),
                        None => {
                            return Err(invalid_params("notify.show `body` must be a string"));
                        }
                    },
                };
                let level: nestty_core::notifier::Level = match params.get("level") {
                    None | Some(serde_json::Value::Null) => nestty_core::notifier::Level::default(),
                    Some(v) => serde_json::from_value(v.clone()).map_err(|_| {
                        invalid_params("notify.show `level` must be one of `info`, `warn`, `error`")
                    })?,
                };
                match &notifier {
                    Some(n) => match n.notify(&title, &body, level) {
                        Ok(()) => Ok(json!({ "shown": true })),
                        Err(e) => {
                            log::warn!("notify.show failed: {e}");
                            Err(internal_error(format!("notify subprocess: {e}")))
                        }
                    },
                    None => {
                        log::debug!("notify.show: no Notifier for this platform; dropping");
                        Ok(json!({ "shown": false, "reason": "no_notifier" }))
                    }
                }
            });
        }

        // Dispatch channel: TabManager owns the original sender (used by
        // plugin JS bridges). Trigger sink gets a clone so trigger-fired
        // legacy actions (anything not in the registry yet) can fall through
        // to `socket::dispatch` via the same path.
        let (dispatch_tx, plugin_dispatch_rx) = std::sync::mpsc::channel();

        // `LiveTriggerSink` tries the in-process registry first and
        // falls through to `socket::dispatch` (fire-and-forget caveat
        // documented on the sink). PumpState bundles every per-tick
        // drain target so the timer and the config
        // hot-reload callback can both invoke the same `pump_all` sequence.
        // Exact-match context subscriptions (not `*` and not glob) so high-
        // frequency unrelated kinds cannot flood the bounded ctx queues.
        let sink: Arc<dyn TriggerSink> =
            Arc::new(LiveTriggerSink::new(actions.clone(), dispatch_tx.clone()));
        // Engine needs the bus to publish synthesized
        // `<trigger_name>.awaited` events; without `with_publish_bus`
        // the `await` primitive silently no-ops.
        let triggers = Arc::new(TriggerEngine::with_publish_bus(sink, event_bus.clone()));
        triggers.set_triggers(config.triggers.clone());
        let pump_state = Rc::new(RefCell::new(PumpState {
            ctx_focused: event_bus.subscribe("panel.focused"),
            ctx_exited: event_bus.subscribe("panel.exited"),
            ctx_cwd: event_bus.subscribe("terminal.cwd_changed"),
            trigger_subs: TriggerSubscriptions::new(),
        }));
        pump_state
            .borrow_mut()
            .reconcile_triggers(&event_bus, &config.triggers);
        eprintln!(
            "[nestty] trigger engine: {} configured ({:?}) | {} bus pattern(s) subscribed",
            triggers.count(),
            triggers.names(),
            pump_state.borrow().trigger_subs_len()
        );

        // Cut-over state — daemon advertises via `gui.register` whether
        // IT owns trigger dispatch. While the flag is `true`, this GUI's
        // local engine is cleared and `watch_config` defers triggers to
        // the daemon. The flag flips back to `false` whenever
        // `gui_client::run` exits (disconnect / error), restoring local
        // authority from the cached triggers — that's the
        // disconnect-resilience contract for headless-daemon offline.
        let host_triggers_active = Arc::new(AtomicBool::new(false));
        let cached_triggers = Arc::new(Mutex::new(config.triggers.clone()));
        let (ht_tx, ht_rx) = channel::<bool>();

        // Plugin discovery — sorted so plugin-by-name resolvers (panel
        // open, statusbar module timer) agree with the daemon's
        // `resolve_by_name` on duplicate manifest names.
        let plugins = nestty_core::plugin::discover_sorted_plugins();
        for p in &plugins {
            eprintln!(
                "[nestty] plugin loaded: {} v{}",
                p.manifest.plugin.name, p.manifest.plugin.version
            );
        }

        // Plugin lifecycle belongs to nesttyd; the manifests discovered
        // above stay for panel/statusbar/command rendering only.

        // Per-instance under `runtime_dir()` (0700) — fs-perms gate connect(2).
        let socket_path = nestty_core::paths::gui_socket_path(std::process::id())
            .to_string_lossy()
            .into_owned();
        let socket_rx = socket::start_server(&socket_path, event_bus.clone());

        // Always attempt to attach to nesttyd. Daemon-absent is fine:
        // gui_client's reconnect_loop quietly polls (1→30s backoff) and
        // the GUI runs fully through its own in-process supervisor/socket
        // in the meantime. When a daemon eventually shows up, the GUI
        // auto-registers and starts serving daemon→GUI Invokes too.
        // event_bus is passed so daemon-published events (chained
        // workflow completions, plugin events) bridge into the GUI bus
        // and feed the local TriggerEngine.
        crate::gui_client::spawn(dispatch_tx.clone(), event_bus.clone(), ht_tx);

        let tab_manager = TabManager::new(
            config,
            &window,
            event_bus.clone(),
            plugins.clone(),
            dispatch_tx,
            actions.clone(),
        );

        // Try restoring from the previous session, else seed a single
        // default tab. We must do this before any window-close save can
        // overwrite a persisted session with an empty snapshot.
        if let Some(session) = crate::session::load() {
            tab_manager.restore_session(&window, &session);
        } else {
            tab_manager.add_tab(&window);
        }

        // Save the session on window close. Best-effort: any I/O error
        // is logged but does not block exit.
        let tm_for_save = tab_manager.clone();
        window.connect_close_request(move |_| {
            let session = tm_for_save.snapshot_session();
            if session.tabs.is_empty() {
                // The visible state has nothing terminal to restore.
                // Leaving an older session.json on disk would surface
                // stale tabs on next launch — delete instead.
                crate::session::clear();
            } else {
                crate::session::save(&session);
            }
            gtk4::glib::Propagation::Proceed
        });

        // Status bar
        let statusbar = Rc::new(StatusBar::new(config, &plugins));

        // Window-level background layer. Sits as the base child of an
        // Overlay so every tab/panel above it (notebook, statusbar,
        // terminals, plugin webviews) renders on top of the same image
        // — no more "background only on the first terminal" surprise.
        let background = BackgroundLayer::new(config);

        // Layout: vertical box with notebook + statusbar
        let layout = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        layout.set_hexpand(true);
        layout.set_vexpand(true);
        if config.statusbar.position == "top" {
            layout.append(&statusbar.container);
            layout.append(&tab_manager.notebook);
        } else {
            layout.append(&tab_manager.notebook);
            layout.append(&statusbar.container);
        }

        let root_overlay = gtk4::Overlay::new();
        root_overlay.set_child(Some(&background.bg_picture));
        root_overlay.add_overlay(&background.tint_overlay);
        root_overlay.add_overlay(&layout);
        // Park the prewarm WebView in the overlay tree so it
        // realizes alongside the rest of the UI and its WebProcess
        // actually spawns instead of sitting idle on an unparented
        // widget. `can_target=false` + opacity 0 + 1×1 size keeps
        // it inert and invisible.
        root_overlay.add_overlay(&prewarm_webview);
        // Use the layout's natural size to drive the overlay so the bg
        // image stretches/letterboxes against the real UI footprint
        // rather than the picture's intrinsic size.
        root_overlay.set_measure_overlay(&layout, true);
        window.set_child(Some(&root_overlay));

        // Config hot-reload (also reloads triggers atomically)
        watch_config(
            &tab_manager,
            &statusbar,
            &background,
            &window_css,
            &plugins,
            &triggers,
            &event_bus,
            &pump_state,
            &context,
            &host_triggers_active,
            &cached_triggers,
        );

        let mgr = tab_manager.clone();
        let win = window.clone();
        let sb = statusbar.clone();
        let bg = background.clone();
        let act = actions.clone();
        let ctx_pump = context.clone();
        let trg_pump = triggers.clone();
        let pump_state_timer = pump_state.clone();
        let bus_for_timer = event_bus.clone();
        let ht_active = host_triggers_active.clone();
        let cached_for_timer = cached_triggers.clone();
        let last_applied_ht = Cell::new(false);
        glib::timeout_add_local(Duration::from_millis(50), move || {
            // Cut-over consumer runs BEFORE pump_all: when the daemon's
            // register ack lands with `host_triggers=true`, clear the
            // local engine on the SAME tick so no events dispatch
            // locally before the cut-over is applied.
            //
            // Edge-triggered: every `gui.register` ack sends a value
            // (true OR false), and disconnect drop-guards always send
            // `false`. Most are no-ops vs the current state — applying
            // them anyway would clobber preflight/pending await state
            // on every normal reconnect.
            if let Some(value) = drain_to_latest(&ht_rx)
                && value != last_applied_ht.get()
            {
                ht_active.store(value, Ordering::SeqCst);
                last_applied_ht.set(value);
                let cached = cached_for_timer.lock().unwrap().clone();
                apply_host_triggers(
                    value,
                    &trg_pump,
                    &mut pump_state_timer.borrow_mut(),
                    &bus_for_timer,
                    &cached,
                );
                log::info!(
                    "host_triggers cut-over: active={value} | engine={} | bus pattern(s)={}",
                    trg_pump.count(),
                    pump_state_timer.borrow().trigger_subs_len()
                );
            }

            // `pump_all` drains context-driving events first, then trigger
            // events with per-event context snapshot. Single helper used by
            // both this timer and the hot-reload callback so semantics match.
            pump_state_timer.borrow().pump_all(&ctx_pump, &trg_pump);

            // Drop expired pending awaits and emit `<trigger>.awaited`
            // with null payload for `on_timeout = "fire_with_default"`.
            // Cheap on every tick: pending list is typically tiny.
            trg_pump.sweep_pending_awaits();

            // Process socket commands. After each, drain ONLY context
            // receivers (not trigger queues — those are handled at the start
            // and end of tick). A dispatched command can publish events
            // (`tab.new` → `panel.focused`) and the very next command in the
            // same batch (e.g. `context.snapshot`) must see those events
            // applied to ContextService.
            while let Ok(cmd) = socket_rx.try_recv() {
                socket::dispatch(cmd, &mgr, &win, &sb, &bg, &act, &bus_for_timer);
                pump_state_timer.borrow().drain_context_only(&ctx_pump);
            }
            while let Ok(cmd) = plugin_dispatch_rx.try_recv() {
                socket::dispatch(cmd, &mgr, &win, &sb, &bg, &act, &bus_for_timer);
                pump_state_timer.borrow().drain_context_only(&ctx_pump);
            }
            glib::ControlFlow::Continue
        });

        // `window.restored` event publication.
        //
        // Watch the toplevel's `GDK_TOPLEVEL_STATE_SUSPENDED` bit and
        // publish `window.restored` on the 1→0 transition — i.e. the
        // compositor told us we were no longer visible/active and now
        // we are again. The Hyprland WebKit-panel freeze (see
        // `docs/troubleshooting.md`) leaves rendering stuck after this
        // transition; user wires a trigger on this event to run
        // two separate `hyprctl dispatch resizewindowpixel
        // '<delta>,class:com.marshall.nestty'` calls (the empirically
        // verified cure form for the multi-window-on-workspace case;
        // see `examples/triggers/hyprland-webkit-fix.toml`). Generic
        // hook — nestty core has no Hyprland knowledge, user supplies
        // the cure command via `[[triggers]]`.
        //
        // Detection only — the `system.spawn` action that runs the
        // cure lives in WR-2 (`trigger_sink::handle_system_spawn`).
        // SUSPENDED toggling on Hyprland is verified end-to-end (see
        // troubleshooting.md and the cure log captured during WR-2
        // testing).
        //
        // Connected at `realize` because `Window::surface()` returns
        // None until the window is realized; connecting at construct
        // time silently no-ops.
        let bus_for_state = event_bus.clone();
        let last_suspended = Rc::new(Cell::new(false));
        // Initialize so the FIRST 1→0 transition isn't suppressed by
        // the leading-edge debounce. 1s back-dating is plenty: the
        // user can't trigger a workspace cycle in the first 200ms of
        // startup anyway.
        let last_fired = Rc::new(Cell::new(Instant::now() - Duration::from_secs(1)));
        window.connect_realize(move |w| {
            let Some(surface) = w.surface() else {
                eprintln!("[nestty] window.restored: realize fired with no surface — disabled");
                return;
            };
            let Ok(toplevel) = surface.downcast::<gdk::Toplevel>() else {
                eprintln!(
                    "[nestty] window.restored: surface is not a gdk::Toplevel — disabled (compositor/backend mismatch?)"
                );
                return;
            };
            // Seed `last_suspended` with the toplevel's CURRENT state so
            // a window that's already SUSPENDED at attach time (e.g.,
            // nestty launched on a non-current Hyprland workspace) still
            // emits `window.restored` on the first 1→0 transition. The
            // default `false` would suppress it because `prev == current`
            // == false even though the surface had been suspended all
            // along.
            last_suspended
                .set(toplevel.state().contains(gdk::ToplevelState::SUSPENDED));
            let bus = bus_for_state.clone();
            let last = last_suspended.clone();
            let last_fire = last_fired.clone();
            toplevel.connect_state_notify(move |tl| {
                let suspended = tl.state().contains(gdk::ToplevelState::SUSPENDED);
                let prev = last.replace(suspended);
                // Only the 1→0 transition fires `window.restored`.
                if !prev || suspended {
                    return;
                }
                // 1→0 transition. Apply 200ms leading-edge debounce
                // so quick ping-pong (alt-tab back and forth) doesn't
                // spam triggers — once we've fired, suppress until
                // the window goes back into stable use.
                let now = Instant::now();
                if now.duration_since(last_fire.get()) < Duration::from_millis(200) {
                    return;
                }
                last_fire.set(now);
                eprintln!("[nestty] window.restored: SUSPENDED bit cleared, publishing event");
                bus.publish(BusEvent::new(
                    "window.restored",
                    "nestty.window",
                    json!({}),
                ));
            });
        });

        // Cleanup socket and tear down service plugins on window
        // destroy. `shutdown_all` sends the documented `shutdown`
        // notification, drops the writer-channel sender (so child
        // stdin closes on EOF), and SIGKILLs anything still alive
        // after a brief grace window — children don't outlive the GUI.
        let socket_path_cleanup = socket_path.clone();
        let prewarm_path_cleanup = prewarm_path.clone();
        window.connect_destroy(move |_| {
            socket::cleanup(&socket_path_cleanup);
            let _ = std::fs::remove_file(&prewarm_path_cleanup);
        });

        Self {
            window,
            tab_manager,
            statusbar,
            background,
            prewarm_webview,
        }
    }

    pub fn present(&self) {
        self.window.present();
        // Focus the terminal after the window is mapped
        let mgr = self.tab_manager.clone();
        glib::idle_add_local_once(move || {
            if let Some(panel) = mgr.active_panel() {
                panel.grab_focus();
            }
        });
    }
}

fn update_window_bg_css(provider: &gtk4::CssProvider, theme: &nestty_core::theme::Theme) {
    provider.load_from_string(&format!(
        "window {{ background-color: {}; }}",
        theme.background
    ));
}

#[allow(clippy::too_many_arguments)]
fn watch_config(
    tab_manager: &Rc<TabManager>,
    statusbar: &Rc<StatusBar>,
    background: &Rc<BackgroundLayer>,
    window_css: &gtk4::CssProvider,
    plugins: &[nestty_core::plugin::LoadedPlugin],
    triggers: &Arc<TriggerEngine>,
    event_bus: &Arc<CoreEventBus>,
    pump_state: &Rc<RefCell<PumpState>>,
    context: &Arc<ContextService>,
    host_triggers_active: &Arc<AtomicBool>,
    cached_triggers: &Arc<Mutex<Vec<Trigger>>>,
) {
    let config_path = NesttyConfig::config_path();
    let file = gio::File::for_path(&config_path);

    let monitor = match file.monitor_file(gio::FileMonitorFlags::NONE, gio::Cancellable::NONE) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("[nestty] failed to watch config: {}", e);
            return;
        }
    };

    let mgr = tab_manager.clone();
    let sb = statusbar.clone();
    let bg = background.clone();
    let win_css = window_css.clone();
    let pl = plugins.to_vec();
    let trg = triggers.clone();
    let bus = event_bus.clone();
    let ps = pump_state.clone();
    let ctx = context.clone();
    let ht_active = host_triggers_active.clone();
    let cached = cached_triggers.clone();
    monitor.connect_changed(move |_, _, _, event| {
        if !matches!(
            event,
            gio::FileMonitorEvent::Changed | gio::FileMonitorEvent::Created
        ) {
            return;
        }

        let config = match NesttyConfig::load() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[nestty] config reload error: {}", e);
                return;
            }
        };

        eprintln!("[nestty] config reloaded");
        let theme = nestty_core::theme::Theme::by_name(&config.theme.name).unwrap_or_default();
        update_window_bg_css(&win_css, &theme);
        mgr.update_config(&config);
        sb.reload(&config, &pl);
        bg.apply_config(&config);

        // Always refresh the cache from disk — even when daemon is
        // authoritative — so a later disconnect restores the freshest
        // set, not the startup snapshot.
        *cached.lock().unwrap() = config.triggers.clone();

        if ht_active.load(Ordering::SeqCst) {
            eprintln!("[nestty] triggers reload deferred to daemon (host_triggers_active)");
            return;
        }

        trg.set_triggers(config.triggers.clone());
        // Pump_all (context drain → trigger drain) on the OLD receiver set
        // before reconcile drops any: a pattern-narrowing reload (e.g.
        // `terminal.*` → `terminal.cwd_changed`) would otherwise discard
        // pending events the new trigger set would have matched, and stale
        // `{context.*}` interpolation would feed any in-flight dispatches.
        ps.borrow().pump_all(&ctx, &trg);
        ps.borrow_mut().reconcile_triggers(&bus, &config.triggers);
        eprintln!(
            "[nestty] triggers reloaded: {} active ({:?}) | {} bus pattern(s) subscribed",
            trg.count(),
            trg.names(),
            ps.borrow().trigger_subs_len()
        );
    });

    std::mem::forget(monitor);
}

/// Consume every queued value on `rx` and return the most recent.
/// Used by the 50ms cut-over consumer to collapse a fast flap
/// (`true→false→true` within one tick) to the final state.
pub fn drain_to_latest<T>(rx: &Receiver<T>) -> Option<T> {
    let mut latest = None;
    while let Ok(v) = rx.try_recv() {
        latest = Some(v);
    }
    latest
}

/// Apply a host_triggers state change to the GUI's local
/// TriggerEngine + PumpState. `true` clears the engine (daemon
/// authoritative); `false` restores from the cached config triggers.
/// Both branches reconcile pump subscriptions symmetrically.
pub fn apply_host_triggers(
    value: bool,
    engine: &TriggerEngine,
    pump_state: &mut PumpState,
    bus: &Arc<CoreEventBus>,
    cached: &[Trigger],
) {
    if value {
        engine.set_triggers(Vec::new());
        pump_state.reconcile_triggers(bus, &[]);
    } else {
        engine.set_triggers(cached.to_vec());
        pump_state.reconcile_triggers(bus, cached);
    }
}

/// Per-tick drain targets (context receivers + trigger subscriptions) so
/// the GTK timer and hot-reload callback share one `pump_all` sequence
/// with identical context-then-triggers ordering.
pub struct PumpState {
    ctx_focused: EventReceiver,
    ctx_exited: EventReceiver,
    ctx_cwd: EventReceiver,
    trigger_subs: TriggerSubscriptions,
}

impl PumpState {
    /// Order across the three is commutative for ContextService's state.
    pub fn drain_context_only(&self, ctx: &ContextService) {
        while let Some(event) = self.ctx_focused.try_recv() {
            ctx.apply_event(&event);
        }
        while let Some(event) = self.ctx_exited.try_recv() {
            ctx.apply_event(&event);
        }
        while let Some(event) = self.ctx_cwd.try_recv() {
            ctx.apply_event(&event);
        }
    }

    /// Context first (so `{context.*}` interpolation is fresh), then
    /// triggers. One context snapshot per dispatched event.
    pub fn pump_all(&self, ctx: &ContextService, engine: &TriggerEngine) {
        self.drain_context_only(ctx);
        self.trigger_subs.drain_into(|event| {
            let snap = ctx.snapshot();
            engine.dispatch(&event, Some(&snap));
        });
    }

    pub fn reconcile_triggers(&mut self, bus: &Arc<CoreEventBus>, triggers: &[Trigger]) {
        self.trigger_subs.reconcile(bus, triggers);
    }

    pub fn trigger_subs_len(&self) -> usize {
        self.trigger_subs.len()
    }
}

/// One bus receiver per unique `event_kind` pattern across active triggers.
/// Reconciled at startup and on hot reload: still-needed patterns keep their
/// existing receivers (pending events preserved across unrelated reloads),
/// removed patterns drop (queues GC'd lazily on next publish).
pub struct TriggerSubscriptions {
    receivers: HashMap<String, EventReceiver>,
}

impl TriggerSubscriptions {
    pub fn new() -> Self {
        Self {
            receivers: HashMap::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.receivers.len()
    }

    /// Reduces requested patterns via `covering_patterns` first so overlap
    /// (`*` plus `panel.focused`) collapses to a single broader receiver —
    /// otherwise duplicate delivery would double-fire side effects.
    /// New patterns get fresh `subscribe_unbounded` receivers.
    pub fn reconcile(&mut self, bus: &Arc<CoreEventBus>, triggers: &[Trigger]) {
        // The pump drives three flavors of event into the engine:
        // (1) `when.event_kind`, (2) `await.event_kind`, and
        // (3) `<action>.completed`/`.failed` for await-bearing triggers.
        // Missing subscription #3 degrades await to "registers preflight,
        // never promotes" — see docs/workflow-runtime.md "Async correlation".
        let mut raw: Vec<String> = Vec::with_capacity(triggers.len() * 3);
        for t in triggers {
            raw.push(t.when.event_kind.clone());
            if let Some(aw) = &t.r#await {
                raw.push(aw.event_kind.clone());
                raw.push(format!("{}.completed", t.action));
                raw.push(format!("{}.failed", t.action));
            }
        }
        let needed: std::collections::HashSet<String> =
            covering_patterns(raw).into_iter().collect();
        self.receivers.retain(|pattern, _| needed.contains(pattern));
        for pattern in needed {
            self.receivers
                .entry(pattern.clone())
                .or_insert_with(|| bus.subscribe_unbounded(pattern.clone()));
        }
    }

    /// Drain every receiver fully. Ordering invariant: registry-sourced
    /// `<X>.completed`/`.failed` events must run BEFORE `await.event_kind`
    /// in the same tick — otherwise a same-tick awaited reply hits a
    /// preflight that hasn't promoted yet and the workflow times out.
    /// HashMap iteration order is unspecified, so drain → Vec → stable-sort.
    pub fn drain_into<F: FnMut(BusEvent)>(&self, mut f: F) {
        let mut events: Vec<BusEvent> = Vec::new();
        for rx in self.receivers.values() {
            while let Some(event) = rx.try_recv() {
                events.push(event);
            }
        }
        // Trust-bound priority: only `nestty.action`-sourced
        // `.completed`/`.failed` (the same gate `try_promote_or_drop_preflight`
        // enforces) gets priority 0. A plugin-published event whose kind
        // happens to end with `.completed` (Todo's watcher emits one) stays
        // at normal priority so it can't reorder ahead of other follow-ups.
        events.sort_by_key(|e| {
            let is_completion_fan_out = e.source
                == nestty_core::action_registry::COMPLETION_EVENT_SOURCE
                && (e.kind.ends_with(".completed") || e.kind.ends_with(".failed"));
            if is_completion_fan_out { 0u8 } else { 1u8 }
        });
        for event in events {
            f(event);
        }
    }
}

impl Default for TriggerSubscriptions {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nestty_core::action_registry::ActionRegistry;
    use nestty_core::trigger::{SecurityBlock, TriggerEngine, TriggerSink, WhenSpec};
    use serde_json::Value;
    use std::sync::mpsc::channel;

    fn mk_trigger(name: &str, kind: &str, action: &str) -> Trigger {
        Trigger {
            name: name.into(),
            when: WhenSpec {
                event_kind: kind.into(),
                payload_match: serde_json::Map::new(),
            },
            action: action.into(),
            params: Value::Null,
            condition: None,
            r#await: None,
            security: SecurityBlock::default(),
        }
    }

    fn mk_engine() -> (Arc<CoreEventBus>, Arc<TriggerEngine>) {
        let bus = Arc::new(CoreEventBus::new());
        let registry = Arc::new(ActionRegistry::new());
        let sink: Arc<dyn TriggerSink> = registry as Arc<dyn TriggerSink>;
        let engine = Arc::new(TriggerEngine::with_publish_bus(sink, bus.clone()));
        (bus, engine)
    }

    #[test]
    fn drain_to_latest_empty_returns_none() {
        let (_tx, rx) = channel::<bool>();
        assert!(drain_to_latest(&rx).is_none());
    }

    #[test]
    fn drain_to_latest_returns_last_value() {
        let (tx, rx) = channel::<bool>();
        tx.send(true).unwrap();
        tx.send(false).unwrap();
        tx.send(true).unwrap();
        assert_eq!(drain_to_latest(&rx), Some(true));
        assert!(drain_to_latest(&rx).is_none());
    }

    #[test]
    fn apply_host_triggers_true_clears_engine_and_subs() {
        let (bus, engine) = mk_engine();
        let cached = vec![mk_trigger("t1", "panel.focused", "system.log")];
        engine.set_triggers(cached.clone());
        let mut pump = PumpState {
            ctx_focused: bus.subscribe("panel.focused"),
            ctx_exited: bus.subscribe("panel.exited"),
            ctx_cwd: bus.subscribe("terminal.cwd_changed"),
            trigger_subs: TriggerSubscriptions::new(),
        };
        pump.reconcile_triggers(&bus, &cached);
        assert!(pump.trigger_subs_len() > 0);

        apply_host_triggers(true, &engine, &mut pump, &bus, &cached);
        assert_eq!(engine.count(), 0);
        assert_eq!(pump.trigger_subs_len(), 0);
    }

    #[test]
    fn apply_host_triggers_false_restores_from_cache() {
        let (bus, engine) = mk_engine();
        let cached = vec![
            mk_trigger("a", "panel.focused", "system.log"),
            mk_trigger("b", "terminal.cwd_changed", "system.log"),
        ];
        let mut pump = PumpState {
            ctx_focused: bus.subscribe("panel.focused"),
            ctx_exited: bus.subscribe("panel.exited"),
            ctx_cwd: bus.subscribe("terminal.cwd_changed"),
            trigger_subs: TriggerSubscriptions::new(),
        };
        // Start in cut-over (daemon-authoritative) state.
        apply_host_triggers(true, &engine, &mut pump, &bus, &cached);
        assert_eq!(engine.count(), 0);

        apply_host_triggers(false, &engine, &mut pump, &bus, &cached);
        assert_eq!(engine.count(), 2);
        assert_eq!(pump.trigger_subs_len(), 2);
    }
}
