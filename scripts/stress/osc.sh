#!/usr/bin/env bash
# OSC (Operating System Command) sequence stress. Tests OSC 8
# hyperlink dedup, OSC 52 clipboard policy gate, and DECSCUSR cursor
# shape transitions.
#
# What to watch:
#   - DECSCUSR cycle: cursor visibly changes shape (block ↔ underline
#     ↔ beam, blinking ↔ steady). Default = block on shape 0.
#   - OSC 8 links: terminal underlines + makes them clickable
#     (Cmd+click should open https://example.com/N).
#   - OSC 52: with default `[security] osc52 = "deny"`, expect a
#     stderr warning per burst. With `"allow"`, the final clipboard
#     value should be "osc52-final-write".
#
# Usage: ./scripts/stress/osc.sh

set -u

echo "=== DECSCUSR cycle (shapes 0..6, 0.5s each) ==="
for s in 0 1 2 3 4 5 6; do
    printf "\e[%d q" "$s"
    case "$s" in
        0) label="(reset/default)";;
        1) label="(blinking block)";;
        2) label="(steady block)";;
        3) label="(blinking underline)";;
        4) label="(steady underline)";;
        5) label="(blinking beam)";;
        6) label="(steady beam)";;
    esac
    printf "  shape=%d %s\n" "$s" "$label"
    sleep 0.5
done
printf "\e[0 q"
echo

echo "=== OSC 8 hyperlink density (100 unique URIs in one wrapped line) ==="
for i in $(seq 1 100); do
    printf '\e]8;;https://example.com/%d\e\\link%03d\e]8;;\e\\ ' "$i" "$i"
done
echo
echo

echo "=== OSC 8 hyperlink dedup (same id+uri repeated 200x — should coalesce) ==="
for i in $(seq 1 200); do
    printf '\e]8;id=dup;https://example.com/dup\e\\dup\e]8;;\e\\ '
done
echo
echo

echo "=== OSC 52 burst (50 writes — last one is 'osc52-final-write') ==="
echo "Expected with osc52=\"deny\" (default): 50 warnings on stderr."
echo "Expected with osc52=\"allow\":         clipboard = 'osc52-final-write'."
for i in $(seq 1 49); do
    payload=$(printf 'osc52-burst-%02d' "$i" | base64)
    printf '\e]52;c;%s\a' "$payload"
done
final=$(printf 'osc52-final-write' | base64)
printf '\e]52;c;%s\a' "$final"
echo
echo "Done. Check pbpaste / stderr."
