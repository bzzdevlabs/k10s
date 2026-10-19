#!/usr/bin/env bash
# Request latency/throughput through the NodePort: echo (direct) and chain (extra ClusterIP/kube-proxy hop).
# LG_PLACE=host  : loadgen on the host -> 127.0.0.1:3008x (kind extraPortMappings; on Docker Desktop this crosses the VM port forwarder)
# LG_PLACE=node  : loadgen copied into the kind node, -> <node IP>:3008x (isolates kube-proxy/NodePort cost from the Docker port-forward)
source "$(dirname "$0")/lib.sh"
check_disk
DUR="${LAT_DURATION:-20}"; WARM="${LAT_WARMUP:-3}"; CONNS="${LAT_CONNS:-1 16 64}"; PLACES="${LAT_PLACES:-host node}"
REPLICAS=1 apply_manifest echo.yaml; $K apply -f "$K8S_DIR/manifests/chain.yaml" >/dev/null
wait_deploy_ready echo 1 >/dev/null; wait_deploy_ready chain 1 >/dev/null
wait_http $NODEPORT_ECHO; wait_http $NODEPORT_CHAIN
CP=$(CP_NODE); NODEIP=$($K get node "$CP" -o jsonpath='{.status.addresses[?(@.type=="InternalIP")].address}')
docker cp "$BENCH_DIR/bin/loadgen" "$CP:/usr/local/bin/loadgen" >/dev/null
: > "$TMP/runs.jsonl"
for place in $PLACES; do
 for target in echo chain; do
  port=$NODEPORT_ECHO; [ $target = chain ] && port=$NODEPORT_CHAIN
  for c in $CONNS; do
   for r in $(seq "$REPEATS"); do
    cpu0=$(nstat cpu "$CP")
    if [ $place = host ]; then
      loadgen -addr "$HOSTADDR:$port" -conns "$c" -duration "${DUR}s" -warmup "${WARM}s" -label "$place/$target" > "$TMP/o.json" || true
    else
      docker exec "$CP" loadgen -addr "$NODEIP:$port" -conns "$c" -duration "${DUR}s" -warmup "${WARM}s" -label "$place/$target" > "$TMP/o.json" || true
    fi
    cpu1=$(nstat cpu "$CP")
    python3 - "$TMP/o.json" "$place" "$target" "$c" "$r" "$cpu0" "$cpu1" "$(( DUR + WARM ))" >> "$TMP/runs.jsonl" <<'PY'
import json,sys
o=json.load(open(sys.argv[1])); o.update({"place":sys.argv[2],"target":sys.argv[3],"repeat":int(sys.argv[5]),
  "node_cpu_cores_avg_incl_warmup":(int(sys.argv[7])-int(sys.argv[6]))/1e6/float(sys.argv[8])}); print(json.dumps(o))
PY
    tail -1 "$TMP/runs.jsonl" | python3 -c 'import json,sys; o=json.loads(sys.stdin.read()); print("%s c=%s rep=%s rps=%.0f p50=%s p99=%s err=%s"%(o["label"],o["conns"],o["repeat"],o["rps"],o["latency_us"]["p50"],o["latency_us"]["p99"],o["errors"]))' >&2
   done
  done
 done
done
write_result latency
