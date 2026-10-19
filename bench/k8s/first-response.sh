#!/usr/bin/env bash
# One pod: scale 0->1, time to PodScheduled, Ready, and first HTTP 200 through the NodePort.
source "$(dirname "$0")/lib.sh"
check_disk
REPLICAS=0 apply_manifest echo.yaml
: > "$TMP/runs.jsonl"
for r in $(seq "$REPEATS"); do
  wait_pods_gone app=echo; sleep 5
  rm -f "$TMP"/t_*
  ( until [ -n "$($K get pod -l app=echo -o jsonpath='{.items[0].spec.nodeName}' 2>/dev/null)" ]; do sleep 0.02; done; now_ms > "$TMP/t_sched" ) &
  p1=$!
  ( until [ "$($K get deploy echo -o jsonpath='{.status.readyReplicas}' 2>/dev/null)" = 1 ]; do sleep 0.02; done; now_ms > "$TMP/t_ready" ) &
  p2=$!
  ( until http_ok $NODEPORT_ECHO 0.3; do sleep 0.01; done; now_ms > "$TMP/t_http" ) &
  p3=$!
  t0=$(now_ms); $K scale deploy/echo --replicas=1 >/dev/null; t1=$(now_ms)
  wait $p1 $p2 $p3
  python3 - "$t0" "$t1" "$TMP" >> "$TMP/runs.jsonl" <<'PY'
import json,sys
t0=int(sys.argv[1]); d=sys.argv[3]; g=lambda n:int(open(f"{d}/t_{n}").read())-t0
s,rd,h=g("sched"),g("ready"),g("http")
print(json.dumps({"scale_cmd_ms":int(sys.argv[2])-t0,"to_scheduled_ms":s,"to_ready_ms":rd,"to_first_http_ms":h,"scheduled_to_first_http_ms":h-s}))
PY
  tail -1 "$TMP/runs.jsonl" >&2
  $K scale deploy/echo --replicas=0 >/dev/null
done
wait_pods_gone app=echo
write_result first_response
