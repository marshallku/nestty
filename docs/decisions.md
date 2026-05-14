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
