#!/usr/bin/env bash
# Stream a long file while you interact with the scrollback. Reproduces
# the conditions that exposed the double-render bug fix: high-rate
# trackpad scroll + concurrent PTY output.
#
# What to watch:
#   - Trackpad / mouse wheel scroll stays smooth while output flows.
#   - Shift+wheel (host-side scroll override) still works.
#   - Cmd+Home jumps to top, Cmd+End snaps back to live bottom.
#   - Typing any key auto-scrolls to bottom (scrollToBottomOnInput).
#
# Usage: ./scripts/stress/scroll-race.sh [path-to-content]
#        Without an arg, generates a 200k-line fixture.

set -u

CONTENT=${1:-}
CLEANUP=""
if [ -z "$CONTENT" ] || [ ! -r "$CONTENT" ]; then
    CONTENT=$(mktemp -t scroll-fixture.XXXXXX)
    CLEANUP="$CONTENT"
    echo "[fixture] generating 200k rows at $CONTENT..."
    seq 1 200000 | awk '{
        printf "%s  row %-6d  payload=%-30s  squared=%d\n",
            strftime("%Y-%m-%d %H:%M:%S"), $1, $1"-"$1"-"$1, $1*$1
    }' > "$CONTENT"
fi

trap '[ -n "$CLEANUP" ] && rm -f "$CLEANUP"' EXIT

cat <<'EOF'
=== streaming starts in 3s ===
Try while it runs:
  - Trackpad: two-finger scroll up/down rapidly
  - Mouse wheel: scroll up while output flows (does motion stay smooth?)
  - Cmd+Home then Cmd+End
  - Shift+PageUp / Shift+PageDown
  - Type a single letter — view should snap to bottom
  - Ctrl+C to stop early
EOF
sleep 3

cat "$CONTENT"
