#!/usr/bin/env bash
# Full k8s baseline: single-node suite, then the 3-node suite (idle + worker failure). ~1 h. Idempotent: results are overwritten,
# clusters are recreated (RECREATE=1) and deleted at the end (KEEP_CLUSTER=1 to keep).
#   SKIP_THREE=1 skips the 3-node part   REPEATS=3 (cheap measurements)   QUICK=1 shortens windows (smoke test only)
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"; cd "$HERE"
export REPEATS="${REPEATS:-3}"
if [ "${QUICK:-0}" = 1 ]; then export REPEATS=1 IDLE_WINDOW=10 SETTLE=10 LAT_DURATION=3 LAT_WARMUP=1 LAT_CONNS=16 NODEFAIL_CAP=60 NS_LIST="1 10" POD_SETTLE=5; fi
cleanup() { [ "${KEEP_CLUSTER:-0}" = 1 ] || { VARIANT=single ./delete-cluster.sh >/dev/null 2>&1 || true; VARIANT=three ./delete-cluster.sh >/dev/null 2>&1 || true; }; }
trap cleanup EXIT
"$HERE/../workloads/build.sh"
export RECREATE=1
export VARIANT=single
./create-cluster.sh; ./load-images.sh
./idle.sh; ./pods.sh; ./first-response.sh; ./latency.sh; ./failover.sh; ./rollout.sh; ./sizes.sh
./delete-cluster.sh
if [ "${SKIP_THREE:-0}" != 1 ]; then
  export VARIANT=three
  ./create-cluster.sh; ./load-images.sh
  ./idle.sh; ./nodefail.sh
  ./delete-cluster.sh
fi
python3 "$HERE/../summarize.py" k8s
