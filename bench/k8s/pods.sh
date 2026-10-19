#!/usr/bin/env bash
# Per-pod overhead + startup: scale echo 0 -> N (N=1,10,50,100), REPEATS times each.
source "$(dirname "$0")/lib.sh"
check_disk
NS="${NS_LIST:-1 10 50 100}"
REPLICAS=0 apply_manifest echo.yaml
: > "$TMP/runs.jsonl"
N0=$(nodes | head -1)
for n in $NS; do
  for r in $(seq "$REPEATS"); do
    check_disk
    wait_pods_gone app=echo; sleep $(( n >= 50 ? 20 : 10 ))
    nstat snap $(nodes) > "$TMP/a.json"
    t0=$(now_ms); $K scale deploy/echo --replicas="$n" >/dev/null
    ready=$(wait_deploy_ready echo "$n" 300)
    log "N=$n rep=$r ready_ms=$ready"
    sleep "${POD_SETTLE:-20}"
    nstat snap $(nodes) > "$TMP/b.json"
    nstat pods "$TMP/a.json" "$TMP/b.json" "$n" > "$TMP/r.json"
    python3 - "$TMP/r.json" "$ready" "$r" >> "$TMP/runs.jsonl" <<'PY'
import json,sys
r=json.load(open(sys.argv[1])); r["all_ready_ms"]=int(sys.argv[2]); r["repeat"]=int(sys.argv[3]); print(json.dumps(r))
PY
    $K scale deploy/echo --replicas=0 >/dev/null
  done
done
wait_pods_gone app=echo
write_result pods
