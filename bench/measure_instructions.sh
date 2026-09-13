#!/usr/bin/env bash
# Deterministic instruction-count profile of zahttp's request path, via
# valgrind --tool=callgrind.
#
# Builds the binary exactly as documented (rustc -O -C debuginfo=0, the
# module tree under main.rs picked up automatically), runs it under
# callgrind (single process — zahttp never execs a child, so no
# --trace-children is needed), drives bench/workload.py's fixed, seeded
# request mix against it, and reports the total instruction count (Ir)
# plus a per-function breakdown via callgrind_annotate. Ir is the
# deterministic counter this benchmark gates on: unlike wall-clock, it is
# reproducible to within a handful of instructions across runs on the
# same binary and workload (see bench/PROFILE_DECODED.md).
#
# Usage: bench/measure_instructions.sh [connections] [requests-per-conn]
set -euo pipefail
cd "$(dirname "$0")/.."

CONNECTIONS="${1:-20}"
REQS_PER_CONN="${2:-100}"
BIN="$(mktemp /tmp/zahttp_cgbench.XXXXXX)"
OUT_DIR="$(mktemp -d /tmp/zahttp_cgout.XXXXXX)"
CG_OUT="$OUT_DIR/callgrind.out"
SERVER_LOG="$(mktemp /tmp/zahttp_cgserver.XXXXXX)"
trap 'rm -f "$BIN" "$SERVER_LOG"; rm -rf "$OUT_DIR"' EXIT

echo "building: rustc -O -C debuginfo=0 -o $BIN main.rs" >&2
rustc -O -C debuginfo=0 -o "$BIN" main.rs

nohup valgrind --tool=callgrind --callgrind-out-file="$CG_OUT" -- "$BIN" >"$SERVER_LOG" 2>&1 &
CG_PID=$!

# callgrind runs the target directly under its own PID (no fork+exec
# wrapper the way strace uses one), so $! is already the profiled
# process. Give it time to finish loading and bind the listener.
for _ in $(seq 1 100); do
    grep -q "zahttp on" "$SERVER_LOG" 2>/dev/null && break
    sleep 0.1
done
if ! grep -q "zahttp on" "$SERVER_LOG" 2>/dev/null; then
    echo "server never started" >&2
    cat "$SERVER_LOG" >&2
    exit 1
fi
sleep 0.3

python3 bench/workload.py --connections "$CONNECTIONS" --requests-per-conn "$REQS_PER_CONN" --seed 1337

sleep 0.3
kill -TERM "$CG_PID"
wait "$CG_PID" 2>/dev/null || true

echo "=== callgrind Ir summary ($((CONNECTIONS * REQS_PER_CONN)) requests, seed 1337) ==="
tail -6 "$SERVER_LOG"
echo ""
echo "=== top self-cost functions (callgrind_annotate --threshold=99.5) ==="
callgrind_annotate --threshold=99.5 "$CG_OUT" 2>/dev/null | sed -n '/^Ir /,/^$/p' | head -40
