#!/usr/bin/env bash
# Idle footprint of the whole cluster: REPEATS windows of 60 s (IDLE_WINDOW) after a settle period.
source "$(dirname "$0")/lib.sh"
check_disk
SETTLE="${SETTLE:-45}"; WIN="${IDLE_WINDOW:-60}"
log "settling ${SETTLE}s"; sleep "$SETTLE"
: > "$TMP/runs.jsonl"
for r in $(seq "$REPEATS"); do
  nstat snap $(nodes) > "$TMP/a.json"; sleep "$WIN"; nstat snap $(nodes) > "$TMP/b.json"
  docker stats --no-stream --format '{"name":"{{.Name}}","cpu":"{{.CPUPerc}}","mem":"{{.MemUsage}}"}' $(nodes) > "$TMP/stats.txt"
  nstat idle "$TMP/a.json" "$TMP/b.json" > "$TMP/r.json"
  python3 - "$TMP" >> "$TMP/runs.jsonl" <<'PY'
import json,sys
d=sys.argv[1]; r=json.load(open(d+"/r.json"))
r["docker_stats"]=[json.loads(l) for l in open(d+"/stats.txt") if l.strip()]
print(json.dumps(r))
PY
  log "idle window $r done"
done
write_result idle
