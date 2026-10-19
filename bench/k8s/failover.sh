#!/usr/bin/env bash
# Pod failover. (a) 1 replica: delete pod (graceful and forced); outage seen by a fresh-connection probe every 20 ms.
# (b) 3 replicas: delete one pod under 300 req/s open-loop load; count errors.
source "$(dirname "$0")/lib.sh"
check_disk
: > "$TMP/runs.jsonl"
pod_names() { $K get pods -l app=echo -o jsonpath='{.items[*].metadata.name}'; }
one_replica() { # mode: graceful|forced
  local mode=$1 r
  for r in $(seq "$REPEATS"); do
    REPLICAS=1 apply_manifest echo.yaml; $K scale deploy/echo --replicas=1 >/dev/null
    wait_deploy_ready echo 1 >/dev/null; wait_http $NODEPORT_ECHO; sleep 3
    old=$(pod_names)
    python3 "$K8S_DIR/probe.py" $NODEPORT_ECHO "$TMP/probe.txt" 14 20 & pp=$!
    sleep 3
    t0=$(now_ms)
    if [ $mode = forced ]; then $K delete pod "$old" --grace-period=0 --force --wait=false >/dev/null 2>&1
    else $K delete pod "$old" --wait=false >/dev/null; fi
    # replacement Ready = a pod with a different name that is Ready
    while :; do
      new=$($K get pods -l app=echo -o jsonpath='{range .items[?(@.status.containerStatuses[0].ready==true)]}{.metadata.name}{"\n"}{end}' 2>/dev/null | grep -v "^$old$" | head -1 || true)
      [ -n "$new" ] && break; [ $(( $(now_ms) - t0 )) -gt 120000 ] && break; sleep 0.05
    done
    tready=$(( $(now_ms) - t0 ))
    wait $pp
    python3 "$K8S_DIR/analyze_probe.py" "$TMP/probe.txt" "$t0" | python3 -c "
import json,sys; o=json.loads(sys.stdin.read()); o.update({'scenario':'1-replica-$mode','repeat':$r,'replacement_ready_ms':$tready}); print(json.dumps(o))" | tee -a "$TMP/runs.jsonl" >&2
  done
}
one_replica graceful
one_replica forced
# (b) 3 replicas under load
for r in $(seq "$REPEATS"); do
  REPLICAS=3 apply_manifest echo.yaml; $K scale deploy/echo --replicas=3 >/dev/null
  wait_deploy_ready echo 3 >/dev/null; wait_http $NODEPORT_ECHO; sleep 3
  victim=$(pod_names | awk '{print $1}')
  ( sleep 5; $K delete pod "$victim" --wait=false >/dev/null ) &
  loadgen -addr $HOSTADDR:$NODEPORT_ECHO -conns 16 -rate 300 -duration 12s -warmup 0s -label failover-3rep > "$TMP/o.json" || true
  wait
  python3 -c "
import json; o=json.load(open('$TMP/o.json')); print(json.dumps({'scenario':'3-replicas-delete-1-under-300rps','repeat':$r,'requests':o['requests'],'errors':o['errors'],'errors_by_kind':o['errors_by_kind'],'latency_us':o['latency_us']}))" | tee -a "$TMP/runs.jsonl" >&2
  wait_deploy_ready echo 3 >/dev/null
done
REPLICAS=1 apply_manifest echo.yaml; $K scale deploy/echo --replicas=0 >/dev/null
write_result failover
