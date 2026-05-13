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

[[ -x "$DAEMON" ]] || { echo "build first: cargo build -p nestty-daemon"; exit 2; }

WORK="$(mktemp -d -t nestty-e2e.XXXXXX)"
DAEMON_LOG="$WORK/daemon.log"
GUI_LOG="$WORK/gui.log"
SOCKET="$WORK/nesttyd.sock"

export NESTTY_SOCKET="$SOCKET"
export RUST_LOG="${RUST_LOG:-info}"

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
        # ignore everything else

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

step 5 "frozen GUI → heartbeat-miss unregister"
kill -STOP "$GUI_PID"
# Two misses: interval(10s) + timeout(5s) ×2 ≈ 30s. Give it 40.
wait_for "consecutive misses on $client_2" "$DAEMON_LOG" 40 \
    || fail "no heartbeat-miss after freezing mock-gui"
wait_for "gui unregistered: client_id=$client_2" "$DAEMON_LOG" 5 \
    || fail "unregister log missing after heartbeat misses"
pass "heartbeat-miss path fired and unregistered ${client_2:0:8}…"

# Resume so wait in cleanup doesn't hang
kill -CONT "$GUI_PID" 2>/dev/null || true

echo
echo "=== AUTO E2E COMPLETE ==="
echo
echo "Manual visual check (independent of this run — the trap already shut things down):"
echo "  1. start daemon in one terminal:  $DAEMON"
echo "  2. start GUI in another:           NESTTY_DAEMON_CLIENT=1 cargo run -p nestty-linux"
echo "     (both default to the same socket path; do not set NESTTY_SOCKET unless"
echo "      you start the daemon with the same override)"
echo "  3. confirm: tabs/panels render normally, no extra startup latency, panel commands work"
echo "  4. kill the daemon; confirm GUI logs reconnect_loop and reattaches when daemon restarts"
