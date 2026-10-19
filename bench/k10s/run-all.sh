#!/usr/bin/env bash
# Full k10s suite: build (k10s release binary + loadgen), then idle, overhead/density, startup/cold start, latency
# (closed + open loop, single node + 3-node), failover, rolling update, sizes; then results/k10s/SUMMARY.md.
# ~1 h. Idempotent: results are overwritten; every node runs on localhost ports K10S_PORT_BASE (28080)..+400 and is
# killed at exit. QUICK=1 shortens everything (smoke test, numbers meaningless). Single scripts can be run alone
# (they reuse the copied binary in bench/.cache/k10s/bin; `prepare` refreshes it).
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"; REPO="$(cd "$HERE/../.." && pwd)"
export REPEATS="${REPEATS:-3}"
if [ "${QUICK:-0}" = 1 ]; then
  export REPEATS=1 IDLE_WINDOW=5 SETTLE=2 LAT_DURATION=3 LAT_WARMUP=1 LAT_CONNS="1 16" NS_INST="1 100" NS_SVC="10 1000" \
    POD_SETTLE=3 STARTUP_NS="1 100" STARTUP_CLUSTER_NS="10" COLD_N=10 OPEN_RATES_echo=2000 OPEN_RATES_chain=2000 OPEN_RATES_compute=200 \
    IDLE_CONFIGS="single-8 cluster-3"
fi
if [ "${SKIP_BUILD:-0}" != 1 ]; then
  ( cd "$REPO" && CARGO_TARGET_DIR=target/node cargo build --release -p k10s )
  NO_DOCKER=1 "$HERE/../workloads/build.sh"
fi
# shellcheck disable=SC1091
[ "${SKIP_PREPARE:-0}" = 1 ] || ( source "$HERE/lib.sh"; prepare_k10s )
for s in ${K10S_SCRIPTS:-idle overhead startup latency failover rollout sizes}; do
  echo "=== k10s/$s.sh ($(date +%T), load $(cut -d' ' -f1-3 /proc/loadavg))" >&2
  "$HERE/$s.sh"
done
python3 "$HERE/../summarize.py" k10s
