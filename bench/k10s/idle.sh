#!/usr/bin/env bash
# Idle footprint (same definitions as k8s/idle.sh): REPEATS windows of IDLE_WINDOW (60 s) after SETTLE seconds, per
# configuration, with 0 services and with the 4 reference services deployed (echo, chain, compute, counter; one
# request each through every node's ingress = one warm instance each).
#   single-8: one node, 8 shards   single-1: one node, 1 shard   cluster-3: 3 nodes x CLUSTER_SHARDS shards (localhost)
# Memory: cgroup working set (memory.current - inactive_file) of each node's scope, RSS/PSS from smaps_rollup.
# CPU: cgroup cpu.stat usage over the window.
source "$(dirname "$0")/lib.sh"
check_disk
SETTLE="${SETTLE:-15}"; WIN="${IDLE_WINDOW:-60}"; CONFIGS="${IDLE_CONFIGS:-single-8 single-1 cluster-3}"
: > "$TMP/runs.jsonl"
windows() { # windows <config> <services> <pids_csv> <addrs_csv>
  local cfg=$1 nsvc=$2 pids=$3 addrs=$4 r
  log "$cfg services=$nsvc: settling ${SETTLE}s"; sleep "$SETTLE"
  for r in $(seq "$REPEATS"); do
    $PY snap "$pids" > "$TMP/a.json"; sleep "$WIN"; $PY snap "$pids" "$addrs" > "$TMP/b.json"
    $PY idle "$TMP/a.json" "$TMP/b.json" > "$TMP/r.json"
    l0=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["loadavg"][0])' "$TMP/a.json")
    l1=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["loadavg"][0])' "$TMP/b.json")
    add_fields "$TMP/r.json" config="\"$cfg\"" services="$nsvc" repeat="$r" noisy="$(noisy_flag "$l0" "$l1")" >> "$TMP/runs.jsonl"
    tail -1 "$TMP/runs.jsonl" | python3 -c 'import json,sys; o=json.loads(sys.stdin.read()); print("%s svc=%d rep=%d rss=%.2f MiB pss=%.2f MiB ws=%s MiB cpu=%.4f cores load=%s"%(o["config"],o["services"],o["repeat"],o["rss_sum_mib"],o["pss_sum_mib"],round(o.get("working_set_mib",-1),2),o["cpu_cores"],o["loadavg_end"][0]))' >&2
  done
}
for cfg in $CONFIGS; do
  case $cfg in
    single-*)
      sh=${cfg#single-}
      start_node "idle-$cfg" "$BASE_PORT" --shards "$sh"; P=$LAST_PID; A="127.0.0.1:$BASE_PORT"
      windows "$cfg" 0 "$P" "$A"
      apply_text "$A" "$REF_MANIFEST"; warm_refs "$A"
      windows "$cfg" 4 "$P" "$A"
      stop_node "$P" ;;
    cluster-*)
      n=${cfg#cluster-}
      start_cluster idle-cl "$n"
      PIDS=$(pids_csv "${CL_PIDS[@]}"); ADDRS=$(pids_csv "${CL_HTTP[@]}")
      windows "$cfg" 0 "$PIDS" "$ADDRS"
      apply_text "${CL_HTTP[0]}" "$REF_MANIFEST"; sleep 1; warm_refs "${CL_HTTP[@]}"
      windows "$cfg" 4 "$PIDS" "$ADDRS"
      stop_cluster ;;
  esac
done
write_result idle
