#!/usr/bin/env bash
# Shared config/helpers for the k8s benchmarks. Source me.
set -euo pipefail
K8S_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BENCH_DIR="$(cd "$K8S_DIR/.." && pwd)"
export PATH="$BENCH_DIR/bin:$PATH"
VARIANT="${VARIANT:-single}"                       # single | three
NODE_IMAGE="${NODE_IMAGE:-kindest/node:v1.34.0@sha256:7416a61b42b1662ca6ca89f02028ac133a309a2a30ba309614e8ec94d976dc5a}"
if [ "$VARIANT" = three ]; then CLUSTER="${CLUSTER:-k10s-bench3}"; else CLUSTER="${CLUSTER:-k10s-bench}"; fi
export KUBECONFIG="${KUBECONFIG_BENCH:-$BENCH_DIR/results/.kubeconfig-$CLUSTER}"   # private kubeconfig: never touches ~/.kube
K="kubectl --context kind-$CLUSTER"
RESULTS="${RESULTS:-$BENCH_DIR/results/k8s}"
REPEATS="${REPEATS:-3}"
NODEPORT_ECHO=30080; NODEPORT_CHAIN=30081; NODEPORT_COMPUTE=30082
HOSTADDR=127.0.0.1
mkdir -p "$RESULTS"

now_ms() { echo $(( $(date +%s%N) / 1000000 )); }
nodes() { docker ps --filter "label=io.x-k8s.kind.cluster=$CLUSTER" --format '{{.Names}}' | sort; }
log() { echo "[$(date +%T)] $*" >&2; }
free_gb() { df -BG --output=avail /home | tail -1 | tr -dc 0-9; }
check_disk() { local f; f=$(free_gb); if [ "$f" -lt 3 ]; then echo "ABORT: only ${f}G free on /home" >&2; exit 3; fi; }
nstat() { python3 "$K8S_DIR/nodestat.py" "$@"; }

# apply a manifest from $K8S_DIR/manifests (with simple @@VAR@@ substitution)
apply_manifest() { sed -e "s#@@REPLICAS@@#${REPLICAS:-1}#g" "$K8S_DIR/manifests/$1" | $K apply -f - >/dev/null; }

wait_deploy_ready() { # name want timeout_s -> echoes ms elapsed since call
  local name=$1 want=$2 to=${3:-300} t0 r; t0=$(now_ms)
  while :; do
    r=$($K get deploy "$name" -o jsonpath='{.status.readyReplicas}' 2>/dev/null || true)
    [ "${r:-0}" = "$want" ] && break
    [ $(( $(now_ms) - t0 )) -gt $((to*1000)) ] && { echo "timeout waiting $name" >&2; return 1; }
    sleep 0.1
  done
  echo $(( $(now_ms) - t0 ))
}
wait_pods_gone() { local t0; t0=$(now_ms); while [ -n "$($K get pods -l "$1" -o name 2>/dev/null)" ]; do
  [ $(( $(now_ms) - t0 )) -gt 180000 ] && return 1; sleep 0.5; done; }
http_ok() { curl -s -o /dev/null --max-time "${2:-1}" -w '%{http_code}' "http://$HOSTADDR:$1/" 2>/dev/null | grep -q '^200$'; }
wait_http() { local t0; t0=$(now_ms); until http_ok "$1" 1; do [ $(( $(now_ms) - t0 )) -gt 120000 ] && return 1; sleep 0.2; done; }

SUFFIX=""; [ "$VARIANT" = three ] && SUFFIX="-three"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/k10sbench.XXXXXX")"; trap 'rm -rf "$TMP"' EXIT
# write_result <name>: wraps $TMP/runs.jsonl with the environment into $RESULTS/<name><suffix>.json
write_result() {
  nstat env "$CLUSTER" "$KUBECONFIG" "$NODE_IMAGE" > "$TMP/env.json"
  nstat wrap "$1" "$TMP/env.json" "$TMP/runs.jsonl" > "$RESULTS/$1$SUFFIX.json"
  log "wrote $RESULTS/$1$SUFFIX.json"
}
CP_NODE() { echo "$CLUSTER-control-plane"; }
