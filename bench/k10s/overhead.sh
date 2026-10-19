#!/usr/bin/env bash
# Per-workload overhead (cf. k8s/pods.sh: echo scaled 0 -> N, node-level memory delta / N measured 20 s after Ready).
# A k10s instance is an actor created on demand on the node that serves the request, so "N replicas" has two analogues:
#  instances: ONE service, N live instances. echo never suspends (a shard reuses its warm instance), so the service is
#             `hold` (= echo that spins HOLD_MS ms and is therefore preempted): N concurrent requests force N instances.
#             Baseline = service applied, 0 instances; then N concurrent requests; measured POD_SETTLE s later
#             (node runs with --idle-timeout 3600 so idle instances are not reclaimed; the default is 30 s).
#  services:  N distinct echo services (same module), one request each = one instance each. Baseline = empty node;
#             reported as total/N, split into per-service state (after apply) and per-instance (after the requests).
# Density: the services variant continues to 1k / 10k / 100k (DENSITY_REPEATS, default 1, for N >= 10k).
source "$(dirname "$0")/lib.sh"
check_disk
NS_INST="${NS_INST:-1 10 50 100 1000}"; NS_SVC="${NS_SVC:-1 10 50 100 1000 10000 100000}"
HOLD_MS="${HOLD_MS:-50}"; POD_SETTLE="${POD_SETTLE:-20}"; DENSITY_REPEATS="${DENSITY_REPEATS:-1}"
A="127.0.0.1:$BASE_PORT"
: > "$TMP/runs.jsonl"
emit() { # emit <report.json> key=value... (+ loadavg noise flag)
  local f=$1; shift
  l=$(python3 -c 'import json,sys; o=json.load(open(sys.argv[1])); print(o["loadavg_start"][0], o["loadavg_end"][0])' "$f")
  add_fields "$f" "$@" noisy="$(noisy_flag $l)" >> "$TMP/runs.jsonl"
}
for n in $NS_INST; do
  for r in $(seq "$REPEATS"); do
    check_disk
    start_node ovh "$BASE_PORT" --shards "$SHARDS" --idle-timeout 3600; P=$LAST_PID
    apply_text "$A" "service hold { code = \"hold.k10\" memory = 64KiB max_instances = $(( n > 64 ? n : 64 )) config = \"$HOLD_MS\" }"
    sleep 2
    $PY snap "$P" "$A" > "$TMP/a.json"
    $PY burst "$A" /hold "$n" > "$TMP/burst.json"
    sleep "$POD_SETTLE"
    $PY snap "$P" "$A" > "$TMP/b.json"
    live=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["status"][0]["instances_live"])' "$TMP/b.json")
    $PY delta "$TMP/a.json" "$TMP/b.json" "$live" > "$TMP/d.json"
    emit "$TMP/d.json" variant='"instances"' n_target="$n" repeat="$r" hold_ms="$HOLD_MS" burst="$(cat "$TMP/burst.json")"
    tail -1 "$TMP/runs.jsonl" | python3 -c 'import json,sys; o=json.loads(sys.stdin.read()); print("instances N=%d live=%d rep=%d ws/inst=%.1f KiB rss/inst=%.1f KiB pss/inst=%.1f KiB"%(o["n_target"],o["n"],o["repeat"],o.get("working_set_kib_per_unit",-1),o["rss_kib_per_unit"],o["pss_kib_per_unit"]))' >&2
    stop_node "$P"
  done
done
for n in $NS_SVC; do
  reps=$REPEATS; [ "$n" -ge 10000 ] && reps=$DENSITY_REPEATS
  $PY manifest e "$n" echo.k10 memory=64KiB > "$CACHE/mod/ovh-$n.k10s"
  for r in $(seq "$reps"); do
    check_disk
    start_node ovh "$BASE_PORT" --shards "$SHARDS" --idle-timeout 3600; P=$LAST_PID
    sleep 2
    $PY snap "$P" "$A" > "$TMP/a.json"
    t0=$(now_ms); "$K10S" apply "$CACHE/mod/ovh-$n.k10s" --node "$A" > /dev/null; apply_ms=$(( $(now_ms) - t0 ))
    sleep 2
    $PY snap "$P" "$A" > "$TMP/b.json"
    $PY get-many "$A" "@e:$n" 8 64 > "$TMP/touch.json"
    sleep "$POD_SETTLE"
    $PY snap "$P" "$A" > "$TMP/c.json"
    $PY delta "$TMP/a.json" "$TMP/c.json" "$n" > "$TMP/total.json"
    $PY delta "$TMP/a.json" "$TMP/b.json" "$n" > "$TMP/svc.json"
    $PY delta "$TMP/b.json" "$TMP/c.json" "$n" > "$TMP/inst.json"
    python3 - "$TMP" > "$TMP/d.json" <<'PY'
import json,sys
d=sys.argv[1]; t=json.load(open(d+"/total.json")); s=json.load(open(d+"/svc.json")); i=json.load(open(d+"/inst.json"))
keep=("rss_kib_per_unit","pss_kib_per_unit","working_set_kib_per_unit","status_rss_kib_per_unit","pagetables_kib_per_unit","kernel_kib_per_unit")
t["per_service_state"]={k:s.get(k) for k in keep}; t["per_instance"]={k:i.get(k) for k in keep}
print(json.dumps(t))
PY
    # CLI-side time (parse + load the code files) measured against an unreachable node: `k10s apply` time minus this
    # is the node side
    t0=$(now_ms); "$K10S" apply "$CACHE/mod/ovh-$n.k10s" --node 127.0.0.1:1 >/dev/null 2>&1 || true; cli_ms=$(( $(now_ms) - t0 ))
    emit "$TMP/d.json" variant='"services"' n_target="$n" repeat="$r" apply_cmd_ms="$apply_ms" apply_cli_only_ms="$cli_ms" touch="$(cat "$TMP/touch.json")"
    tail -1 "$TMP/runs.jsonl" | python3 -c 'import json,sys; o=json.loads(sys.stdin.read()); print("services N=%d rep=%d ws/unit=%.1f KiB (state %.1f + instance %.1f) rss/unit=%.1f pss/unit=%.1f apply=%d ms (cli %d ms) live=%s"%(o["n"],o["repeat"],o.get("working_set_kib_per_unit",-1),o["per_service_state"].get("working_set_kib_per_unit") or -1,o["per_instance"].get("working_set_kib_per_unit") or -1,o["rss_kib_per_unit"],o["pss_kib_per_unit"],o["apply_cmd_ms"],o["apply_cli_only_ms"],o.get("instances_live_after")))' >&2
    stop_node "$P"
  done
  rm -f "$CACHE/mod/ovh-$n.k10s"
done
write_result overhead
