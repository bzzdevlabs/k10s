#!/usr/bin/env bash
# Failover (cf. k8s/failover.sh and k8s/nodefail.sh). Probe = k8s/probe.py unchanged in behaviour: one fresh-connection
# GET every 20 ms, 0.5 s timeout, analysed by k8s/analyze_probe.py relative to the event time.
#  node-kill (3 nodes, default gossip: probe 200 ms, suspicion x4): SIGKILL the node hosting a 1-replica service while
#     probing it through another node's ingress -> first success, 100 % success (first success after the last failed
#     probe), failed probes; plus when the survivors suspect / declare the victim dead and re-place the service.
#  node-stop: same with SIGSTOP (hung node: no TCP reset), the k8s nodefail "pause" analogue.
#  kill-1-of-2-under-300rps: service with 2 replicas on the two other nodes, open-loop 300 req/s (16 conns) through the
#     third node's ingress; SIGKILL one replica's node after 5 s; loadgen errors (cf. k8s "3 replicas, delete 1 under
#     300 rps"; in k10s a node that hosts a replica always serves it locally, hence the non-hosting ingress).
#  instance-trap (single node; the pod-delete analogue): a request that traps (`crash?trap=1`) destroys its instance;
#     probes keep running; trap -> next successful request (a new instance is created on demand).
source "$(dirname "$0")/lib.sh"
check_disk
SCEN="${FAILOVER_SCENARIOS:-node-kill node-stop kill-1-of-2-under-300rps instance-trap}"
K8S_DIR="$BENCH_DIR/k8s"
: > "$TMP/runs.jsonl"
probe_extra() { # probe_extra <probefile> <t0_ms>: first success after the event (by send time)
  python3 - "$1" "$2" <<'PY'
import json,sys
rows=[tuple(map(int,l.split())) for l in open(sys.argv[1]) if l.strip()]; t0=int(sys.argv[2])
after=[(t,c) for t,c in rows if t>=t0]
ok=[t for t,c in after if c==200]
print(json.dumps({"first_success_after_event_ms": (ok[0]-t0) if ok else None, "probes_after_event": len(after)}))
PY
}
pick() { # pick <placement.json> <exclude-node0:1|0> -> "svc host_id host_addr [host2_id host2_addr]"
  python3 - "$1" <<'PY'
import json,sys
d=json.load(open(sys.argv[1])); me=d["id"]
for s,ids in sorted(d["placement"].items()):
    if me not in ids:
        print(s, " ".join(f"{i} {d['members'][i]}" for i in ids)); break
PY
}
pid_of_addr() { local i; for i in "${!CL_HTTP[@]}"; do if [ "${CL_HTTP[$i]}" = "$1" ]; then echo "${CL_PIDS[$i]}"; fi; done; }
for sc in $SCEN; do
  for r in $(seq "$REPEATS"); do
    check_disk; l0=$(load1)
    case $sc in
      node-kill|node-stop)
        sig=KILL; [ $sc = node-stop ] && sig=STOP
        start_cluster fo 3
        apply_text "${CL_HTTP[0]}" "$($PY manifest v 12 echo.k10 memory=64KiB)"; sleep 1
        $PY placement "${CL_HTTP[0]}" > "$TMP/pl.json"; read -r SVC HID HADDR < <(pick "$TMP/pl.json")
        VPID=$(pid_of_addr "$HADDR"); PORT0=${CL_HTTP[0]##*:}
        for _ in $(seq 50); do curl -s -o /dev/null "http://${CL_HTTP[0]}/$SVC"; done
        python3 "$K8S_DIR/probe.py" "$PORT0" "$TMP/probe.txt" 14 20 "/$SVC" & pp=$!
        sleep 3
        t0=$(now_ms); kill -"$sig" "$VPID"
        $PY watch-failover "${CL_HTTP[0]}" "$HID" "$SVC" "$t0" 20 > "$TMP/watch.json" &
        wp=$!
        wait $pp; wait $wp || true
        [ $sig = STOP ] && kill -KILL "$VPID" 2>/dev/null
        python3 "$K8S_DIR/analyze_probe.py" "$TMP/probe.txt" "$t0" > "$TMP/a.json"
        probe_extra "$TMP/probe.txt" "$t0" > "$TMP/e.json"
        python3 - "$TMP/a.json" "$TMP/e.json" "$TMP/watch.json" > "$TMP/r.json" <<'PY'
import json,sys
o={}
for f in sys.argv[1:]: o.update(json.load(open(f)))
print(json.dumps(o))
PY
        stop_cluster ;;
      kill-1-of-2-under-300rps)
        start_cluster fo 3
        apply_text "${CL_HTTP[0]}" "$($PY manifest s 12 echo.k10 memory=64KiB replicas=2)"; sleep 1
        $PY placement "${CL_HTTP[0]}" > "$TMP/pl.json"; read -r SVC HID HADDR H2ID H2ADDR < <(pick "$TMP/pl.json")
        VPID=$(pid_of_addr "$HADDR")
        for _ in $(seq 50); do curl -s -o /dev/null "http://${CL_HTTP[0]}/$SVC"; done
        ( sleep 5; kill -KILL "$VPID" ) & kp=$!
        loadgen -addr "${CL_HTTP[0]}" -path "/$SVC" -conns 16 -rate 300 -duration 12s -warmup 0s -label failover-2rep > "$TMP/o.json" || true
        wait $kp
        python3 -c "
import json; o=json.load(open('$TMP/o.json')); print(json.dumps({'requests':o['requests'],'errors':o['errors'],'errors_by_kind':o['errors_by_kind'],'latency_us':o['latency_us']}))" > "$TMP/r.json"
        stop_cluster ;;
      instance-trap)
        start_node fo-trap "$BASE_PORT" --shards "$SHARDS"; P=$LAST_PID; A="127.0.0.1:$BASE_PORT"
        apply_text "$A" 'service crash { code = "crash.k10" memory = 64KiB }'
        for _ in $(seq 20); do curl -s -o /dev/null "http://$A/crash"; done
        python3 "$K8S_DIR/probe.py" "$BASE_PORT" "$TMP/probe.txt" 8 20 /crash & pp=$!
        sleep 3
        KD="$KD" python3 - "$A" > "$TMP/t.json" <<'PY'
import json,os,sys,time
sys.path.insert(0, os.environ["KD"]); import k10s
a=sys.argv[1]
t0=time.time()*1000; p0=time.perf_counter()
st,_=k10s.http_get(a,"/crash?trap=1"); p1=time.perf_counter()
st2,_=k10s.http_get(a,"/crash"); p2=time.perf_counter()
print(json.dumps({"t0_ms":t0,"trap_status":st,"trap_request_ms":(p1-p0)*1000,"next_status":st2,"trap_response_to_next_ok_ms":(p2-p1)*1000,"event_to_next_ok_ms":(p2-p0)*1000}))
PY
        t0=$(python3 -c 'import json,sys; print(int(json.load(open(sys.argv[1]))["t0_ms"]))' "$TMP/t.json")
        wait $pp
        python3 "$K8S_DIR/analyze_probe.py" "$TMP/probe.txt" "$t0" > "$TMP/a.json"
        $PY status "$A" > "$TMP/st.json"
        python3 - "$TMP/a.json" "$TMP/t.json" "$TMP/st.json" > "$TMP/r.json" <<'PY'
import json,sys
o=json.load(open(sys.argv[1])); o.update(json.load(open(sys.argv[2])))
s=[x for x in json.load(open(sys.argv[3]))["services"] if x["name"]=="crash"][0]
o.update({"svc_traps":s["traps"],"svc_destroyed":s["destroyed"],"svc_created":s["created"]}); print(json.dumps(o))
PY
        stop_node "$P" ;;
    esac
    add_fields "$TMP/r.json" scenario="\"$sc\"" repeat="$r" loadavg_start="$l0" loadavg_end="$(load1)" noisy="$(noisy_flag "$l0" "$(load1)")" | tee -a "$TMP/runs.jsonl" >&2
  done
done
write_result failover
