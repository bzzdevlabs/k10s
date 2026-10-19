#!/usr/bin/env bash
# Request latency/throughput through the NodePort: echo (direct) and chain (extra ClusterIP/kube-proxy hop).
# LG_PLACE=host  : loadgen on the host -> 127.0.0.1:3008x (kind extraPortMappings; on Docker Desktop this crosses the VM port forwarder)
# LG_PLACE=node  : loadgen copied into the kind node, -> <node IP>:3008x (isolates kube-proxy/NodePort cost from the Docker port-forward)
# LAT_TARGETS (default "echo chain"; add "compute" for /?n=1000 on NodePort 30082). Every run also records the node
# cgroup CPU over the measured window (sampled after the warm-up and after loadgen exits; for place=node the loadgen's
# own CPU, which runs inside the node, is subtracted) -> sut_cpu_us_per_req, the 1-min load average, and the host CPU used
# by everything except the Docker Desktop VM and the host loadgen (other_cpu_cores; noisy if > NOISY_CPU, 0.5 cores).
source "$(dirname "$0")/lib.sh"
check_disk
DUR="${LAT_DURATION:-20}"; WARM="${LAT_WARMUP:-3}"; CONNS="${LAT_CONNS:-1 16 64}"; PLACES="${LAT_PLACES:-host node}"
TARGETS="${LAT_TARGETS:-echo chain}"
# LAT_MODE=open (see latency-open.sh): open loop at OPEN_RATES_<target> req/s with OPEN_CONNS (64) connections instead
# of the closed-loop LAT_CONNS sweep; results go to latency-open.json
MODE="${LAT_MODE:-closed}"; OPEN_CONNS="${OPEN_CONNS:-64}"
OPEN_RATES_echo="${OPEN_RATES_echo:-5000 20000}"; OPEN_RATES_chain="${OPEN_RATES_chain:-5000 20000}"; OPEN_RATES_compute="${OPEN_RATES_compute:-500 2000}"
REPLICAS=1 apply_manifest echo.yaml; $K apply -f "$K8S_DIR/manifests/chain.yaml" >/dev/null
wait_deploy_ready echo 1 >/dev/null; wait_deploy_ready chain 1 >/dev/null
wait_http $NODEPORT_ECHO; wait_http $NODEPORT_CHAIN
if [[ " $TARGETS " == *" compute "* ]]; then
  $K apply -f "$K8S_DIR/manifests/compute.yaml" >/dev/null; wait_deploy_ready compute 1 >/dev/null; wait_http $NODEPORT_COMPUTE
fi
CP=$(CP_NODE); NODEIP=$($K get node "$CP" -o jsonpath='{.status.addresses[?(@.type=="InternalIP")].address}')
docker cp "$BENCH_DIR/bin/loadgen" "$CP:/usr/local/bin/loadgen" >/dev/null
# "busy_jiffies vm_jiffies": host busy CPU (/proc/stat) and the Docker Desktop VM process (qemu; -1 if absent)
QEMU_PID=$(pgrep -x qemu-system-x86 | head -1 || true)
hostcpu() {
  local b q=-1
  b=$(awk 'NR==1{print $2+$3+$4+$7+$8+$9}' /proc/stat)
  [ -n "$QEMU_PID" ] && [ -r "/proc/$QEMU_PID/stat" ] && q=$(sed 's/.*) //' "/proc/$QEMU_PID/stat" | awk '{print $12+$13}')
  echo "$b $q"
}
: > "$TMP/runs.jsonl"
for place in $PLACES; do
 for target in $TARGETS; do
  port=$NODEPORT_ECHO; path=/; [ $target = chain ] && port=$NODEPORT_CHAIN
  if [ $target = compute ]; then port=$NODEPORT_COMPUTE; path='/?n=1000'; fi
  plan=$CONNS; [ "$MODE" = open ] && { rv="OPEN_RATES_$target"; plan=${!rv}; }
  for x in $plan; do
   c=$x; rate_args=(); [ "$MODE" = open ] && { c=$OPEN_CONNS; rate_args=(-rate "$x"); }
   for r in $(seq "$REPEATS"); do
    l0=$(cut -d' ' -f1 /proc/loadavg)
    cpu0=$(nstat cpu "$CP")
    if [ $place = host ]; then
      loadgen -addr "$HOSTADDR:$port" -path "$path" -conns "$c" ${rate_args[@]+"${rate_args[@]}"} -duration "${DUR}s" -warmup "${WARM}s" -label "$place/$target" > "$TMP/o.json" &
    else
      docker exec "$CP" loadgen -addr "$NODEIP:$port" -path "$path" -conns "$c" ${rate_args[@]+"${rate_args[@]}"} -duration "${DUR}s" -warmup "${WARM}s" -label "$place/$target" > "$TMP/o.json" &
    fi
    lgp=$!; sleep "$WARM"; cpuw0=$(nstat cpu "$CP"); hb0=$(hostcpu); tw0=$(date +%s.%N)
    wait $lgp || true
    cpu1=$(nstat cpu "$CP"); hb1=$(hostcpu); tw1=$(date +%s.%N)
    python3 - "$TMP/o.json" "$place" "$target" "$c" "$r" "$cpu0" "$cpu1" "$(( DUR + WARM ))" "$cpuw0" "$tw0" "$tw1" "$l0" "$(cut -d' ' -f1 /proc/loadavg)" "${NOISY_CPU:-0.5}" "$hb0" "$hb1" >> "$TMP/runs.jsonl" <<'PY'
import json,os,sys
o=json.load(open(sys.argv[1])); o.update({"place":sys.argv[2],"target":sys.argv[3],"repeat":int(sys.argv[5]),
  "node_cpu_cores_avg_incl_warmup":(int(sys.argv[7])-int(sys.argv[6]))/1e6/float(sys.argv[8])})
w=float(sys.argv[11])-float(sys.argv[10]); cores=(int(sys.argv[7])-int(sys.argv[9]))/1e6/w
lg=o["loadgen_cpu_s"]["total"]/o["duration_s"] if sys.argv[2]=="node" else 0.0   # loadgen inside the node cgroup
l0,l1=float(sys.argv[12]),float(sys.argv[13])
o.update({"node_cpu_window_s":w,"node_cpu_cores_window":cores,"sut_cpu_cores":cores-lg,
  "sut_cpu_us_per_req":((cores-lg)*o["duration_s"]*1e6/o["requests"]) if o["requests"] else None,
  "loadavg_start":l0,"loadavg_end":l1})
# host-side noise: host busy CPU - Docker Desktop VM (qemu, i.e. everything in the VM) - host loadgen
(b0,q0),(b1,q1)=[tuple(map(int,x.split())) for x in sys.argv[15:17]]
hz=os.sysconf("SC_CLK_TCK"); busy=(b1-b0)/hz/w
o["host_busy_cores"]=busy
if q0>=0 and q1>=0:
    o["vm_cpu_cores"]=(q1-q0)/hz/w
    o["other_cpu_cores"]=busy-o["vm_cpu_cores"]-(o["loadgen_cpu_s"]["total"]/o["duration_s"] if sys.argv[2]=="host" else 0.0)
    o["noisy"]=o["other_cpu_cores"]>float(sys.argv[14])
else:
    o["noisy"]=max(l0,l1)>1.0
print(json.dumps(o))
PY
    tail -1 "$TMP/runs.jsonl" | python3 -c 'import json,sys; o=json.loads(sys.stdin.read()); print("%s c=%s rate=%s rep=%s rps=%.0f p50=%s p99=%s err=%s sut_cpu=%.1f us/req"%(o["label"],o["conns"],o["target_rps"],o["repeat"],o["rps"],o["latency_us"]["p50"],o["latency_us"]["p99"],o["errors"],o["sut_cpu_us_per_req"] or -1))' >&2
   done
  done
 done
done
if [ "$MODE" = open ]; then write_result latency-open; else write_result latency; fi
