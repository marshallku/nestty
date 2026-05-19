#!/usr/bin/env bash
# Raw PTY → grid → draw throughput. Runs a few firehose scenarios
# back-to-back with a wall-clock timer per scenario.
#
# What to watch:
#   - No dropped / garbled / mis-ordered lines.
#   - Renderer keeps up (no visible "snapshot lag" where text appears
#     in chunks after a freeze).
#   - On Activity Monitor, Nestty CPU stays below ~80% sustained.
#
# Usage: ./scripts/stress/throughput.sh

set -u

scenario() {
    local name="$1"; shift
    echo "=== $name ==="
    local start=$(date +%s)
    "$@" || true
    local elapsed=$(($(date +%s) - start))
    echo "--- $name done in ${elapsed}s ---"
    echo
    sleep 1
}

scenario "hex dump of 50MB urandom (16-col mixed-byte rows)" \
    bash -c 'head -c 52428800 /dev/urandom | xxd'

scenario "seq 1..1M (cheapest per-row, max line rate)" \
    seq 1 1000000

scenario "find -type f under / (sustained mixed-width line stream)" \
    bash -c 'find / -type f 2>/dev/null | head -200000'

scenario "CJK firehose (wide chars bypass walk_row's ASCII aggregation)" \
    bash -c 'yes "$(printf "%.0s가나다라마바사아자차카타파하 " {1..10})" | head -n 30000'

echo "All throughput scenarios complete."
