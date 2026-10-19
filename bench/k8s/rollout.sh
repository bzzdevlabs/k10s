#!/usr/bin/env bash
# Rolling update of a 10-replica echo deployment, alternating image tag v1 <-> v2 (REPEATS*2 updates, default strategy 25%/25%, no readiness probe).
source "$(dirname "$0")/lib.sh"
check_disk
REPLICAS=10 apply_manifest echo.yaml; $K scale deploy/echo --replicas=10 >/dev/null
$K set image deploy/echo echo=k10s-bench/echo:v1 >/dev/null
wait_deploy_ready echo 10 >/dev/null; wait_http $NODEPORT_ECHO; sleep 5
: > "$TMP/runs.jsonl"
cur=v1
for i in $(seq $(( REPEATS * 2 ))); do
  next=v2; [ $cur = v2 ] && next=v1
  python3 "$K8S_DIR/probe.py" $NODEPORT_ECHO "$TMP/probe.txt" 40 20 & pp=$!
  sleep 2
  t0=$(now_ms)
  $K set image deploy/echo echo=k10s-bench/echo:$next >/dev/null
  $K rollout status deploy/echo --timeout=300s >/dev/null
  total=$(( $(now_ms) - t0 ))
  # fully done = no terminating/old pods left
  while [ "$($K get pods -l app=echo --no-headers 2>/dev/null | wc -l)" != 10 ]; do sleep 0.1; done
  total_clean=$(( $(now_ms) - t0 ))
  sleep 1
  wait $pp
  python3 "$K8S_DIR/analyze_probe.py" "$TMP/probe.txt" "$t0" | python3 -c "
import json,sys; o=json.loads(sys.stdin.read()); o.update({'update':'$cur->$next','rollout_status_ms':$total,'rollout_and_old_pods_gone_ms':$total_clean}); print(json.dumps(o))" | tee -a "$TMP/runs.jsonl" >&2
  cur=$next
done
$K scale deploy/echo --replicas=0 >/dev/null
write_result rollout
