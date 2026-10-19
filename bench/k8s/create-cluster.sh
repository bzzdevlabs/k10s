#!/usr/bin/env bash
# Create (idempotently) the kind cluster. VARIANT=single|three  RECREATE=1 to force.
# NOTE: single and three both bind host ports 30080-30082: do not run both at once.
source "$(dirname "$0")/lib.sh"
check_disk
if [ "$VARIANT" = single ]; then CFG=kind-single.yaml; else CFG=kind-three.yaml; fi
if kind get clusters 2>/dev/null | grep -qx "$CLUSTER"; then
  if [ "${RECREATE:-0}" = 1 ]; then kind delete cluster --name "$CLUSTER"; else log "cluster $CLUSTER exists, reusing"; fi
fi
if ! kind get clusters 2>/dev/null | grep -qx "$CLUSTER"; then
  other=$(kind get clusters 2>/dev/null | grep -E '^k10s-bench3?$' | grep -vx "$CLUSTER" || true)
  [ -n "$other" ] && { echo "ABORT: cluster $other holds the same host ports; delete it first" >&2; exit 1; }
  t0=$(now_ms)
  kind create cluster --name "$CLUSTER" --image "$NODE_IMAGE" --config "$K8S_DIR/$CFG" --kubeconfig "$KUBECONFIG" --wait 300s
  log "cluster created in $(( $(now_ms) - t0 )) ms"
  echo $(( $(now_ms) - t0 )) > "$RESULTS/.create_ms_$CLUSTER"
fi
$K wait --for=condition=Ready nodes --all --timeout=300s >/dev/null
$K -n kube-system wait --for=condition=Ready pods --all --timeout=300s >/dev/null
log "cluster $CLUSTER ready"
