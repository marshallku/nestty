use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc;

use gtk4::ApplicationWindow;
use serde_json::json;

use nestty_core::action_registry::ActionRegistry;
use nestty_core::event_bus::Event as BusEvent;
use nestty_core::protocol::{Event, Request, Response};

use vte4::prelude::*;

use crate::background::BackgroundLayer;
use crate::panel::Panel;
use crate::tabs::TabManager;

const WALLPAPER_CACHE: &str = ".cache/terminal-wallpapers.txt";
const BG_MODE_FILE: &str = ".cache/nestty-bg-mode";
const BUS_SOURCE_NESTTY_LINUX: &str = "nestty-linux";

pub use nestty_daemon::socket::{EventBus, SocketCommand, new_event_bus};

pub fn broadcast(bus: &EventBus, event: &Event) {
    bus.publish(BusEvent::new(
        event.event_type.clone(),
        BUS_SOURCE_NESTTY_LINUX,
        event.data.clone(),
    ));
}

pub fn start_server(socket_path: &str, event_bus: EventBus) -> mpsc::Receiver<SocketCommand> {
    let (tx, rx) = mpsc::channel();

    // Reuse `nesttyd`'s hardened prep+bind: atomic 0700 parent with
    // ownership check (DirBuilder::mode only sets perms on fresh
    // creates — naive code lets an attacker-precreated lax dir slip
    // through), and chmod 0600 on the bound socket.
    let path = std::path::Path::new(socket_path);
    match nestty_daemon::socket::prepare_socket_path(path) {
        nestty_daemon::socket::SocketPrep::Fresh
        | nestty_daemon::socket::SocketPrep::StaleCleared => {}
        nestty_daemon::socket::SocketPrep::InUse => {
            eprintln!("[nestty] gui socket {socket_path} already in use by another instance");
            return rx;
        }
        nestty_daemon::socket::SocketPrep::NotSocket => {
            eprintln!("[nestty] gui socket path {socket_path} exists but is not a socket");
            return rx;
        }
        nestty_daemon::socket::SocketPrep::Error(msg) => {
            eprintln!("[nestty] gui socket prep failed: {msg}");
            return rx;
        }
    }

    let listener = match nestty_daemon::socket::bind_listener(path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[nestty] failed to bind socket at {socket_path}: {e}");
            return rx;
        }
    };

    eprintln!("[nestty] socket server listening at {socket_path}");

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let stream = match stream {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("[nestty] socket accept error: {e}");
                    continue;
                }
            };

            let tx = tx.clone();
            let event_bus = event_bus.clone();
            std::thread::spawn(move || {
                let reader = match stream.try_clone() {
                    Ok(s) => BufReader::new(s),
                    Err(e) => {
                        eprintln!("[nestty] socket clone error: {e}");
                        return;
                    }
                };
                let mut writer = stream;

                for line in reader.lines() {
                    let line = match line {
                        Ok(l) => l,
                        Err(_) => break,
                    };
                    if line.is_empty() {
                        continue;
                    }

                    let request: Request = match serde_json::from_str(&line) {
                        Ok(r) => r,
                        Err(e) => {
                            let err = Response::error(
                                String::new(),
                                "parse_error",
                                &format!("Invalid JSON: {e}"),
                            );
                            let _ = writeln!(writer, "{}", serde_json::to_string(&err).unwrap());
                            let _ = writer.flush();
                            continue;
                        }
                    };

                    // Handle event.subscribe in the socket thread (long-lived connection)
                    if request.method == "event.subscribe" {
                        let resp = Response::success(
                            request.id.clone(),
                            json!({ "status": "subscribed" }),
                        );
                        let _ = writeln!(writer, "{}", serde_json::to_string(&resp).unwrap());
                        let _ = writer.flush();

                        // Unbounded: external wire contract must not drop events on slow clients.
                        let rx = event_bus.subscribe_unbounded("*");
                        while let Some(ev) = rx.recv() {
                            let wire = Event::new(ev.kind, ev.payload).with_source(ev.source);
                            let json = match serde_json::to_string(&wire) {
                                Ok(j) => j,
                                Err(_) => continue,
                            };
                            if writeln!(writer, "{json}").is_err() {
                                break;
                            }
                            if writer.flush().is_err() {
                                break;
                            }
                        }
                        return;
                    }

                    let (reply_tx, reply_rx) = mpsc::channel();
                    let cmd = SocketCommand {
                        request,
                        reply: reply_tx,
                    };

                    if tx.send(cmd).is_err() {
                        break;
                    }

                    match reply_rx.recv() {
                        Ok(response) => {
                            let _ =
                                writeln!(writer, "{}", serde_json::to_string(&response).unwrap());
                            let _ = writer.flush();
                        }
                        Err(_) => break,
                    }
                }
            });
        }
    });

    rx
}

/// Dispatch consumes the SocketCommand so async handlers (webview.execute_js) can
/// capture the reply sender and respond from a callback.
pub fn dispatch(
    cmd: SocketCommand,
    mgr: &Rc<TabManager>,
    window: &ApplicationWindow,
    statusbar: &Rc<crate::statusbar::StatusBar>,
    background: &Rc<BackgroundLayer>,
    actions: &Arc<ActionRegistry>,
) {
    let req = &cmd.request;

    // Action Registry: try registered handlers first. New commands register
    // through the registry; legacy commands stay in the match below until
    // migrated. `try_dispatch` returns false on miss so we fall through.
    //
    // `try_dispatch` (vs the old `try_invoke`) is what keeps the GTK main
    // loop responsive: synchronous handlers (system.ping, context.snapshot,
    // etc.) still run inline so fast paths pay no scheduling overhead, but
    // blocking handlers — i.e. service-plugin RPC — are spawned onto a
    // worker thread by the registry. Either way `cmd.reply.send` lands
    // exactly once with the response, and the dispatcher returns
    // immediately for the blocking case so a slow plugin can't stall the
    // socket-server thread or the GTK timer that pumps it.
    let req_id_for_reply = req.id.clone();
    let reply = cmd.reply.clone();
    if actions.try_dispatch(
        &req.method,
        req.params.clone(),
        Box::new(move |result| {
            let resp = match result {
                Ok(value) => Response::success(req_id_for_reply, value),
                Err(err) => Response {
                    id: req_id_for_reply,
                    ok: false,
                    result: None,
                    error: Some(err),
                },
            };
            let _ = reply.send(resp);
        }),
    ) {
        return;
    }

    match req.method.as_str() {
        "background.set" => {
            let resp = handle_bg_set(req, background);
            let _ = cmd.reply.send(resp);
        }

        "background.clear" => {
            let resp = handle_bg_clear(req, background);
            let _ = cmd.reply.send(resp);
        }

        "background.next" => {
            let resp = handle_bg_next(req, background);
            let _ = cmd.reply.send(resp);
        }

        "background.toggle" => {
            let resp = handle_bg_toggle(req, background);
            let _ = cmd.reply.send(resp);
        }

        "background.set_tint" => {
            let resp = handle_bg_set_tint(req, background);
            let _ = cmd.reply.send(resp);
        }

        "tab.new" => {
            mgr.add_tab(window);
            let _ = cmd
                .reply
                .send(Response::success(req.id.clone(), json!({ "status": "ok" })));
        }

        "tab.close" => {
            mgr.close_focused(window);
            let _ = cmd
                .reply
                .send(Response::success(req.id.clone(), json!({ "status": "ok" })));
        }

        "tab.list" => {
            let count = mgr.tab_count();
            let current = mgr.current_tab();
            let _ = cmd.reply.send(Response::success(
                req.id.clone(),
                json!({ "count": count, "current": current }),
            ));
        }

        "tab.info" => {
            let _ = cmd
                .reply
                .send(Response::success(req.id.clone(), mgr.tab_info()));
        }

        "split.horizontal" => {
            mgr.split_focused(gtk4::Orientation::Horizontal, window);
            let _ = cmd
                .reply
                .send(Response::success(req.id.clone(), json!({ "status": "ok" })));
        }

        "split.vertical" => {
            mgr.split_focused(gtk4::Orientation::Vertical, window);
            let _ = cmd
                .reply
                .send(Response::success(req.id.clone(), json!({ "status": "ok" })));
        }

        "session.list" => {
            let _ = cmd.reply.send(Response::success(
                req.id.clone(),
                json!(mgr.all_panels_info()),
            ));
        }

        "session.info" => {
            let resp = match req.params.get("id").and_then(|v| v.as_str()) {
                Some(id) => match mgr.panel_info_by_id(id) {
                    Some(info) => Response::success(req.id.clone(), info),
                    None => Response::error(
                        req.id.clone(),
                        "not_found",
                        &format!("Panel not found: {id}"),
                    ),
                },
                None => Response::error(req.id.clone(), "invalid_params", "Missing 'id' param"),
            };
            let _ = cmd.reply.send(resp);
        }

        // -- WebView commands --
        "webview.open" => {
            let resp = handle_webview_open(req, mgr, window);
            let _ = cmd.reply.send(resp);
        }

        "webview.navigate" => {
            let resp = handle_webview_navigate(req, mgr);
            let _ = cmd.reply.send(resp);
        }

        "webview.back" => {
            let resp = with_webview_panel(req, mgr, |wv| {
                wv.go_back();
                Response::success(req.id.clone(), json!({ "status": "ok" }))
            });
            let _ = cmd.reply.send(resp);
        }

        "webview.forward" => {
            let resp = with_webview_panel(req, mgr, |wv| {
                wv.go_forward();
                Response::success(req.id.clone(), json!({ "status": "ok" }))
            });
            let _ = cmd.reply.send(resp);
        }

        "webview.reload" => {
            let resp = with_webview_panel(req, mgr, |wv| {
                wv.reload();
                Response::success(req.id.clone(), json!({ "status": "ok" }))
            });
            let _ = cmd.reply.send(resp);
        }

        "webview.execute_js" => {
            handle_webview_execute_js(cmd, mgr);
            // Response sent from callback
        }

        "webview.get_content" => {
            handle_webview_get_content(cmd, mgr);
            // Response sent from callback
        }

        "webview.screenshot" => {
            handle_webview_screenshot(cmd, mgr);
            // Response sent from callback
        }

        "webview.query" => {
            handle_webview_query(cmd, mgr);
            // Response sent from callback
        }

        "webview.query_all" => {
            handle_webview_query_all(cmd, mgr);
            // Response sent from callback
        }

        "webview.get_styles" => {
            handle_webview_get_styles(cmd, mgr);
            // Response sent from callback
        }

        "webview.click" => {
            handle_webview_click(cmd, mgr);
            // Response sent from callback
        }

        "webview.fill" => {
            handle_webview_fill(cmd, mgr);
            // Response sent from callback
        }

        "webview.scroll" => {
            handle_webview_scroll(cmd, mgr);
            // Response sent from callback
        }

        "webview.page_info" => {
            handle_webview_page_info(cmd, mgr);
            // Response sent from callback
        }

        "webview.devtools" => {
            let resp = handle_webview_devtools(req, mgr);
            let _ = cmd.reply.send(resp);
        }

        // -- Tab bar commands --
        "tabs.toggle_bar" => {
            let visible = mgr.toggle_tab_bar();
            let _ = cmd.reply.send(Response::success(
                req.id.clone(),
                json!({ "visible": visible }),
            ));
        }

        "tab.rename" => {
            let resp = match (
                req.params.get("id").and_then(|v| v.as_str()),
                req.params.get("title").and_then(|v| v.as_str()),
            ) {
                (Some(id), Some(title)) => {
                    if mgr.rename_tab(id, title) {
                        Response::success(req.id.clone(), json!({ "status": "ok" }))
                    } else {
                        Response::error(
                            req.id.clone(),
                            "not_found",
                            &format!("Panel not found: {id}"),
                        )
                    }
                }
                _ => Response::error(
                    req.id.clone(),
                    "invalid_params",
                    "Missing 'id' and/or 'title' param",
                ),
            };
            let _ = cmd.reply.send(resp);
        }

        // -- Terminal agent commands --
        "terminal.read" => {
            let resp = handle_terminal_read(req, mgr);
            let _ = cmd.reply.send(resp);
        }

        "terminal.state" => {
            let resp = handle_terminal_state(req, mgr);
            let _ = cmd.reply.send(resp);
        }

        "terminal.exec" => {
            let resp = handle_terminal_exec(req, mgr);
            let _ = cmd.reply.send(resp);
        }

        "terminal.feed" => {
            let resp = handle_terminal_feed(req, mgr);
            let _ = cmd.reply.send(resp);
        }

        "terminal.history" => {
            let resp = handle_terminal_history(req, mgr);
            let _ = cmd.reply.send(resp);
        }

        "terminal.context" => {
            let resp = handle_terminal_context(req, mgr);
            let _ = cmd.reply.send(resp);
        }

        "agent.approve" => {
            handle_agent_approve(cmd, window);
        }

        "claude.start" => {
            let resp = handle_claude_start(req, mgr, window);
            let _ = cmd.reply.send(resp);
        }

        "theme.list" => {
            let themes: Vec<&str> = nestty_core::theme::Theme::list().to_vec();
            let current = mgr.current_theme_name();
            let _ = cmd.reply.send(Response::success(
                req.id.clone(),
                json!({ "themes": themes, "current": current }),
            ));
        }

        "plugin.list" => {
            let plugins: Vec<serde_json::Value> = mgr
                .plugins()
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
                        })).collect::<Vec<_>>(),
                        "commands": m.commands.iter().map(|cd| json!({
                            "name": cd.name,
                            "description": cd.description,
                        })).collect::<Vec<_>>(),
                        "modules": m.modules.iter().map(|md| json!({
                            "name": md.name,
                            "exec": md.exec,
                            "interval": md.interval,
                            "position": md.position,
                        })).collect::<Vec<_>>(),
                    })
                })
                .collect();
            let _ = cmd.reply.send(Response::success(
                req.id.clone(),
                json!({ "plugins": plugins }),
            ));
        }

        "plugin.open" => {
            let resp = handle_plugin_open(req, mgr);
            let _ = cmd.reply.send(resp);
        }

        "statusbar.show" => {
            statusbar.set_visible(true);
            let _ = cmd.reply.send(Response::success(
                req.id.clone(),
                json!({ "visible": true }),
            ));
        }

        "statusbar.hide" => {
            statusbar.set_visible(false);
            let _ = cmd.reply.send(Response::success(
                req.id.clone(),
                json!({ "visible": false }),
            ));
        }

        "statusbar.toggle" => {
            let visible = statusbar.toggle();
            let _ = cmd.reply.send(Response::success(
                req.id.clone(),
                json!({ "visible": visible }),
            ));
        }

        _ => {
            // Unknown to GUI — proxy to the daemon on a worker thread.
            crate::daemon_forward::forward(cmd.request.clone(), cmd.reply.clone());
        }
    }
}

// -- Background helpers --

fn handle_bg_set(req: &Request, bg: &Rc<BackgroundLayer>) -> Response {
    let path = req.params.get("path").and_then(|v| v.as_str());
    match path {
        Some(p) => {
            let path = Path::new(p);
            if !path.exists() {
                return Response::error(
                    req.id.clone(),
                    "not_found",
                    &format!("File not found: {p}"),
                );
            }
            bg.set_image(path);
            Response::success(req.id.clone(), json!({ "status": "ok" }))
        }
        None => Response::error(req.id.clone(), "invalid_params", "Missing 'path' param"),
    }
}

fn handle_bg_clear(req: &Request, bg: &Rc<BackgroundLayer>) -> Response {
    bg.clear_image();
    Response::success(req.id.clone(), json!({ "status": "ok" }))
}

fn handle_bg_next(req: &Request, bg: &Rc<BackgroundLayer>) -> Response {
    if !is_bg_active() {
        return Response::success(
            req.id.clone(),
            json!({ "status": "ok", "mode": "deactive" }),
        );
    }
    match select_random_image() {
        Some(img) => {
            let path = Path::new(&img);
            if !path.exists() {
                return Response::error(
                    req.id.clone(),
                    "not_found",
                    &format!("File not found: {img}"),
                );
            }
            bg.set_image(path);
            Response::success(req.id.clone(), json!({ "status": "ok", "path": img }))
        }
        None => Response::error(req.id.clone(), "no_images", "No images in wallpaper cache"),
    }
}

fn handle_bg_toggle(req: &Request, bg: &Rc<BackgroundLayer>) -> Response {
    let now_active = toggle_bg_mode();
    if now_active {
        if let Some(img) = select_random_image() {
            bg.set_image(Path::new(&img));
        }
    } else {
        bg.clear_image();
    }
    let mode = if now_active { "active" } else { "deactive" };
    Response::success(req.id.clone(), json!({ "status": "ok", "mode": mode }))
}

fn handle_bg_set_tint(req: &Request, bg: &Rc<BackgroundLayer>) -> Response {
    let opacity = req.params.get("opacity").and_then(|v| v.as_f64());
    match opacity {
        Some(o) => {
            bg.set_tint(o);
            Response::success(req.id.clone(), json!({ "status": "ok" }))
        }
        None => Response::error(req.id.clone(), "invalid_params", "Missing 'opacity' param"),
    }
}

// -- WebView command helpers --

fn handle_webview_open(
    req: &Request,
    mgr: &Rc<TabManager>,
    window: &ApplicationWindow,
) -> Response {
    let url = match req.params.get("url").and_then(|v| v.as_str()) {
        Some(u) => u,
        None => return Response::error(req.id.clone(), "invalid_params", "Missing 'url' param"),
    };
    let mode = req
        .params
        .get("mode")
        .and_then(|v| v.as_str())
        .unwrap_or("tab");

    let panel_id = match mode {
        "split_h" => match mgr.split_focused_webview(url, gtk4::Orientation::Horizontal, window) {
            Some(id) => id,
            None => {
                return Response::error(req.id.clone(), "no_panel", "No focused panel to split");
            }
        },
        "split_v" => match mgr.split_focused_webview(url, gtk4::Orientation::Vertical, window) {
            Some(id) => id,
            None => {
                return Response::error(req.id.clone(), "no_panel", "No focused panel to split");
            }
        },
        _ => mgr.add_webview_tab(url, window),
    };

    Response::success(req.id.clone(), json!({ "panel_id": panel_id }))
}

fn handle_webview_navigate(req: &Request, mgr: &Rc<TabManager>) -> Response {
    let id = match req.params.get("id").and_then(|v| v.as_str()) {
        Some(id) => id,
        None => return Response::error(req.id.clone(), "invalid_params", "Missing 'id' param"),
    };
    let url = match req.params.get("url").and_then(|v| v.as_str()) {
        Some(u) => u,
        None => return Response::error(req.id.clone(), "invalid_params", "Missing 'url' param"),
    };

    match mgr.find_panel_by_id(id) {
        Some(panel) => match panel.as_webview() {
            Some(wv) => {
                wv.navigate(url);
                Response::success(req.id.clone(), json!({ "status": "ok" }))
            }
            None => Response::error(req.id.clone(), "wrong_panel_type", "Panel is not a webview"),
        },
        None => Response::error(
            req.id.clone(),
            "not_found",
            &format!("Panel not found: {id}"),
        ),
    }
}

fn with_webview_panel(
    req: &Request,
    mgr: &Rc<TabManager>,
    f: impl FnOnce(&crate::webview::WebViewPanel) -> Response,
) -> Response {
    let id = match req.params.get("id").and_then(|v| v.as_str()) {
        Some(id) => id,
        None => return Response::error(req.id.clone(), "invalid_params", "Missing 'id' param"),
    };
    match mgr.find_panel_by_id(id) {
        Some(panel) => match panel.as_webview() {
            Some(wv) => f(wv),
            None => Response::error(req.id.clone(), "wrong_panel_type", "Panel is not a webview"),
        },
        None => Response::error(
            req.id.clone(),
            "not_found",
            &format!("Panel not found: {id}"),
        ),
    }
}

fn handle_webview_execute_js(cmd: SocketCommand, mgr: &Rc<TabManager>) {
    let req = &cmd.request;
    let id = match req.params.get("id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => {
            let _ = cmd.reply.send(Response::error(
                req.id.clone(),
                "invalid_params",
                "Missing 'id' param",
            ));
            return;
        }
    };
    let code = match req.params.get("code").and_then(|v| v.as_str()) {
        Some(c) => c.to_string(),
        None => {
            let _ = cmd.reply.send(Response::error(
                req.id.clone(),
                "invalid_params",
                "Missing 'code' param",
            ));
            return;
        }
    };

    let panel = match mgr.find_panel_by_id(&id) {
        Some(p) => p,
        None => {
            let _ = cmd.reply.send(Response::error(
                req.id.clone(),
                "not_found",
                &format!("Panel not found: {id}"),
            ));
            return;
        }
    };
    let wv = match panel.as_webview() {
        Some(wv) => wv,
        None => {
            let _ = cmd.reply.send(Response::error(
                req.id.clone(),
                "wrong_panel_type",
                "Panel is not a webview",
            ));
            return;
        }
    };

    let req_id = req.id.clone();
    let reply = cmd.reply;
    wv.execute_js(&code, move |result| {
        let resp = match result {
            Ok(value) => Response::success(req_id, json!({ "result": value })),
            Err(e) => Response::error(req_id, "js_error", &e),
        };
        let _ = reply.send(resp);
    });
}

fn handle_webview_get_content(cmd: SocketCommand, mgr: &Rc<TabManager>) {
    let req = &cmd.request;
    let id = match req.params.get("id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => {
            let _ = cmd.reply.send(Response::error(
                req.id.clone(),
                "invalid_params",
                "Missing 'id' param",
            ));
            return;
        }
    };
    let format = req
        .params
        .get("format")
        .and_then(|v| v.as_str())
        .unwrap_or("text");
    let js_code = match format {
        "html" => "document.documentElement.outerHTML".to_string(),
        _ => "document.body.innerText".to_string(),
    };

    let panel = match mgr.find_panel_by_id(&id) {
        Some(p) => p,
        None => {
            let _ = cmd.reply.send(Response::error(
                req.id.clone(),
                "not_found",
                &format!("Panel not found: {id}"),
            ));
            return;
        }
    };
    let wv = match panel.as_webview() {
        Some(wv) => wv,
        None => {
            let _ = cmd.reply.send(Response::error(
                req.id.clone(),
                "wrong_panel_type",
                "Panel is not a webview",
            ));
            return;
        }
    };

    let req_id = req.id.clone();
    let reply = cmd.reply;
    wv.execute_js(&js_code, move |result| {
        let resp = match result {
            Ok(content) => Response::success(req_id, json!({ "content": content })),
            Err(e) => Response::error(req_id, "js_error", &e),
        };
        let _ = reply.send(resp);
    });
}

fn handle_webview_screenshot(cmd: SocketCommand, mgr: &Rc<TabManager>) {
    let req = &cmd.request;
    let id = match req.params.get("id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => {
            let _ = cmd.reply.send(Response::error(
                req.id.clone(),
                "invalid_params",
                "Missing 'id' param",
            ));
            return;
        }
    };

    let panel = match mgr.find_panel_by_id(&id) {
        Some(p) => p,
        None => {
            let _ = cmd.reply.send(Response::error(
                req.id.clone(),
                "not_found",
                &format!("Panel not found: {id}"),
            ));
            return;
        }
    };
    let wv = match panel.as_webview() {
        Some(wv) => wv,
        None => {
            let _ = cmd.reply.send(Response::error(
                req.id.clone(),
                "wrong_panel_type",
                "Panel is not a webview",
            ));
            return;
        }
    };

    let req_id = req.id.clone();
    let reply = cmd.reply;
    let path = req
        .params
        .get("path")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    wv.snapshot(move |result| {
        let resp = match result {
            Ok(base64_png) => {
                if let Some(path) = path {
                    // Decode and save to file
                    match gtk4::glib::base64_decode(&base64_png) {
                        data if !data.is_empty() => match std::fs::write(&path, &data) {
                            Ok(_) => Response::success(req_id, json!({ "path": path })),
                            Err(e) => Response::error(req_id, "io_error", &e.to_string()),
                        },
                        _ => Response::error(req_id, "decode_error", "Failed to decode PNG"),
                    }
                } else {
                    Response::success(req_id, json!({ "image": base64_png }))
                }
            }
            Err(e) => Response::error(req_id, "snapshot_error", &e),
        };
        let _ = reply.send(resp);
    });
}

/// Helper: run a JS snippet from webview::js module on a webview panel, send result via reply
fn run_js_command(cmd: SocketCommand, mgr: &Rc<TabManager>, js_code: String) {
    let req = &cmd.request;
    let id = match req.params.get("id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => {
            let _ = cmd.reply.send(Response::error(
                req.id.clone(),
                "invalid_params",
                "Missing 'id' param",
            ));
            return;
        }
    };

    let panel = match mgr.find_panel_by_id(&id) {
        Some(p) => p,
        None => {
            let _ = cmd.reply.send(Response::error(
                req.id.clone(),
                "not_found",
                &format!("Panel not found: {id}"),
            ));
            return;
        }
    };
    let wv = match panel.as_webview() {
        Some(wv) => wv,
        None => {
            let _ = cmd.reply.send(Response::error(
                req.id.clone(),
                "wrong_panel_type",
                "Panel is not a webview",
            ));
            return;
        }
    };

    let req_id = req.id.clone();
    let reply = cmd.reply;
    wv.execute_js(&js_code, move |result| {
        let resp = match result {
            Ok(json_str) => {
                // Parse the JSON string returned by JS to embed as structured data
                match serde_json::from_str::<serde_json::Value>(&json_str) {
                    Ok(val) => Response::success(req_id, json!({ "result": val })),
                    Err(_) => Response::success(req_id, json!({ "result": json_str })),
                }
            }
            Err(e) => Response::error(req_id, "js_error", &e),
        };
        let _ = reply.send(resp);
    });
}

fn handle_webview_query(cmd: SocketCommand, mgr: &Rc<TabManager>) {
    let selector = match cmd.request.params.get("selector").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => {
            let _ = cmd.reply.send(Response::error(
                cmd.request.id.clone(),
                "invalid_params",
                "Missing 'selector' param",
            ));
            return;
        }
    };
    let js = crate::webview::js::query_selector(&selector);
    run_js_command(cmd, mgr, js);
}

fn handle_webview_query_all(cmd: SocketCommand, mgr: &Rc<TabManager>) {
    let selector = match cmd.request.params.get("selector").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => {
            let _ = cmd.reply.send(Response::error(
                cmd.request.id.clone(),
                "invalid_params",
                "Missing 'selector' param",
            ));
            return;
        }
    };
    let limit = cmd
        .request
        .params
        .get("limit")
        .and_then(|v| v.as_u64())
        .unwrap_or(50) as u32;
    let js = crate::webview::js::query_selector_all(&selector, limit);
    run_js_command(cmd, mgr, js);
}

fn handle_webview_get_styles(cmd: SocketCommand, mgr: &Rc<TabManager>) {
    let selector = match cmd.request.params.get("selector").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => {
            let _ = cmd.reply.send(Response::error(
                cmd.request.id.clone(),
                "invalid_params",
                "Missing 'selector' param",
            ));
            return;
        }
    };
    let properties: Vec<&str> = cmd
        .request
        .params
        .get("properties")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    let js = crate::webview::js::get_styles(&selector, &properties);
    run_js_command(cmd, mgr, js);
}

fn handle_webview_click(cmd: SocketCommand, mgr: &Rc<TabManager>) {
    let selector = match cmd.request.params.get("selector").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => {
            let _ = cmd.reply.send(Response::error(
                cmd.request.id.clone(),
                "invalid_params",
                "Missing 'selector' param",
            ));
            return;
        }
    };
    let js = crate::webview::js::click(&selector);
    run_js_command(cmd, mgr, js);
}

fn handle_webview_fill(cmd: SocketCommand, mgr: &Rc<TabManager>) {
    let selector = match cmd.request.params.get("selector").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => {
            let _ = cmd.reply.send(Response::error(
                cmd.request.id.clone(),
                "invalid_params",
                "Missing 'selector' param",
            ));
            return;
        }
    };
    let value = match cmd.request.params.get("value").and_then(|v| v.as_str()) {
        Some(v) => v.to_string(),
        None => {
            let _ = cmd.reply.send(Response::error(
                cmd.request.id.clone(),
                "invalid_params",
                "Missing 'value' param",
            ));
            return;
        }
    };
    let js = crate::webview::js::fill(&selector, &value);
    run_js_command(cmd, mgr, js);
}

fn handle_webview_scroll(cmd: SocketCommand, mgr: &Rc<TabManager>) {
    let selector = cmd
        .request
        .params
        .get("selector")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let x = cmd
        .request
        .params
        .get("x")
        .and_then(|v| v.as_i64())
        .unwrap_or(0) as i32;
    let y = cmd
        .request
        .params
        .get("y")
        .and_then(|v| v.as_i64())
        .unwrap_or(0) as i32;
    let js = crate::webview::js::scroll(selector.as_deref(), x, y);
    run_js_command(cmd, mgr, js);
}

fn handle_webview_page_info(cmd: SocketCommand, mgr: &Rc<TabManager>) {
    let js = crate::webview::js::page_info();
    run_js_command(cmd, mgr, js);
}

fn handle_webview_devtools(req: &Request, mgr: &Rc<TabManager>) -> Response {
    use webkit6::prelude::WebViewExt;
    let action = req
        .params
        .get("action")
        .and_then(|v| v.as_str())
        .unwrap_or("show");
    with_webview_panel(req, mgr, |wv| {
        if let Some(inspector) = wv.webview.inspector() {
            match action {
                "show" => inspector.show(),
                "close" => inspector.close(),
                "attach" => inspector.attach(),
                "detach" => inspector.detach(),
                other => {
                    return Response::error(
                        req.id.clone(),
                        "invalid_params",
                        &format!("Unknown action: {other}. Use show/close/attach/detach"),
                    );
                }
            }
            Response::success(req.id.clone(), json!({ "status": "ok" }))
        } else {
            Response::error(req.id.clone(), "no_inspector", "Inspector not available")
        }
    })
}

// -- Utility functions --

fn home_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"))
}

fn select_random_image() -> Option<String> {
    let cache_path = home_dir().join(WALLPAPER_CACHE);
    let content = std::fs::read_to_string(cache_path).ok()?;
    let lines: Vec<&str> = content.lines().filter(|l| !l.is_empty()).collect();
    if lines.is_empty() {
        return None;
    }
    use std::time::SystemTime;
    let seed = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos() as usize;
    Some(lines[seed % lines.len()].to_string())
}

fn is_bg_active() -> bool {
    let mode_path = home_dir().join(BG_MODE_FILE);
    match std::fs::read_to_string(mode_path) {
        Ok(s) => s.trim() != "deactive",
        Err(_) => true,
    }
}

fn toggle_bg_mode() -> bool {
    let mode_path = home_dir().join(BG_MODE_FILE);
    let new_active = !is_bg_active();
    let mode = if new_active { "active" } else { "deactive" };
    let _ = std::fs::write(mode_path, mode);
    new_active
}

pub fn cleanup(socket_path: &str) {
    let _ = std::fs::remove_file(socket_path);
}

// -- Terminal agent command helpers --

fn resolve_terminal(
    req: &Request,
    mgr: &Rc<TabManager>,
) -> Result<Rc<crate::panel::PanelVariant>, Response> {
    // If id is provided, find that specific panel
    if let Some(id) = req.params.get("id").and_then(|v| v.as_str()) {
        let panel = mgr.find_panel_by_id(id).ok_or_else(|| {
            Response::error(
                req.id.clone(),
                "not_found",
                &format!("Panel not found: {id}"),
            )
        })?;
        if panel.as_terminal().is_none() {
            return Err(Response::error(
                req.id.clone(),
                "wrong_panel_type",
                "Panel is not a terminal",
            ));
        }
        return Ok(panel);
    }

    // No id: try active panel first, then fall back to any terminal panel
    if let Some(panel) = mgr.active_panel()
        && panel.as_terminal().is_some()
    {
        return Ok(panel);
    }

    // Active panel is not a terminal (e.g. plugin/webview) — find any terminal
    mgr.find_first_terminal()
        .ok_or_else(|| Response::error(req.id.clone(), "no_terminal", "No terminal panel found"))
}

fn handle_terminal_read(req: &Request, mgr: &Rc<TabManager>) -> Response {
    let panel = match resolve_terminal(req, mgr) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let term = panel.as_terminal().unwrap();

    // Optional range params
    let has_range = req.params.get("start_row").is_some();
    let text = if has_range {
        let start_row = req
            .params
            .get("start_row")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let start_col = req
            .params
            .get("start_col")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let end_row = req
            .params
            .get("end_row")
            .and_then(|v| v.as_i64())
            .unwrap_or_else(|| term.terminal.row_count() - 1);
        let end_col = req
            .params
            .get("end_col")
            .and_then(|v| v.as_i64())
            .unwrap_or_else(|| term.terminal.column_count() - 1);
        term.read_range(start_row, start_col, end_row, end_col)
    } else {
        term.read_screen()
    };

    let (col, row) = term.terminal.cursor_position();
    Response::success(
        req.id.clone(),
        json!({
            "text": text,
            "cursor": [row, col],
            "rows": term.terminal.row_count(),
            "cols": term.terminal.column_count(),
        }),
    )
}

fn handle_terminal_state(req: &Request, mgr: &Rc<TabManager>) -> Response {
    let panel = match resolve_terminal(req, mgr) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let term = panel.as_terminal().unwrap();
    Response::success(req.id.clone(), term.state())
}

fn handle_terminal_exec(req: &Request, mgr: &Rc<TabManager>) -> Response {
    let panel = match resolve_terminal(req, mgr) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let command = match req.params.get("command").and_then(|v| v.as_str()) {
        Some(c) => c,
        None => {
            return Response::error(req.id.clone(), "invalid_params", "Missing 'command' param");
        }
    };
    let term = panel.as_terminal().unwrap();
    // Send command + newline to execute
    term.feed_input(&format!("{command}\n"));
    Response::success(req.id.clone(), json!({ "status": "ok" }))
}

fn handle_terminal_feed(req: &Request, mgr: &Rc<TabManager>) -> Response {
    let panel = match resolve_terminal(req, mgr) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let text = match req.params.get("text").and_then(|v| v.as_str()) {
        Some(t) => t,
        None => return Response::error(req.id.clone(), "invalid_params", "Missing 'text' param"),
    };
    let term = panel.as_terminal().unwrap();
    // Send raw text (no newline appended)
    term.feed_input(text);
    Response::success(req.id.clone(), json!({ "status": "ok" }))
}

fn handle_terminal_history(req: &Request, mgr: &Rc<TabManager>) -> Response {
    let panel = match resolve_terminal(req, mgr) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let term = panel.as_terminal().unwrap();

    // Number of scrollback lines to read (default 100)
    let lines = req
        .params
        .get("lines")
        .and_then(|v| v.as_i64())
        .unwrap_or(100);

    let row_count = term.terminal.row_count();
    let col_count = term.terminal.column_count();

    // Negative rows access scrollback in VTE
    let start_row = -lines;
    let end_row = row_count - 1;

    let text = term.read_range(start_row, 0, end_row, col_count - 1);
    Response::success(
        req.id.clone(),
        json!({
            "text": text,
            "lines_requested": lines,
            "rows": row_count,
            "cols": col_count,
        }),
    )
}

fn handle_terminal_context(req: &Request, mgr: &Rc<TabManager>) -> Response {
    let panel = match resolve_terminal(req, mgr) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let term = panel.as_terminal().unwrap();

    let state = term.state();
    let screen = term.read_screen();

    // Recent scrollback (last 50 lines above visible area)
    let history_lines = req
        .params
        .get("history_lines")
        .and_then(|v| v.as_i64())
        .unwrap_or(50);
    let col_count = term.terminal.column_count();
    let history = term.read_range(-history_lines, 0, -1, col_count - 1);

    Response::success(
        req.id.clone(),
        json!({
            "state": state,
            "screen": screen,
            "history": history,
        }),
    )
}

/// Spawns a tab at `workspace_path` and runs `tmux new-session -A -s
/// <name> 'claude [...]'`. The `-A` attaches to an existing same-name
/// session (re-running on a worktree re-attaches live claude rather
/// than stacking duplicates) or creates one. `prompt` (mutually
/// exclusive with `resume_session`) is paste-seeded via
/// `spawn_claude_prompt_seeder` once two readiness checks confirm the
/// pane is claude — failures log but don't propagate.
fn handle_claude_start(
    req: &Request,
    mgr: &Rc<TabManager>,
    window: &ApplicationWindow,
) -> Response {
    let workspace_path_str = match req.params.get("workspace_path") {
        Some(serde_json::Value::String(s)) if !s.is_empty() => s.clone(),
        Some(_) => {
            return Response::error(
                req.id.clone(),
                "invalid_params",
                "'workspace_path' must be a non-empty string",
            );
        }
        None => {
            return Response::error(
                req.id.clone(),
                "invalid_params",
                "Missing 'workspace_path' param",
            );
        }
    };
    let raw_path = std::path::Path::new(&workspace_path_str);
    let canon = match std::fs::canonicalize(raw_path) {
        Ok(p) => p,
        Err(e) => {
            return Response::error(
                req.id.clone(),
                "not_found",
                &format!("workspace_path {workspace_path_str:?}: {e}"),
            );
        }
    };
    if !canon.is_dir() {
        return Response::error(
            req.id.clone(),
            "invalid_params",
            &format!("workspace_path {} is not a directory", canon.display()),
        );
    }

    // session_name: explicit or derived. tmux forbids `:` and `.`
    // in session names; we restrict further to ASCII alphanumeric
    // + `-_` so the value stays safe to embed in shell commands
    // without needing further escaping.
    let session_name = match req.params.get("session_name") {
        Some(serde_json::Value::String(s)) if !s.is_empty() => {
            if let Err(e) = validate_tmux_session_name(s) {
                return Response::error(
                    req.id.clone(),
                    "invalid_params",
                    &format!("session_name: {e}"),
                );
            }
            s.clone()
        }
        Some(serde_json::Value::Null) | None => derive_session_name(&canon),
        Some(other) => {
            return Response::error(
                req.id.clone(),
                "invalid_params",
                &format!("'session_name' must be a string, got {other}"),
            );
        }
    };

    // resume_session: optional claude session id. Validated
    // permissively (anything non-empty), single-quote-escaped
    // before embedding in the tmux command.
    let resume_session = match req.params.get("resume_session") {
        Some(serde_json::Value::String(s)) if !s.is_empty() => {
            for c in s.chars() {
                if c.is_control() || c == '\0' {
                    return Response::error(
                        req.id.clone(),
                        "invalid_params",
                        "resume_session contains control characters",
                    );
                }
            }
            Some(s.clone())
        }
        Some(serde_json::Value::Null) | None => None,
        Some(other) => {
            return Response::error(
                req.id.clone(),
                "invalid_params",
                &format!("'resume_session' must be a string, got {other}"),
            );
        }
    };

    // Prompt seeding via tmux paste-buffer. Caller can
    // pass a (possibly multi-line) prompt that we deliver to claude's
    // REPL once the session is alive. `prompt` and `resume_session`
    // are mutually exclusive — `--resume` restores an existing
    // conversation (its own context wins), seeding new text on top
    // would just confuse claude.
    let prompt_to_seed = match req.params.get("prompt") {
        Some(serde_json::Value::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(serde_json::Value::String(_)) | Some(serde_json::Value::Null) | None => None,
        Some(other) => {
            return Response::error(
                req.id.clone(),
                "invalid_params",
                &format!("'prompt' must be a string, got {other}"),
            );
        }
    };
    if prompt_to_seed.is_some() && resume_session.is_some() {
        return Response::error(
            req.id.clone(),
            "invalid_params",
            "'prompt' and 'resume_session' are mutually exclusive — \
             resume restores existing context; prompt seeds a new conversation",
        );
    }

    let claude_cmd = match &resume_session {
        Some(id) => format!("claude --resume {}", shell_single_quote(id)),
        None => "claude".to_string(),
    };
    let tmux_command = format!(
        "tmux new-session -A -s {} {}\n",
        shell_single_quote(&session_name),
        shell_single_quote(&claude_cmd),
    );

    // Pass the tmux command as `initial_input` so it's fed from
    // inside VTE's spawn_async success callback — eliminates
    // the race where a feed_input call after add_tab_with_cwd
    // could write to a PTY whose child shell isn't attached yet.
    let (panel, tab_index) =
        mgr.add_tab_with_cwd_and_initial_input(window, Some(&canon), Some(tmux_command));
    let panel_id = panel.id().to_string();
    if panel.as_terminal().is_none() {
        // add_tab_with_cwd_and_initial_input always returns a
        // terminal panel today. If that ever changes, we want to
        // know.
        return Response::error(
            req.id.clone(),
            "internal_error",
            "claude.start expected a terminal panel",
        );
    }

    // Background-seed the prompt once claude's REPL is up. Polling
    // `tmux capture-pane` for a readiness signal avoids a fixed
    // sleep that's either too short (race) or too long (wastes the
    // user's time before they can interact). Runs in its own thread
    // so claude.start returns to the caller immediately.
    if let Some(prompt) = prompt_to_seed {
        spawn_claude_prompt_seeder(session_name.clone(), prompt);
    }

    // Return both identifiers — `panel_id` is the UUID consumed by
    // session.info / session.list, `tab` is the numeric index
    // consumed by tab-bar UI. Same shape as the `tab.created`
    // event payload so caller code can be uniform.
    Response::success(
        req.id.clone(),
        json!({
            "panel_id": panel_id,
            "tab": tab_index,
            "tmux_session": session_name,
            "workspace_path": canon.display().to_string(),
        }),
    )
}

/// Last 1-2 path components joined by `-`, lowercased + sanitized.
/// Two components, not one, so `<root>/feature/foo` doesn't collapse
/// to `foo` and collide with siblings sharing a leaf name.
fn derive_session_name(path: &std::path::Path) -> String {
    let mut parts: Vec<String> = Vec::new();
    for comp in path.components().rev() {
        if let std::path::Component::Normal(seg) = comp
            && let Some(s) = seg.to_str()
        {
            parts.push(s.to_string());
            if parts.len() == 2 {
                break;
            }
        }
    }
    parts.reverse();
    let joined = parts.join("-");
    sanitize_session_name(&joined)
}

fn sanitize_session_name(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        let safe = if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
            c.to_ascii_lowercase()
        } else {
            '-'
        };
        out.push(safe);
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "claude".to_string()
    } else {
        trimmed
    }
}

fn validate_tmux_session_name(s: &str) -> Result<(), String> {
    if s.is_empty() {
        return Err("cannot be empty".to_string());
    }
    if s.starts_with('-') {
        return Err("cannot start with '-' (would look like a flag)".to_string());
    }
    for c in s.chars() {
        if !(c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            return Err(format!(
                "invalid character {c:?} (allowed: ASCII alphanumeric and - _)"
            ));
        }
    }
    Ok(())
}

/// Waits for claude's REPL in `session_name`'s tmux pane, then pastes
/// `prompt` via tmux load-buffer + paste-buffer + Enter.
///
/// **Trust boundary**: `tmux new-session -A` attaches if the session
/// already exists, ignoring our `claude` command. Pasting into a
/// pre-existing shell pane would EXECUTE the prompt as a shell command
/// and exfiltrate `linked_kb` content into history. The seeder pastes
/// only when BOTH gates pass:
/// 1. `pane_current_command` is claude (or the node binary it's built
///    on); shells (`zsh`/`bash`/`sh`/`fish`) hard-skip.
/// 2. `capture-pane` shows claude-specific markers (banner / `Try "`);
///    generic `> ` or box-drawing are insufficient since shells emit those.
///
/// Failures log; never propagated (claude.start already returned).
fn spawn_claude_prompt_seeder(session_name: String, prompt: String) {
    std::thread::spawn(move || {
        // Initial settle so capture-pane has SOMETHING to inspect.
        std::thread::sleep(std::time::Duration::from_millis(400));

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
        let mut saw_claude_marker = false;
        loop {
            if std::time::Instant::now() >= deadline {
                break;
            }
            if let Ok(out) = std::process::Command::new("tmux")
                .args(["capture-pane", "-p", "-t", &session_name])
                .output()
            {
                let s = String::from_utf8_lossy(&out.stdout);
                // Claude-specific markers only. Dropped the generic
                // "> " / box-drawing matchers — they fire on shells too.
                if s.contains("Anthropic")
                    || s.contains("Try \"")
                    || s.contains("claude --")
                    || s.to_ascii_lowercase().contains("claude code")
                {
                    saw_claude_marker = true;
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }

        // Cross-check: what's the current foreground command in the
        // pane? `pane_current_command` is the kernel's view, not
        // capture-pane's rendered text — survives even if claude's
        // banner has scrolled off.
        let current_cmd = std::process::Command::new("tmux")
            .args([
                "display-message",
                "-p",
                "-t",
                &session_name,
                "#{pane_current_command}",
            ])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_lowercase())
            .unwrap_or_default();
        let pane_is_claude = matches!(current_cmd.as_str(), "claude" | "node");

        if !saw_claude_marker || !pane_is_claude {
            eprintln!(
                "[claude.start] refusing to paste prompt into session {session_name:?}: \
                 saw_claude_marker={saw_claude_marker}, pane_current_command={current_cmd:?}. \
                 Pre-existing tmux session may be a shell or a non-claude process; user \
                 can paste the prompt manually."
            );
            return;
        }

        // Write the prompt to a temp file → load-buffer → paste-buffer.
        // Going through a buffer is what makes multi-line + special-char
        // payloads safe; send-keys -l would also work but each special
        // character needs care.
        let mut tmpf = match tempfile::NamedTempFile::new() {
            Ok(f) => f,
            Err(e) => {
                eprintln!("[claude.start] tempfile failed: {e}");
                return;
            }
        };
        if let Err(e) = std::io::Write::write_all(&mut tmpf, prompt.as_bytes()) {
            eprintln!("[claude.start] write prompt failed: {e}");
            return;
        }
        let buf_name = format!("nestty-claude-{}", uuid::Uuid::new_v4());
        let path_str = match tmpf.path().to_str() {
            Some(s) => s,
            None => {
                eprintln!("[claude.start] tempfile path is not UTF-8 — aborting seed");
                return;
            }
        };
        let load = std::process::Command::new("tmux")
            .args(["load-buffer", "-b", &buf_name, path_str])
            .status();
        if !matches!(load, Ok(s) if s.success()) {
            eprintln!("[claude.start] tmux load-buffer failed: {load:?}");
            return;
        }
        let paste = std::process::Command::new("tmux")
            .args(["paste-buffer", "-t", &session_name, "-b", &buf_name, "-d"])
            .status();
        if !matches!(paste, Ok(s) if s.success()) {
            eprintln!("[claude.start] tmux paste-buffer failed: {paste:?}");
            return;
        }
        // Submit. claude's REPL needs TWO Enters for long pastes:
        // bracketed-paste-collapse mode renders the input as
        // `[Pasted text #1 +N lines]` once a paste exceeds claude's
        // inline threshold, so the first Enter commits/expands the
        // paste placeholder and the second sends it to the model.
        // Short pastes that don't collapse get submitted by the
        // first Enter; the second hits an already-empty input and
        // claude no-ops on it. Two-Enter covers both cases without
        // inspecting claude's UI state.
        let _ = std::process::Command::new("tmux")
            .args(["send-keys", "-t", &session_name, "Enter"])
            .status();
        std::thread::sleep(std::time::Duration::from_millis(200));
        let _ = std::process::Command::new("tmux")
            .args(["send-keys", "-t", &session_name, "Enter"])
            .status();
    });
}

/// POSIX-safe single-quote escape: wrap in `'…'`, replace any
/// embedded `'` with `'\''`. Result is a single shell token.
fn shell_single_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

fn handle_agent_approve(cmd: SocketCommand, window: &ApplicationWindow) {
    let req = &cmd.request;
    let title = req
        .params
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("Agent Action");
    let message = match req.params.get("message").and_then(|v| v.as_str()) {
        Some(m) => m,
        None => {
            let _ = cmd.reply.send(Response::error(
                req.id.clone(),
                "invalid_params",
                "Missing 'message' param",
            ));
            return;
        }
    };
    let actions = req
        .params
        .get("actions")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect::<Vec<_>>()
        })
        .unwrap_or_else(|| vec!["Approve".to_string(), "Deny".to_string()]);

    let dialog = gtk4::AlertDialog::builder()
        .modal(true)
        .message(title)
        .detail(message)
        .buttons(actions.iter().map(|s| s.as_str()).collect::<Vec<_>>())
        .default_button(0)
        .cancel_button(actions.len() as i32 - 1)
        .build();

    let req_id = req.id.clone();
    let actions_clone = actions.clone();
    dialog.choose(Some(window), gtk4::gio::Cancellable::NONE, move |result| {
        let resp = match result {
            Ok(idx) => {
                let action = actions_clone
                    .get(idx as usize)
                    .cloned()
                    .unwrap_or_else(|| "unknown".to_string());
                let approved = idx == 0;
                Response::success(
                    req_id.clone(),
                    json!({
                        "approved": approved,
                        "action": action,
                        "index": idx,
                    }),
                )
            }
            Err(_) => Response::success(
                req_id.clone(),
                json!({
                    "approved": false,
                    "action": "cancelled",
                    "index": -1,
                }),
            ),
        };
        let _ = cmd.reply.send(resp);
    });
}

// -- Plugin command helpers --

fn handle_plugin_open(req: &Request, mgr: &Rc<TabManager>) -> Response {
    let plugin_name = match req.params.get("plugin").and_then(|v| v.as_str()) {
        Some(n) => n,
        None => return Response::error(req.id.clone(), "invalid_params", "Missing 'plugin' param"),
    };
    let panel_name = req
        .params
        .get("panel")
        .and_then(|v| v.as_str())
        .unwrap_or("main");

    // `resolve_by_name` honors the duplicate-winner rule the daemon
    // uses for `plugin.<name>.<cmd>` / `_module.run`, so panel open
    // targets the same manifest as command dispatch and module exec.
    let plugin = match nestty_core::plugin::resolve_by_name(mgr.plugins(), plugin_name) {
        Some(p) => p.clone(),
        None => {
            return Response::error(
                req.id.clone(),
                "not_found",
                &format!("Plugin not found: {plugin_name}"),
            );
        }
    };

    match mgr.add_plugin_tab(&plugin, panel_name) {
        Some(panel_id) => Response::success(req.id.clone(), json!({ "panel_id": panel_id })),
        None => Response::error(
            req.id.clone(),
            "not_found",
            &format!("Panel '{panel_name}' not found in plugin '{plugin_name}'"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_tmux_session_name_accepts_normal() {
        for s in ["main", "feat-foo", "user_team", "release-1-2", "abc123"] {
            validate_tmux_session_name(s).unwrap_or_else(|e| panic!("rejected {s:?}: {e}"));
        }
    }

    #[test]
    fn validate_tmux_session_name_rejects_bad() {
        for s in [
            "",
            "-flag",
            "feat:foo",
            "feat.foo",
            "has space",
            "foo/bar",
            "x\0y",
        ] {
            assert!(
                validate_tmux_session_name(s).is_err(),
                "should reject {s:?}"
            );
        }
    }

    #[test]
    fn sanitize_session_name_lowercases_and_replaces_bad_chars() {
        assert_eq!(sanitize_session_name("Feature/Foo"), "feature-foo");
        assert_eq!(sanitize_session_name("v1.2.3"), "v1-2-3");
        assert_eq!(sanitize_session_name("ALL-CAPS"), "all-caps");
        assert_eq!(sanitize_session_name("---trim---"), "trim");
        // Empty-after-sanitize falls back to a non-empty default
        // so callers always have a usable session name.
        assert_eq!(sanitize_session_name("///"), "claude");
    }

    #[test]
    fn derive_session_name_uses_last_two_path_components() {
        let p = std::path::Path::new("/home/user/dev/nestty-worktrees/feature/foo");
        assert_eq!(derive_session_name(p), "feature-foo");
        let p2 = std::path::Path::new("/home/user/dev/myrepo");
        // Only one path-after-root component left? "dev-myrepo".
        assert_eq!(derive_session_name(p2), "dev-myrepo");
    }

    #[test]
    fn derive_session_name_sanitizes_uppercase_and_dots() {
        let p = std::path::Path::new("/x/Feature.Branch/PROJ-456");
        assert_eq!(derive_session_name(p), "feature-branch-proj-456");
    }

    #[test]
    fn shell_single_quote_round_trips() {
        // No special chars: just wrapping.
        assert_eq!(shell_single_quote("simple"), "'simple'");
        // Embedded single quote uses '\''.
        assert_eq!(shell_single_quote("foo'bar"), "'foo'\\''bar'");
        // Empty string is still quoted (a valid empty shell arg).
        assert_eq!(shell_single_quote(""), "''");
        // Whitespace and special chars passed through inside the
        // quotes — the whole point of single-quoting is shell
        // doesn't interpret them.
        assert_eq!(shell_single_quote("a; b $C"), "'a; b $C'");
    }
}
