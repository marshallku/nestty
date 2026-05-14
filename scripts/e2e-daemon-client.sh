#!/usr/bin/env bash
# End-to-end verification of the daemon ↔ GUI socket link.
#
# Runs nesttyd against an isolated socket path, then exercises:
#   1. register handshake
#   2. heartbeat survival
#   3. daemon restart → GUI re-register
#   4. frozen GUI → heartbeat-miss unregister
#
# A small Python mock client (stdlib only) stands in for nestty-linux so the
# script doesn't need GTK / a display. Real nestty GUI verification is a
# manual step listed at the bottom.

set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DAEMON="$REPO/target/debug/nesttyd"
NESTCTL="$REPO/target/debug/nestctl"

[[ -x "$DAEMON" ]] || { echo "build first: cargo build -p nestty-daemon"; exit 2; }
[[ -x "$NESTCTL" ]] || { echo "build first: cargo build -p nestty-cli"; exit 2; }

WORK="$(mktemp -d -t nestty-e2e.XXXXXX)"
# Step 12's marker path is interpolated into both a TOML string and
# a `/bin/sh -c` command. Use a STRICT allowlist (alphanumerics +
# `._/-`) so no shell metacharacter (`;`, `&`, backtick, `>`, `|`,
# newline, quote, dollar, etc.) and no TOML-corrupting char (`"`,
# `\`) can sneak in via a maliciously-set TMPDIR. Default /tmp is
# always safe.
if ! printf '%s' "$WORK" | LC_ALL=C grep -Eq '^[A-Za-z0-9._/-]+$'; then
    echo "Refusing to run: WORK path '$WORK' contains characters outside [A-Za-z0-9._/-]. Set TMPDIR to a plain ASCII path." >&2
    exit 2
fi
DAEMON_LOG="$WORK/daemon.log"
GUI_LOG="$WORK/gui.log"
SOCKET="$WORK/nesttyd.sock"

export NESTTY_SOCKET="$SOCKET"
export RUST_LOG="${RUST_LOG:-info}"
# Tiny pool + test-only action so step 6 can force saturation.
export NESTTYD_POOL_WORKERS=2
export NESTTYD_POOL_QUEUE=2
export NESTTYD_E2E_TEST_ACTIONS=1
# Isolate plugin discovery: daemon is the sole plugin host since Step
# 5b, so without this it would spawn every plugin from the user's real
# ~/.config/nestty/plugins/ during the test.
mkdir -p "$WORK/xdg-config/nestty/plugins"
export XDG_CONFIG_HOME="$WORK/xdg-config"

# Stub plugin for Step 8 (plugin.<name>.<cmd> + _module.run dispatch).
# No [[services]] entries so the supervisor doesn't try to spawn a
# non-existent binary. `hello` command emits JSON so the dispatcher's
# "parse stdout as JSON" path is exercised; `clock` module emits a
# fixed string so the test can assert exact stdout.
STUB_DIR="$WORK/xdg-config/nestty/plugins/e2e-stub"
mkdir -p "$STUB_DIR"
cat > "$STUB_DIR/plugin.toml" <<'TOML'
[plugin]
name = "e2e-stub"
title = "E2E Stub"
version = "0.0.1"

[[commands]]
name = "hello"
exec = "printf '{\"msg\":\"hi\"}'"

[[modules]]
name = "clock"
exec = "printf 'tick'"
interval = 60
TOML

DAEMON_PID=""
GUI_PID=""
DAEMON_STARTS=0

cleanup() {
    local rc=$?
    [[ -n "$GUI_PID" ]] && { kill -CONT "$GUI_PID" 2>/dev/null || true; kill -KILL "$GUI_PID" 2>/dev/null || true; }
    [[ -n "$DAEMON_PID" ]] && kill -TERM "$DAEMON_PID" 2>/dev/null || true
    wait 2>/dev/null || true
    if (( rc != 0 )); then
        echo
        echo "=== daemon.log (tail 80) ==="
        tail -n 80 "$DAEMON_LOG" 2>/dev/null || true
        echo "=== gui.log (tail 40) ==="
        tail -n 40 "$GUI_LOG" 2>/dev/null || true
    fi
    echo
    echo "logs preserved: $WORK"
}
trap cleanup EXIT

pass() { printf '  \033[32mPASS\033[0m %s\n' "$*"; }
fail() { printf '  \033[31mFAIL\033[0m %s\n' "$*"; exit 1; }
step() { printf '\n\033[1m[%s]\033[0m %s\n' "$1" "$2"; }

wait_for() {
    local pattern=$1 file=$2 timeout_s=${3:-10}
    local deadline=$(( SECONDS + timeout_s ))
    while (( SECONDS < deadline )); do
        if grep -qE "$pattern" "$file" 2>/dev/null; then return 0; fi
        sleep 0.2
    done
    return 1
}

# Waits until grep counts at least N matches — survives the "first entry
# already there" trap that plain wait_for falls into.
wait_for_count() {
    local pattern=$1 file=$2 min_count=$3 timeout_s=${4:-10}
    local deadline=$(( SECONDS + timeout_s ))
    while (( SECONDS < deadline )); do
        local n
        # `grep -c` exits 1 on zero matches; under set -e the `n=$(...)`
        # form would abort the whole script. Force n=0 explicitly.
        n=$(grep -cE "$pattern" "$file" 2>/dev/null) || n=0
        (( n >= min_count )) && return 0
        sleep 0.2
    done
    return 1
}

start_daemon() {
    DAEMON_STARTS=$(( DAEMON_STARTS + 1 ))
    "$DAEMON" >>"$DAEMON_LOG" 2>&1 &
    DAEMON_PID=$!
    # Count-based wait so restart doesn't false-positive on the prior
    # daemon's "listening on" line in the accumulated log.
    wait_for_count 'nesttyd listening on' "$DAEMON_LOG" "$DAEMON_STARTS" 5 \
        || fail "daemon did not start (pid=$DAEMON_PID, attempt #$DAEMON_STARTS)"
    [[ -S "$SOCKET" ]] || fail "socket not created at $SOCKET"
}

start_gui_mock() {
    python3 - "$SOCKET" >>"$GUI_LOG" 2>&1 <<'PY' &
import json, os, socket, sys, time, uuid

sock_path = sys.argv[1]
def log(msg): print(f"[mock-gui {os.getpid()}] {msg}", flush=True)

def connect_once():
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.connect(sock_path)
    f = s.makefile("rwb", buffering=0)
    return s, f

CAPABILITIES = [
    "tab","split","terminal","webview","background",
    "statusbar","agent.ui","plugin.open","session","search",
]
PROTOCOL_VERSION = 1

def register(f):
    req_id = str(uuid.uuid4())
    req = {
        "id": req_id,
        "method": "gui.register",
        "params": {
            "window_id": str(uuid.uuid4()),
            "capabilities": CAPABILITIES,
            "want_primary": True,
            "version": "mock-1.0",
            "protocol_version": PROTOCOL_VERSION,
        },
    }
    f.write((json.dumps(req) + "\n").encode())
    line = f.readline()
    if not line:
        raise RuntimeError("daemon closed before register ack")
    resp = json.loads(line)
    if not resp.get("ok"):
        raise RuntimeError(f"register rejected: {resp}")
    log(f"registered: {resp['result']}")

def serve(f):
    for line in f:
        if not line.strip(): continue
        msg = json.loads(line)
        # Invoke (daemon → gui)
        if "invoke" in msg:
            method = msg["invoke"]
            resp = {"id": msg["id"], "ok": True, "result": msg.get("params", {})}
            f.write((json.dumps(resp) + "\n").encode())
            if method != "_ping":
                log(f"echoed invoke: {method}")
        # Event from the daemon's auto-subscribe-all forwarder.
        elif "type" in msg and "data" in msg:
            src = msg.get("source", "")
            log(f"event: type={msg['type']} source={src}")

backoff = 1.0
while True:
    try:
        s, f = connect_once()
        register(f)
        backoff = 1.0
        serve(f)
        log("daemon closed connection; reconnecting")
    except (ConnectionRefusedError, FileNotFoundError):
        log(f"daemon unreachable; sleeping {backoff}s")
        time.sleep(backoff)
        backoff = min(backoff * 2, 30.0)
    except Exception as e:
        log(f"error: {e!r}; sleeping {backoff}s")
        time.sleep(backoff)
        backoff = min(backoff * 2, 30.0)
PY
    GUI_PID=$!
}

step 1 "start nesttyd"
start_daemon
pass "daemon listening on $SOCKET"

step 2 "register handshake"
start_gui_mock
wait_for 'gui registered: client_id=' "$DAEMON_LOG" 5 \
    || fail "register not observed in daemon.log"
wait_for 'registered: \{' "$GUI_LOG" 5 \
    || fail "mock-gui did not log register success"
client_1=$(grep -oE 'gui registered: client_id=[^ ]+' "$DAEMON_LOG" | tail -n1 | cut -d= -f2)
pass "register OK; primary client_id=${client_1:0:8}…"

step 3 "heartbeat survival (25s)"
# Snapshot the unregister count so we measure the window, not the
# accumulated log. (`grep -c` exits 1 on zero matches; force 0 under set -e.)
misses_before=$(grep -cE 'consecutive misses' "$DAEMON_LOG" 2>/dev/null) || misses_before=0
sleep 25
misses_after=$(grep -cE 'consecutive misses' "$DAEMON_LOG" 2>/dev/null) || misses_after=0
if (( misses_after > misses_before )); then
    fail "unexpected heartbeat unregister during survival window (before=$misses_before after=$misses_after)"
fi
if ! kill -0 "$GUI_PID" 2>/dev/null; then
    fail "mock-gui exited during survival window"
fi
pass "no unregister; mock-gui still alive"

step 4 "daemon restart → re-register"
kill -TERM "$DAEMON_PID"; wait "$DAEMON_PID" 2>/dev/null || true; DAEMON_PID=""
wait_for 'daemon closed connection' "$GUI_LOG" 5 \
    || fail "mock-gui did not detect daemon disconnect"
pass "mock-gui saw disconnect"

start_daemon
wait_for_count 'gui registered: client_id=' "$DAEMON_LOG" 2 15 \
    || fail "no re-register after daemon restart (count stayed at 1)"
client_2=$(grep -oE 'gui registered: client_id=[^ ]+' "$DAEMON_LOG" | tail -n1 | cut -d= -f2)
[[ "$client_2" != "$client_1" ]] || fail "client_id reused after restart (got $client_2)"
pass "re-registered; new client_id=${client_2:0:8}…"

step 5 "event bridge — completion forwarded to GUI"
# Fire a plain (non-GUI) connection that invokes __test.slow_blocking;
# the daemon's ActionRegistry publishes `<action>.completed` on its bus
# after the handler returns. The mock GUI (still registered from step 4)
# must observe that wire Event via the auto-subscribe-all forwarder —
# this is the chained-trigger preservation contract for Step 5b.
python3 - "$SOCKET" >/dev/null 2>&1 <<'PY'
import json, socket, sys, uuid
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(sys.argv[1])
f = s.makefile("rwb", buffering=0)
req = {"id": str(uuid.uuid4()), "method": "__test.slow_blocking", "params": {"ms": 50}}
f.write((json.dumps(req) + "\n").encode())
f.readline()  # ignore response
PY
wait_for 'event: type=__test\.slow_blocking\.completed source=nestty\.action' "$GUI_LOG" 5 \
    || fail "event bridge: completion event missing or source field stripped (need source=nestty.action so chained triggers fire)"
pass "event bridge delivered __test.slow_blocking.completed with source=nestty.action"

step 6 "frozen GUI → heartbeat-miss unregister"
kill -STOP "$GUI_PID"
# Two misses: interval(10s) + timeout(5s) ×2 ≈ 30s. Give it 40.
wait_for "consecutive misses on $client_2" "$DAEMON_LOG" 40 \
    || fail "no heartbeat-miss after freezing mock-gui"
wait_for "gui unregistered: client_id=$client_2" "$DAEMON_LOG" 5 \
    || fail "unregister log missing after heartbeat misses"
pass "heartbeat-miss path fired and unregistered ${client_2:0:8}…"

# Resume so wait in cleanup doesn't hang
kill -CONT "$GUI_PID" 2>/dev/null || true
# Mock GUI is done with its part; let it exit so the burst doesn't
# fight a half-attached client over the same socket.
kill -KILL "$GUI_PID" 2>/dev/null || true
GUI_PID=""

step 7 "pool saturation → overloaded"
# Daemon's per-connection handler dispatches synchronously (recv_timeout
# 120s), so concurrency comes from concurrent connections, not concurrent
# requests on one socket. Fire N independent connections and assert ≥ 6
# bounce back as overloaded.
# Pool capacity = workers(2) + queue(2) = 4 in flight max.
BURST_LOG="$WORK/burst.log"
python3 - "$SOCKET" >"$BURST_LOG" 2>&1 <<'PY'
import json, socket, sys, threading, time, uuid

sock_path = sys.argv[1]
N = 12

def one_call(rid, out):
    try:
        s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        s.connect(sock_path)
        f = s.makefile("rwb", buffering=0)
        req = {"id": rid, "method": "__test.slow_blocking", "params": {"ms": 400}}
        f.write((json.dumps(req) + "\n").encode())
        line = f.readline()
        out.append(json.loads(line) if line else None)
    except Exception as e:
        out.append({"id": rid, "error": {"code": "connect_failed", "message": str(e)}})

threads = []
results = []
for _ in range(N):
    rid = str(uuid.uuid4())
    result_slot = []
    results.append(result_slot)
    t = threading.Thread(target=one_call, args=(rid, result_slot), daemon=True)
    threads.append(t)
# Start them as close together as the GIL lets us.
for t in threads:
    t.start()
for t in threads:
    t.join(timeout=15)
flat = [r[0] for r in results if r]
received = sum(1 for m in flat if m is not None)
overloaded = sum(1 for m in flat if m and (m.get("error") or {}).get("code") == "overloaded")
print(json.dumps({"received": received, "overloaded": overloaded, "expected_n": N}))
PY
SUMMARY=$(tail -n1 "$BURST_LOG")
received=$(python3 -c "import sys,json; print(json.loads(sys.argv[1])['received'])" "$SUMMARY")
overloaded=$(python3 -c "import sys,json; print(json.loads(sys.argv[1])['overloaded'])" "$SUMMARY")
(( received == 12 )) || fail "burst: expected 12 responses, got $received"
(( overloaded >= 6 )) || fail "burst: expected ≥6 overloaded, got $overloaded"
pass "burst: $received/12 responded, $overloaded overloaded"

step 8 "daemon-hosted plugin.<name>.<cmd> + _module.run dispatch"
# Calls go to the well-known daemon socket directly — no GUI involved.
# The stub plugin was dropped into XDG_CONFIG_HOME before step 1, so
# the daemon has it in its registry. `hello` returns JSON → daemon
# parses; `_module.run` returns {stdout, exit_code}.
DISP_LOG="$WORK/dispatch.log"
python3 - "$SOCKET" >"$DISP_LOG" 2>&1 <<'PY'
import json, socket, sys, uuid

sock_path = sys.argv[1]

def call(method, params):
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.connect(sock_path)
    f = s.makefile("rwb", buffering=0)
    req = {"id": str(uuid.uuid4()), "method": method, "params": params}
    f.write((json.dumps(req) + "\n").encode())
    line = f.readline()
    return json.loads(line) if line else None

out = {}
out["cmd"] = call("plugin.e2e-stub.hello", {})
out["module"] = call("_module.run", {"plugin": "e2e-stub", "module": "clock"})
print(json.dumps(out))
PY
SUMMARY=$(tail -n1 "$DISP_LOG")
cmd_msg=$(python3 -c "import sys,json; print((json.loads(sys.argv[1])['cmd'].get('result') or {}).get('msg'))" "$SUMMARY")
[[ "$cmd_msg" == "hi" ]] || fail "plugin.e2e-stub.hello: expected result.msg='hi', got $cmd_msg (summary: $SUMMARY)"
pass "plugin.e2e-stub.hello → {msg: 'hi'}"

mod_stdout=$(python3 -c "import sys,json; print((json.loads(sys.argv[1])['module'].get('result') or {}).get('stdout'))" "$SUMMARY")
[[ "$mod_stdout" == "tick" ]] || fail "_module.run: expected result.stdout='tick', got $mod_stdout (summary: $SUMMARY)"
pass "_module.run e2e-stub/clock → stdout='tick'"

# Negative paths: unknown plugin, unknown module.
ERR_LOG="$WORK/dispatch-err.log"
python3 - "$SOCKET" >"$ERR_LOG" 2>&1 <<'PY'
import json, socket, sys, uuid
sock_path = sys.argv[1]
def call(method, params):
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.connect(sock_path)
    f = s.makefile("rwb", buffering=0)
    req = {"id": str(uuid.uuid4()), "method": method, "params": params}
    f.write((json.dumps(req) + "\n").encode())
    line = f.readline()
    return json.loads(line) if line else None
print(json.dumps({
    "unknown_plugin": call("_module.run", {"plugin": "nope", "module": "clock"}),
    "unknown_module": call("_module.run", {"plugin": "e2e-stub", "module": "nope"}),
}))
PY
ERR_SUMMARY=$(tail -n1 "$ERR_LOG")
up_code=$(python3 -c "import sys,json; print((json.loads(sys.argv[1])['unknown_plugin'].get('error') or {}).get('code'))" "$ERR_SUMMARY")
[[ "$up_code" == "not_found" ]] || fail "_module.run unknown plugin: expected error.code='not_found', got $up_code"
um_code=$(python3 -c "import sys,json; print((json.loads(sys.argv[1])['unknown_module'].get('error') or {}).get('code'))" "$ERR_SUMMARY")
[[ "$um_code" == "not_found" ]] || fail "_module.run unknown module: expected error.code='not_found', got $um_code"
pass "negative paths return not_found"

step 9 "GUI→daemon bridge via _bus.publish + echo gate"
# Verifies the Stage B bridge: a registered GUI can publish events
# onto the daemon bus, those events get `bridge_id` set, and the
# symmetric daemon→GUI forwarder DOES NOT echo them back to the
# originating GUI. Positive control: a daemon-native completion
# event (`bridge_id=None`) still flows through.
BRIDGE_LOG="$WORK/bridge.log"
python3 - "$SOCKET" >"$BRIDGE_LOG" 2>&1 <<'PY'
import json, socket, sys, threading, time, uuid

sock_path = sys.argv[1]

# Test 1: unauthorized — fresh connection, no register, _bus.publish
# should be rejected.
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(sock_path)
f = s.makefile("rwb", buffering=0)
req = {"id": "unauth-1", "method": "_bus.publish",
       "params": {"kind": "e2e.test", "source": "unauth",
                  "timestamp_ms": 1, "payload": {}}}
f.write((json.dumps(req) + "\n").encode())
unauth_resp = json.loads(f.readline())
s.close()

# Test 2: register + _bus.publish + listen for echo + positive control.
gui = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
gui.connect(sock_path)
gf = gui.makefile("rwb", buffering=0)
reg = {"id": str(uuid.uuid4()), "method": "gui.register",
       "params": {"window_id": str(uuid.uuid4()),
                  "capabilities": ["tab","split","terminal"],
                  "want_primary": True, "protocol_version": 1}}
gf.write((json.dumps(reg) + "\n").encode())
reg_resp = json.loads(gf.readline())
host_triggers = reg_resp.get("result", {}).get("host_triggers")

# Send _bus.publish — daemon should publish on bus with bridge_id set.
pub_id = str(uuid.uuid4())
pub = {"id": pub_id, "method": "_bus.publish",
       "params": {"kind": "e2e.bridge_test", "source": "e2e-mock",
                  "timestamp_ms": 42, "payload": {"marker": "v1"}}}
gf.write((json.dumps(pub) + "\n").encode())

# Collect events / responses for 1.5s. Expectations:
#   - one Response with id=pub_id, ok=true, result.queued=true
#   - ZERO Events of type=e2e.bridge_test (echo gate verified)
# Then trigger positive control:
#   - daemon-native __test.slow_blocking from a SEPARATE connection
#     produces __test.slow_blocking.completed with source=nestty.action
#     and bridge_id=None → forwarder DOES deliver
collected = []
stop = threading.Event()
def drain():
    # Read bytes raw + split on '\n'. Avoids readline's interaction
    # with socket-level timeouts on a file wrapper.
    buf = bytearray()
    gui.settimeout(0.2)
    while not stop.is_set():
        try:
            chunk = gui.recv(4096)
            if not chunk:
                return
            buf.extend(chunk)
            while b"\n" in buf:
                line, _, rest = buf.partition(b"\n")
                buf = bytearray(rest)
                if not line.strip():
                    continue
                try:
                    collected.append(json.loads(line))
                except json.JSONDecodeError:
                    continue
        except socket.timeout:
            continue
        except OSError:
            return
t = threading.Thread(target=drain, daemon=True)
t.start()

# Fire positive control after a short delay so the echo window
# is fully sampled. Longer total window helps when CI hosts are slow
# to schedule the drain thread.
time.sleep(0.5)
ctrl = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
ctrl.connect(sock_path)
cf = ctrl.makefile("rwb", buffering=0)
ctrl_req = {"id": str(uuid.uuid4()), "method": "__test.slow_blocking",
            "params": {"ms": 50}}
cf.write((json.dumps(ctrl_req) + "\n").encode())
cf.readline()
ctrl.close()

time.sleep(2.0)
stop.set()
gui.close()

print(json.dumps({
    "unauth": unauth_resp,
    "host_triggers_advertised": host_triggers,
    "register_ok": reg_resp.get("ok"),
    "publish_resp": next((m for m in collected if m.get("id") == pub_id), None),
    "echoed_test_kind": [
        m for m in collected
        if m.get("type") == "e2e.bridge_test"
    ],
    "saw_positive_control": any(
        m.get("type") == "__test.slow_blocking.completed"
        and m.get("source") == "nestty.action"
        for m in collected
    ),
}))
PY
B_SUMMARY=$(tail -n1 "$BRIDGE_LOG")
unauth_code=$(python3 -c "import sys,json; print((json.loads(sys.argv[1])['unauth'].get('error') or {}).get('code'))" "$B_SUMMARY")
[[ "$unauth_code" == "unauthorized" ]] || fail "_bus.publish without register: expected unauthorized, got $unauth_code"
pass "_bus.publish rejects unregistered caller (unauthorized)"

ht_advertised=$(python3 -c "import sys,json; print(json.loads(sys.argv[1])['host_triggers_advertised'])" "$B_SUMMARY")
[[ "$ht_advertised" == "False" || "$ht_advertised" == "True" ]] || fail "host_triggers ack field missing"
pass "register ack advertises host_triggers (got $ht_advertised)"

pub_ok=$(python3 -c "import sys,json; print((json.loads(sys.argv[1]).get('publish_resp') or {}).get('ok'))" "$B_SUMMARY")
[[ "$pub_ok" == "True" ]] || fail "registered _bus.publish: expected ok=true, got $pub_ok (summary: $B_SUMMARY)"
pub_queued=$(python3 -c "import sys,json; print(((json.loads(sys.argv[1]).get('publish_resp') or {}).get('result') or {}).get('queued'))" "$B_SUMMARY")
[[ "$pub_queued" == "True" ]] || fail "registered _bus.publish: expected result.queued=true, got $pub_queued (summary: $B_SUMMARY)"
pass "registered _bus.publish accepted with queued=true"

echoes=$(python3 -c "import sys,json; print(len(json.loads(sys.argv[1])['echoed_test_kind']))" "$B_SUMMARY")
[[ "$echoes" == "0" ]] || fail "bridge echo gate leaked $echoes copies of e2e.bridge_test back to the GUI"
pass "bridge echo gate: zero e2e.bridge_test echoes"

ctrl_seen=$(python3 -c "import sys,json; print(json.loads(sys.argv[1])['saw_positive_control'])" "$B_SUMMARY")
[[ "$ctrl_seen" == "True" ]] || fail "positive control: daemon-native completion didn't reach the GUI; can't trust the echo-absence above"
pass "positive control: __test.slow_blocking.completed delivered (forwarder alive)"

step 10 "host_triggers=true cut-over → daemon dispatches trigger from bridged event"
# Tear down the running daemon and restart it with NESTTYD_HOST_TRIGGERS=1
# plus a config.toml containing a `[[trigger]]` that listens on a
# synthetic kind and fires `system.log`. A registered mock GUI then
# pushes the matching event via `_bus.publish`; the daemon's
# TriggerEngine consumes it (via the pump thread that's now running)
# and the configured system.log emits to daemon stderr.
kill -TERM "$DAEMON_PID" 2>/dev/null || true
wait "$DAEMON_PID" 2>/dev/null || true
DAEMON_PID=""

mkdir -p "$XDG_CONFIG_HOME/nestty"
cat > "$XDG_CONFIG_HOME/nestty/config.toml" <<'TOML'
[[triggers]]
name = "e2e-cutover"
action = "system.log"
params = { message = "e2e-cutover-fired" }
[triggers.when]
event_kind = "e2e.cutover"
TOML

# Record the daemon log size BEFORE the restart so the assertion can
# isolate the new run's stderr from prior step output.
DAEMON_LOG_OFFSET=$(wc -c < "$DAEMON_LOG")

NESTTYD_HOST_TRIGGERS=1 "$DAEMON" >>"$DAEMON_LOG" 2>&1 &
DAEMON_PID=$!
DAEMON_STARTS=$(( DAEMON_STARTS + 1 ))
wait_for_count 'nesttyd listening on' "$DAEMON_LOG" "$DAEMON_STARTS" 5 \
    || fail "daemon did not restart with NESTTYD_HOST_TRIGGERS=1"
wait_for 'trigger engine: 1 configured' "$DAEMON_LOG" 5 \
    || fail "daemon did not load the cut-over trigger config"
wait_for 'dispatch=ON' "$DAEMON_LOG" 2 \
    || fail "daemon did not log dispatch=ON for host_triggers=1"

CUTOVER_LOG="$WORK/cutover.log"
python3 - "$SOCKET" >"$CUTOVER_LOG" 2>&1 <<'PY'
import json, socket, sys, uuid

sock_path = sys.argv[1]

def read_response(f, target_id, timeout=2.0):
    # A registered GUI socket interleaves async Event frames (from the
    # daemon's auto-subscribe-all forwarder) with RPC responses. Read
    # lines until we find the response with the expected id; skip
    # everything else.
    import select, time
    deadline = time.monotonic() + timeout
    sock = f.raw if hasattr(f, "raw") else None
    while time.monotonic() < deadline:
        line = f.readline()
        if not line:
            return None
        try:
            msg = json.loads(line)
        except json.JSONDecodeError:
            continue
        if msg.get("id") == target_id and "ok" in msg:
            return msg
    return None

gui = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
gui.connect(sock_path)
gui.settimeout(2.0)
f = gui.makefile("rwb", buffering=0)
reg_id = str(uuid.uuid4())
reg = {"id": reg_id, "method": "gui.register",
       "params": {"window_id": str(uuid.uuid4()),
                  "capabilities": ["tab"],
                  "want_primary": True, "protocol_version": 1}}
f.write((json.dumps(reg) + "\n").encode())
reg_resp = read_response(f, reg_id)
pub_id = str(uuid.uuid4())
pub = {"id": pub_id, "method": "_bus.publish",
       "params": {"kind": "e2e.cutover", "source": "e2e-mock",
                  "timestamp_ms": 1, "payload": {}}}
f.write((json.dumps(pub) + "\n").encode())
pub_resp = read_response(f, pub_id)
gui.close()
print(json.dumps({
    "host_triggers_advertised": (reg_resp or {}).get("result", {}).get("host_triggers"),
    "publish_ok": (pub_resp or {}).get("ok"),
}))
PY
C_SUMMARY=$(tail -n1 "$CUTOVER_LOG")
ht_ack=$(python3 -c "import sys,json; print(json.loads(sys.argv[1])['host_triggers_advertised'])" "$C_SUMMARY")
[[ "$ht_ack" == "True" ]] || fail "register ack with NESTTYD_HOST_TRIGGERS=1 didn't return host_triggers=true (got $ht_ack)"
pass "register ack advertises host_triggers=true under NESTTYD_HOST_TRIGGERS=1"

pub_ok=$(python3 -c "import sys,json; print(json.loads(sys.argv[1])['publish_ok'])" "$C_SUMMARY")
[[ "$pub_ok" == "True" ]] || fail "_bus.publish: expected ok=true, got $pub_ok"

# Poll daemon stderr for the trigger's system.log output, considering
# only bytes WRITTEN AFTER the restart. The configured params.message
# is "e2e-cutover-fired"; system.log emits "[system.log] <message>".
fire_deadline=$(( SECONDS + 5 ))
fired=0
while (( SECONDS < fire_deadline )); do
    if dd if="$DAEMON_LOG" bs=1 skip="$DAEMON_LOG_OFFSET" 2>/dev/null \
        | grep -q '\[system\.log\] e2e-cutover-fired'; then
        fired=1; break
    fi
    sleep 0.2
done
(( fired == 1 )) || fail "daemon's TriggerEngine did not dispatch the bridged event (system.log line absent)"
pass "daemon dispatched bridged e2e.cutover → system.log fired"

step 11 "nestctl event publish fires daemon trigger end-to-end"
# Rewrite the trigger config so the new e2e.publish kind matches a
# fresh `system.log` action. The daemon's 2s mtime watcher picks up
# the change without restart. Then nestctl event publish from a
# separate shell process fires the trigger; daemon dispatches the
# trigger and emits the configured system.log to stderr.
cat > "$XDG_CONFIG_HOME/nestty/config.toml" <<'TOML'
[[triggers]]
name = "e2e-publish"
action = "system.log"
params = { message = "e2e-publish-fired" }
[triggers.when]
event_kind = "e2e.publish"
TOML

# Watcher tick is 2s; allow a couple ticks for the file mtime to
# overtake the prior config write.
wait_for_count 'trigger config reloaded' "$DAEMON_LOG" 1 8 \
    || fail "daemon's config watcher did not pick up the new trigger config"

PUBLISH_LOG_OFFSET=$(wc -c < "$DAEMON_LOG")
PUBLISH_OUT=$("$NESTCTL" event publish e2e.publish '{"hello": "world"}' 2>&1)
echo "[nestctl publish output] $PUBLISH_OUT"
echo "$PUBLISH_OUT" | grep -q 'queued' \
    || fail "nestctl event publish did not return queued: got $PUBLISH_OUT"
pass "nestctl event publish returned queued"

# Verify the daemon dispatched the trigger via the watcher-reloaded
# config. Offset-based grep so prior steps' system.log lines don't
# false-match.
fire_deadline=$(( SECONDS + 5 ))
fired=0
while (( SECONDS < fire_deadline )); do
    if dd if="$DAEMON_LOG" bs=1 skip="$PUBLISH_LOG_OFFSET" 2>/dev/null \
        | grep -q '\[system\.log\] e2e-publish-fired'; then
        fired=1; break
    fi
    sleep 0.2
done
(( fired == 1 )) || fail "daemon did not dispatch the nestctl-published trigger (system.log line absent)"
pass "daemon dispatched nestctl-published e2e.publish → system.log fired"

step 12 "system.spawn inherits primary GUI's curated env"
# Register a mock GUI with custom HYPRLAND_INSTANCE_SIGNATURE, fire
# a system.spawn trigger that writes the env var to a marker file,
# verify the child saw the GUI's value (not the daemon's empty/absent
# value). Proves the Stage E env merge end-to-end:
# GUI capture → daemon whitelist → primary_gui_env → Command::envs.
ENV_MARKER="$WORK/spawn-env-marker"
ENV_RELOAD_COUNT_BEFORE=$(grep -cE 'trigger config reloaded' "$DAEMON_LOG" 2>/dev/null) || ENV_RELOAD_COUNT_BEFORE=0
cat > "$XDG_CONFIG_HOME/nestty/config.toml" <<TOML
[[triggers]]
name = "e2e-envspawn"
action = "system.spawn"
params = { argv = ["/bin/sh", "-c", "echo \"\$HYPRLAND_INSTANCE_SIGNATURE\" > $ENV_MARKER"] }
[triggers.when]
event_kind = "e2e.envspawn"
TOML
wait_for_count 'trigger config reloaded' "$DAEMON_LOG" "$(( ENV_RELOAD_COUNT_BEFORE + 1 ))" 8 \
    || fail "daemon's config watcher did not pick up the env-spawn trigger"

ENV_LOG="$WORK/envspawn.log"
python3 - "$SOCKET" >"$ENV_LOG" 2>&1 <<'PY'
import json, socket, sys, threading, time, uuid

sock_path = sys.argv[1]

def read_response(f, target_id, timeout=2.0):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        line = f.readline()
        if not line:
            return None
        try:
            msg = json.loads(line)
        except json.JSONDecodeError:
            continue
        if msg.get("id") == target_id and "ok" in msg:
            return msg
    return None

gui = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
gui.connect(sock_path)
gui.settimeout(2.0)
f = gui.makefile("rwb", buffering=0)
reg_id = str(uuid.uuid4())
reg = {"id": reg_id, "method": "gui.register",
       "params": {"window_id": str(uuid.uuid4()),
                  "capabilities": ["tab"],
                  "want_primary": True, "protocol_version": 1,
                  "gui_env": {
                      "HYPRLAND_INSTANCE_SIGNATURE": "e2e-fake-hypr-sig",
                  }}}
f.write((json.dumps(reg) + "\n").encode())
reg_resp = read_response(f, reg_id)
pub_id = str(uuid.uuid4())
pub = {"id": pub_id, "method": "_bus.publish",
       "params": {"kind": "e2e.envspawn", "source": "e2e-mock",
                  "timestamp_ms": 1, "payload": {}}}
f.write((json.dumps(pub) + "\n").encode())
pub_resp = read_response(f, pub_id)
# Keep the connection alive briefly — primary_gui_env is keyed off
# the active registration. Unregistering immediately could race the
# spawn worker depending on scheduling.
time.sleep(1.5)
gui.close()
print(json.dumps({
    "register_ok": (reg_resp or {}).get("ok"),
    "publish_ok": (pub_resp or {}).get("ok"),
}))
PY
E_SUMMARY=$(tail -n1 "$ENV_LOG")
reg_ok=$(python3 -c "import sys,json; print(json.loads(sys.argv[1])['register_ok'])" "$E_SUMMARY")
[[ "$reg_ok" == "True" ]] || fail "GUI register with gui_env failed: $E_SUMMARY"
pub_ok=$(python3 -c "import sys,json; print(json.loads(sys.argv[1])['publish_ok'])" "$E_SUMMARY")
[[ "$pub_ok" == "True" ]] || fail "_bus.publish failed: $E_SUMMARY"

# Marker file should now contain the GUI-supplied signature. system.spawn
# is fire-and-forget so poll briefly for the file.
marker_deadline=$(( SECONDS + 5 ))
captured=""
while (( SECONDS < marker_deadline )); do
    if [[ -s "$ENV_MARKER" ]]; then
        captured=$(cat "$ENV_MARKER")
        break
    fi
    sleep 0.2
done
[[ "$captured" == "e2e-fake-hypr-sig" ]] \
    || fail "system.spawn child did not see GUI env (expected 'e2e-fake-hypr-sig', got '$captured')"
pass "system.spawn child inherited HYPRLAND_INSTANCE_SIGNATURE from primary GUI's gui_env"

echo
echo "=== AUTO E2E COMPLETE ==="
echo
echo "Manual visual check (independent of this run — the trap already shut things down):"
echo "  1. start daemon in one terminal:  $DAEMON"
echo "  2. start GUI in another:           cargo run -p nestty-linux"
echo "     (daemon-client mode is on by default since Step 5a; no env var needed."
echo "      Both default to the same socket path — do not set NESTTY_SOCKET unless"
echo "      you start the daemon with the same override.)"
echo "  3. confirm: tabs/panels render normally, no extra startup latency, panel commands work"
echo "  4. kill the daemon; confirm GUI logs reconnect_loop and reattaches when daemon restarts"
