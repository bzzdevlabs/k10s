#!/usr/bin/env bash
# Request latency/throughput through the k10s HTTP ingress, same loadgen flags as k8s/latency.sh and native.py:
# closed loop, LAT_CONNS (1 16 64) connections, LAT_DURATION 20 s after LAT_WARMUP 3 s, REPEATS times.
#   single-8 : one node (SHARDS=8), loadgen on the same host -> 127.0.0.1:<port>/<service>
#              targets: echo (/echo), chain (/chain: chain -> `call` echo), compute (/compute?n=1000)
#   cluster-3: 3 nodes; echo through the node that hosts it (local) and through another node (forwarded, 1 hop)
#   open loop: fixed rates (OPEN_RATES_<target>), -conns OPEN_CONNS (64) bounds in-flight requests; latency from the
#              intended send time (no coordinated omission).
# CPU of the system under test: cgroup cpu.stat of every node over the measured window (sampled after the warm-up and
# after loadgen exits) -> sut_cpu_us_per_req. The loadgen's own CPU is reported separately (loadgen_cpu_s).
source "$(dirname "$0")/lib.sh"
check_disk
DUR="${LAT_DURATION:-20}"; WARM="${LAT_WARMUP:-3}"; CONNS="${LAT_CONNS:-1 16 64}"
TARGETS="${LAT_TARGETS:-echo chain compute}"; PARTS="${LAT_PARTS:-single cluster open}"
OPEN_CONNS="${OPEN_CONNS:-64}"
# noisy = CPU used by everything else on the host during the measured window (host busy - SUT - loadgen) > NOISY_CPU cores
# (the load average is useless here: the benchmark itself keeps ~10 threads runnable)
NOISY_CPU="${NOISY_CPU:-0.5}"
OPEN_RATES_echo="${OPEN_RATES_echo:-5000 20000}"; OPEN_RATES_chain="${OPEN_RATES_chain:-5000 20000}"; OPEN_RATES_compute="${OPEN_RATES_compute:-500 2000}"
path_of() { case $1 in echo) echo /echo;; chain) echo /chain;; compute) echo '/compute?n=1000';; esac; }
: > "$TMP/runs.jsonl"
# run_lg <pids_csv> <addr> <path> <label> <extra fields> -- <loadgen flags...>
run_lg() {
  local pids=$1 addr=$2 path=$3 label=$4 fields=$5; shift 6
  local l0; l0=$(load1)
  loadgen -addr "$addr" -path "$path" -duration "${DUR}s" -warmup "${WARM}s" -label "$label" "$@" > "$TMP/o.json" &
  local lg=$!
  sleep "$WARM"; local c0 t0 h0; c0=$($PY cpu "$pids"); h0=$($PY hostbusy); t0=$(date +%s.%N)
  wait "$lg" || true
  local c1 t1 h1; c1=$($PY cpu "$pids"); h1=$($PY hostbusy); t1=$(date +%s.%N)
  python3 - "$TMP/o.json" "$c0" "$c1" "$t0" "$t1" "$l0" "$(load1)" "$NOISY_CPU" "$fields" "$h0" "$h1" >> "$TMP/runs.jsonl" <<'PY'
import json,os,sys
o=json.load(open(sys.argv[1])); c0,c1=int(sys.argv[2]),int(sys.argv[3]); w=float(sys.argv[5])-float(sys.argv[4])
l0,l1,thr=float(sys.argv[6]),float(sys.argv[7]),float(sys.argv[8])
o.update(json.loads(sys.argv[9]))
busy=(int(sys.argv[11])-int(sys.argv[10]))/os.sysconf("SC_CLK_TCK")/w
sut=(c1-c0)/1e6/w; lg=o["loadgen_cpu_s"]["total"]/o["duration_s"]
o.update({"sut_cpu_s":(c1-c0)/1e6,"sut_cpu_window_s":w,"sut_cpu_cores":sut,
          "sut_cpu_us_per_req":(sut*o["duration_s"]*1e6)/o["requests"] if o["requests"] else None,
          "host_busy_cores":busy,"other_cpu_cores":busy-sut-lg,
          "loadavg_start":l0,"loadavg_end":l1,"noisy":busy-sut-lg>thr})
print(json.dumps(o))
PY
  tail -1 "$TMP/runs.jsonl" | python3 -c 'import json,sys; o=json.loads(sys.stdin.read()); print("%s %s c=%s rate=%s rep=%s rps=%.0f p50=%s p99=%s err=%s sut_cpu=%.2f cores %.1f us/req other=%.2f cores load=%.1f"%(o["label"],o.get("target"),o["conns"],o["target_rps"],o["repeat"],o["rps"],o["latency_us"]["p50"],o["latency_us"]["p99"],o["errors"],o["sut_cpu_cores"],o["sut_cpu_us_per_req"] or -1,o["other_cpu_cores"],o["loadavg_end"]))' >&2
}
if [[ " $PARTS " == *" single "* || " $PARTS " == *" open "* ]]; then
  start_node lat "$BASE_PORT" --shards "$SHARDS"; P=$LAST_PID; A="127.0.0.1:$BASE_PORT"
  apply_text "$A" "$REF_MANIFEST"; warm_refs "$A"; sleep 1
  if [[ " $PARTS " == *" single "* ]]; then
    for t in $TARGETS; do for c in $CONNS; do for r in $(seq "$REPEATS"); do
      run_lg "$P" "$A" "$(path_of "$t")" "k10s-single-$SHARDS/$t" "{\"system\":\"k10s\",\"place\":\"single-$SHARDS\",\"target\":\"$t\",\"repeat\":$r}" -- -conns "$c"
    done; done; done
  fi
  if [[ " $PARTS " == *" open "* ]]; then
    for t in $TARGETS; do rates="OPEN_RATES_$t"; for rate in ${!rates}; do for r in $(seq "$REPEATS"); do
      run_lg "$P" "$A" "$(path_of "$t")" "k10s-single-$SHARDS-open/$t" "{\"system\":\"k10s\",\"place\":\"single-$SHARDS\",\"target\":\"$t\",\"repeat\":$r}" -- -conns "$OPEN_CONNS" -rate "$rate"
    done; done; done
  fi
  stop_node "$P"
fi
if [[ " $PARTS " == *" cluster "* ]]; then
  start_cluster lat-cl 3
  apply_text "${CL_HTTP[0]}" "$REF_MANIFEST"; sleep 1; warm_refs "${CL_HTTP[@]}"
  PIDS=$(pids_csv "${CL_PIDS[@]}")
  $PY placement "${CL_HTTP[0]}" > "$TMP/pl.json"
  read -r HOST OTHER < <(python3 - "$TMP/pl.json" <<'PY'
import json,sys
d=json.load(open(sys.argv[1])); h=d["members"][d["placement"]["echo"][0]]
print(h, next(a for a in d["members"].values() if a!=h))
PY
)
  log "echo hosted on $HOST; forwarded via $OTHER"
  for via in local forwarded; do
    addr=$HOST; [ $via = forwarded ] && addr=$OTHER
    for c in $CONNS; do for r in $(seq "$REPEATS"); do
      run_lg "$PIDS" "$addr" /echo "k10s-cluster-3-$via/echo" "{\"system\":\"k10s\",\"place\":\"cluster-3-$via\",\"target\":\"echo\",\"repeat\":$r}" -- -conns "$c"
    done; done
  done
  stop_cluster
fi
write_result latency
