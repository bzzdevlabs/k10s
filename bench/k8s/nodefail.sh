#!/usr/bin/env bash
# 3-node variant: freeze (docker pause) or kill (docker stop) a worker, record what Kubernetes actually does.
# MODES="pause stop"; NODEFAIL_CAP=390 seconds max wait per mode.
source "$(dirname "$0")/lib.sh"
[ "$VARIANT" = three ] || { echo "run with VARIANT=three" >&2; exit 1; }
check_disk
CAP="${NODEFAIL_CAP:-390}"; MODES="${MODES:-pause stop}"
: > "$TMP/runs.jsonl"
VICTIM="$CLUSTER-worker"
for mode in $MODES; do
  log "mode=$mode: preparing (6 replicas spread over nodes)"
  REPLICAS=6 apply_manifest echo.yaml; $K scale deploy/echo --replicas=6 >/dev/null
  $K delete pod -l app=echo --wait=true >/dev/null   # fresh spread
  wait_deploy_ready echo 6 >/dev/null; wait_http $NODEPORT_ECHO; sleep 5
  dist=$($K get pods -l app=echo -o jsonpath='{range .items[*]}{.spec.nodeName}{"\n"}{end}' | sort | uniq -c | tr -s ' ' | tr '\n' ';')
  python3 "$K8S_DIR/probe.py" $NODEPORT_ECHO "$TMP/probe.txt" "$CAP" 100 & pp=$!
  python3 "$K8S_DIR/nodefail_watch.py" "$KUBECONFIG" "kind-$CLUSTER" "$VICTIM" "$CAP" "$TMP/watch.jsonl" & pw=$!
  sleep 5
  tf=$(now_ms)
  if [ $mode = pause ]; then docker pause "$VICTIM" >/dev/null; else docker stop -t 1 "$VICTIM" >/dev/null; fi
  log "worker $mode at t0; waiting up to ${CAP}s (early exit 20s after full recovery)"
  while [ $(( $(now_ms) - tf )) -lt $(( (CAP-8) * 1000 )) ]; do
    sleep 5
    if python3 "$K8S_DIR/nodefail_analyze.py" "$TMP/watch.jsonl" "$TMP/probe.txt" "$tf" "$VICTIM" 2>/dev/null | grep -q '"all_replicas_ready_on_healthy_nodes_s": [0-9]'; then sleep 20; break; fi
  done
  kill $pw $pp 2>/dev/null || true; wait $pw $pp 2>/dev/null || true
  python3 "$K8S_DIR/nodefail_analyze.py" "$TMP/watch.jsonl" "$TMP/probe.txt" "$tf" "$VICTIM" | python3 -c "
import json,sys; o=json.loads(sys.stdin.read()); o.update({'mode':'$mode','cap_s':$CAP,'waited_s':round(($(now_ms)-$tf)/1000),'initial_pod_distribution':'$dist'}); print(json.dumps(o))" | tee -a "$TMP/runs.jsonl" >&2
  # restore
  if [ $mode = pause ]; then docker unpause "$VICTIM" >/dev/null; else docker start "$VICTIM" >/dev/null; fi
  $K wait --for=condition=Ready node/"$VICTIM" --timeout=180s >/dev/null || log "WARN: victim not Ready again"
  $K taint node "$VICTIM" node.kubernetes.io/unreachable- >/dev/null 2>&1 || true
  $K scale deploy/echo --replicas=0 >/dev/null; wait_pods_gone app=echo || true
  $K -n kube-system wait --for=condition=Ready pods --all --timeout=180s >/dev/null || true
  sleep 10
done
write_result nodefail
