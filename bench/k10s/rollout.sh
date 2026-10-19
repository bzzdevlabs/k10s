#!/usr/bin/env bash
# Rolling update under probe load (cf. k8s/rollout.sh: 10-replica echo, image v1 <-> v2, REPEATS*2 updates, probe every
# 20 ms). k10s: re-apply the manifest with a new version of the service (`version.ten`, config "v1" <-> "v2": a
# changed spec replaces the service; in-flight/queued requests finish on the old version).
#  single-8 : one node; besides the probe, open-loop 1000 req/s (16 conns) keeps several instances busy.
#  cluster-3: 3 nodes, replicas = 10 (stacked over the 3 nodes), apply on node 0, probe through node 0,
#             open-loop 1000 req/s through node 1.
# Measured: apply -> every node's ingress answers "hello <new>" (polled every 5 ms) = "rollout complete";
# -> no retired (old-version) service left in any node's status = "old version gone"; failed probes; loadgen errors.
source "$(dirname "$0")/lib.sh"
check_disk
K8S_DIR="$BENCH_DIR/k8s"; MODES="${ROLLOUT_MODES:-single-8 cluster-3}"
: > "$TMP/runs.jsonl"
man() { echo "service web { code = \"version.k10\" memory = 64KiB replicas = $1 config = \"$2\" }"; }
for mode in $MODES; do
  if [ "$mode" = cluster-3 ]; then
    start_cluster ro 3; ADDRS=("${CL_HTTP[@]}"); REPL=10; LGADDR=${CL_HTTP[1]}
  else
    start_node ro "$BASE_PORT" --shards "$SHARDS"; P=$LAST_PID; ADDRS=("127.0.0.1:$BASE_PORT"); REPL=1; LGADDR=${ADDRS[0]}
  fi
  apply_text "${ADDRS[0]}" "$(man $REPL v1)"; sleep 1
  for a in "${ADDRS[@]}"; do for _ in $(seq 20); do curl -s -o /dev/null "http://$a/web"; done; done
  cur=v1
  for i in $(seq $(( REPEATS * 2 ))); do
    next=v2; [ $cur = v2 ] && next=v1
    man $REPL $next > "$CACHE/mod/ro.k10s"
    l0=$(load1)
    python3 "$K8S_DIR/probe.py" "${ADDRS[0]##*:}" "$TMP/probe.txt" 12 20 /web & pp=$!
    loadgen -addr "$LGADDR" -path /web -conns 16 -rate 1000 -duration 11s -warmup 0s -label rollout-load > "$TMP/o.json" & lp=$!
    sleep 3
    t0=$(now_ms)
    $PY rollout "$K10S" "$CACHE/mod/ro.k10s" "$(pids_csv "${ADDRS[@]}")" /web "hello $next" > "$TMP/ro.json"
    wait $pp; wait $lp || true
    python3 "$K8S_DIR/analyze_probe.py" "$TMP/probe.txt" "$t0" > "$TMP/a.json"
    python3 - "$TMP/a.json" "$TMP/ro.json" "$TMP/o.json" > "$TMP/r.json" <<'PY'
import json,sys
o=json.load(open(sys.argv[1])); o.update(json.load(open(sys.argv[2]))); l=json.load(open(sys.argv[3]))
o["load"]={"requests":l["requests"],"errors":l["errors"],"errors_by_kind":l["errors_by_kind"],"p99_us":l["latency_us"]["p99"],"max_us":l["latency_us"]["max"]}
print(json.dumps(o))
PY
    add_fields "$TMP/r.json" mode="\"$mode\"" update="\"$cur->$next\"" replicas="$REPL" loadavg_start="$l0" loadavg_end="$(load1)" noisy="$(noisy_flag "$l0" "$(load1)")" | tee -a "$TMP/runs.jsonl" >&2
    cur=$next
    sleep 2
  done
  if [ "$mode" = cluster-3 ]; then stop_cluster; else stop_node "$P"; fi
done
rm -f "$CACHE/mod/ro.k10s"
write_result rollout
