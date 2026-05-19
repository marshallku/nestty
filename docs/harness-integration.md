# Harness Integration Plan

## Vision

nestty becomes the user's **personal automation hub**: a headless daemon that
runs under `systemd --user` (Linux) or `launchd` (macOS), hosts the trigger
engine + plugin supervisor + action registry + event bus, and relays events
from every workflow source the user already runs — Slack, Discord, Calendar,
Jira, Claude Code hooks, Codex broker, ai-browser, life-assistant, and any
shell-driven `nestctl event publish` call. The GUI process (`nestty`) becomes
an optional viewer/shell on top of that daemon, no longer the host.

The trigger engine is the single hub. The GUI is one of its clients.

## Architecture pivot — daemon-first

### Why now

The current host code (TriggerEngine / ActionRegistry / EventBus /
ServiceSupervisor / socket server) lives entirely inside the `nestty-linux`
GUI process. Bootstrap is at `nestty-linux/src/window.rs` lines 125–450
(~330 LOC). The supervisor + socket + trigger fan-out total ~4150 LOC next to
GTK code.

Consequences today:

- Close the GUI → all triggers stop, all plugin daemons (Slack Gateway, Discord
  Gateway, Calendar poller, …) exit. Automation is GUI-bound.
- Over SSH, hooks can't reach the local bus (socket is GUI-process-local).
- macOS GUI is a stub, so the entire automation surface is unusable on Mac
  despite the Rust core being portable.
- Multi-display means running multiple nestty windows; each spawns its own
  supervisor + socket today.

`nestty-ffi` already exposes `TriggerEngine + ActionRegistry + EventBus` to
macOS Swift via C-ABI (407 LOC). The "core decoupled from shell" idea exists.
The Linux side hasn't claimed it yet.

### Current code map (codex-reviewed)

`socket.rs` is **not pure transport** — it imports GTK, VTE, WebKit,
`TabManager`, `BackgroundLayer`, `StatusBar`, `ApplicationWindow`. Most of its
2k LOC is GUI command execution dispatched from socket messages. Treating it
as host code (as an earlier draft did) was wrong. The actual split is finer:

```
nestty-core/  (already shared)
  action_registry  event_bus  trigger  condition  context
  plugin protocol  config  theme  error  fs_atomic

nestty-linux/src/  (mixed)
  service_supervisor.rs   (1778 LOC, host)            ← move whole
  trigger_sink.rs         ( 344 LOC, host)            ← move whole
  socket.rs               (2042 LOC, MIXED)           ← split:
    ├─ transport (accept loop, framing, NESTTY_SOCKET) → move
    ├─ daemon-owned action dispatch (event.*, plugin.*,
    │   agent.*, theme.*, todo.* CLI shortcuts, ...)  → move
    └─ GUI-owned action dispatch (tab.*, split.*,
        terminal.*, webview.*, background.*,
        statusbar.*, plugin.open panel, agent.approve
        UI prompts, ...)                              ← STAYS in GUI
                                                        as a GUI-client
                                                        handler module
  window.rs               (~600 LOC; host wiring 125–450)  ← split bootstrap
  app.rs panel.rs plugin_panel.rs webview.rs
  terminal.rs tabs.rs split.rs background.rs
  search.rs statusbar.rs                                    ← stay GUI

nestty-cli/   (already shared, no change)
nestty-ffi/   (C-ABI to Swift, 407 LOC, role re-scoped — see Out of scope)
nestty-macos/ (Swift, separate pivot — see Out of scope)
plugins/*     (already shared, no change)
```

### Target shape

```
       systemd --user (Linux) / launchd LaunchAgent (macOS)
                  │ supervises
                  ▼
            nesttyd  ◄────────────── nestctl (CLI client)
            ├─ TriggerEngine                 ← always-on
            ├─ ActionRegistry (daemon-owned actions)
            ├─ ServiceSupervisor (plugins)
            ├─ EventBus (origin-tagged)
            ├─ Notifier  (libnotify / osascript)
            ├─ GuiRegistry (active GUI clients)
            └─ Unix socket (well-known path)
                  ▲
                  │ bidirectional protocol — GUI registers,
                  │ daemon proxies GUI-owned actions to it,
                  │ events stream the other way
                  ▼
            nestty GUI (GTK4 on Linux / AppKit on macOS)
              VTE/terminal · tabs · splits · panels · search
              background · statusbar
              + handlers for daemon-proxied GUI commands
```

### Crate plan

| Crate | Status | Role |
|---|---|---|
| `nestty-core` | extend | + `Notifier` trait, + `PlatformPaths`, + event `origin` tagging, + `GuiClient` registration types |
| `nestty-daemon` | **new** | binary `nesttyd`. Owns supervisor, socket transport, daemon action dispatch, trigger fan-out, GUI registry, Notifier impls behind `cfg(target_os)`. ~3000 LOC (relocated supervisor + trigger_sink + half of socket.rs). |
| `nestty-cli` | no change | client only; socket protocol unchanged from CLI viewpoint |
| `nestty-linux` | shrink | GTK shell + GUI-owned action handlers, now invoked via daemon proxy rather than direct socket dispatch. ~3500 LOC remaining. |
| `nestty-ffi` | re-scoped | macOS Swift talks to local `nesttyd` over socket once macOS shell pivots. Until then, FFI in-process embed is retained for macOS-only standalone builds — see Out of scope. |
| `nestty-macos` | unchanged for this plan | Linux daemon pivot does not pull macOS shell along. Separate phase. |
| `plugins/*` | no change | spawned by `nesttyd`'s supervisor; stdio protocol unchanged |

### Platform abstraction

Three platform points, all behind `cfg` gates inside `nestty-daemon`:

**1. `Notifier` trait** in `nestty-core::notifier`:

```rust
pub trait Notifier: Send + Sync {
    fn notify(&self, title: &str, body: &str, level: Level) -> Result<()>;
}
pub enum Level { Info, Warn, Error }
```

- Linux: D-Bus call to `org.freedesktop.Notifications` (zbus). Fallback: shell out to `notify-send`.
- macOS: `osascript -e 'display notification …'`. Subprocess per call — fine at human-driven rate. Watch burst behavior under heavy trigger fan-out; switch to a long-lived `UserNotifications` XPC client if measured cost matters.

Registered as action `notify.show {title, body, level?}` — any trigger or plugin can fire it.

**2. `PlatformPaths`** (`nestty-core::paths`, `cfg` functions, not a trait):

- Linux: `${XDG_RUNTIME_DIR:-/tmp}/nestty/` (socket + pids), `~/.local/state/nestty/` (state).
- macOS: `~/Library/Caches/nestty/` (socket), `~/Library/Application Support/nestty/` (state).

Socket moves from `/tmp/nestty-{PID}.sock` to `${runtime_dir}/socket`. Daemon owns a stable path; PID discovery retired. **Migration audit list below** — this change has more consumers than nestctl.

**3. Service installation** — install scripts, not runtime code:

- Linux: drop `nestty-daemon.service` into `~/.config/systemd/user/`, `systemctl --user enable --now`. `Restart=on-failure`, `After=default.target`.
- macOS: drop `com.marshall.nestty.daemon.plist` into `~/Library/LaunchAgents/`, `launchctl bootstrap gui/<uid>`. `RunAtLoad=true`, `KeepAlive=true`.

`scripts/install-dev.sh` and `scripts/install-macos.sh` get a `--with-daemon` flag (default on).

### GUI ↔ daemon protocol (must be designed before migration)

This is the load-bearing contract codex flagged as missing. The current socket
is server→client streaming + one-shot requests. The daemon-first design needs
bidirectional routing because daemon-side triggers/CLI need to invoke
GUI-owned actions (tab, split, terminal, webview, background, statusbar, panel
open, agent.approve UI prompts).

Minimum semantics:

1. **GUI registration.** A GUI sends a normal `Request` with
   `method: "gui.register"` to identify itself; a connection becomes
   addressable as a GUI at the moment that call succeeds (no
   first-message constraint — the connection acts as a generic client
   until then). Canonical wire shape lives in
   [gui-daemon-protocol.md § `gui.register` schema](./gui-daemon-protocol.md).
   Summary: GUI advertises `window_id` + capabilities (`tab`/`split`/
   `webview`/`background`/`statusbar`/`agent.ui`/`plugin.open`/`terminal`/
   `search`/`session`) + `want_primary: bool`. Capabilities are advertised,
   not assumed — a future minimal/headless GUI omits what it can't render.

2. **Active GUI selection / default target policy.**
   - At most one registered GUI is "primary" at any time.
   - **First GUI to register with `want_primary: true`** becomes primary
     (not just "first to register" — a GUI that explicitly passes
     `want_primary: false` never becomes primary by default).
   - A GUI may bid for primary later via `gui.set_primary`.
   - GUI-owned commands target primary by default; explicit
     `target_client_id` on `Request` overrides (see protocol doc §
     "Explicit targeting" for placement).

3. **Request/response correlation.** Every GUI-bound `Invoke` carries an
   `id`; GUI replies with a `Response` echoing the same `id`. Daemon
   maintains a pending-Invoke map with timeout (default 5s) → `gui_timeout`
   error on miss.

4. **Disconnect handling.** Daemon pings each registered GUI every 10s via
   `Invoke` (`_ping`); GUI replies with a normal `Response`. Heartbeat is
   unidirectional (daemon→GUI), not symmetric. On GUI disconnect or two
   consecutive `_ping` invokes without a matching `Response`:
   - Primary slot transfers to next registered, if any.
   - Pending requests targeting the disconnected GUI fail with
     `gui_disconnected`.
   - No request queueing across disconnect — drop, surface error to caller.

5. **`no_gui` error contract.** When no GUI is registered:
   - GUI-owned actions (`tab.*`, `split.*`, etc.) return `no_gui` immediately.
   - Daemon-owned actions (`event.*`, `plugin.*`, `todo.*`, etc.) work as
     normal.
   - `nestctl` surfaces `no_gui` with a one-line hint: "no nestty window
     attached; run `nestty &` first".

6. **Event subscription scoping.** Two paths, deliberately separate:
   - **Generic clients** (CLI, plugins) keep using `event.subscribe { patterns }`
     exactly as today. Wire shape unchanged.
   - **Registered GUIs** auto-subscribe to all events at `gui.register`
     time — the monitor panel and statusbar widgets want broad visibility
     and the round-trip is friction. A GUI may narrow via `gui.subscribe
     { patterns }` or stop via `gui.unsubscribe` (GUI-connection methods,
     not bus operations).

   Heartbeat is delivered over `Invoke`, not via the event stream —
   non-GUI subscribers (`nestctl event subscribe`, plugin `subscribes`)
   stay byte-compatible with today's output.

This protocol is the **first design deliverable**, not the last. Code work on
the supervisor relocation can start in parallel, but no GUI disconnect from
in-process supervisor until this protocol is implemented and the GUI client
handler module is rewriting daemon-proxied requests.

### What stays in the GUI process

Pixel/PTY-bound code only:

- **VTE / SwiftTerm terminal widget** (owns PTY)
- **Tabs / splits / window layout**
- **Plugin panels** (WebKit `<panel.html>` + JS bridge)
- **Background image rendering**
- **Search-in-terminal, statusbar**
- **GUI command handlers**: the half of current `socket.rs` that
  manipulates `TabManager` / `BackgroundLayer` / `StatusBar` /
  `ApplicationWindow` / WebKit panel. After the pivot, these handlers
  receive requests from `nesttyd` over the bidirectional protocol instead
  of dispatching directly inside the same process.

The GUI is *both* a UI process *and* an RPC handler for the daemon. The pivot
inverts the call direction but doesn't move the GTK code.

### Migration path (codex-resliced)

Step 3 in the earlier draft was not atomic. Resliced:

1. **Protocol design + spec.** Document `gui.register`, capability negotiation,
   primary/secondary target policy, request/response correlation, disconnect
   handling, `no_gui` contract. Land as `docs/gui-daemon-protocol.md`. No
   code yet. ~half day.

2. **Daemon binary scaffolding.** New `nestty-daemon` crate + `nesttyd` bin.
   Sliced into three commits to keep each diff reviewable per the user's
   small-unit save convention:
   - **2a (scaffolding):** crate + binary entry; `PlatformPaths` in
     `nestty-core`; transport (UnixListener bind, accept loop, framing);
     minimal dispatch handling `system.ping`; socket-permission hardening
     (parent dir 0700, socket 0600); stale-socket cleanup; sigterm-safe
     shutdown via stale-detect on next start (no async-signal handler).
     `nestctl` discovery aware of the new well-known path but still prefers
     legacy GUI socket during migration. ~half day.
   - **2b (supervisor import):** import `service_supervisor` +
     `trigger_sink` (audit confirmed both are GTK-free, depend only on
     `nestty-core`). `nesttyd` activates plugins on start and pushes bus
     events. Plugin manifest discovery uses the same path as today. ~half
     day.
   - **2c (pdeathsig / orphan reaping):** dedicated long-lived spawner
     thread (so the fork-thread never exits while nesttyd is alive) OR
     `pidfd_open` + epoll path. Re-introduces crash-safe child reaping
     that Phase 9.5 rolled back. ~half day.
   ~1.5 days total.

3. **Relocate clean modules.** Move `service_supervisor.rs` and
   `trigger_sink.rs` from `nestty-linux/src/` to `nestty-daemon/src/`. Update
   imports. Two-binary build: daemon + GUI share module code via the new
   crate. No behavior change. ~half day.

4. **Split `socket.rs`.** Extract transport + daemon-owned dispatch into
   `nestty-daemon::socket`. The GUI-owned handlers stay in `nestty-linux`,
   regrouped as `nestty-linux::gui_handlers`. Add a thin "GUI client" mode in
   `nestty-linux` that connects to `nesttyd` and registers via the new
   protocol — but, for now, daemon still does *everything* in-process; GUI
   handlers are wired both via in-process dispatch AND via the new protocol
   path under a feature flag. Validates the protocol on real GUI commands
   without a flag day. ~2 days.

5. **Switch dispatch to GUI client mode.** Remove the in-process dispatch
   path; daemon always proxies GUI-owned commands over the protocol. `tab.*`,
   `split.*`, `terminal.*`, `webview.*`, `background.*`, `statusbar.*`,
   `plugin.open`, `agent.approve` UI prompts all flow daemon → GUI. ~1 day,
   plus a day of soak testing.

6. **Install scripts + socket path consumers.** Ship systemd unit / launchd
   plist. Audit and update every place that injects or discovers
   `/tmp/nestty-{PID}.sock` — see audit list below. ~1 day.

7. **Switch default to daemon-attached; keep `--standalone` as permanent
   build feature.** The `--standalone` mode (daemon-in-process) ships behind
   a `--features standalone` build flag for single-user no-systemd setups,
   CI, and first-use bootstrapping. The migration only changes the *default*
   to daemon-attached; the in-process path is not deleted. ~half day for
   flag plumbing.

Total: ~6 working days. Each step is independently testable; tree stays
green at every commit boundary.

#### Socket path consumer audit (step 6)

Codex flagged this as wider than just nestctl discovery. Inventory the
following before deleting the legacy path:

- `nestty-cli/src/main.rs` — `NESTTY_SOCKET` env discovery + `/tmp/nestty-*.sock` glob
- Plugin manifests + plugin runtimes that read `NESTTY_SOCKET` from env
- macOS Swift `SocketServer.swift` (separate path; coordinate with macOS pivot)
- Keybinding scripts / desktop entry that pass `NESTTY_SOCKET`
- terminal agent commands (`nestctl agent ...`) that spawn helpers with the env var
- statusbar plugin clients
- Documentation: `CLAUDE.md`, `docs/cli.md`, install scripts' help text

Symptom of an outdated reference: hard-coded `/tmp/nestty-*.sock` somewhere.
`rg -n 'nestty-.*\.sock|NESTTY_SOCKET'` catches them.

### Trade-offs

- **Two processes instead of one** — more failure modes (daemon up, GUI socket
  connect fails; vice versa). systemd/launchd absorbs daemon restart; GUI
  reconnects.
- **Latency on GUI-owned actions** — round-trip GUI → socket → daemon → socket
  → GUI. Sub-millisecond on Unix socket, but exists. Acceptable for
  human-driven actions; verify under trigger-burst load.
- **macOS Notifier subprocess** — fine at human rate, untested at trigger
  burst. Defer XPC if measured cost matters.
- **Multi-GUI fan-out** — kept simple: one primary + optional secondaries with
  explicit targeting. Don't promise eventual-consistency between GUIs until
  there's a concrete need.

## Event sources to relay

Each is a plugin or external integration that pushes events through the
daemon's bus. Uniform shape: emits `<source>.<name>` events (with `origin`
tag — see Trust boundary), optionally exposes `<source>.<verb>` actions.

### In-tree plugins (already exist, no architectural change)

| Plugin | Events emitted | Notes |
|---|---|---|
| `calendar` | `calendar.event_imminent` | Polls Google Calendar. `onStartup`. Origin: `internal`. |
| `slack` | `slack.message`, `slack.mention`, `slack.dm`, `slack.reaction`, `slack.raw` | Gateway WebSocket. `onStartup`. Origin: `internal`. |
| `discord` | `discord.message`, `discord.mention`, `discord.dm`, `discord.reaction`, `discord.raw` | Gateway WebSocket. `onStartup`. Origin: `internal`. |
| `jira` | (single-action triggers) | Phase 16. |
| `kb` | (action surface) | `~/docs` tree. |
| `todo` | `todo.created`, `todo.changed`, `todo.completed`, `todo.deleted`, `todo.start_requested` | File-watcher. Origin: `internal`. |
| `llm` | (action surface, usage record) | Anthropic API. |
| `git` | (action surface) | worktree ops. |
| `bookmark` | (action surface) | URL bookmarks. |

Plugin-published events are always `internal` origin — they enter via the
plugin stdio protocol, not the socket.

### A. Claude harness hooks → `claude` plugin

**Goal.** Surface every Claude Code hook fire on the bus so triggers can react.

Prerequisite: **`nestctl event publish`** subcommand. Non-blocking,
silent-fail on missing socket. Tags every emitted event with
`origin: external` (see Trust boundary below).

**Actions**: `claude.session_state`, `claude.list_dirty`, `claude.last_handoff`, `claude.list_sessions`.

**Events** (one new line per hook, `command -v nestctl && nestctl event publish ... &`):
- `claude.tool_used` — PostToolUse `track-edit.sh`
- `claude.commit_blocked` — `pre-commit-gate.sh` non-zero exit
- `claude.review_approved` — `codex-review.sh` on `VERDICT: APPROVED`
- `claude.session_stopped` — Stop hook `auto-handoff.sh`
- `claude.user_prompt` — UserPromptSubmit (optional)

**Trigger examples** (each requires `accept_external = true`):
- `claude.commit_blocked` → `notify.show {level=warn}` toast
- `claude.review_approved` → `kb.append` to a daily log
- `claude.session_stopped` → `todo.create` for in-progress items

State lives in existing harness files; no duplicate cache. ~400 LOC plugin + ~10 hook script edits. Effort: M (1–2 days).

### C. ai-browser → `browser` plugin

**Actions**: `browser.navigate`, `browser.screenshot`, `browser.network_logs`, `browser.eval` (opt-in for JS sandbox), `browser.get_text`, `browser.console_logs`.

**Events (later)**: `browser.navigation_complete`, `browser.console_error`. Origin: `internal` (plugin-published).

**Triggers**:
- `todo.start_requested {linked_jira}` → `browser.navigate` → `browser.screenshot` → `kb.ensure`
- `/verify` skill output → `notify.show` with screenshot path
- `calendar.event_imminent` with location URL → preload

ai-browser daemon socket is stable. Plugin is a thin RPC adapter. Missing daemon → `unauthenticated`-class error. Effort: M (~2 days).

### E. Codex broker → `codex` plugin

**Actions**: `codex.ask`, `codex.plan`, `codex.delegate`, `codex.review`, `codex.job_status`, `codex.job_result`.

**Events**: `codex.job_completed {id, verdict}` — addresses the "codex 현황 확인" ask via subscribe rather than poll. Origin: `internal`.

**Triggers**:
- `todo.start_requested` → `codex.plan` before tab opens
- `claude.commit_blocked {reason=missing-review}` → `codex.review`, then re-fire commit

Depends on Option A for the second example. Effort: M (~2 days).

### D. `/handoff` and `/catchup` ↔ KB plugin

`/catchup` writes `~/docs/{daily,weekly,topics}`. `/handoff` writes `~/.claude/handoffs/latest.md`. KB plugin manages the same tree.

- `/catchup` skill → `nestctl call kb.ensure` instead of direct `Write`.
- `/handoff` skill → optionally inline `nestctl call todo.list` (in_progress) + `nestctl call event.history --limit 5`.
- Both fall back to direct `Write` if socket connect fails.

Effort: S (~1 day). Includes adding `event.history` (ring buffer, ~half day).

### H. life-assistant bridge

**Goal.** Mirror life-assistant's scheduler output and plugin activity onto
the nestty bus.

**Context.** `~/dev/life-assistant` is substantial on its own — own plugin
runtime (20+ first-party plugins), `robfig/cron` scheduler, dashboard with
goals/pipelines/triggers/workflows, AI-triggerable post-execution analysis via
`claude -p` subprocess. **Bridge, not absorb.**

**Bridge path (decided: push)**:

life-assistant scheduler gets a ~30 LOC patch: after every plugin `Execute`,
call `nestctl event publish lifeassistant.job_completed --json '{...}'` (or
the structured equivalent). Same hook point that already routes to Discord.
Non-blocking; on `nestctl` failure, life-assistant continues with Discord
delivery as today. Trade-off: cross-repo coupling, but cleanest event
semantics. Alternatives (Discord-filter, REST poll, log tail) listed in commit
notes for future revisit.

**`lifeassistant` plugin** (nestty side):

- Actions: `lifeassistant.list_jobs`, `lifeassistant.run_job`, `lifeassistant.job_status`, `lifeassistant.list_users`.
- Events received (external origin, from the bridge):
  `lifeassistant.job_completed`, `lifeassistant.job_failed`, `lifeassistant.notification`.

**Triggers** (require `accept_external = true`):
- `lifeassistant.job_completed {plugin=pricealert}` → `notify.show` on laptop
- `lifeassistant.job_failed` → `todo.create {workspace=life-assistant}`
- `lifeassistant.job_completed {plugin=newscurator}` → `kb.ensure` daily note

Effort: M (~2 days). Plugin = REST adapter + event receiver. Plus the
~30 LOC life-assistant patch.

### I. Cron triggers — nestty-native scheduler primitive

**Goal.** Time-driven triggers for nestty-internal automation that doesn't
belong in life-assistant (e.g., "every 30min refresh ai-browser session
cookie", "every hour if any todo is `blocked` notify").

**Surface**:

```toml
[[triggers]]
name = "refresh-browser-cookie"
[triggers.when]
cron = "*/30 * * * *"        # 5-field cron
timezone = "Asia/Seoul"      # default: system TZ
on_missed = "skip"           # skip | run_latest | run_all (default skip)
[triggers.then]
action = "browser.eval"
params = { js = "..." }
```

**Implementation** (codex-flagged: not just `tokio::time::Interval`):

- Use `tokio-cron-scheduler` or hand-rolled with `chrono-tz`. TZ + DST handled
  by `chrono-tz`.
- **Missed-run policy** after laptop sleep / daemon restart:
  - `skip` (default): drop missed fires
  - `run_latest`: fire once with `missed: true` payload flag
  - `run_all`: fire each missed slot in order
- **Dedupe on config reload.** Each cron entry keyed by `(trigger_name, cron_expr, timezone)`. Reload diffs the set and reuses unchanged entries' next-fire time. No re-firing the same slot after `triggers.toml` edit.
- **Per-trigger named events.** Each cron entry publishes
  `time.<trigger_name>`, not a shared `time.tick`. Multiple cron triggers
  don't collapse into one event stream.
- Origin: `internal` (daemon-published).

~300 LOC + `chrono-tz` + `tokio-cron-scheduler` deps. Effort: S (~1 day) for
the sink + ~half day for TOML schema + tests.

### B. Claude session monitor panel

Visualization slice on top of A + E + H. Single `panel.html`: current
session, dirty files, reviewed marker state, last 20 hook fires timeline,
in-flight codex jobs, in-flight life-assistant jobs. Subscribes to `claude.*`,
`codex.*`, `lifeassistant.*` via WebKit panel JS bridge.

Effort: S (~half day per data source).

### Parked

- **`tmux-powertools` deeper integration** — wait for concrete pain point.
- **`workflow-audit` panel** — defer until audit-run cadence justifies.

## Trust boundary — `nestctl event publish` auth

Codex flagged: adding socket-driven event publish lets any process with socket
access synthesize events. If a trigger binds `system.spawn` to that event,
arbitrary code runs. With SSH `RemoteForward`, that includes any process on
the remote account.

**Decision: source-tagged events + two-step trigger opt-in** (Option (e) above).

### Mechanism

1. **Event bus carries `origin` per event.**
   - `internal` — plugin stdio publishes, daemon-internal code (Phase 14
     chained `<action>.completed`, cron `time.*`, action-result events).
   - `external` — socket clients via `nestctl event publish`. Includes hook
     fires, life-assistant bridge, manual CLI invocations, anything reaching
     the bus through the socket.
2. **TriggerEngine fan-out filters by origin.** A trigger fires on an event
   if either:
   - event origin = `internal`, OR
   - event origin = `external` AND trigger declares
     `[security] accept_external = true`.
3. **Privileged actions need a second opt-in.** Actions that can spawn
   processes or write outside the KB sandbox (`system.spawn`,
   `git.worktree_add` with arbitrary paths, anything that touches `exec`-class
   surface) require the trigger to also declare
   `[security] allow_privileged = true`.
4. **Default-deny on both axes.** A freshly authored trigger fires only on
   internal events and cannot run privileged actions. To wire a hook event
   to a `system.spawn`, the user explicitly opts in twice — making the trust
   decision visible in TOML review.

### Example TOML

```toml
[[triggers]]
name = "commit-blocked-toast"
[triggers.when]
event = "claude.commit_blocked"
[triggers.security]
accept_external = true        # hook events are external
[triggers.then]
action = "notify.show"        # not privileged
params = { title = "Commit blocked", body = "${event.reason}", level = "warn" }
```

```toml
[[triggers]]
name = "auto-codex-review-on-block"
[triggers.when]
event = "claude.commit_blocked"
condition = { "event.reason" = "missing-review" }
[triggers.security]
accept_external = true
allow_privileged = true       # codex.delegate spawns a subprocess
[triggers.then]
action = "codex.delegate"
params = { prompt_file = "${state_dir}/last-review.md" }
```

### What this buys, what it doesn't

**Buys:**
- No new platform code; pure Rust logic in the bus + trigger engine.
- SSH compatibility preserved: `RemoteForward` still works; remote-originated
  events are simply tagged external, like all other socket publishes.
- Two-step opt-in for privileged combos: user can't accidentally wire a
  hostile-controllable event to a code-exec action.
- Existing TOML stays valid (internal-only triggers don't need the new clauses).

**Doesn't buy:**
- Protection against same-UID processes that read the user's existing TOML
  and *legitimately* fire allowed triggers. That's the existing Unix
  same-UID trust model and is out of scope for this pivot.
- Protection against a compromised plugin (stdio publishes are internal).
  Plugin sandboxing is a separate concern.

### Implementation notes

- `event_bus::Origin { Internal, External }` lives as a field on
  `event_bus::Event` (not as a publish argument) — `TriggerEngine` reads
  the origin off the `&Event` it dispatches against. Default = `Internal`;
  the daemon's `handle_events_publish` is the single chokepoint that
  stamps `External`.
- `TriggerEngine::dispatch` gates BEFORE evaluating user-supplied
  `condition`: a misconfigured condition that always returns true must
  not subvert the two opt-ins.
- Privileged-action list lives in `nestty_core::trigger::is_privileged_action`
  — a static `matches!(action, "system.spawn")` for now. Registry-marked
  privileged actions (via `ActionRegistry::register_privileged`) are a
  follow-up; the canonical dangerous action (`system.spawn`) is intercepted
  outside the registry today and only needs the static check.
- Migration: existing triggers without `[security]` parse cleanly as
  `SecurityBlock::default()` (both flags false). No breakage.
- `nestctl event publish --quiet` exits 0 on transport failures so hook
  scripts don't break when nesttyd is down. Schema errors still exit 1
  even under `--quiet` — a malformed publish call is a caller bug, not a
  transport failure.

#### Known gaps (follow-up tickets)

These were flagged during plan review (`/codex-plan` round 2 + /cross-review
round 3-4) and deliberately scoped out of the first slice:

- **Bridge wire origin propagation.** `protocol::Event` (the wire format
  for daemon ↔ GUI bridge) doesn't carry an origin field. An external
  event published on the daemon bus crosses to the GUI bus without
  origin info and reaches GUI-side triggers as `Internal`. Acceptable
  for Option A's threat model (the dangerous action — `system.spawn` —
  lives daemon-side), but a Linux GUI-side trigger that calls a
  privileged action on a hook-originated event would NOT be gated.
- **External events cannot satisfy ANY pending await.** Conservative
  workaround for the laundering risk: without per-pending origin
  tracking, an external event satisfying an internal trigger's
  `await` clause would publish an `Internal`-tagged `<trigger>.awaited`
  event, and downstream triggers without `accept_external` could chain
  on external-derived data. We close the laundering hole today by
  skipping the await state machine entirely for `External` events at
  `TriggerEngine::dispatch`. The trade-off: a legitimate
  external-accepting trigger (e.g. accept_external=true, awaits a
  follow-up `slack.dm`) cannot complete via an externally-published
  follow-up event either. Fix when needed: store `origin` on
  `PendingAwait` + propagate to the synthesized `.awaited` event, then
  drop only when policy mismatches.
- **Causal taint for `.completed` events.** Action `.completed` events
  from the registry default to `Internal` regardless of the originating
  trigger's input event origin. A trigger with `accept_external = true`
  can fire an action; the resulting `.completed` is `Internal`, and
  downstream triggers without `accept_external` can chain on it.
  Mitigation if exploited: rewrite the completion stamper to inherit
  origin from the causal chain (same fix shape as `.awaited`).
- **macOS FFI origin plumbing.** `nestty-ffi` and the Swift `BusEvent`
  don't carry origin. Out of scope while macOS shell stays a stub.
- **Registry-marked privileged actions.** `ActionRegistry::register_privileged`
  is not wired yet. The static `is_privileged_action` list is the only
  privileged set today.

~200 LOC + audit. Effort: M (~1 day). Done in commit-of-record below.

## SSH considerations

With the daemon running at boot under systemd/launchd, the local-machine SSH
problem disappears: hooks always have a daemon to talk to.

For *remote* SSH sessions (Claude Code on a remote box, hooks fire there),
the recommended path remains `~/.ssh/config` `RemoteForward` of the
well-known socket:

```sshconfig
Host my-remote
    RemoteForward ${XDG_RUNTIME_DIR}/nestty/socket ${XDG_RUNTIME_DIR}/nestty/socket
```

Hooks on the remote then hit a local socket forwarding to the workstation
daemon. Unix socket only — no TCP, no new attack surface beyond SSH itself.

**Remote-originated events are tagged `external`**, identical to local hook
publishes. The trust-boundary opt-in covers them uniformly: triggers that
want to fire on SSH-originated hook events must declare
`accept_external = true`. No special case for SSH.

Fallback path preserved: hooks silently skip publish when socket connect
fails. Machines without the forward keep working, just without nestty bus
delivery.

## CLI completeness

Every action must be reachable from `nestctl` without GUI. Audit checklist:

| Surface | Today | After pivot |
|---|---|---|
| Plugin actions (`<plugin>.<verb>`) | `nestctl plugin run` works | Same |
| Tab / split / window mgmt | `nestctl tab/split` (GUI required) | Same — daemon proxies to primary GUI, returns `no_gui` when none |
| Event subscribe | `nestctl event subscribe` works | Same |
| Event publish | **missing** | **Add** (Option A prerequisite) — emits `external` origin |
| Event history | **missing** | **Add** (Option D) — ring buffer, ~half day |
| Notify | **missing** | **Add** as `notify.show` action |
| Service / plugin lifecycle | `nestctl plugin list/start/stop` | Same |
| Trigger reload / debug | partial | Audit + complete |

Nothing in the daemon should be GUI-reachable but not CLI-reachable.

## Cross-cutting needs

- **`notify.show` action** — backed by `Notifier`. **Shipped** in decisions.md #38. Subprocess on both platforms (`notify-send` Linux, `osascript -e 'on run argv'` macOS — argv-passed to avoid AppleScript injection). Registered `blocking_silent` on both daemon and GUI in-process registries so the trigger pump never stalls and `.completed` doesn't fan-out. zbus / direct D-Bus deferred unless burst rates from life-assistant Option H prove the subprocess cost matters.
- **`event.history` action** — in-memory ring buffer of last N events, read-only. ~half day. Used by Option D + B.
- **Service install** — install scripts ship the unit/plist. ~half day each.
- **Privileged-action audit** — list every action that warrants
  `allow_privileged` gating. ~half day.

## Infrastructure debt

Codex flagged these as more urgent under daemon-first, not less:

- **Phase 9.4** — `try_dispatch` spawns one OS thread per blocking call.
  Hook bursts (Option A) and life-assistant fan-out (Option H) make this
  critical earlier than the original roadmap assumed. Fix: shared bounded
  thread pool. **Move to step 3 of sequencing**, not "optional later".
- **Phase 9.5** — `pdeathsig` rolled back. A long-running daemon under
  systemd/launchd that orphans plugin children on SIGKILL/segfault is worse
  than the GUI case. Re-introduce via dedicated spawner thread or
  `pidfd_open` + epoll. Bundle with daemon scaffolding (migration step 2).
- **Phase 10** — `calendar.list_events` blocks supervisor on slow Google API.
  Independent of daemon split; tackle when 9.4's bounded pool lands.

## Out of scope

- **Absorbing life-assistant.** Owns its own world. Bridge (Option H).
- **TCP / network socket on the daemon.** SSH `RemoteForward` covers remote
  access. TCP means auth tokens and a new attack surface.
- **Multi-user daemon.** Single-UID. Each user runs own `nesttyd`.
- **Replacing Discord/Slack clients in the GUI.** nestty mirrors events;
  reading/replying stays in those clients (or via plugin actions).
- **macOS shell pivot to daemon socket-client.** macOS currently has its own
  Swift `SocketServer.swift` plus Rust trigger engine via `nestty-ffi`
  (in-process embed). Pivoting macOS to talk to `nesttyd` over the same
  socket protocol is a separate phase — needs its own design (XPC vs Unix
  socket from a sandboxed AppKit app, launchd integration, retiring or
  re-scoping `nestty-ffi`). For this plan, macOS keeps the in-process FFI
  embed; Linux daemon ships first.

## Suggested sequencing

1. **GUI ↔ daemon protocol spec** (Migration step 1) — design deliverable, no code. ~half day.
2. **Daemon scaffolding** (step 2) — three thin commits per Migration step 2's slicing: 2a daemon binary + ping + permission hardening (~½ day), 2b supervisor + trigger_sink import (~½ day), 2c pdeathsig / orphan reaping (~½ day). Total ~1.5 days.
3. **Phase 9.4 thread pool** — bounded-worker pool before hook/life-assistant bursts arrive. ~1 day.
4. **Relocate supervisor + trigger_sink** (step 3) — ~half day.
5. **Split socket.rs + dual dispatch under flag** (step 4) — ~2 days.
6. **Switch dispatch to GUI client mode** (step 5) — ~1 day + soak day.
7. **Install scripts + socket path audit** (step 6) — ~1 day.
8. **Cross-cutting: `Notifier` + `notify.show` + `nestctl event publish` (with origin tagging) + `event.history` + privileged-action audit** — ~1.5 days bundled.
9. **Trust boundary — `[security]` block in trigger TOML** — ~half day. Land before A so hooks have the opt-in mechanism available.
10. **Option A** (`claude` plugin) — ~1–2 days.
11. **Option I** (cron triggers) — ~1 day.
12. **Option H** (life-assistant bridge + scheduler patch) — ~2 days.
13. **Option B** (monitor panel) — ~1 day on top of A+E+H.
14. **Option C** (`browser` plugin) — ~2 days.
15. **Option E** (`codex` plugin) — ~2 days.
16. **Option D** (skill ↔ KB sync) — ~1 day.

Steps 1–12 (~12 working days) deliver: daemon-first nestty with Claude hooks
+ life-assistant + time triggers on the bus, secured by the trust boundary,
with the monitor panel showing all data streams. That's the "personal
automation hub" working end-to-end. Everything after composes on that
foundation.
