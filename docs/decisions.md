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
