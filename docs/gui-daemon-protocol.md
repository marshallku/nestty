# GUI ↔ Daemon Protocol

Design contract for the daemon-first pivot. Read alongside
[harness-integration.md](./harness-integration.md) — this doc is the
load-bearing detail behind that plan's "GUI ↔ daemon protocol" subsection
and Migration step 1.

**Status:** spec, no implementation yet. Step 1 of sequencing.

## Why this is its own doc

The pivot moves the trigger engine / supervisor / event bus out of the GUI
process and into `nesttyd`. GUI-owned commands (`tab.*`, `split.*`,
`terminal.*`, `webview.*`, `background.*`, `statusbar.*`, `plugin.open`,
`agent.approve` UI prompts) still need to mutate GUI state — but the
*caller* (a trigger, a `nestctl` invocation, a plugin's action chain) now
lives in the daemon.

Current socket is one-way: clients send `Request`, server replies with
`Response`, plus a separate streaming subscription for `Event`. The pivot
needs **bidirectional request/response** because the daemon must now invoke
methods *on the GUI*. Today's protocol has no such concept.

This doc specifies the minimum additive changes to support that without
breaking `nestctl` or any plugin.

## Baseline (what stays unchanged)

The current **wire format** is preserved verbatim. Plugins (which read
`NESTTY_SOCKET` from env set by the supervisor) and any third-party tool
that reads `NESTTY_SOCKET` continue to work without recompilation.

**Socket path changes**, so anything that hardcodes `/tmp/nestty-*.sock` or
uses the current `nestctl` discovery glob will not find the daemon socket
until rebuilt or env-injected. The full consumer list is in
[harness-integration.md § Socket path consumer audit](./harness-integration.md).
`nestctl` itself is rebuilt as part of the workspace, so the audit's job is
external scripts and per-machine config.

### Transport

- Unix socket at well-known path (see `nestty_core::paths::socket_path`):
  - Linux: `${XDG_RUNTIME_DIR}/nestty/socket` when set, else
    `/tmp/nestty-{uid}/socket` (uid-namespaced so multi-user `/tmp` doesn't
    race on first-binder).
  - macOS: `~/Library/Caches/nestty/socket`.
- Newline-delimited JSON. One JSON object per line.
- The daemon honors `NESTTY_SOCKET` (override path) and accepts an optional
  `--legacy-socket /tmp/nestty.sock` flag for a transitional second listener
  during the audit-and-update window. The flag is meant to be temporary and
  removed before final release.

### Message types (from `nestty-core/src/protocol.rs`)

```rust
struct Request  { id: String, method: String, params: Value }
struct Response { id: String, ok: bool, result: Option<Value>, error: Option<ResponseError> }
struct ResponseError { code: String, message: String }
struct Event    { type: String, data: Value }
```

A connection currently sees two shapes from the server:

1. **Response** lines correlated to client `Request.id`.
2. **Event** lines on a subscription-mode connection (after sending
   `event.subscribe`).

Clients dispatch by inspecting the keys: a line with `id` + `ok` is a
Response; a line with `type` + `data` is an Event.

## What changes

Four additions, no removals:

1. **GUI client registration** — new `gui.register` method. The daemon learns
   which connections are GUI processes, what they can do, and which one is
   primary.
2. **Daemon-to-client invocation** — a new line shape, `Invoke`, sent from
   daemon to a registered GUI. The GUI replies with a normal `Response`
   carrying the matching `id`. This is how `tab.*` / `webview.*` etc. reach
   the GUI after the migration.
3. **Event `origin` field** — `Event.data` gains a top-level
   `_origin: "internal" | "external"` key, consumed by the trigger engine's
   security gate. CLI consumers can ignore it; existing JSON parsing keeps
   working.
4. **Optional `target_client_id` on `Request`** — a new top-level optional
   field (serde-skip-if-none, default `None`) lets a caller address a
   specific registered GUI for GUI-owned methods. Existing serializers
   omit the field; existing parsers accept Requests without it. Daemon-only
   methods and plugin actions ignore the field — the daemon strips it before
   forwarding to plugins.

Existing `Response` is unchanged. `Request` gets one optional additive field
(see #4) — wire-compatible with every existing client because both
serializers and parsers tolerate its absence. Existing
`LEGACY_DISPATCH_METHODS` keep their names. The daemon decides per-method
whether to handle locally or proxy to a GUI via `Invoke`.

## Connection lifecycle

### Generic client (CLI, plugin, hook)

Same as today:

```
client connect
  → Request { method: "tab.new", id: "abc" }
  ← Response { id: "abc", ok: true, result: ... }
client disconnect
```

No `gui.register` needed. The daemon treats unregistered connections as
"generic". They can call any daemon-owned method and can call GUI-owned
methods, which the daemon proxies to the primary GUI (returns `no_gui` if
none).

### GUI client (nestty / Nestty.app)

```
gui connect
  → Request { method: "gui.register", id: "abc",
              params: { window_id, capabilities, display, want_primary } }
  ← Response { id: "abc", ok: true,
               result: { client_id, primary: true|false } }
  ... normal operation, see "Daemon → GUI invoke" below ...
gui disconnect (or heartbeat timeout)
  → daemon transfers primary slot if any other GUI is registered
```

A connection BECOMES a GUI at the moment its `gui.register` call succeeds.
Before that, it is treated as a generic client — it can call any
daemon-owned method, subscribe to events via `event.subscribe`, etc.
Daemon-side state for that connection (pending subscriptions, in-flight
requests) carries over unchanged when `gui.register` succeeds; the daemon
just additionally records `client_id` and capabilities, and is now allowed
to issue `Invoke` on the connection.

There is no "must send register as first message" constraint — a hook
script that opens a socket, publishes an event, and disconnects is a
perfectly valid generic client and never registers. Conversely, a GUI that
takes a few ms to construct its window state and only then sends
`gui.register` is equally fine.

A connection can register only once per lifetime. A second `gui.register`
on the same connection returns `error.code = "already_registered"`.

### Connection identity

- The daemon assigns `client_id` (UUID v4) on `gui.register`. The GUI uses
  this id implicitly — every byte on this socket connection belongs to that
  `client_id`. There is no explicit `client_id` field in subsequent messages
  from the GUI; the connection itself is the identity.
- The daemon may include `target_client_id` in `Invoke` messages it sends —
  but since `Invoke` flows over a specific connection, the target is implicit
  there too. The field exists for logging/debugging in the daemon, not on
  the wire.

## `gui.register` schema

```jsonc
// Request
{
  "id": "abc",
  "method": "gui.register",
  "params": {
    "window_id": "<uuid generated by GUI>",
    "capabilities": ["tab", "split", "webview", "background",
                     "statusbar", "agent.ui", "plugin.open",
                     "terminal", "search"],
    "display": "wayland-0",         // or "x11:0", "macos-cg", etc. Informational.
    "want_primary": true,            // bid for primary slot
    "version": "0.1.0",              // GUI build version, informational
    "protocol_version": 1            // GUI's expected wire protocol version
  }
}

// Response (success)
{
  "id": "abc",
  "ok": true,
  "result": {
    "client_id": "550e8400-e29b-...",
    "primary": true,                // true if this GUI now holds primary
    "daemon_version": "0.1.0",
    "protocol_version": 1
  }
}

// Response (capability mismatch / version skew)
{
  "id": "abc",
  "ok": false,
  "error": { "code": "incompatible",
             "message": "daemon protocol_version=2, gui sent protocol_version=1" }
}
```

### Capabilities

A string list. The daemon uses it to decide whether a GUI-owned method can be
proxied to a given client. A minimal GUI omits capabilities for features it
doesn't render.

Initial vocabulary (extensible):

| Capability | Methods this gates |
|---|---|
| `tab` | `tab.new`, `tab.close`, `tab.list`, `tab.info`, `tab.rename`, `tabs.toggle_bar`, `claude.start` |
| `split` | `split.horizontal`, `split.vertical` |
| `terminal` | `terminal.read`, `terminal.state`, `terminal.exec`, `terminal.feed`, `terminal.history`, `terminal.context` |
| `webview` | all `webview.*` |
| `background` | all `background.*` |
| `statusbar` | all `statusbar.*` |
| `agent.ui` | `agent.approve` (interactive prompt) |
| `plugin.open` | open a plugin panel |
| `search` | in-terminal search |
| `session` | `session.list`, `session.info` |

If a method is dispatched and no registered GUI advertises the matching
capability, the daemon returns `no_gui` — same as no GUI at all.

### `want_primary` and primary policy

- If no GUI is currently primary, the first GUI to register with
  `want_primary: true` becomes primary. `want_primary: false` registers a
  secondary.
- A GUI may later bid via `gui.set_primary` (new method) at any time; the
  daemon transfers primary atomically — pending `Invoke`s already on the
  previous primary's connection complete normally.
- When the primary disconnects, the daemon picks the next secondary that
  declared `want_primary: true` at registration (most recent first). If no
  candidate exists, no primary; GUI-owned methods return `no_gui`.

## Daemon → GUI invoke

New line shape. The daemon initiates a request; the GUI replies with a
standard `Response`.

```jsonc
// Daemon → GUI (on a registered GUI connection)
{
  "id": "daemon-gen-uuid",         // daemon-issued correlation id
  "invoke": "tab.new",              // method name
  "params": { ... }                 // method-specific
}

// GUI → Daemon (on same connection)
{
  "id": "daemon-gen-uuid",          // echoes the daemon's id
  "ok": true,
  "result": { ... }
}
```

### Distinguishing from a normal Request

The `Invoke` shape uses `invoke` instead of `method` as the verb-bearing
field. So a single connection sees four possible JSON shapes:

| Direction | Shape | Discriminator |
|---|---|---|
| Client → Daemon | Request | has `method` |
| Daemon → Client | Response | has `id` + `ok` |
| Daemon → Client | Event | has `type` + `data` |
| Daemon → GUI | Invoke | has `invoke` |

The GUI's reader switches on these four. The CLI's reader keeps the existing
two-way switch — it never sees Invoke because it never registers as a GUI.

(Alternative considered: reuse `Request` for `Invoke` with the daemon as the
sender. Rejected because plugins and clients also send `Request`, and a
single discriminator field (`invoke` vs `method`) makes intent crisp on
inspection — important for debugging `nestctl event subscribe` output.)

### Correlation and timeouts

- The daemon maintains a pending-Invoke map: `id → (oneshot sender, deadline,
  method)`.
- Default timeout: 5 seconds for fast UI ops (`tab.new`, `split.*`,
  `webview.navigate`, etc.). Methods that can legitimately take longer
  (e.g., `webview.execute_js` against a slow page) declare a per-method
  timeout in the daemon's method table.
- On timeout: daemon completes the original CLI/trigger request with
  `error.code = "gui_timeout"`, drops the pending entry.
- On GUI disconnect mid-invoke: daemon completes pending entries for that
  connection with `error.code = "gui_disconnected"`.

### Concurrency

A single GUI connection can have multiple in-flight invokes. The GUI MUST
process them with whatever concurrency the GTK / AppKit main loop allows —
typically serial on the main thread for state mutation. Order of `Response`
lines on the wire doesn't have to match issue order; correlation is by `id`
only.

## Routing rules

### GUI-owned vs daemon-owned method subset

`LEGACY_DISPATCH_METHODS` is a flat list of historical socket methods. Only
a **subset** of them is genuinely GUI-owned (touches `TabManager` /
`BackgroundLayer` / `StatusBar` / WebKit panel / VTE PTY). The rest are
daemon-owned and stay handled inline post-migration.

**GUI-owned subset** (must route to a registered GUI):

| Capability | Methods |
|---|---|
| `tab` | `tab.new`, `tab.close`, `tab.list`, `tab.info`, `tab.rename`, `tabs.toggle_bar`, `claude.start` |
| `split` | `split.horizontal`, `split.vertical` |
| `terminal` | `terminal.read`, `terminal.state`, `terminal.exec`, `terminal.feed`, `terminal.history`, `terminal.context` |
| `webview` | all `webview.*` |
| `background` | `background.set`, `background.clear`, `background.next`, `background.toggle`, `background.set_tint` |
| `statusbar` | `statusbar.show`, `statusbar.hide`, `statusbar.toggle` |
| `agent.ui` | `agent.approve` |
| `plugin.open` | `plugin.open` (opens panel — UI side) |
| `session` | `session.list`, `session.info` (per-window tab sessions) |

`claude.start` is GUI-owned because the current implementation
(`nestty-linux/src/socket.rs:1394` onward) takes `TabManager` +
`ApplicationWindow` and calls `add_tab_with_cwd_and_initial_input` —
it returns `panel_id` and `tab` references, so its response contract is
GUI-bound. Listed under `tab` capability.

**Daemon-owned residue in `LEGACY_DISPATCH_METHODS`** (handled inline,
do NOT route to GUI):

- `theme.list` — reads theme directory
- `plugin.list` — queries supervisor

The implementation should drive routing from this capability table, not
from raw membership in `LEGACY_DISPATCH_METHODS`. Methods added in the
future are classified at registration time, not via the legacy list.

### Dispatch order

When the daemon receives a CLI `Request`:

1. **Daemon-owned method?** (`event.*`, `plugin.run` for plugin actions,
   `todo.*` CLI shortcuts, all plugin-action proxies, `notify.show`,
   `theme.list`, `plugin.list`, etc. — *not* `claude.start`, which is
   GUI-owned per the table above) → handle inline, reply on the same
   connection.
2. **GUI-owned method?** (matches the capability table above)
   - If `target_client_id` is set: route to that specific client per the
     "Explicit targeting" precedence rules.
   - Otherwise: if a primary GUI is registered AND advertises the matching
     capability, issue an `Invoke` on that connection, await the `Response`,
     forward `result` / `error` to the original CLI client (preserving
     `error.code`).
   - Otherwise: reply with `error.code = "no_gui"`, message = "no nestty
     window attached for capability `<cap>`; start nestty or pass
     `--target_client_id` to an alternate GUI".
3. **Unknown method?** → `error.code = "unknown_method"`.

### Explicit targeting

CLI may pass `--target_client_id <id>` (new `nestctl` flag) to address a
specific GUI by id. The flag value is placed in the `Request.target_client_id`
top-level optional field (not inside `params`, so method-specific param
schemas never collide):

```jsonc
{
  "id": "abc",
  "method": "tab.new",
  "target_client_id": "550e8400-e29b-...",   // optional; absent = primary
  "params": { ... }
}
```

Routing precedence:

1. If `target_client_id` is present and refers to a registered GUI advertising
   the matching capability → invoke that specific client.
2. If `target_client_id` is present but the id is unknown or lacks the
   capability → `error.code = "unknown_client"` (new) or `"no_gui"`
   respectively.
3. If absent → primary GUI per the policy in `gui.register` Response. If no
   primary → `no_gui`.

Daemon-only methods (anything not in the GUI-owned capability table above —
this includes all plugin actions, all `event.*`, `notify.show`, `theme.list`,
`plugin.list`, etc.) ignore `target_client_id`. The daemon also strips the
field before forwarding to plugin stdio so plugin protocol stays unchanged.

## Events with origin

Existing `Event { type, data }` shape unchanged on the wire. The daemon adds
a stable key to `data`:

```jsonc
{
  "type": "claude.commit_blocked",
  "data": {
    "_origin": "external",           // or "internal"
    "session_id": "abc-123",
    "repo": "/home/marshall/dev/nestty",
    "reason": "missing-review"
  }
}
```

`_origin` is one of:

- `internal` — plugin stdio publishes; daemon-internal code (Phase 14
  chained `<action>.completed`, cron `time.*`, action-result events).
- `external` — events arriving via `nestctl event publish`, including hook
  fires, life-assistant bridge, manual CLI invocations.

`_origin` is set by the daemon at publish time; clients cannot fake it.

TriggerEngine consults `_origin` against each trigger's `[security]` clause
(see `harness-integration.md` § Trust boundary). Subscribers see `_origin`
verbatim in the event payload — useful for the monitor panel to badge
external vs internal flows distinctly.

The reserved `_`-prefix on `_origin` is to avoid collision with plugin event
payload keys. Plugins are not allowed to set `_origin`; the daemon
overrides any client-supplied value.

## Subscriptions

Two distinct paths, kept separate to satisfy the "existing clients keep
working" promise:

### Generic subscription (unchanged)

Any client (CLI, plugin, third-party tool) sends:

```jsonc
{ "id": "abc", "method": "event.subscribe", "params": { "patterns": ["claude.*"] } }
```

Daemon streams Events matching the pattern set on the same connection. No
pattern = all events. Same shape as today. Plugin stdio subscribers
(`subscribes` field in `plugin.toml`) hit the same path internally.

### GUI subscription (new)

A registered GUI is **automatically subscribed to all events** at
`gui.register` time. No separate `event.subscribe` needed — the monitor
panel, tab badge, statusbar widgets all want broad visibility, and adding
the round-trip is friction.

A GUI may narrow via `gui.subscribe { patterns: [...] }` (optional, sets the
filter set replacing the implicit all-match) and `gui.unsubscribe` (clears
filter and stops receiving Events on this connection). These are
GUI-connection methods, not bus operations.

Heartbeat (next section) is delivered over `Invoke`, not via the event
stream, so subscribers never see ping traffic regardless of which
subscription path they used.

## Heartbeat

Heartbeat is point-to-point on registered GUI connections only. It uses the
`Invoke` shape (daemon → GUI), keeping it strictly off the event bus so
non-GUI subscribers — `nestctl event subscribe`, plugin `subscribes` —
never see ping traffic and stay byte-compatible with today's wire output.

```jsonc
// Daemon → GUI every 10s on each registered GUI connection
{ "id": "daemon-gen-uuid", "invoke": "_ping", "params": { "ts": 1234567890 } }

// GUI → Daemon (same connection, normal Response)
{ "id": "daemon-gen-uuid", "ok": true, "result": { "ts": 1234567890 } }
```

If two consecutive `_ping` invokes go without Response within 20s + jitter,
the daemon considers the connection dead, drops the registration, fails
pending invokes with `gui_disconnected`, transfers primary if applicable.

`_ping` reuses the Invoke shape rather than introducing a fifth line type —
keeps the wire vocabulary at four shapes (Request / Response / Event /
Invoke) and lets the GUI's existing Invoke handler dispatch it as a
no-op method. Round-trip latency is also measurable from the same
correlation map used for real methods, which is useful for `nestctl plugin
diagnose gui` style introspection.

## Error vocabulary additions

New codes on `Response.error.code`:

| Code | Meaning |
|---|---|
| `no_gui` | GUI-owned method dispatched but no GUI with matching capability registered |
| `gui_timeout` | GUI didn't respond to Invoke within the method's timeout |
| `gui_disconnected` | GUI connection dropped mid-Invoke |
| `incompatible` | `gui.register` protocol/version mismatch |
| `unregistered` | A method requiring `gui.register` was called on an unregistered connection (currently none — placeholder for future GUI-internal ops) |
| `unknown_client` | `target_client_id` referred to a client_id that is not currently registered |
| `already_registered` | Second `gui.register` attempted on a connection that already holds a `client_id` |

Existing codes (`unknown_method`, `invalid_params`, plugin-specific codes)
unchanged.

## Versioning

`protocol_version: 1` in the `gui.register` response. The GUI compares
against its compiled-in expected version:

- Equal: proceed.
- GUI newer than daemon: GUI continues with daemon's older version (graceful
  degrade). Optional in-app warning.
- Daemon newer than GUI: daemon may refuse with `error.code = "incompatible"`
  if the version delta crosses a breaking change. v1 → v2 is the next
  decision point.

Backward-compatible additions (new capabilities, new methods, new event
types) don't bump `protocol_version`. Breaking changes (removing a field,
changing a discriminator) do.

## What `nestctl` doesn't see

`nestctl` connects as a generic client. It does not register. It does not
participate in Invoke. Concretely:

- `nestctl tab new` → daemon proxies to primary GUI → reply forwarded back.
  Looks identical to today from the CLI's view.
- `nestctl event subscribe` → daemon streams Events, including `_origin`
  field which CLI may surface or ignore. Existing parsers that read only
  `type` + `data` keep working.
- `nestctl event publish foo.x --json '{"k":"v"}'` (new) → daemon tags origin
  external, publishes on bus.

CLI clients never need to know `client_id` or `gui.register` unless they
explicitly want to address a specific GUI (`--target_client_id`).

## Migration semantics

**Step 5a (current):** the GUI always starts `gui_client::spawn()` —
no env-var gate. It opens a daemon-attached connection at startup and
reconnects (1→30s backoff) whenever the daemon goes away. The
in-process `socket::dispatch` path is still wired for the legacy
nestctl-via-per-instance-socket flow; daemon→GUI Invokes route through
the daemon path. Daemon-absent is benign: the reconnect loop polls
quietly while the GUI runs entirely through its in-process supervisor.

**Step 5b (next, separate phase):** the in-process plugin supervisor
inside nestty-linux is removed and the daemon becomes the sole plugin
host. After this point the GUI cannot run plugins without a daemon
attached. The standalone build feature in § Resolved decisions #3
covers single-user no-systemd setups and CI by re-embedding the daemon
in-process when compiled with `--features standalone`.

Earlier sequencing steps (4a → 4b) migrated each GUI-owned method one
at a time so a regression in one path didn't break the others. By the
time Step 5a flipped the default-on switch, every method in
§ Routing rules had been validated through the daemon path with real-
GUI smoke runs (Phase 4b + 9.4 + 9.4.b verification).

## Resolved decisions

These were originally open but resolved during the codex pressure-test pass.
Listed here as the canonical answer to consult during implementation.

1. **Subscription model — GUI auto-subscribes-all on register.** Generic
   clients keep using `event.subscribe { patterns }`. GUIs can narrow via
   `gui.subscribe { patterns }` (replaces filter set) or
   `gui.unsubscribe`. See § Subscriptions.

2. **Heartbeat shape — `_ping` over `Invoke`, not Event.** Keeps the
   non-GUI subscriber stream byte-identical to today's. See § Heartbeat.

3. **Standalone fallback — permanent build feature.** `nestty --standalone`
   (daemon-in-process) ships as a build-flag-gated permanent mode for
   single-user no-systemd setups, CI, and first-use bootstrapping. It is
   not deleted at the end of migration. The migration just switches the
   *default* from in-process to daemon-attached. The harness-integration
   sequencing step that previously said "delete `--standalone`" is updated
   to "switch default; standalone retained behind `--features standalone`".

4. **`target_client_id` placement — top-level optional `Request` field.**
   See "What changes" #4 and "Explicit targeting".

5. **Primary policy — first GUI with `want_primary: true` wins, not first
   to register.** A GUI registering with `want_primary: false` never
   becomes primary by default. See `gui.register` schema.

## Still open (resolve before step 4)

1. **`Invoke` ordering guarantees.** If the daemon issues `tab.new` then
   `tab.list` back-to-back, must the GUI reply in order? Today (in-process)
   they're serialized on the GTK main loop. The new path could reorder.
   Proposed answer: **no ordering guarantee**; callers that need
   "tab.new completes before tab.list" must await the first reply. Most
   callers already do.

2. **Capability evolution.** If a new GUI method is added (say
   `terminal.scroll_top`), does it need a new capability or roll into
   `terminal`? Proposed: **roll into the broadest existing capability**
   unless the new method genuinely depends on optional rendering backends.
   Capabilities are about "can this GUI build do X at all", not
   "is X a separate feature".

These don't block protocol implementation — they shape it. Resolve via the
next codex round before step 4.

## Out of scope for this doc

- **TCP transport.** Unix socket only; SSH remote use via `RemoteForward`.
- **Authentication beyond same-UID.** Unix socket fs permissions cover it.
- **macOS XPC.** Mac uses the same Unix socket protocol. XPC could come
  later if sandboxing requires it; that's part of the separate macOS shell
  pivot (see harness-integration.md § Out of scope).
- **Multi-daemon federation.** Single daemon per user. No "talk to my
  laptop's daemon from my desktop" mode; that's what SSH is for.

## Implementation surface (forward reference)

Code touch points when implementation lands:

- `nestty-core/src/protocol.rs` — add `Invoke` struct, extend Event with
  origin handling helpers.
- `nestty-daemon/src/socket.rs` (relocated transport) — pending-Invoke map,
  GUI registry, capability routing.
- `nestty-daemon/src/gui_registry.rs` (new) — `GuiClient` records, primary
  selection, capability lookup.
- `nestty-linux/src/gui_client.rs` (new) — outbound connection,
  `gui.register` on connect, Invoke handler dispatching to existing
  `gui_handlers` module.
- `nestty-linux/src/gui_handlers.rs` (new — split from `socket.rs`) — the
  GUI-owned half of current dispatch, function-call shaped (not socket
  shaped).
- `nestty-cli/src/main.rs` — optional `--target_client_id` flag, `event
  publish` subcommand.

Tests required at each step are listed in the parent doc's sequencing.
