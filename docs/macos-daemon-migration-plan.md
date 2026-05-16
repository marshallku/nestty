# macOS Daemon-First Migration Plan (v3 — codex round 1/2/3 reflected, user-approved)

## Context

Linux는 monolithic GUI에서 daemon-client 아키텍처로 이전 완료 (`83c5122` "Make daemon-client mode the default"). `nestty-daemon` crate / `nesttyd` binary가 trigger engine, plugin supervisor, action registry, context service, event bus를 host. `nestty-linux` GUI는 그 client.

macOS app (`nestty-macos`, Swift/AppKit + SwiftTerm)은 여전히 monolithic — 모든 것이 in-process. 이 plan은 그 architectural debt를 닫는다.

## Source-of-truth observations (codex-verified)

- `nestty-core/src/paths.rs:20` — macOS runtime dir 이미 `~/Library/Caches/nestty`; `daemon_socket_path()` reusable.
- `nestty-core/src/event_bus.rs:29` — `bridge_id`는 `serde(skip)` local-only field (wire에 안 실림).
- `nestty-daemon/src/main.rs:516` — daemon plugin 자식에 `NESTTY_SOCKET=<daemon socket>` 이미 설정.
- `nestty-daemon/src/main.rs:103-137` — daemon이 ContextService, ServiceSupervisor, `context.snapshot` already-owned.
- `nestty-daemon/src/socket.rs:177` — socket perms (0600) + dir prep 코드 reuse.
- `nestty-daemon/src/gui_registry.rs:24` — GUI env whitelist는 PATH 일부러 제외 (trust boundary).
- `nestty-daemon/src/gui_registry.rs:405` — daemon→GUI wire events에 bridge_id 없음.
- `nestty-linux/src/gui_client.rs:23` — forwarder allowlist (terminal.output 등 제외).
- `nestty-linux/src/gui_client.rs:219, 230, 285, 323` — forwarder ordering, drop guard, bridge_id model.
- `nestty-macos/Sources/Nestty/AppDelegate.swift:58` — `pluginSupervisor.discoverAndStart()` 무조건 실행. PR2에서 conditional.
- `nestty-macos/Sources/Nestty/Keybindings.swift:166` — `action:` keybindings은 local `ActionRegistry.tryDispatch` 호출.
- `nestty-macos/Sources/Nestty/PluginPanelController.swift:145, 180` — local registry + `{"event": ...}` shape 사용.
- `nestty-macos/Sources/Nestty/EventBus.swift:45-52` — 현재 socket event shape `{"event": ...}` (daemon은 `{"type": ...}`); subscriber는 JSON 문자열만 받음.

## Goal

macOS Nestty.app은 nestty-linux의 `gui_client.rs`와 동일한 daemon client. `nesttyd`가 trigger engine, plugin supervisor, action registry, context service의 owner. Nestty.app은 GUI 책임만 (terminal panes, web panels, plugin panels, status bar, menus, keybindings).

## Fallback contract (사용자 결정)

- **Daemon up + ack received** → daemon이 plugin/trigger/context owner. macOS in-process supervisor + engine은 idle.
- **Daemon down (startup failure or disconnect)** → plugin/trigger actions return `daemon_unavailable` RPCError. Local engine은 GUI-only triggers (tab/split/terminal/background)만 fire. Local supervisor는 자동 재시작 안 함 (Linux와 일치, narrower fallback).

## PR sequence

### PR 1 — Daemon Darwin smoke + docs (TINY)

**Goal:** `nesttyd`가 Darwin에서 동작 + 매뉴얼 시작 docs.

- `cargo build --release -p nestty-daemon`이 Darwin에서 통과
- Smoke: `nesttyd &` → `nestctl call system.ping`이 ok 응답
- `docs/macos-app.md`에 "Manual nesttyd start" 섹션 추가, `~/Library/Caches/nestty/socket` 명시
- Tests: `cargo test -p nestty-daemon`이 macOS에서 통과

**Risk:** 매우 낮음. 코드 변경 minimal — 빌드/테스트 통과 + docs.

**Deliverable:** `nesttyd` 가 macOS에서 standalone process로 동작. Nestty.app은 아직 모놀리식.

---

### PR 2 — DaemonClient connect/register + plugin gate + daemon-forward fallback

**Goal:** Nestty.app이 daemon client가 되고, plugin ownership 단일화. 동시에 unmatched method를 daemon으로 forward해서 keybindings/panels/triggers 끊김 방지.

#### 2.1 DaemonClient.swift (new)
- `nestty-linux/src/gui_client.rs` 패턴 mirror
- 경로: `~/Library/Caches/nestty/socket`
- Capped backoff reconnect (100ms → 5s)
- `gui.register` 송신 (capabilities: `tab`, `split`, `webview`, `background`, `statusbar`, `terminal`, `agent.ui`, `plugin.open`, `search`, `session`)
- Ack 파싱 (기존 daemon ack 필드만): `{client_id, primary, daemon_version, protocol_version, host_triggers}` — `plugins_owned` 필드 추가 X (codex round 2 C3)

#### 2.2 Auto-spawn-on-connect with single-flight lock
- 첫 connect 실패 → `~/Library/Caches/nestty/.spawn.lock`에 `flock(LOCK_EX | LOCK_NB)`
- Lock 잡히면: live socket probe (1초 timeout) → 여전히 실패면 detached `nesttyd` fork
- Lock 못 잡으면: 다른 프로세스 spawning 중 → 짧은 sleep + retry connect
- pidfile은 diagnostic metadata only

#### 2.3 Plugin supervisor lifecycle gate (codex C1, C5)
- App start 시 daemon connect 먼저 시도. **성공하면 in-process `PluginSupervisor.discoverAndStart()` 호출 X.**
- Connect 실패 / disconnect: PluginSupervisor 시작 X. Plugin actions은 (5)에서 stub로 처리.
- **PluginSupervisor 클래스 자체는 PR 5에서 삭제** (codex round 3 I4 — physical deletion 미루기, rollback 안전성).

#### 2.4 Daemon-forward fallback (was PR5, codex round 3 C1)
- `SocketServer`의 unknown_method handler가 daemon으로 forward (Linux `daemon_forward.rs` 패턴)
- DaemonClient에 `forward(method, params, completion)` API
- `Keybindings.swift:166` (`action:` dispatch), `PluginPanelController.swift:180` (panel JS bridge), FFI trigger callback이 모두 동일 forwarding path 사용
- Daemon down 시: `daemon_unavailable` 에러 반환

#### 2.5 daemon_unavailable stubs for known actions (codex round 3 I1)
- Manifest discovery (`PluginManifestStore.discover()`)로 known plugin actions 파악
- 각 action을 `ActionRegistry.register`로 stub 등록 → daemon connected이면 forward, 아니면 `daemon_unavailable`
- Unknown method는 `unknown_method` 그대로 (사용자 오타 vs daemon 장애 구분)

#### 2.6 NESTTY_SOCKET routing (codex round 2 C4)
- Daemon-owned 자식: `NESTTY_SOCKET=<daemon socket>` (이미 main.rs:516, 변경 X)
- GUI-owned 자식 (`spawn:` keybindings, statusbar `_module.run` until PR 5): per-GUI socket (현재 동작 유지)
- 새 변수 추가 X

#### 2.7 Tests
- Unit:
  - `gui.register` JSON shape (capabilities array, `plugins_owned` 없음)
  - Single-flight lock under concurrent attempts
  - Plugin gate: daemon up → discoverAndStart 호출 X; daemon down → in-process도 X
  - Daemon-forward fallback round-trip
  - Stub registration: known plugin actions이 ActionRegistry에 등록됨
- E2E:
  - Daemon up → Nestty.app launch → only daemon-side echo plugin process
  - Daemon down → `nestctl call echo.ping` returns `daemon_unavailable`
  - Daemon spawned mid-session via auto-spawn → reconnect ack → plugin actions available

**Deliverable:** Nestty.app daemon client. Plugin ownership 충돌 없음. Local callers (keybindings/panels/triggers) 끊김 없음.

---

### PR 3 — Invoke handler + Linux gui_client guardrails

**Goal:** daemon → GUI command path + Linux 안전 메커니즘.

- DaemonClient reader thread receives `Invoke {id, method, params}`
- Hop to main actor → `handleCommand` 호출 (extract from `SocketServer.swift:156` — both sources share)
- Send `Response {id, result | error}` back

**Linux guardrails (mandatory, codex round 2 C5):**
- `_ping` fast path
- Bounded `writer_tx` with `try_send` (`67b6a03`)
- Frame cap on incoming reads (`61b1f5d`)
- Invoke worker pool with generation gating (`eb2e58d`); on reconnect bump generation → drop in-flight stale invokes
- Watchdog timeout

**Tests:** unit per guardrail; e2e — `nestctl call tab.new` round-trip; stress 1000 rapid invokes.

---

### PR 4a — EventBus 구조화 + bridge_id model + inbound daemon event republish

**Goal:** EventBus가 typed events + metadata (bridge_id 포함)를 다루도록 refactor. forwarder는 아직 OFF, local engine 그대로 ON.

#### 4a.1 EventBus 구조화 (codex round 3 C2 — 핵심 prereq)
- 현재 `EventChannel`은 JSON 문자열만 전달 → typed `Event { kind: String, source: String, data: [String: Any], bridge_id: String? }` payload로 refactor
- Subscriber API가 typed event 받도록 변경 (또는 기존 JSON 채널 + 새 structured 채널 parallel — Swift는 후자가 깔끔)
- Existing subscribers (SocketServer, NesttyEngine, ContextService) 모두 typed 채널로 마이그레이션

#### 4a.2 bridge_id (Linux 일치, round 2 C2)
- Wire JSON에 `bridge_id` 절대 안 실림 (`serde(skip)` 패턴)
- Native events는 `bridge_id = nil`
- Inbound daemon event parsing (DaemonClient): daemon shape `{"type": ..., "data": {...}}` → fresh local `bridge_id` 부여 → local bus에 republish
- Local subscriber는 typed event 받음 (4a.1 결과)

#### 4a.3 Wire shape compat (codex round 3 I2)
- DaemonClient는 daemon shape `{"type":...}` 인식
- Legacy SocketServer (nestctl 직접 연결)는 `{"event":...}` 그대로 (back-compat)
- **PluginPanelController.swift:145**가 `event` key를 parse 중 — dual parser 추가 (둘 다 받게) 또는 명시적 migration window 노트
- 향후 daemon-shape으로 fully align은 별도 PR

#### 4a.4 Forwarder OFF, engine ON (이번 PR 한정)
- GUI → daemon outbound forwarder 코드 추가 X (PR4b)
- `NesttyEngine.disable()` 호출 안 함 — local engine이 native events fire

#### Tests
- Unit:
  - Typed event API
  - bridge_id가 wire JSON에 안 들어감
  - Inbound republish 시 fresh bridge_id 부여
  - Daemon shape parser
  - PluginPanelController dual parser
- E2E:
  - Daemon에서 `event.publish foo {}` → DaemonClient → republish → local subscribers 모두 받음
  - Local에서 native event → local engine fire (forwarder OFF, daemon에 안 감)

**Deliverable:** macOS가 daemon events를 받음. EventBus는 PR4b의 forwarder가 사용할 수 있는 typed model.

---

### PR 4b — GUI→daemon forwarder + ContextService bridging + host-trigger cut-over

**Goal:** PR4a의 typed model 위에 outbound forwarder, context bridging, cut-over 활성.

#### 4b.1 GUI → daemon outbound forwarder (codex round 3 C4 — allowlist)
- EventBus에 typed subscriber로 attach
- **Linux의 curated allowlist를 mirror** (`gui_client.rs:23`):
  - 포함: `panel.focused`, `panel.exited`, `terminal.cwd_changed`, `terminal.exec.completed`, plugin events, etc.
  - **제외**: `terminal.output` (wire saturation), 기타 high-frequency internals
- 각 event를 daemon에 `_bus.publish` 송신
- **Skip events with `bridge_id != nil`** (이미 bridge된 event는 echo 방지)

#### 4b.2 Context bridging (round 2 C3)
- `panel.focused` / `panel.exited` / `terminal.cwd_changed` 가 forwarder를 통해 daemon에 도달
- Daemon-side ContextService.apply가 처리 → daemon-side context 업데이트
- Daemon trigger interpolation은 daemon ContextService에서
- macOS-side ContextService는 living (local engine fallback용 + `context.snapshot` socket command — daemon owner면 forward)

#### 4b.3 host_triggers cut-over (round 2 C2)
- **Order critical:**
  1. PR4a inbound republish 살아있음 ✓
  2. PR4b outbound forwarder 시작
  3. **그 다음** register ack의 `host_triggers=true` 처리해서 `nesttyEngine.disable()`
- `NesttyEngine.disable()`: callbacks clear, trigger list empty, dispatch_event no-op
- **Drop guard** (즉시, codex round 3 I3 — graceful drain X):
  - DaemonClient disconnect 시 즉시 `nesttyEngine.enable()` — local engine 회복
  - Reconnect 성공 + `host_triggers=true` → 다시 disable
  - Race: forwarder thread는 writer 실패로 자연스럽게 종료, drain timeout 안 줌

#### Tests
- Unit:
  - Forwarder allowlist (excluded kinds 안 forward)
  - Forwarder skip on bridge_id
  - Drop guard (disconnect 즉시 engine.enable)
  - Engine disable/enable 멱등
- E2E (daemon with `NESTTYD_HOST_TRIGGERS=1`):
  - Trigger fire via daemon, no local double-fire
  - Kill daemon → local GUI-only triggers 회복, plugin actions은 unavailable
  - Reconnect → cut-over 재적용
  - Context interpolation: daemon trigger sees correct `{context.active_panel}`
  - terminal.output spam 시 wire 안 막힘 (allowlist 효과)

**Deliverable:** macOS가 Linux Stage E equivalent. Daemon-hosted triggers + plugin RPC + context interpolation 모두 동작.

---

### PR 5 — Status bar daemon-routing + remaining socket commands + PluginSupervisor 삭제 + missing Linux churn

**Goal:** Daemon 위로 운영의 마지막 마일.

#### 5.1 Status bar `[[modules]]` daemon-routing (`3d18515` mirror)
- macOS `StatusModuleRunner`가 `_module.run` (daemon `register_blocking_silent`로 등록)을 호출
- Module exec lifecycle은 daemon-owned

#### 5.2 PluginSupervisor 클래스 + supporting code 삭제 (round 3 I4)
- PR2에서 startup만 stop → PR5에서 implementation, manifest discovery (panel discovery는 별도로 keep), 관련 Swift 코드 모두 삭제
- 단, `PluginManifestStore.discover()`는 panel/statusbar manifest 발견을 위해 keep (codex round 3 I5 — duplicate-name winner rule이 panels/statusbar에도 적용)

#### 5.3 GUI env curation in `gui.register` (`0fa2e65` + `734e403` mirror, codex round 3 C3 — 정확한 whitelist)
- macOS Nestty.app이 `gui.register` 시 curated env 첨부
- **Daemon `gui_registry.rs:24` whitelist 정확히 mirror — PATH 제외, secrets-adjacent 제외**
- Daemon이 system.spawn 자식에게 forward

#### 5.4 Config reload deferral while host_triggers active (`ceae13b` mirror)
- macOS config watcher가 reload 시도 시 host_triggers active면 daemon에 위임 (또는 deferral)

#### 5.5 Frame-size bounded reads (mirror PR 3 가드 sanity check)
- DaemonClient의 모든 socket reader frame cap 재확인

#### 5.6 잔여 socket commands
- macOS-missing 중 daemon-side already-implemented 것은 daemon-forward fallback이 자동 해결 (PR2 도입)
- macOS-only ones (예: webview.screenshot via WKWebView.takeSnapshot)는 GUI에 남고 daemon이 Invoke로 호출 (PR3 path)

#### 5.7 Duplicate plugin-name winner consistency (round 3 I5)
- 두 manifest dir이 같은 plugin name을 export할 때 daemon의 결정 규칙을 panels/statusbar manifest discovery에도 적용

#### Tests
- E2E full plugin set: kb.search/git.list_workspaces/slack.message — daemon-side, results identical to monolithic
- Status bar modules daemon-side
- nestctl이 GUI socket으로 plugin 명령 → daemon-forward → 정상 응답
- GUI env curation: spawned child의 env에 PATH가 macOS GUI의 PATH로 leak 안 됨

**Deliverable:** 아키텍처 전환 완료. 단일 source of truth.

---

### PR 6 — Cmd+Shift+P command palette port [DONE]

**Goal:** `f8a77c8` "Add Ctrl+Shift+P command palette over ActionRegistry"의 macOS 포트.

- 새 `CommandPalette.swift` (`CommandPaletteController` + filter helpers)
- Action surface = `actionRegistry.names()` ∪ macOS-local `LEGACY_DISPATCH_METHODS` 미러 (handleCommand switch arms)
- Cmd+Shift+P (mac convention, not Ctrl+Shift+P) — keyCode-based match (IME-immune; Korean/JP IME가 char `p`를 자기 문자로 번역해 char-match 깨지는 거 방지)
- Enter 시 `handleCommand`로 라우팅 (registry hit → legacy switch → daemon fallback 모두 커버)
- Destructive guard: `tab.close` NSAlert with Cancel-default + Cancel-on-stray-Enter (Linux decision 29 미러)
- Focus restore: Esc/Cancel 경로에만 (post-dispatch는 action 자체에 위임 — `tab.close`/`tab.new`/`split.*`가 active view 자체를 변형하므로 stale responder 복원 시 crash/conflict)
- Re-entry guard: `commandPaletteController != nil` 체크로 holding-key가 sheet 쌓는 것 방지
- v1 limitations: empty params only; param-required actions는 stderr `invalid_params` (Linux와 동일)
- Tests: e2e via AppleScript — 열기/system.ping 디스패치/destructive guard cancel/Esc 닫힘/re-entry guard 확인

---

### PR 7 — launchd auto-load (OPTIONAL)

**Goal:** macOS UX polish.

- `~/Library/LaunchAgents/com.marshall.nesttyd.plist` 생성
- `scripts/install-macos.sh --launchd` opt-in flag (`launchctl load`)
- Auto-spawn-on-connect (PR2)는 fallback 유지

---

## Cross-cutting concerns

- **Backwards compatibility**: PR 1 ~ 4a는 daemon 없이도 정상 동작 (graceful degrade). PR 4b가 cut-over 게이트.
- **Trigger semantics parity**: bridge_id local-only model (Linux 일치)이 echo 방지에 critical.
- **Socket path**: `~/Library/Caches/nestty/socket` (paths.rs already done). Perms 0600 (`bbd7c9c`).

## Risks / unknowns (status update)

| # | 원래 risk | 상태 |
|---|---|---|
| 1 | macOS launchd vs auto-spawn | RESOLVED — auto-spawn-on-connect 우선, launchd PR7 옵션 (codex I3) |
| 2 | Multiple GUI instances | INFORMATIONAL — protocol은 어쨌든 동작, macOS는 single-instance가 일반 |
| 3 | In-process plugin double-spawn | RESOLVED — PR2 lifecycle gate + daemon-forward fallback (codex C1, C1, C1 across 3 rounds) |
| 4 | gui_client.rs 복잡도 | RESOLVED — PR3에 mandatory guardrails 명시 |
| 5 | ContextService 위치 | RESOLVED — PR4b에서 daemon-side로 bridging |

## Out of scope

- Cursor visibility issue with background image (별도 WIP, post-migration queue).
- Universal binary support for nesttyd (arm64-only).
- Code-signing for nesttyd binary distribution.
- macOS launchd full integration (PR7 옵션, post-MVP).

## Plan rationale

3 round의 codex pressure-test로 6 + 5 + 4 = 15개 CRITICAL + 8개 INFORMATIONAL을 발견 + 반영. v0 → v3 변경 폭이 round마다 줄어드는 건 plan이 수렴 중이라는 신호 (round 3에서 새 critical은 architecture 위반보다는 sequencing tightening). 더 이상 round 가지 않고 implementation 시작.
