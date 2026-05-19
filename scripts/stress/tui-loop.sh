#!/usr/bin/env bash
# Cycle through installed TUI apps. Each app runs for $DURATION
# seconds when `timeout`/`gtimeout` is available; otherwise it runs
# until you Ctrl+C out. Missing TUIs print a `[skip]` notice.
#
# What to watch:
#   - btop/htop refresh smoothly, no tearing in gauges/sparklines.
#   - nvim cursor visible in NORMAL and INSERT modes (regression
#     guard for the cursor-on-top + inverse-glyph fix).
#   - lazygit panel borders + diff colors render correctly.
#   - tmux mouse-mode scroll forwards to tmux's own scrollback
#     (wheel up inside a pane scrolls THAT pane, not the host grid).
#
# Usage: ./scripts/stress/tui-loop.sh [duration-seconds]

set -u

DURATION=${1:-10}

# Stock macOS ships neither `timeout` nor `gtimeout` — without
# coreutils we just run each TUI until the user Ctrl+Cs. Linux + macOS
# w/ `brew install coreutils` both bind one of the two names.
TIMEOUT_BIN=""
if command -v timeout >/dev/null 2>&1; then
    TIMEOUT_BIN=timeout
elif command -v gtimeout >/dev/null 2>&1; then
    TIMEOUT_BIN=gtimeout
else
    echo "[note] no timeout/gtimeout on PATH — each TUI runs until Ctrl+C."
    echo "       install via: brew install coreutils"
    echo
fi

run_tui() {
    local cmd="$1"; shift
    if ! command -v "$cmd" >/dev/null 2>&1; then
        echo "[skip] $cmd not installed"
        return
    fi
    if [ -n "$TIMEOUT_BIN" ]; then
        echo "=== $cmd (${DURATION}s — Ctrl+C to skip early) ==="
    else
        echo "=== $cmd (no timeout — Ctrl+C to move on) ==="
    fi
    sleep 1
    if [ -n "$TIMEOUT_BIN" ]; then
        "$TIMEOUT_BIN" "${DURATION}s" "$cmd" "$@" || true
    else
        "$cmd" "$@" || true
    fi
    sleep 1
}

run_tui htop
run_tui btop
run_tui lazygit

# nvim — open a large generated buffer with treesitter-able content.
if command -v nvim >/dev/null 2>&1; then
    BUF=$(mktemp -t nvim-stress.XXXXXX)
    seq 1 20000 | awk '{printf "%6d  the quick brown fox jumps over the lazy dog %d\n", $1, $1*$1}' > "$BUF"
    echo "=== nvim on 20k-line buffer (${DURATION}s) ==="
    echo "Try: j/k spam, Ctrl+D/U, :%s/fox/CAT/g, then :q!"
    sleep 1
    if [ -n "$TIMEOUT_BIN" ]; then
        "$TIMEOUT_BIN" "${DURATION}s" nvim "$BUF" || true
    else
        nvim "$BUF" || true
    fi
    rm -f "$BUF"
fi

# tmux — only test if not already inside one.
if command -v tmux >/dev/null 2>&1 && [ -z "${TMUX:-}" ]; then
    echo "=== tmux 3-way split (${DURATION}s) ==="
    sleep 1
    if [ -n "$TIMEOUT_BIN" ]; then
        "$TIMEOUT_BIN" "${DURATION}s" tmux new-session -A -s nestty-stress \
            "tail -F /var/log/system.log 2>/dev/null || cat" \
            \; split-window -h "command -v htop >/dev/null && htop || top" \
            \; split-window -v "yes 'tmux right-bottom pane'" \
            || true
    else
        tmux new-session -A -s nestty-stress \
            "tail -F /var/log/system.log 2>/dev/null || cat" \
            \; split-window -h "command -v htop >/dev/null && htop || top" \
            \; split-window -v "yes 'tmux right-bottom pane'" \
            || true
    fi
    tmux kill-session -t nestty-stress 2>/dev/null || true
fi

echo "TUI rotation complete."
