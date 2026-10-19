#!/usr/bin/env bash
# Startup (cf. k8s/pods.sh "all Ready" and k8s/first-response.sh).
#  apply:   fresh node (empty data dir), t0 -> `k10s apply` of N echo services (precompiled .k10 = images already
#           loaded) -> every service answers HTTP 200 (polled in 10 ms sweeps of pipelined GETs). Instances are created
#           by those first requests, so this is deploy + cold start of N services. N = STARTUP_NS (1 10 50 100 1000).
#           Also on a 3-node cluster (apply on node 0, every service answering through every node's ingress).
#  cold:    first request to a never-instantiated service (fresh TCP connection, from Python) vs the second request
#           (same client, fresh connection): the difference is instance creation + the first invocation's extra work.
source "$(dirname "$0")/lib.sh"
check_disk
NS="${STARTUP_NS:-1 10 50 100 1000}"; CL_NS="${STARTUP_CLUSTER_NS:-1 10 100}"; COLD_N="${COLD_N:-50}"
A="127.0.0.1:$BASE_PORT"
: > "$TMP/runs.jsonl"
emit() { local f=$1; shift; add_fields "$f" "$@" >> "$TMP/runs.jsonl"; tail -1 "$TMP/runs.jsonl" | python3 -c 'import json,sys; o=json.loads(sys.stdin.read()); o.pop("rows",None); o.pop("apply_out",None); print(json.dumps(o)[:400])' >&2; }
for n in $NS; do
  $PY manifest s "$n" echo.k10 memory=64KiB > "$CACHE/mod/st-$n.k10s"
  for r in $(seq "$REPEATS"); do
    start_node st "$BASE_PORT" --shards "$SHARDS"; P=$LAST_PID
    sleep 1; l0=$(load1)
    $PY serve-time "$K10S" "$CACHE/mod/st-$n.k10s" "$A" s "$n" > "$TMP/r.json"
    emit "$TMP/r.json" variant='"single"' repeat="$r" loadavg_start="$l0" loadavg_end="$(load1)" noisy="$(noisy_flag "$l0" "$(load1)")"
    stop_node "$P"
  done
done
for n in $CL_NS; do
  [ -f "$CACHE/mod/st-$n.k10s" ] || $PY manifest s "$n" echo.k10 memory=64KiB > "$CACHE/mod/st-$n.k10s"
  for r in $(seq "$REPEATS"); do
    start_cluster st-cl 3
    sleep 1; l0=$(load1)
    $PY serve-time "$K10S" "$CACHE/mod/st-$n.k10s" "$(pids_csv "${CL_HTTP[@]}")" s "$n" > "$TMP/r.json"
    emit "$TMP/r.json" variant='"cluster-3"' repeat="$r" loadavg_start="$l0" loadavg_end="$(load1)" noisy="$(noisy_flag "$l0" "$(load1)")"
    stop_cluster
  done
done
rm -f "$CACHE"/mod/st-*.k10s
# cold start: never-instantiated services
for r in $(seq "$REPEATS"); do
  start_node cold "$BASE_PORT" --shards "$SHARDS"; P=$LAST_PID
  { $PY manifest c "$COLD_N" echo.k10 memory=64KiB; $PY manifest k "$COLD_N" compute.k10 memory=64KiB cpu_ms=60000
    $PY manifest cka "$COLD_N" echo.k10 memory=64KiB; $PY manifest kka "$COLD_N" compute.k10 memory=64KiB cpu_ms=60000; } > "$CACHE/mod/cold.k10s"
  "$K10S" apply "$CACHE/mod/cold.k10s" --node "$A" >/dev/null
  sleep 1; l0=$(load1)
  $PY cold "$A" c "$COLD_N" > "$TMP/c1.json"
  $PY cold "$A" k "$COLD_N" '?n=1' > "$TMP/c2.json"
  $PY status "$A" > "$TMP/st.json"
  for f in c1:echo c2:compute; do
    python3 - "$TMP/${f%%:*}.json" "$TMP/st.json" "${f#*:}" > "$TMP/r.json" <<'PY'
import json,sys
o=json.load(open(sys.argv[1])); st=json.load(open(sys.argv[2])); pre="c" if sys.argv[3]=="echo" else "k"
sv=[s for s in st["services"] if s["name"].startswith(pre) and s["name"][len(pre):].lstrip("ka").isdigit()]
o["server_init_ms_per_instance"]=sum(s["init_ms_total"] for s in sv)/max(1,sum(s["created"] for s in sv))
o["module"]=sys.argv[3]; o.pop("rows"); print(json.dumps(o))
PY
    emit "$TMP/r.json" variant='"cold"' repeat="$r" loadavg_start="$l0" loadavg_end="$(load1)" noisy="$(noisy_flag "$l0" "$(load1)")"
  done
  stop_node "$P"
done
write_result startup
