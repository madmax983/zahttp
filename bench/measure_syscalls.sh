#!/usr/bin/env bash
# Deterministic syscall-count profile of zahttp's response-write path.
#
# Builds the binary exactly as documented (rustc --edition 2021 -O), runs it
# under `strace -f -c` (needed because each connection is served on its own
# std::thread), drives bench/workload.py's fixed, seeded request mix against
# it, and reports the strace -c summary. `write`, `sendto`, and `writev` are
# the syscalls the response path can issue; their combined count is the
# deterministic counter this benchmark gates on (see bench/PROFILE.md).
#
# Usage: bench/measure_syscalls.sh [connections] [requests-per-conn]
set -euo pipefail
cd "$(dirname "$0")/.."

CONNECTIONS="${1:-20}"
REQS_PER_CONN="${2:-100}"
BIN="$(mktemp /tmp/zahttp_bench.XXXXXX)"
STRACE_OUT="$(mktemp /tmp/zahttp_strace.XXXXXX)"
trap 'rm -f "$BIN" "$STRACE_OUT"' EXIT

echo "building: rustc --edition 2021 -O -o $BIN main.rs" >&2
rustc --edition 2021 -O -o "$BIN" main.rs

nohup strace -f -c -o "$STRACE_OUT" -- "$BIN" >/tmp/zahttp_bench_server.log 2>&1 &
STRACE_PID=$!
# strace forks-and-execs the target as its direct child; `pgrep -f "$BIN"`
# would also match strace's own command line (it names $BIN as an argv),
# so find the tracee specifically as strace's child process.
for _ in $(seq 1 50); do
    SERVER_PID="$(pgrep -P "$STRACE_PID" | head -1 || true)"
    [ -n "${SERVER_PID:-}" ] && break
    sleep 0.1
done
if [ -z "${SERVER_PID:-}" ]; then
    echo "server never started" >&2
    exit 1
fi
sleep 0.3

python3 bench/workload.py --connections "$CONNECTIONS" --requests-per-conn "$REQS_PER_CONN" --seed 1337

sleep 0.3
kill -TERM "$SERVER_PID"
wait "$STRACE_PID" 2>/dev/null || true

echo "=== strace -f -c summary (rustc -O build, $((CONNECTIONS * REQS_PER_CONN)) requests, seed 1337) ==="
cat "$STRACE_OUT"

WRITE_FAMILY=$(awk '$NF=="write"||$NF=="sendto"||$NF=="writev"{s+=$4} END{print s+0}' "$STRACE_OUT")
echo ""
echo "write-family syscalls (write+sendto+writev): $WRITE_FAMILY"
