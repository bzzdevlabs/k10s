#!/usr/bin/env bash
# Bare-process baseline: build, scale test (N=1,10,50,100), latency test vs one instance. Writes bench/results/native/*.json + SUMMARY.md
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"; BENCH="$(cd "$HERE/.." && pwd)"
export REPEATS="${REPEATS:-3}"
OUT="${RESULTS:-$BENCH/results/native}"; mkdir -p "$OUT"
NO_DOCKER=1 "$BENCH/workloads/build.sh"
ulimit -n 65536 2>/dev/null || true
python3 "$HERE/native.py" scale "$BENCH/bin/echo" "$OUT/scale.json" "$REPEATS"
python3 "$HERE/native.py" latency "$BENCH/bin" "$OUT/latency.json" "$REPEATS"
python3 "$BENCH/summarize.py" native
