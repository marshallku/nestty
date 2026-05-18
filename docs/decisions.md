# Technical Decisions

## 1. Tauri v2 Abandoned → Native Platform UIs

**Problem:** Tauri IPC introduced noticeable input latency in the terminal. Every keypress went through JS → Tauri invoke → Rust → PTY, and output went PTY → Rust → Tauri event → JS → xterm.js. The round-trip was perceptible.

**Decision:** Switched to platform-native UIs with a shared Rust core:

- Linux: GTK4 + VTE4 (VTE handles PTY internally, zero IPC overhead)
- macOS: Swift/AppKit (SwiftTerm or Ghostty embedding, TBD)

**Tradeoff:** More code per platform, but terminal responsiveness is non-negotiable.

## 2. VTE Handles PTY on Linux

**Rationale:** VTE has its own optimized PTY management. Using `portable-pty` alongside VTE would mean double PTY handling. Let VTE do what it does best.

**Consequence:** `nestty-core/pty.rs` and `state.rs` were removed — both platforms handle PTY natively (VTE on Linux, SwiftTerm on macOS).

## 3. Unix Socket for IPC (D-Bus Removed)

**Original:** D-Bus was used for background control (SetBackground, ClearBackground, SetTint). A Unix socket server was later added for richer control (50+ commands).

**Decision:** Removed D-Bus entirely. The socket API is the sole IPC mechanism on all platforms. D-Bus only had 3 background methods, all duplicated by socket commands. No external consumers existed.

**GTK thread safety:** Socket server threads use `mpsc::channel` + `glib::timeout_add_local(50ms)` polling to safely dispatch commands on the GTK main thread.

## 4. Window-level Background Compositing

**Stack:** `bg_picture` (window-overlay child) → `tint_overlay` (overlay) → layout box (overlay) → notebook → panels (terminal / plugin webview / external webview, all transparent)

`BackgroundLayer` (in `nestty-linux/src/background.rs`) lives at the window level. The root `gtk4::Overlay` has the `bg_picture` as its base child and adds the tint plus the actual UI layout as overlays. Every panel sits over this single image so the background is consistent across tabs (terminals, todo, etc.) instead of being painted per-terminal.

**Critical details to keep the layer visible through every panel:**

1. VTE: `terminal.set_clear_background(false)` + bg color `RGBA(0,0,0,0)` (always — not conditional on whether an image is loaded). VTE otherwise paints its own opaque background and hides the layer.
2. WebKit (plugin + external webview): `webview.set_background_color(RGBA(0,0,0,0))` so blank pages don't paint opaque white over the layer.
3. CSS: `notebook header`, `notebook > stack`, `.nestty-statusbar` are all `background-color: transparent`. Plugin user CSS sets `html, body { background-color: transparent }`.
4. `bg_picture` and `tint_overlay` use `set_can_target(false)` so input events pass through to the panels above.

When no image is configured, `bg_picture` is hidden and the window's CSS `window { background-color: <theme.background> }` provides the solid theme color underneath.

**Why moved here from per-`TerminalPanel`:** the previous design only rendered the image inside the first terminal's overlay, so opening a non-terminal panel (todo plugin, webview) hid the image entirely, and split terminals each rendered their own copy with independent positioning. Window-level layer fixes both.

## 5. Binary Names: nestty + nestctl

**Problem:** Both nestty-linux and nestty-cli had `[[bin]] name = "nestty"`, causing Cargo output filename collision.

**Decision:** CLI binary renamed to `nestctl` (follows kubectl, sysctl naming convention).

## 6. Theme System

**Design:** Themes are defined as `Theme` structs in `nestty-core/theme.rs` with semantic color slots (foreground, background, 16-color palette, surface/overlay/accent UI colors). 10 built-in themes are embedded. All UI components (terminal, tab bar, search bar, webview URL bar, window background) use theme colors via CSS generation functions.

**Config:** `[theme] name = "catppuccin-mocha"` selects the active theme. Hot-reloads on config change.

**Built-in themes:** catppuccin-mocha (default), catppuccin-latte, catppuccin-frappe, catppuccin-macchiato, dracula, nord, tokyo-night, gruvbox-dark, one-dark, solarized-dark.

## 7. cmux V2 Protocol for Socket Communication

**Format:** Newline-delimited JSON with UUID request IDs.
**Reference:** ~/dev/cmux/ (Marshall's macOS terminal multiplexer)

This protocol is used by both nestctl and the nestty-linux socket server. D-Bus remains for system integration (background control), while the socket API handles all rich control (tabs, splits, webview, terminal agent, approval workflow).

## 8. Forced Dark Theme

**Problem:** When VTE background is transparent (for bg images) and no image is loaded yet, the system GTK theme shows through. On light themes this makes the terminal white.

**Fix:** Force dark theme in `app.rs` via `set_gtk_application_prefer_dark_theme(true)` + CSS `window { background-color: #1e1e2e; }`.

## 9. Rust Edition 2024

Using the latest Rust edition. No compatibility concerns since the project is new.

## 10. In-Terminal Search via VTE Regex

**Problem:** Popular terminals (Ghostty, Kitty) lack built-in Ctrl+F search, requiring piping through external tools.

**Decision:** Implemented search using VTE4's built-in `search_set_regex` / `search_find_next` / `search_find_previous` with PCRE2 regex. Search bar is a `gtk4::Box` overlay at the bottom of each terminal panel.

**UX details:**

- Search text is preserved when closing, but fully selected on reopen (type to replace, Enter to reuse)
- `glib::idle_add_local_once` is needed for `select_region` — GTK4 Entry ignores selection before focus is fully settled

## 12. macOS Split Panes: NSSplitViewDelegate for Equal Initial Sizing

**Problem:** Getting `NSSplitView` to start at exactly 50/50 on initial layout is unreliable. Two failed approaches:

1. `DispatchQueue.main.asyncAfter(deadline: .now() + 0.05)` + `setPosition`: timing is unpredictable. The timer may fire before layout resolves (position ignored) or after a subsequent split has already started.
2. `override func layout()` + `setPosition`: NSSplitView calls `resizeSubviews` (which commits subview frames) before calling `layout()`. By the time `layout()` fires, the wrong frames are already in place.

**Decision:** Use `NSSplitViewDelegate.splitView(_:resizeSubviewsWithOldSize:)`. This delegate method is the exact hook where NSSplitView asks "how should I size my subviews?" — set frames directly here. An `initialSizeSet` flag ensures this only runs once per `EqualSplitView` instance; subsequent calls fall back to `adjustSubviews()` to allow user dragging.

## 13. macOS Split Panes: Hierarchical (Not Flat) Splitting

**Problem:** When splitting a pane that is already part of a split, two approaches are possible:

- **Flat:** Add the new pane as a sibling in the parent branch → all siblings resize equally. If you have [A|B] and split A, result is [A|newPane|B] with each pane at 33%.
- **Hierarchical:** Replace A's leaf with a new 2-child branch → only A's space is divided. If you have [A|B] and split A, result is [(A|newPane)|B] with A and newPane each at 25%, B untouched at 50%.

**Decision:** Always use hierarchical splitting. The flat approach is surprising because splitting one pane causes other panes to shrink. "Split this pane in half" is a more intuitive mental model than "add a pane to this group."

**Implementation:** `SplitNode.splitting(_:with:orientation:)` always wraps the target leaf in a new 2-child branch, regardless of the parent branch's orientation. `removing(_:)` collapses a branch to its single remaining child when a pane is closed.

## 14. macOS: Async Socket Handler via DispatchSemaphore + ResultBox

**Problem:** Some socket commands (e.g. `webview.execute_js`, `webview.get_content`) get their results from WKWebView callbacks, which run on the main thread asynchronously after the initial dispatch. The socket thread needs to block until the result is available.

**Decision:** Changed `SocketServer.commandHandler` from a synchronous `(method, params) -> Any?` signature to a completion-based `(method, params, completion: (Any?) -> Void) -> Void`. The socket thread blocks on a `DispatchSemaphore`. The main thread calls completion (possibly from a WKWebView callback), which stores the value in a `ResultBox: @unchecked Sendable` and signals the semaphore.

**Why `ResultBox`:** Swift 6 strict concurrency rejects capturing a `var` local in an `@MainActor` closure sent to another thread. A `final class` box with `@unchecked Sendable` is safe because the semaphore serializes all access — the socket thread never reads until after the signal.

## 15. macOS: NesttyPanel Protocol for Mixed Terminal+WebView Splits

**Problem:** `SplitNode` and `PaneManager` were typed to `TerminalViewController`. Adding WebView panels required either a union type or polymorphism.

**Decision:** Introduced `NesttyPanel: AnyObject` protocol with common interface (`view`, `currentTitle`, `startIfNeeded()`, `applyBackground`, etc.). `SplitNode` uses `case leaf(any NesttyPanel)`. Identity comparison uses `ObjectIdentifier` since `any NesttyPanel` is not `Equatable`.

**Tradeoff:** `any NesttyPanel` existentials have a small overhead vs. concrete types, but panel operations are infrequent (split/close/focus) so the overhead is negligible.

## 11. Configurable Tab Position

**Decision:** Tab bar position (`top`, `bottom`, `left`, `right`) is configurable via `[tabs] position` in config. Uses `gtk4::Notebook::set_tab_pos()`. Hot-reloads on config change.

**Rationale:** Vertical tabs (left/right) make better use of widescreen displays and are preferred by some users.

## 16. Project Scope: Personal Workflow Runtime, Not Just Terminal

**Problem:** Original framing was "terminal-centric programmable workspace" — a terminal with extensions for browser panels and AI. As integration scope grew to encompass calendars, messengers, knowledge bases, and trigger-driven automation, the "terminal with some extras" framing became inadequate. Every new integration was adding ad-hoc wiring between its source, the UI surfaces that render it, and the actions that operate on it.

**Decision:** Reframe nestty as a **personal workflow runtime** that surfaces through a terminal. `nestty-core` gains three central abstractions — Event Bus, Action Registry, Context Service — and existing features (socket event stream, plugin system, AI agent integration) consume them rather than reimplementing per-feature wiring.

**Tradeoff:** Larger architectural surface and more upfront design. The alternative — adding each integration ad-hoc — produces n×m wiring between n sources and m consumers (UI panels, triggers, AI agent, future KB indexer). The three abstractions turn this into n + m.

**Scope guardrails:**

- Do not build service clients from scratch when a mature web UI exists. Embed in the existing WebView panel (`webkit6` on Linux, `WKWebView` on macOS).
- Implement native event streams only for persistent push (WebSocket gateways, webhooks). Everything else polls via provider.
- Knowledge base is the last layer, built on the three abstractions — not a parallel system.

**See:** [workflow-runtime.md](./workflow-runtime.md) for the abstraction design and first vertical PoC plan.

## 17. Plugin-First for External Integrations (Post-Phase 8)

**Problem:** ADR 16 reframed nestty as a personal workflow runtime, with the implicit assumption that integrations like Calendar, Slack, KB, Notion, and LLM would land as modules in `nestty-core`/`nestty-linux`. As the integration surface grew it became clear this would make nestty a kitchen-sink monolith, lock the user to specific backends (e.g. KB always means `~/docs` if KB is built in), and make third-party contributions painful. The user explicitly raised the comparison to VSCode-style extensions.

**Decision:** Every external integration is a **service plugin** — a long-running supervised subprocess that speaks newline-delimited JSON over stdin/stdout and registers itself with `nestty-core` via a manifest-declared `[[services]]` section. The KB action protocol (and similar contracts) lives in `nestty-core` as documented protocol; implementations live in plugins. `nestty-core` and `nestty-linux` own the runtime primitives only — Event Bus, Action Registry, Trigger Engine, Context Service, Plugin Loader.

**Tradeoff:** First-call latency of a few hundred milliseconds (lazy plugin spawn) and IPC overhead per action call vs in-process performance. Acceptable at personal scale; the gain is extensibility, swappability of backends, crash isolation, and language-agnostic plugin authoring. Subprocess + stdio is the dominant pattern across the editor/IDE ecosystem (VSCode language servers, Neovim remote plugins, LSP) so it carries proven integration patterns.

**Key sub-decisions** (with research validation):
- **Lazy activation** like VSCode `activationEvents`. Initial instinct toward eager startup was wrong — research showed mature systems uniformly chose lazy.
- **LSP-style initialization handshake** for capability and version negotiation.
- **Manifest-declared `provides`/`subscribes` as source of truth + lexical-name conflict resolution** at load time. The runtime `initialize` response is checked asymmetrically against the manifest — applied identically to BOTH `provides` AND `subscribes`: subset allowed (degraded mode — nestty wires up only what runtime declared); superset rejected with warning (extras dropped, plugin keeps running for manifest-approved set). The pre-spawn ownership analysis stays accurate. Two enabled plugins with overlapping `provides` resolve via alphabetical `[plugin].name` ordering (the existing canonical plugin identifier from [plugins.md](./plugins.md)) — deterministic across runs and filesystems. User controls precedence by enabling/disabling plugins, or by editing the manifest `[plugin].name` if a finer override is needed.
- **Subprocess + stdio + newline-JSON**, NOT WASM yet. WASM (Zed's choice) adds Wasmtime runtime and WIT compilation barriers that personal-scale nestty doesn't yet need.

**See:** [service-plugins.md](./service-plugins.md) for full vision, decisions, rationale, research sources, and the Phase 9–18 roadmap.

## 18. Service Plugin Supervisor Threading: One Reader, One Writer, Workers for Recursive Calls

**Problem:** A service plugin can call back into nestty via `action.invoke` — a registered action handler that the registry might dispatch to *another* service plugin (or even back to the same one). If the reader thread that just received that inbound `action.invoke` synchronously calls `registry.invoke`, and that handler resolves through `invoke_remote` on the same service, we deadlock: the response we're blocking on is the response that this very reader thread is responsible for delivering.

**Decision:** Per running service, supervise with three OS threads — one writer (drains outgoing channel into child stdin), one reader (parses child stdout, dispatches frames), one stderr-tail (logs). On every inbound `action.invoke` request, the reader spawns a short-lived worker thread that runs `registry.invoke` and sends the response. Notifications (`event.publish`, `log`) stay on the reader thread because they don't recurse.

For the action-handler side: `invoke_remote` blocks the calling thread on a oneshot channel up to the action timeout. Since the calling thread is the dispatcher (socket→GTK timer or trigger sink worker), this is acceptable. The supervisor's response routing is decoupled because `dispatch_invocation` spawns its own worker that owns the reply channel.

**Tradeoff:** Higher thread count per service (3 + 1 wait + 1 per `subscribes` glob, plus transient workers per inbound recursive call). Justified at personal scale (a handful of services). The alternative — a single-threaded event loop with futures — would let us avoid threads but adds an async runtime dependency to `nestty-linux` that nothing else needs yet.

**See:** `nestty-linux/src/service_supervisor.rs` for the implementation.

## 19. Bounded Worker Pool for ActionRegistry (Phase 9.4)

**Problem:** `ActionRegistry::try_dispatch` was spawning a fresh OS thread per blocking handler call. Combined with the daemon's per-connection handler threads and the GUI client's per-Invoke workers from Step 4b, a burst of slow plugin calls + concurrent webview operations could blow the process thread count. Three rounds of codex-plan pressure-test surfaced that the *caller policy* — how long to wait when the pool is saturated — couldn't be uniform: daemon connection threads tolerate brief backpressure, but GTK pump / heartbeat reader paths cannot block at all without breaking the live-ness contract Step 4b established.

**Decision:** Replace the unbounded spawn with a bounded `ThreadPool` (crossbeam-channel, configurable workers + queue). Jobs implement a `Cancelable` trait — `run()` on a worker, `cancel()` synchronously on the caller's thread when the queue is full. Saturation surfaces as a new `overloaded` error code; `cancel()` also publishes `<action>.failed` so triggers waiting on completion don't hang. v1 wires the pool only from `nesttyd/main.rs`; `nestty-linux`'s in-process registry and `gui_client.rs` reader stay on the legacy spawn path (explicit scope-out, follow-up sub-steps).

**Tradeoff:** Fixed upper bound on concurrency means burst load rejects requests during saturation rather than degrading via thread thrash. Acceptable because the alternative (spawn fallback under saturation) defeats the bound entirely. Explicit `pool.shutdown()` in `main.rs` guards against an Arc cycle (registry → handler closure → supervisor → registry) that could prevent automatic `Drop`. The `Cancelable` trait abstraction (vs raw `FnOnce`) is the only way the registry can keep ownership of its `Responder` across both `run` and `cancel` paths — boxing a `FnOnce` into a generic `Job` would orphan the responder on rejection.

**See:** `nestty-core/src/thread_pool.rs` + `nestty-core/src/action_registry.rs` (`DispatchJob` impl).

## 20. Daemon Is the Sole Plugin Host (Step 5b)

**Problem:** After Step 5a flipped the daemon-attached default for the GUI, the in-process `ServiceSupervisor` in `nestty-linux` and the daemon's own optional supervisor (gated by `NESTTYD_HOST_PLUGINS`) both still existed. Running them simultaneously double-spawned plugins; running neither broke functionality. Keeping the GUI as the host meant the daemon-attached connection was decoration — anything CLI-facing or trigger-facing still bypassed it. Worse, an SSH session running nesttyd alone (no GUI) had no plugin host at all, defeating the headless story the daemon was meant to enable.

**Decision:** The daemon is the **sole** plugin host. Three rounds of codex-plan landed on this integrated scope:

1. `nesttyd/main.rs` always activates the supervisor — no env-var gate.
2. `nestty-linux/src/window.rs` no longer constructs `ServiceSupervisor`. Plugin manifest discovery stays (needed for panel rendering, statusbar modules, command lists) but the lifecycle disappears.
3. A daemon→GUI **event bridge** preserves chained-workflow triggers: each registered GUI client gets a per-connection forwarder thread (`gui_registry::start_event_forwarder`) that drains the daemon's `EventBus` and writes wire `Event { type, data }` lines on the existing socket. The GUI's `gui_client` reader bridges those back into the local `EventBus` so the in-GUI `TriggerEngine` (which still owns triggers in Step 5b — daemon-side triggers are a 5b.2 follow-up) sees `<action>.completed` events from daemon-hosted plugins.
4. The GUI's per-instance socket dispatcher now **forwards** unmatched methods (anything not in the GUI-owned legacy match arms, not in the local `ActionRegistry`) to the daemon over a bounded `ThreadPool`. Worker-isolated so the GTK timer never blocks on a slow plugin reply; saturation surfaces as `overloaded` (same vocabulary as Phase 9.4); daemon-absent surfaces as a new `no_daemon` error code.
5. **Out of scope:** trigger engine relocation (5b.2), context service relocation (5b.2), ~~statusbar `[[modules]]` shell execution (5b.3)~~ — DONE in 5b.3, ~~legacy `plugin.<name>.<cmd>` shell command execution (5b.3)~~ — DONE in 5b.3, ~~GUI per-instance socket permission hardening (5b.4)~~ — DONE in 5b.4.

**Tradeoff:** The chained-trigger preservation requires a new "GUI auto-subscribes-all events" mechanism, doubling daemon event traffic per connected GUI. Acceptable at personal scale because (a) the wire is a local Unix socket, (b) auto-subscribe-all is what the protocol § Resolved decisions #1 spec promised back in Step 4. Forwarding unmatched methods means a service-plugin RPC initiated from inside a nestty child shell now traverses three Unix-socket hops (nestctl → GUI socket → daemon socket → plugin) instead of one. Local domain, but ~3× latency over the legacy in-process call. Acceptable for v1; Step 5b.3 (where nestctl learns to talk to the daemon directly) collapses this back.

The C1 from plan-round-3 — `gui.register` ack vs first forwarded event race — is closed by deliberately starting `start_event_forwarder` AFTER the registration `Response` has been queued (`socket.rs::handle_gui_register`). C3 — GUI socket security regression — is deferred to 5b.4 as a known-equivalent-to-today posture rather than fixed in this step.

**See:** `nestty-daemon/src/gui_registry.rs` (`start_event_forwarder`, `forwarder_loop`), `nestty-linux/src/daemon_forward.rs` (the forward pool + `ForwardJob`), and the e2e step "event bridge — completion forwarded to GUI" in `scripts/e2e-daemon-client.sh`.

## 21. Bridge Echo Prevention via `Event::bridge_id` (Step 5b.2 Stage B)

**Problem:** Step 5b.2 introduces a bidirectional GUI↔daemon event bridge so the daemon-side TriggerEngine can see GUI-published events. Naive implementations either form an echo loop (daemon publishes → daemon→GUI forwarder sends → GUI republishes → GUI→daemon forwarder sends → ...) or break existing semantics. The daemon→GUI bridge already preserves `source` because the trigger engine's `try_promote_or_drop_preflight` gates on `source == "nestty.action"`; rewriting `source` at the bridge boundary (the obvious "easy fix") would silently break chained-trigger preservation.

**Decision:** Each `Event` gains a non-serialized `bridge_id: Option<u64>` field. `#[serde(skip)]` keeps it out of every wire frame so plugins, nestctl, and the existing daemon→GUI `WireEvent` shape are unchanged. The bus's `publish_bridged(event, bridge_id)` thin wrapper stamps the field; outgoing forwarders on both sides skip events whose `bridge_id.is_some()`. The id itself is a per-process monotonic u64 from `next_bridge_id()` — wraparound takes ~10⁹ years, not worth a leak-safe variant.

Four rounds of codex-plan locked in the exact contract: source-tagging was rejected (round-2 C2 — breaks await promotion), wire-frame extension was rejected (would require parser changes and a wider compat surface), the `bridge_id` field with `serde(skip)` was the only option that left every existing path untouched.

The `_bus.publish` ingest method is **wire-only**, not in `ActionRegistry`. Three reasons codex round-3 confirmed: (1) generic socket clients can call any registry method via raw `Request { method }`, which would bypass the registered-GUI auth convention; (2) registry methods auto-publish `<name>.completed` / `.failed` events — for `_bus.publish` that fans out a synthetic completion for every forwarded GUI event, polluting the bus; (3) auth lives at the connection layer (`registered_client_id.is_some()`), which the registry can't see. Special-case in `handle_connection` before `dispatch(...)` is the right placement.

The GUI's outgoing forwarder is **gated on `host_triggers=true`** in the daemon's `gui.register` ack — not on a GUI-side env var. This is the cut-over signal that Stage C will use: when daemon dispatches, GUI's local engine clears. The default (`NESTTYD_HOST_TRIGGERS` unset) preserves today's GUI-authoritative behavior end-to-end.

**Tradeoff:** A per-process counter means a stale event observed across a daemon restart could collide (each daemon starts at 1). Not a real concern because bridge crossings are session-scoped — neither side retains stale events past a disconnect. Tracking the id on every `Event` clone costs ~16 bytes per event; trivial. Stage B intentionally defers the GUI-engine clear (cut-over) to Stage C — Stage B alone with `NESTTYD_HOST_TRIGGERS=1` double-fires (both engines dispatch), documented as known limitation under an opt-in env flag.

**See:** `nestty-core/src/event_bus.rs` (`next_bridge_id`, `publish_bridged`, `Event::bridge_id`), `nestty-daemon/src/socket.rs` (`handle_bus_publish`, `DaemonState.host_triggers`), `nestty-daemon/src/gui_registry.rs` (`forwarder_loop` echo gate), `nestty-linux/src/gui_client.rs` (`start_gui_event_forwarder` + `ForwarderGuard`), and e2e step 9 in `scripts/e2e-daemon-client.sh`.

## 22. Atomic Cut-Over and Daemon Config Watcher (Step 5b.2 Stage C)

**Problem:** Stage B left a documented double-fire under `NESTTYD_HOST_TRIGGERS=1` — the GUI's local TriggerEngine kept dispatching alongside the daemon's. Stage C resolves the cut-over without leaving recovery gaps. Two independent requirements:

1. When the daemon advertises `host_triggers=true` in `gui.register`, the GUI must empty its local engine atomically AND refuse later `watch_config` reloads (which would re-arm it). When the daemon crashes mid-session, the GUI must restore local authority before the reconnect backoff begins — otherwise events fire with no subscriber.
2. The daemon's own engine needs runtime config tracking even with no GUI attached (headless `nesttyd`). A user editing `~/.config/nestty/config.toml` should see the change take effect without restarting the daemon.

**Decision (cut-over):** Three round-by-round codex critiques narrowed the design to:

- A `mpsc::channel::<bool>()` from `gui_client::run` to the GTK 50 ms timer. On register-ack success the run sends `ack.host_triggers`. A `HostTriggersGuard` drop guard mirrors `ForwarderGuard`'s pattern and sends `false` on every `run()` exit (success or error), restoring local authority on daemon disconnect.
- The 50 ms timer's cut-over consumer runs **BEFORE** `pump_all` so a queued `true` clears the engine on the SAME tick rather than letting one more dispatch through.
- The consumer is **edge-triggered**: it only applies when the queued value differs from the last applied state (`Cell<bool>` per-closure). Otherwise every normal reconnect with `host_triggers=false` would call `set_triggers(cached)` and reset preflight/pending await state — same hazard as today's hot-reload but fired on a connection bounce.
- A persistent `Arc<AtomicBool> host_triggers_active` is the source of truth. `watch_config` consults it: when `true`, the disk-reload still updates `cached_triggers` (so a later disconnect restores the FRESHEST config, not the startup snapshot) but skips `set_triggers` + `reconcile_triggers` for the triggers field. Theme, statusbar, background, keybindings still reload normally.
- The `start_gui_event_forwarder` (Stage B) is started **BEFORE** the cut-over signal is sent (codex round-1 C1). Otherwise the GTK timer could clear local subscriptions while the forwarder hasn't subscribed yet, leaving an event-loss window where neither engine fires.
- Tradeoff: `set_triggers` clears in-flight await state on every transition. Documented as acceptable — same as today's hot-reload semantics; trigger configs at personal scale rarely run long chains across reconnect events.
- Post-disconnect window (≤ 50 ms between `Drop` sending `false` and the timer applying it) where the local engine is still empty AND the daemon is gone — events in that window fire with no subscriber. Negligible in practice; only relevant if the daemon crashes mid-session under `NESTTYD_HOST_TRIGGERS=1`.

**Decision (watcher):** Daemon-side config watcher is a 2 s mtime poll thread spawned in `main()`, always running regardless of `host_triggers`:

- `load_triggers_config()` captures the file mtime at the same instant as the initial load and passes it to the watcher. Without that, an edit landing between `main()`'s load and the watcher's first tick would be silently treated as the baseline (codex round-2 C1).
- `apply_reloaded_triggers` mirrors the GUI's `watch_config` ordering when host_triggers=true: `engine.set_triggers(new)` → `pump_state.pump_all(ctx, engine)` on OLD subscribers (flush pending events into the soon-to-be-replaced set) → `pump_state.reconcile_triggers(bus, &new)`. Skipping `pump_all` would discard pending events the new trigger set would have matched during a pattern-narrowing reload.
- When host_triggers=false, **no PumpState exists** (codex round-3 C1 — extended scrutiny uncovered a pre-existing Stage A bug). The watcher only updates `engine.set_triggers` and refreshes `cached_triggers`; no `reconcile_triggers` means no `subscribe_unbounded` receivers are created with nothing to drain them, so the daemon bus doesn't accumulate events under the default daemon configuration.
- `notify` crate dep rejected; 2 s mtime poll is enough for trigger reloads and avoids the wider compat surface.

**Tradeoff:** Tying the cut-over to the daemon's register-ack advertisement means each disconnect/reconnect re-traverses the cut-over → restore cycle. Each transition costs one `set_triggers` (clears preflight/pending await state). Users running long-chain workflows during daemon flap would lose those workflows. Accepted because (a) personal-scale trigger configs are short, (b) the daemon flap itself is the more pressing fault and would already break in-flight workflows for other reasons (forwarded events stop arriving), (c) the alternative — preserve await state across cut-over — adds significant state-machine complexity.

The watcher running even when `host_triggers=false` is intentional: a future Stage D could turn `host_triggers` into a runtime toggle (e.g. on a daemon mode switch) and the watcher already has the engine state ready.

**See:** `nestty-linux/src/window.rs` (`apply_host_triggers`, `drain_to_latest`, `watch_config` skip gate, 50 ms timer cut-over consumer), `nestty-linux/src/gui_client.rs` (`HostTriggersGuard`), `nestty-daemon/src/main.rs` (`config_watcher_loop`, `apply_reloaded_triggers`, `build_pump_state`), and e2e step 10 in `scripts/e2e-daemon-client.sh`.

## 23. `events.publish` Public Surface + `SO_PEERCRED` Source Stamping (Step 5b.2 Stage D)

**Problem:** Headless `nesttyd` runs need a way for external scripts to fire events onto the daemon bus — without that, the entire trigger engine relocation (Stages A-C) has no headless entrypoint. The first instinct was to expose the bus through the existing `ActionRegistry` (e.g., add a `publish` action), but two rounds of codex-plan revealed the wider trust gaps that approach opens.

**Decision:** `events.publish` is a **wire-only** socket method (special-cased in `handle_connection` before generic `dispatch`). NOT in `ActionRegistry`. Three concrete reasons:

1. Generic socket clients can call any registry method via raw `Request { method }`. If `events.publish` were in the registry, every connection-level trust gate (the registered-GUI convention, the future per-method ACL) would be a fiction — the registry routes by name without inspecting the connection.
2. Every registry-routed action auto-publishes `<name>.completed` / `.failed` events. For `events.publish` that would mean a synthetic completion fan-out for every external event — polluting the bus with metadata about the metadata.
3. Trust gates live at the connection layer (peer credentials, registered-GUI flag), which the registry's method-match path has no visibility into.

The public surface accepts `{ kind: String, payload?: Value }` and **daemon-controls `source` and `timestamp_ms`**. `source = format!("client.{pid}")` is stamped via `SO_PEERCRED` on Linux; non-Linux returns `None` → `"client.unknown"`. `timestamp_ms` is filled by `BusEvent::new` from the daemon clock. The caller has no way to set either field. This is what makes spoofing the action-registry completion gate impossible: `try_promote_or_drop_preflight` reads top-level `source` and trusts only `nestty.action`. Since `events.publish` cannot ever produce that top-level value, no chained workflow can be hijacked.

`_bus.publish` (Stage B) and `events.publish` (Stage D) are two separate methods, not one with a registered-GUI branch. The split avoids a complex "if registered, trust the source field, else stamp it" branch with subtle trust gaps:
- `_bus.publish` = bridge variant (registered GUI relays its own events, source/timestamp from caller, `bridge_id` set to prevent echo).
- `events.publish` = public variant (no registration, daemon stamps source/timestamp, no `bridge_id`).

`nestctl event publish <kind> [json-payload]` uses `paths::daemon_socket_path()` directly, bypassing `discover_socket`'s GUI-first preference. Connecting to a GUI socket would return `unknown_method`. The publish subcommand also parses the payload locally before opening the socket so malformed JSON fails with a clear `invalid_argument` instead of being forwarded as a confusing daemon-side error.

**Tradeoff:** `SO_PEERCRED` returns the peer's pid at connect time, not call time. Pid reuse across process death (bounded by Linux's `kernel.pid_max`) could briefly misattribute the source — acceptable for the log-correlation use case. macOS uses `LOCAL_PEERCRED` and would need its own plumbing; for now the macOS daemon stub returns `None` and source becomes `"client.unknown"`. No rate-limiting on `events.publish` — same trust band as today's `nestctl call`, where 0600 socket reachability is the only guard. Documented as known; rate-limit is a follow-up. Payload size cap (64 KiB) was considered but deferred: the daemon's `reader.lines()` has no upper bound regardless, so a cap at the application layer wouldn't meaningfully protect daemon memory. Line-size hardening is a separate commit.

The decision to leave trigger condition / interpolation reading payload-source first (an existing pre-Stage-D semantic, preserved) is a user-config concern: if a trigger author writes `condition = "event.source == 'foo'"`, they're reading payload, not provenance. The daemon's trust gates (`try_promote_or_drop_preflight`) operate on top-level fields, which the daemon controls; user-defined conditions on `event.source` would need to verify payload absence to gate on daemon-controlled provenance. Documented in the user-facing trigger config docs (forthcoming).

**See:** `nestty-daemon/src/socket.rs` (`peer_pid`, `handle_events_publish`), `nestty-cli/src/commands.rs` + `nestty-cli/src/main.rs` (`EventCommand::Publish`, `dispatch_publish`), and e2e step 11 in `scripts/e2e-daemon-client.sh`.

## 24. Curated GUI Env Whitelist for `system.spawn` (Step 5b.2 Stage E)

**Problem:** Stage A relocated `system.spawn` from the GUI's `LiveTriggerSink` to the daemon's `DaemonTriggerSink`, but the child process now inherits the daemon's process env, not the GUI's. Hyprland trigger configs (the user's primary `system.spawn` use case — `hyprctl dispatch` calls) depend on `HYPRLAND_INSTANCE_SIGNATURE`, which the daemon doesn't have because `nesttyd` was started by `systemd --user` or the login shell before the compositor ran. Wayland/X11 tooling (`notify-send`, `pactl`) needs `DISPLAY` / `WAYLAND_DISPLAY` / `DBUS_SESSION_BUS_ADDRESS` for the same reason.

**Decision:** A curated whitelist of session env keys flows from the GUI to the daemon at `gui.register` time. The daemon-side filter is the trust boundary; the GUI-side curation is UX-only.

The 7-key whitelist (`HYPRLAND_INSTANCE_SIGNATURE`, `DISPLAY`, `WAYLAND_DISPLAY`, `XDG_RUNTIME_DIR`, `XDG_SESSION_TYPE`, `XDG_CURRENT_DESKTOP`, `DBUS_SESSION_BUS_ADDRESS`) covers Hyprland + standard Wayland/X11 + session-bus tooling. Anything outside the list — `PATH`, `LD_PRELOAD`, `OPENAI_API_KEY`, terminal aliases — is dropped at `filter_gui_env` BEFORE the env reaches `GuiClient.gui_env`. Codex round-1 C1 surfaced the subtle but important point: GUI-side filtering alone is not load-bearing because any registered client could be a mock (e.g., the e2e test infra itself), and the daemon cannot distinguish a legitimate `nestty-linux` from a malicious script.

`DaemonTriggerSink::handle_system_spawn` merges primary's env via `Command::envs(gui_env)`. Rust's `Command::new` inherits the parent's env by default; `envs` OVERRIDES matching keys without clearing unlisted ones. So `PATH`, `HOME`, `USER` from the daemon persist while `DISPLAY` etc. are pulled from the GUI's filtered map. Critically, `env_clear` is NOT called — that would strip `PATH` and break `/usr/bin/env`-style triggers.

`GuiRegistry::primary_gui_env()` returns `Option<HashMap<String, String>>`. Lock order `clients → primary` mirrors `route()`'s order to avoid an AB-BA deadlock against any caller that already holds `clients` (codex round-1 C2). When no GUI is registered (pure headless mode), the accessor returns `None` and the sink falls back to pure daemon env — the pre-Stage-E behavior, preserved.

**Tradeoff:** Capturing env once at register time, not at spawn time. If the user restarts their Hyprland session while `nesttyd` stays running (rare), the cached signature is stale until reconnect. Refresh-on-register is the simplest path and matches user expectation (session vars are write-once). Pid-reuse semantics on `peer_pid` (Stage D) have the same property: connect-time snapshot wins. macOS daemon stub returns empty `gui_env` map; when the macOS shell eventually ships, it'll need its own env-capture wire.

Curated whitelist drift is the maintenance hazard. New keys added to the list need an audit: each one is a vector if someone misconfigures a trigger to spawn shell-quoted user input that interpolates an env var.

**See:** `nestty-daemon/src/gui_registry.rs` (`GUI_ENV_ALLOWED_KEYS`, `filter_gui_env`, `GuiClient.gui_env`, `primary_gui_env`), `nestty-linux/src/gui_client.rs` (`GUI_ENV_CURATED_KEYS`, `capture_gui_env`), `nestty-daemon/src/daemon_trigger_sink.rs` (`handle_system_spawn` env merge), and e2e step 12 in `scripts/e2e-daemon-client.sh`.

## 25. Daemon `event.subscribe` Bus Projection (Phase 8 closing)

**Problem:** The GUI special-cased `event.subscribe` as a `bus.subscribe_unbounded("*")` projection (nestty-linux/src/socket.rs), but the daemon returned `unknown_method`. `nestctl event subscribe` against the daemon socket failed, and service-plugin authors expecting the documented `event.subscribe { patterns: [...] }` shape had no daemon-side handler.

**Decision:** Mirror the GUI's projection at the daemon's `handle_connection`. One `bus.subscribe_unbounded("*")` per connection, filtering deferred to the handler via `pattern_matches` OR'd across `params.patterns`. `params.patterns` is documented protocol; `None`/`[]` means "all".

Single-subscriber + handler-side filter beats N-subscribers + N-forwarder-threads because cross-pattern event ordering is trivially preserved (FIFO from one receiver) and the bus's `pattern_matches` is sub-microsecond per event. The tradeoff is wasted CPU on the daemon when a narrow pattern is requested — acceptable at typical event rates (≪ 1k/sec); profile-driven optimization can switch to bus-level multi-pattern subscribers later if needed.

**Disconnect detection during quiet bus periods (codex round 2 C1):** the GUI's projection has a latent leak — `rx.recv()` blocks indefinitely with no event, so a client that disconnects during a quiet stretch keeps the connection thread + unbounded bus subscriber alive until the next event lands. The daemon's implementation closes this with `recv_timeout(15s)` + a no-op `writer_tx.send(String::new())` on timeout. The writer thread writes an empty line on the wire, which probes EPIPE: closed socket → writeln fails → writer thread exits → writer_rx drops → subscriber's `send` returns Err → handler returns. `nestctl`'s subscribe reader (`nestty-cli/src/client.rs:69`) already skips empty lines, so the keep-alive is wire-compatible.

**Registered-GUI rejection (codex round 2 I2):** registered GUI connections already receive events via `start_event_forwarder` (Stage A). Running both pumps on one socket duplicates every event. The daemon rejects `event.subscribe` on a registered connection with `error.code = "invalid_request"` and instructs the caller to use `gui.subscribe`/`gui.unsubscribe`. `docs/gui-daemon-protocol.md` reconciled to spell out the exception explicitly.

**Subscribe-before-ack ordering (codex review C1):** the bus subscription is created BEFORE the ack is queued on `writer_tx`. If the ack were queued first, a publisher on a separate connection could publish a matching event between the client receiving the ack and the daemon's reader thread reaching `subscribe_unbounded("*")` — a lost event that violates the "lossless projection" contract. Ordering is `bus.subscribe_unbounded → send ack → enter recv loop`; the receiver is now active by the time the client can act on the ack.

**Tradeoff:** Once `event.subscribe` is active the connection is subscribe-only — the reader thread enters the recv loop and never returns to the request loop. Same contract as the GUI's projection. A second connection is required for further RPC. The "lossless" delivery contract from workflow-runtime.md (`subscribe_unbounded`) means a stuck client lets the bus subscriber's mpsc grow unbounded; the upstream `writer_tx(512)` caps it (full writer_tx blocks the subscriber loop, which lets further events accumulate in the unbounded receiver while the writer is wedged). Pathological case recoverable by killing the client — kernel eventually fails the writer's write → chain unwinds.

**See:** `nestty-daemon/src/socket.rs` (`run_event_subscribe`, `parse_subscribe_patterns`, `SUBSCRIBE_KEEPALIVE`), and e2e step 13 in `scripts/e2e-daemon-client.sh`.

## 26. Completion-Event Fan-Out for Legacy `socket::dispatch` Arms (Phase 8 closing)

**Problem:** `ActionRegistry::with_completion_bus` auto-publishes `<action>.completed`/`<action>.failed` for every action it handles, but nestty-linux's `socket::dispatch` legacy match-arm fallthrough (`tab.*`, `background.*`, `terminal.exec`, `webview.*`, `agent.approve`, `claude.start`, `plugin.list`, …) bypasses the registry on miss and never published. Triggers chaining off `tab.new.completed` got silence. Same for daemon-side: when `dispatch_via_gui` proxied a legacy method to a registered GUI, neither side published.

**Decision:** Publish completion at the source of execution.
- **GUI side**: every legacy match arm funnels its reply through `SocketCommand::reply_with_completion(bus, resp)`, which calls `publish_legacy_completion` then forwards to `cmd.reply.send`. Callback-deferred handlers (webview JS exec via `run_js_command`, agent.approve dialog) capture `bus`/`method`/`silent` clones before moving `cmd.reply` into the closure and call the free function `publish_legacy_completion` directly.
- **Daemon side**: `dispatch_via_gui` and `DaemonTriggerSink::fallthrough_worker` call `publish_legacy_completion` on the daemon bus AFTER `client.invoke` returns. Failure paths — `no_gui`, `unknown_client`, invoke timeout, GUI returning ok=false — emit `.failed` (codex review C2).

**Duplicate avoidance via `SocketCommand.silent_completion`:** the daemon→GUI bridge re-publishes daemon-bus events on the GUI bus, so daemon-proxied actions would double-publish at GUI if both sides published unconditionally. `gui_client::handle_invoke` sets `silent_completion = true` on the SocketCommand it constructs from a daemon Invoke; the GUI's wrapper skips local publish for those, and only the bridged daemon-published event lands on the GUI bus. Direct nestctl→GUI calls leave `silent_completion = false` so GUI-local triggers still fire (codex review C1 round 2).

**`daemon_forward::forward` skipped** (codex review C2 round 1): commands the GUI doesn't know how to handle locally are proxied to the daemon, where the registry publishes natively and the bridge brings the event back to GUI. Wrapping forward would dupe; the wrapper is intentionally absent on that path.

**`LEGACY_SILENT_METHODS`** = `["terminal.read", "terminal.state", "terminal.history", "terminal.context", "tab.list", "tab.info", "session.list", "session.info"]` — read-only / agent-polled paths whose "completion" is the response itself. Publishing would flood the bus without enabling meaningful chained triggers. Mirror of `register_silent`'s semantics on the legacy surface. Consulted by both `SocketCommand::reply_with_completion` (GUI) and `dispatch_via_gui`'s publish helper (daemon).

**Trust framing (codex review I4):** the daemon stamping `source = "nestty.action"` on a GUI-invoked action's completion event represents "the daemon vouches that this GUI-owned action returned this response," NOT "the daemon forwards GUI-provided provenance." `_bus.publish`'s rejection of `.completed`/`.failed` kinds + `nestty.action` source stays intact — that gate prevents GUIs from spoofing arbitrary completion events. The daemon-side publish here is at the trust boundary INSIDE the daemon (post-route, post-invoke, post-response receipt), not a bridge surface.

**Known boundary**: when `host_triggers=true`, the GUI's `TriggerEngine` is cleared and the GUI bus completion events from direct nestctl→GUI calls have no consumer. Symmetrically, the daemon's `TriggerEngine` doesn't see nestctl→GUI-direct completions (the GUI→daemon forwarder allowlist excludes `.completed`). Users running triggers should connect through the daemon socket (the default for `nestctl` discovery) so the completion event lands where the trigger engine is reading.

**See:** `nestty-daemon/src/socket.rs` (`LEGACY_SILENT_METHODS`, `is_legacy_silent`, `SocketCommand::reply_with_completion`, `publish_legacy_completion`, `dispatch_via_gui` publish call), `nestty-daemon/src/daemon_trigger_sink.rs` (`fallthrough_worker` publish call), `nestty-linux/src/socket.rs` (legacy match arms calling `cmd.reply_with_completion`, webview callbacks + agent.approve calling `publish_legacy_completion`), `nestty-linux/src/gui_client.rs` (`silent_completion=true` on daemon-Invoke SocketCommand), and e2e step 14 in `scripts/e2e-daemon-client.sh`.

## 27. Per-Frame Memory Caps + GUI-Client Bounded Writer (5b.2 Stage B follow-ups)

**Problem:** Two hazards codex flagged on the Step 5b.2 reviews that landed as deferred follow-ups, not blockers:
- `BufRead::lines()` on both daemon (`nestty-daemon/src/socket.rs` `handle_connection`) and GUI (`nestty-linux/src/socket.rs` `start_server`) calls `read_until` internally with no upper bound. A peer (legitimately misbehaving or hostile) that streams bytes without `\n` would force the daemon/GUI to buffer the entire stream in memory until either OOM or socket close.
- The GUI-side daemon-client (`nestty-linux/src/gui_client.rs` `spawn`/`run`) uses `mpsc::channel::<String>()` (unbounded) for its writer queue, while the daemon-side equivalent was bounded to `sync_channel(512)` in Step 5b R2 INFO. A wedged daemon socket reader would stall the GUI's writer thread, and the outgoing event forwarder + RPC reply path would accumulate strings without limit.

**Decision:**
- `MAX_FRAME_BYTES = 1 MiB` constant + `pub fn read_line_capped(reader, buf) -> io::Result<Option<String>>` helper in `nestty-daemon/src/socket.rs`. Returns `Ok(None)` on EOF, `Ok(Some(s))` on a full line, `Err(InvalidData)` AS SOON AS the running total would exceed the cap. No resync attempt — codex review C1 surfaced that a peer streaming bytes without `\n` would let any resync loop block on `fill_buf` indefinitely, defeating the cap's purpose. The helper fail-fasts; both callers (daemon `handle_connection`, GUI `start_server`) send a wire-level `frame_too_large` reply (id `""`) and then close the connection. The reasoning: a 1 MiB+ unterminated frame is either a misbehaving client or an attack, and the trust band (0600 socket, one user) doesn't justify a partial recovery path that adds blocking risk.
- GUI-client writer changed to `mpsc::sync_channel::<String>(512)` — matches the daemon side. `register`, `handle_ping`, `write_overloaded`, `handle_invoke`, the outgoing event forwarder, and the test channels all switched from `Sender<String>` to `SyncSender<String>`. Recovery semantics unchanged from the daemon side: writer thread `writeln` fails on a dead socket → exits → writer_rx drops → `send` returns Err → forwarder/jobs surface as `Disconnected`.

**Tradeoff:** 1 MiB cap is well above any legitimate JSON frame we ship today (webview content reads ~256 KiB max, screenshots base64 below ~1 MiB but typically under). Chunked transfer of very large payloads would need to be split across multiple actions — explicit at the cap. The 512 writer buffer matches the daemon side's empirically-validated bound; same recovery loop.

**Test coverage:** 4 new unit tests on `read_line_capped`:
- `returns_full_line` — two consecutive frames, then EOF
- `rejects_oversized_frame_fail_fast` — 1 MiB+32 bytes of `x` without `\n`; asserts `InvalidData` returns promptly (would block forever if any resync logic existed)
- `rejects_overflow_even_when_newline_eventually_arrives` — payload at cap+1 followed by `\n`; asserts the cap is enforced strictly (the helper doesn't peek past the cap to confirm)
- `handles_no_trailing_newline_at_eof` — final partial frame at EOF

The oversized tests use a worker thread to write the payload over `UnixStream::pair` because Linux unix-socket buffers (~208 KiB default) deadlock a synchronous `write_all` if the reader isn't draining concurrently — the test's own write strategy is a microcosm of the production concern.

**See:** `nestty-daemon/src/socket.rs` (`MAX_FRAME_BYTES`, `read_line_capped`, capped reader in `handle_connection`), `nestty-linux/src/socket.rs` (capped reader in `start_server`), `nestty-linux/src/gui_client.rs` (`GUI_CLIENT_WRITER_BUFFER`, all `SyncSender<String>` sites, capped reader in the daemon→GUI receive loop AND `await_register_ack`).

## 28. Supervisor Waiter Pool (Phase 9.5 follow-up)

**Problem:** Phase 9.4 introduced a bounded `ThreadPool` for `ActionRegistry` blocking handlers, but the corresponding waiter on the supervisor side — `ServiceSupervisor::dispatch_invocation`'s `thread::spawn(move || resp_rx.recv_timeout(120s))` — was left as per-call thread spawning. Roadmap entry 240 flagged this as a known limitation: under burst load (many concurrent service invocations) the daemon could accumulate one OS thread per in-flight invoke, each pinned for up to `action_timeout` (default 120s). Not a hot spot today because trigger configs rate-limit invocations, but unbounded growth is a latent footgun for chained workflows that fan out.

**Decision:** Counter-based admission control over per-call `thread::spawn`. `ServiceSupervisor.waiter_active: Arc<AtomicUsize>` tracks concurrent in-flight waiters; `waiter_max` (default 64, env-configurable via `NESTTYD_WAITER_MAX`) caps the count. Each admitted invocation:
1. `fetch_add` on the counter; if pre-increment value ≥ max, decrement back and reply `overloaded` synchronously (no invoke sent).
2. Insert the `pending_responses` entry.
3. Send `action.invoke` synchronously on the caller's thread.
4. `thread::spawn` a waiter that holds a `WaiterPermit` drop-guard; the permit decrements the counter on thread exit (success, timeout, or panic).

We considered using the existing `nestty_core::thread_pool::ThreadPool` (the same primitive that backs the Phase 9.4 ActionRegistry pool), but rejected it after two codex rounds: the pool's bounded queue introduces a "queued but not running" state where a waiter job sits with `resp_rx` ready to receive while no thread is parked to forward to `reply`. A fast service response (or a synthetic send-failure response) can land in `resp_rx` buffered until a worker frees, by which time the caller's `invoke_remote(action_timeout)` has already returned `action_timeout`. The counter primitive avoids this by spawning a thread on every admit — there is no queueing window.

Tradeoff: per-call thread::spawn cost (~30 µs) instead of pool worker reuse. For waiters that spend their lifetime blocked on `recv_timeout`, reuse saves no real work — the cost being saved would be µs of spawn overhead on work units that take ms-to-seconds. Worth it for the simpler ordering story.

**Ordering invariant (resolved across three codex review iterations):**
- v1 (initial): `handle.send(invoke)` BEFORE pool admission. Saturation → caller sees `overloaded` while the service had already executed the action.
- v2: move the send INSIDE the pool worker. Queued workers stale-send long after the caller's `invoke_remote` already returned `action_timeout` and the workflow retried.
- v3 (counter-based, final): drop the pool's queue entirely. Counter admission + immediate `thread::spawn`. Saturation rejects without sending; admission spawns a thread instantly so there is no "queued but not running" state where a response can be buffered into an unattended `resp_rx`.

Under the final design, the SERVICE sees each invoke exactly once across the full saturation × caller-timeout × send-failure matrix. Caller's `reply_rx.recv_timeout(action_timeout)` and waiter's `resp_rx.recv_timeout(action_timeout)` operate on the same clock (both start near `dispatch_invocation` entry); a service reply propagates through reader → resp_tx → resp_rx → waiter → reply → reply_rx. If caller times out first, waiter's `reply.send` returns Err harmlessly. If `handle.send` fails after admission, the caller's thread short-circuits: removes the pending_responses entry, replies the error directly, and lets the permit drop (slot released for the next caller).

`daemon.info` does NOT yet surface the waiter counter / max — could be added; not blocking.

**Test coverage:** 3 unit tests on the admission primitives — `waiter_permit_decrements_on_drop`, `waiter_permit_decrements_once_even_under_panic` (panic-unwind preserves the slot release), and `env_waiter_max_falls_back_to_default_on_invalid` (env parse + zero rejection). The full dispatch_invocation path is exercised by the existing e2e (step 5 heartbeat survival, step 7 pool saturation, step 8 plugin RPC round-trip) which transitively cover the counter admission and waiter-thread lifecycle.

**See:** `nestty-daemon/src/service_supervisor.rs` (`waiter_active` counter, `waiter_max` cap, `WaiterPermit` drop-guard, `env_waiter_max` helper, `dispatch_invocation` admission + send + spawn).

## 29. Command Palette (Phase 8 closing)

**Problem:** Phase 8 closed `ActionRegistry` + completion-event fan-out, but the user still had no in-GUI way to enumerate or fire registered/legacy actions. The roadmap-declared affordance was a Ctrl+Shift+P modal — a fuzzy filter over the registry.

**Decision:** Build a minimal GTK4 modal palette in `nestty-linux/src/command_palette.rs`:
- Modal `Window` (transient on the main window) containing a `SearchEntry` + scrolled `ListBox`.
- Action surface = `actions.names()` (GUI registry — `system.ping`, `system.log`, `context.snapshot` today) ∪ `LEGACY_DISPATCH_METHODS` (~45 entries re-exported from `nestty_daemon::socket`).
- Substring filter (case-insensitive, whitespace-trimmed). Fuzzy matching is a follow-up; substring is sufficient for ≤100 entries.
- Enter on the SearchEntry dispatches the currently selected ListBox row through the existing `dispatch_tx` SocketCommand pump. Empty params (`{}`); actions that need params will surface `invalid_params` via the normal reply path — documented v1 limitation.
- Up/Down navigates the list while focus stays in the SearchEntry. Esc closes (handled both via a capture-phase `EventControllerKey` and via SearchEntry's `stop-search` signal as a safety net — without the capture phase the SearchEntry's built-in Esc handler ate the event and the palette wouldn't dismiss).

**Destructive-action confirmation (codex review C3 round 1):** `tab.close` with empty params would close the active terminal — accidental data loss if the user typed it + hit Enter to dispatch. Added a `DESTRUCTIVE_ACTIONS: &["tab.close"]` const; before dispatching one of those, show a `gtk4::AlertDialog` with `[Cancel, Confirm]` where Cancel is BOTH default and cancel button (codex review C1 round 2 — Confirm-as-default would let a second stray Enter complete the destruction). The user must explicitly select Confirm to proceed.

**Focus restoration (codex review I1 round 2):** the palette captures `mgr.active_panel()` before opening and calls `panel.grab_focus()` on close (Esc, Cancel, or post-dispatch) so typing returns to the previously focused terminal/webview.

**User-keybinding precedence:** `Ctrl+Shift+P` is a built-in default, but `nestty`'s `check_custom_keybinding` runs BEFORE the built-in match — so a user `config.toml` entry like `"ctrl+shift+p" = "spawn:..."` shadows the palette binding. This is intentional: the custom-keybindings feature exists specifically to let users override defaults. Users who want the palette and an existing Ctrl+Shift+P spawn binding move their custom binding to a different key.

**Visible action surface limitation (codex review C2 round 1):** the palette only enumerates GUI-reachable actions today. Daemon-hosted plugin actions (`kb.search`, `slack.send_message`, etc.) ARE dispatchable through socket dispatch's `daemon_forward` fallback, but they're registered in `nesttyd`'s registry, not the GUI's — and the GUI has no `actions.list` RPC to enumerate them. Documented v2 follow-up. The 48 listed entries already cover the dominant interactive workflow surface.

**Tradeoff:** No param prompt in v1. Adding a form builder for actions that need params would double the diff and pull in an opinion about the form-rendering layer. v2 can wire a second-stage form (or just let the user `nestctl call <method> --params '{...}'` for parametric actions).

**See:** `nestty-linux/src/command_palette.rs` (full implementation + 5 unit tests on `filter_actions`), `nestty-linux/src/tabs.rs` (Ctrl+Shift+P key arm + Ctrl+Shift+Left for prev-pane), `nestty-linux/src/window.rs` (TabManager::new wired with actions registry).


## 30. URL click-to-open (Phase 7 closing)

**Problem:** Plain-text URLs in terminal output were not clickable. The roadmap-declared affordance was Ctrl+Click on a URL to open it in the default browser, plus support for OSC 8 hyperlinks (where visible text can differ from the target URL).

**Decision:** Implement in a dedicated `nestty-linux/src/url_click.rs` module installed once per `TerminalPanel` from `tabs.rs::create_panel`. Two match paths feed the same launch handler:

1. **Regex URL detection** via VTE's `match_add_regex` + `check_match_at`. Pattern: `(?i:https?://[^\s<>'"]+)` with `PCRE2_MULTILINE` only. Trailing punctuation (`.,;:!?)]}>`) is stripped post-match by `normalize_url` rather than excluded from the regex — keeps the pattern simple and the trim rule auditable.

2. **OSC 8 hyperlinks** via `set_allow_hyperlink(true)` + `check_hyperlink_at(x, y)`. Wins over the regex tag because OSC 8 emitters set the visible label independently of the URL target (`]8;;https://x\click here]8;;\` — the regex would only see "click here").

**Critical implementation detail — flags + jit:** Initial implementation used `PCRE2_MULTILINE | PCRE2_CASELESS` plus `regex.jit(PCRE2_JIT_COMPLETE)`. `check_match_at` returned `(None, -1)` for every coordinate, even with a trivial pattern like `[a-z]+` — `cargo test` passed, but the live API silently returned no matches. After tracing gnome-console's C source for reference, the working setup is:

- compile flags = `PCRE2_MULTILINE` only (no `PCRE2_CASELESS`)
- inline `(?i:...)` group for case-insensitive matching
- no JIT call

The exact failure mode is undiagnosed (vte4 0.8.0 + VTE 0.84 ABI quirk vs. silent PCRE2 flag rejection vs. JIT interaction). Anchoring to gnome-console's flag set is the safest path until a public reproducer exists.

**Scheme allow-list (Ctrl+click).** `normalize_url` rejects anything other than `http://` and `https://`. The check applies uniformly to OSC 8 hyperlink targets AND regex hits — an OSC 8 emitter could otherwise inject `javascript:` or `file://` payloads through arbitrary terminal output.

**Ctrl+Click gate.** Plain click on a URL would steal text selection — gnome-terminal/foot/kitty all gate on Ctrl. The `GestureClick` is registered with `PropagationPhase::Capture` and `connect_pressed` so VTE's selection gesture doesn't claim the sequence first; on a successful URL launch the gesture state is set to `Claimed` so VTE doesn't see the click.

**`gtk4::UriLauncher` over `xdg-open`.** GIO MIME default-handler resolution, no subprocess `Child` to leak, parent-window hint for any error dialogs.

**OSC 52 status (roadmap Phase 7 item 125):** already closed elsewhere. macOS Tier 0.3 gates `clipboardCopy` via `[security] osc52`; Linux VTE 0.84 is deny-by-default and exposes no toggle property in vte4 0.8.0. No Linux code change needed — roadmap entry updated to back-link to Tier 0.3.

**See:** `nestty-linux/src/url_click.rs` (full implementation + 6 unit tests on `normalize_url`), `nestty-linux/src/tabs.rs::create_panel` (install site), `nestty-linux/src/main.rs` (`mod url_click;`).


## 31. macOS terminal emulator core — migrate off SwiftTerm to alacritty_terminal + custom renderer

**Problem:** SwiftTerm hit four architectural blockers we cannot fix without forking:

1. **IME composition broken** — `MacTerminalView.setMarkedText`, `hasMarkedText`, `markedRange` are stubs (`// nothing`, `false`, `NSRange.empty`). Korean/Japanese users can't see what they're composing until commit. Source: `nestty-macos/.build/checkouts/SwiftTerm/Sources/SwiftTerm/Mac/MacTerminalView.swift:847,877,885`.

2. **Cursor invisibility with image background** — `CaretView` is an NSView overlay; pinning `caretColor` (tried theme.accent → theme.foreground → NSColor.white) works in shell but fails against busy wallpapers in TUIs (Claude Code/Ink-based). Sibling-NSView opaque backdrop synced via 30Hz timer was attempted (`syncCaretBackdrop` poll loop, z-order managed via `addSubview(_:positioned:.below:relativeTo:)`) — stderr confirmed creation + sync, but user reported zero visible rectangle on screen (layer-compositing path we couldn't crack).

3. **Reverse-video over transparent bg renders as transparent** — same class of bug WezTerm #1076 / Microsoft Terminal #7014 / Zed PR #17611 all document. Zed's fix decouples logical ANSI bg from rendered transparency via a sub-layer; SwiftTerm has no such separation.

4. **No smart-cursor / cursor_text_color=background / transparent_background_colors equivalents.** SwiftTerm's render pipeline is monolithic `drawRect` over a full bounds region; per-cell custom drawing hooks don't exist.

**Options considered:**

- **A. Fork SwiftTerm.** Weeks per blocker + permanent rebase burden; architectural limits (NSView caret overlay, no per-cell hook) survive the fork.
- **B. Replace with `alacritty_terminal` Rust crate + own AppKit/CoreText (Metal later) renderer.** Zed's pattern. Estimated 3-4 months for parity (codex consultation, single dev). We own all the painful surfaces (IME, cursor, transparency, future ligatures/decorations/images).
- **C. Wait for libghostty.** `libghostty-vt` is shipping (Zig API merged May 2026, C API in progress, ~6 months to tagged version; libghostty-swift framework on the roadmap but further out). When ready, this is the lowest-effort highest-quality option — Ghostty core proven by millions of DAU, designed-from-start for embedding. But not usable today and timeline is "if ready".
- **D. Custom from-scratch emulator.** What iTerm2/Kitty/Ghostty did. 6-12 months. Right call only if we need full control AND have time AND can't reuse a maintained core.

**Decision: B (alacritty_terminal + custom renderer), via codex-validated hybrid path** — keep SwiftTerm as production renderer; build alacritty renderer behind a runtime/dev flag; migrate by vertical slices: PTY/grid → plain text render → cursor → selection → IME → scrollback → colors/transparency → advanced features. Flip default only after parity passes.

**Why not E (wait for libghostty):** even at the optimistic 6-month timeline, libghostty-swift framework is later than libghostty-vt. We'd be back to "wait or build a renderer ourselves" anyway, just with a different emulator core (which is the smaller half of the work). The renderer is what we're really committing to build. If libghostty-swift ships in time and is better than alacritty_terminal, the renderer survives and the emulator backend is a thin swap.

**Why not D (from-scratch emulator):** Alacritty's terminal core is mature, spec-compliant, and battle-tested in production at scale. Reinventing the VT parser + grid + scrollback is months of work for negative differentiation. Risk: Alacritty maintainers explicitly state `alacritty_terminal` is "not for external use" (alacritty/alacritty#2132) — Zed accepts that risk and effectively maintains their integration; we will too, pinning specific versions and being prepared to fork the crate if upstream diverges.

**Short-term SwiftTerm stopgaps that stay in the tree until migration:**

- `NesttyTerminalView.cursorStyleChanged` bar→block clamp when `nativeBackgroundColor == .clear` (so 2px bar/underline cursors don't become invisible against image)
- `applyCaretColors` pins `caretColor = theme.accent`, `caretTextColor = theme.background` (avoids `NSColor.selectedControlColor` ghost-gray-on-blur)
- `Keybindings.matches` + `CommandPalette.matchesCommandPaletteShortcut` use `keyCode` rather than `charactersIgnoringModifiers` (IME-immune key matching; commit `04e622c`)

**Known limitations documented for users of the SwiftTerm phase:**

- Cursor may be invisible when image background is active + a TUI with busy color palette is running (Claude Code with bright wallpaper). Workaround: increase `[background] tint`, lower `opacity`, or temporarily clear the background.
- Korean/Japanese IME composition does not show preedit text in-cell during composition. The final character commits correctly. Workaround: compose in another app and paste, or rely on muscle memory.

**See:** `docs/macos-renderer-migration-plan.md` for the phased plan, FFI design, and slice-by-slice scope.


## 31. Session persistence (Phase 7 closing)

**Problem:** Closing nestty meant losing the current tab/split layout — no auto-restore of where the user was working. The roadmap-declared affordance was XDG-state-backed session persistence with auto-save on close and auto-restore on next launch.

**Decision:** Implement Linux-only in `nestty-linux/src/session.rs` with a typed JSON schema and an explicit lifecycle wired from `window.rs`. Schema (versioned, strict — no best-effort parsing of mismatched versions):

```
Session { version: u32 = 1, tabs: Vec<TabSnap>, current_tab: usize }
TabSnap { custom_title: Option<String>, root: SplitSnap }
SplitSnap { Terminal { cwd } | Branch { orientation, position, first, second } }
```

**Persisted on `window.connect_close_request`.** `TabManager::snapshot_session()` walks the live SplitNode tree, building SplitSnap. WebView/Plugin panels are elided (their state — page URL, scroll, plugin-internal — is out of v1 scope); a Branch with one surviving child collapses to that child. If the snapshot has zero terminal tabs (all-elided / all-closed), `session::clear()` removes the file so a stale snapshot doesn't survive an "all tabs closed" exit. Atomic write: temp file + `rename`.

**`current_tab` remap (codex C2 round 1).** The notebook's `current_page()` indexes against the original tab list including elided ones. The persisted index has to point into the surviving terminal-only list, so the snapshot loop maps the active notebook index across elisions before storing it.

**Restored on startup.** `window.rs` reads `session::load()` BEFORE seeding any default tab. If `Some(session)` with non-empty tabs: `TabManager::restore_session()` rebuilds tabs and splits via the existing `add_tab_with_cwd` + `TabContent::split` primitives. Split tree restoration is a recursive walk: the new panel created at each Branch level gets the leftmost-Terminal cwd of the snap's `second` subtree (`session::leftmost_cwd`), so each sub-leaf eventually lands in its persisted cwd. `TabManager::new()` was refactored to NOT create a default tab — `window.rs` now decides between restore vs default-add explicitly, eliminating the phantom-empty-tab race (codex C1 round 0).

**cwd cascade (codex C3 round 1).** Reading `last_cwd` alone misses `cd` changes in shells that don't emit OSC 7 (older bash, some POSIX shells). `TerminalPanel::current_cwd()` is a new helper consulting in order: `terminal.current_directory_uri()` (OSC 7) → `/proc/<pid>/cwd` (proc fs) → `last_cwd` (final fallback for the shell-exited edge case). Both `state()` (existing socket query) and snapshot now go through it.

**URI percent-decoding fix (codex C1 round 2).** OSC 7 paths arrive URI-encoded (`file://host/home/me/My%20Project`). The prior `normalize_osc7_uri()` only stripped the host portion; the persisted cwd would contain a literal `%20`, and `spawn_async` would fail to chdir into a directory that doesn't exist on disk. Path is now decoded with `glib::Uri::unescape_string` (falls back to raw on decode failure so we never drop a value).

**Split position not restored.** The snap stores `paned.position()` but restore doesn't re-apply it — uses `TabContent::split`'s default `set_paned_position_deferred` (50/50). Restoring exact pane sizes requires either reaching into the new Paned post-split or threading position through `split()`; documented v2 polish, not worth carrying in v1.

**Custom tab title.** Stored on the TabSnap, not keyed by panel id (codex C5 round 0 — restored panels get new UUIDs). At restore time `rename_tab(root_panel.id(), &title)` re-applies it to the first panel of the restored tab.

**WebView/Plugin elision.** v1 ships terminal-only because:
- WebView state = page URL + scroll + cookies (security surface).
- Plugin state = plugin-internal, requires a plugin protocol extension.
Both are documented v2 work. The fallback behavior is "next launch has a smaller tab set than the previous session" — acceptable for v1.

**See:** `nestty-linux/src/session.rs` (schema + 5 unit tests), `nestty-linux/src/tabs.rs::snapshot_session` / `restore_session` / `restore_split`, `nestty-linux/src/terminal.rs::current_cwd` + percent-decode in `normalize_osc7_uri` (+1 test), `nestty-linux/src/window.rs` (close_request + restore-or-add at startup).


## 32. `action_result` interpolation in `payload_match` (Phase 14.2 deferred slice 1)

**Problem:** The await clause's `payload_match` could only reference the originating event's fields (`{event.<x>}`). Chaining "post to Slack → wait for a reply on the SAME thread" required the response payload's `thread_ts` to flow back into the await's match — but `LiveTriggerSink` returns `Ok({queued: true})` synchronously for blocking/legacy actions, so the real result wasn't available at register time. The 14.2 slice 1 doc explicitly deferred this.

**Decision:** Move `payload_match` interpolation from register-time to promotion-time. The `.completed` event published by `ActionRegistry` already carries the action's real return payload — capture it during `try_promote_or_drop_preflight` and use it as the `action_result` namespace alongside `event.*` when interpolating.

**State machine refit:**

- `PreflightAwait` now stores `payload_match_template: Map<String, Value>` (the un-interpolated form) plus the full `original_event: Event` (vs. just `original_payload` before — the kind/source/timestamp fields are needed for re-interpolation).
- `try_promote_or_drop_preflight`: on `.completed`, capture `event.payload.clone()` as `action_result`, then run `interpolate_value_typed(v, &original_event, None, Some(&action_result))` for every template entry. The fully-resolved match goes into `PendingAwait.payload_match`; the action_result is also stored on PendingAwait for the synthesized `<trigger>.awaited` event downstream.
- `build_awaited_payload` gains a third arg (`action_result: Option<&Value>`); when present, it lands as `action_result:` on the synthesized payload, parallel to `await:`. Downstream triggers thus read `{event.action_result.<field>}` as a regular nested-payload lookup.

**Interpolator extension:**

- `resolve_token` / `resolve_token_value` gain an `action_result: Option<&Value>` arg; both branch on `token.strip_prefix("action_result.")` before falling back to the existing `event.` and `context.` paths.
- `interpolate_value` / `interpolate_value_typed` / `interpolate_string` thread the param through; public callers of `interpolate_value` (e.g. `Trigger::interpolate`) go via the existing no-action-result variant, so the public API is unchanged.

**Posture decisions:**

- **Token-not-found preserves the literal.** If the `.completed` payload has no `ts` field, `{action_result.ts}` resolves to the literal string `"{action_result.ts}"`, the pending match against any real ts fails, and the pending stays until timeout. Better than coercing to `null` and firing on a garbage match. (+1 test `action_result_token_missing_field_keeps_match_open`.)
- **`sweep_pending_awaits` for FireWithDefault**: preflight expiry has no action_result available (action never completed) — pass `None`. Pending expiry has one — pass `Some`. The synthesized `*.awaited` event therefore carries `action_result:` when it makes sense and omits it when it doesn't. **`None` actively removes the key** (codex review C1 round 2): if the firing event is itself an upstream `*.awaited` synthesized event, its payload already has an `action_result:` — without an explicit `remove`, that stale field would leak into the downstream timeout event's payload and `{event.action_result.*}` would read the wrong action's result.
- **No persistence across nestty restart.** Both PreflightAwait and PendingAwait remain RAM-only. The earlier 14.2 deferred slice 2 (persistent journal) is unblocked by this change but not implemented here — minute-scale awaits are unaffected.
- **Context captured at register, replayed at promotion (codex review C1 round 1).** Pre-refactor, `register_preflight_await` interpolated `payload_match` synchronously with the live `Context`, so `{context.active_panel}` resolved at dispatch time. Moving interpolation to promotion meant `None`-context regressed existing templates. PreflightAwait now clones `Context` at register and replays it at promotion. The semantic remains "captured at dispatch" — between dispatch and `.completed` the active panel could change, but the trigger's intent is "match the panel that fired me", not "match whatever is active at promotion".

**Test additions:**

- `payload_match_interpolates_action_result_token` — full happy path: post → capture `ts` from `.completed` → ignore non-matching reply → fire on matching reply, verify the synthesized event carries both `await:` and `action_result:`.
- `action_result_token_missing_field_keeps_match_open` — fail-loud literal preservation.
- `payload_match_interpolates_context_token_captured_at_register` — regression coverage for codex C1 round 1.
- `timeout_with_default_does_not_leak_upstream_action_result` — regression coverage for codex C1 round 2.

**See:** `nestty-core/src/trigger.rs::try_promote_or_drop_preflight` (promotion-time interpolation), `nestty-core/src/trigger.rs::build_awaited_payload` (new action_result arg), `nestty-core/src/trigger.rs::resolve_token` / `resolve_token_value` (action_result branch). The interpolator refactor preserves the no-action_result code path for `Trigger::interpolate` so action `params` interpolation is unaffected — only the awaited-event pathway sees action_result.


## 33. Git workspace file-watcher events (Phase 17.2)

**Problem:** Phase 17.1 shipped CRUD actions for git workspaces and worktrees but no live "something changed in the repo" signal. Status bar widgets, a future git panel, and triggers that want to react to branch/worktree state changes without polling actions all need an event channel.

**Decision:** Add a polling watcher per configured workspace in `plugins/git/src/watcher.rs`. Each watcher thread snapshots `.git/HEAD` (raw line), `.git/refs/heads/**` (recursive loose-ref names), and `.git/worktrees/*` (immediate subdirs) at a fixed interval and emits diffs to the plugin's writer channel as `event.publish` frames.

**Polling, not `notify`:**

- Dependency-free — no new crate, no platform conditional. The `notify` crate would add inotify (Linux) + FSEvents (macOS) backends each with their own pitfalls (FSEvents coalesces edits inside a directory tree, inotify watches per-fd hit a kernel limit fast on many workspaces).
- For status-bar / live-indicator use, 2 s lag is the same order as the user's own click cadence.
- Cheap: a snapshot is `read_to_string(.git/HEAD)` + recursive readdir of refs/heads/ + readdir of worktrees/ — bytes-level fs traffic per workspace per poll.
- The cost of "not real-time" is paid only by these advisory events. `git.worktree_add` and friends still publish their own `<action>.completed` via Phase 14.1's registry fan-out, so chained triggers (Vision Flow 3) hit the bus the instant the action returns.

**Posture decisions:**

- **Loose refs only.** `.git/packed-refs` is intentionally NOT scanned. Branches that exist only there are pre-established as of `git gc` time and don't represent user-initiated changes within the watching window. The trade-off: rare, but a `git gc` running mid-session could collapse loose refs into packed-refs and the watcher would emit spurious `branch_deleted` for them. Acceptable for v1.
- **HEAD-cleared suppresses `git.checkout`.** If `.git/HEAD` becomes unreadable (transient race during operations), the snapshot's `head` is `None`. The diff explicitly skips emitting `checkout {head: null}` — "branch went away" is the `branch_deleted` signal's job. Avoids noisy null-HEAD events during transient races. Tested.
- **First-snapshot baseline.** The watcher loop's first iteration is the baseline; no events fire until the second poll. So a branch created between nestty start and the first snapshot is "already there" from the watcher's perspective. Right contract for a polling watcher.
- **Sorted diffs.** Multiple changes within one poll interval are emitted in deterministic order: HEAD first, then alphabetically-sorted creates, deletes, worktree creates, worktree deletes. Avoids racy test assertions and lets downstream triggers rely on a stable order if they care.

**Threading model:** one detached `thread::spawn` per workspace. No clean shutdown — when the plugin's main thread exits (stdin EOF or SIGTERM from supervisor), the OS reaps the process and the watcher threads die with it. A stop flag (`AtomicBool`) is plumbed through so the loop short-circuits between sleeps, but the plugin never actively sets it today; it's wired for a future shutdown-notification handler.

**Init handshake gate (codex review I1 round 1):** Watcher threads sleep on an `initialized: Arc<AtomicBool>` until `handle_frame` flips it on the `initialized` notification, BEFORE taking the baseline snapshot. Without the gate, a `NESTTY_GIT_POLL_MS=250` setup with slow plugin startup could publish an event before the supervisor's `initialize` → `initialized` handshake completes, and the host would drop it as out-of-protocol.

**Gitdir resolution for secondary worktrees (codex review C1 round 1):** `Config` validation accepts ANY valid git working tree via `git rev-parse --is-inside-work-tree`, including secondary worktrees where `.git` is a FILE (gitlink: `gitdir: <primary>/.git/worktrees/<name>`) rather than a directory. The naive `<path>/.git/HEAD` read would silently fail for those, and the watcher would emit nothing for a valid workspace. Snapshot now delegates to `git rev-parse --git-dir` (per-worktree gitdir, where HEAD lives) + `--git-common-dir` (shared across worktrees, where refs/heads/ and worktrees/ live). Two `git` shell-outs per snapshot — cheap; could be cached but a 2s cadence makes caching premature. E2E test `snapshot_secondary_worktree_resolves_via_git_rev_parse` verifies `.git`-as-file resolution against a real repo.

**Activation flipped to `onStartup`:** Phase 17.1 ran the plugin lazily (`onAction:git.*`) because actions were the only surface. With watchers, the plugin must be alive whenever nestty runs so events flow between action calls. Cheap: a workspace-less config (or zero workspaces) spawns no threads and just sits at stdin.

**Configurability:** `NESTTY_GIT_POLL_MS` overrides the default 2000 ms; values below 250 ms are clamped to protect against accidental tight loops. No "interval = 0 = disabled" mode — to disable, remove the workspace entries or `kill -9` the plugin.

**See:** `plugins/git/src/watcher.rs` (snapshot + diff + spawn + 9 unit tests including a real-`git` E2E), `plugins/git/src/main.rs` (`watcher::spawn(...)` after Config load), `plugins/git/plugin.toml` (version bump to 0.2.0, `activation = "onStartup"`, description mentions emitted events).
