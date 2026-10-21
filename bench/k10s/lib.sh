#!/usr/bin/env bash
# Shared config/helpers for the k10s benchmarks. Source me.
# Every `k10s node` runs in its own transient cgroup (`systemd-run --user --scope`, unit k10sbench-*) so that memory is
# measured exactly like the kind nodes are (cgroup memory.current - inactive_file, cpu.stat); RSS/PSS come from
# /proc/<pid>/smaps_rollup. Without systemd user scopes (NO_SCOPE=1 or unavailable) only RSS/PSS/ticks are recorded.
set -euo pipefail
KD="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BENCH_DIR="$(cd "$KD/.." && pwd)"
REPO="$(cd "$BENCH_DIR/.." && pwd)"
export PATH="$BENCH_DIR/bin:$PATH"
RESULTS="${RESULTS:-$BENCH_DIR/results/k10s}"
REPEATS="${REPEATS:-3}"
CACHE="$BENCH_DIR/.cache/k10s"                      # data dirs, compiled modules, node logs (gitignored)
K10S_BUILT="${K10S_BUILT:-$REPO/target/node/release/k10s}"
K10S="$CACHE/bin/k10s"                               # private copy: a concurrent rebuild cannot change it mid-run
PY="python3 $KD/k10s.py"
BASE_PORT="${K10S_PORT_BASE:-28080}"                 # single node: BASE; cluster node i: http BASE+101+i, gossip +201+i, rpc +301+i
SHARDS="${SHARDS:-8}"                                # single-node default (= available parallelism here)
CLUSTER_SHARDS="${CLUSTER_SHARDS:-2}"                # 3 nodes on one 8-thread machine
NOISY_LOAD="${NOISY_LOAD:-1.0}"                      # a run is flagged noisy when the 1-min load average exceeds this
mkdir -p "$RESULTS" "$CACHE/bin" "$CACHE/mod" "$CACHE/data" "$CACHE/logs"
ulimit -n 65536 2>/dev/null || true

now_ms() { echo $(( $(date +%s%N) / 1000000 )); }
log() { echo "[$(date +%T)] $*" >&2; }
free_gb() { df -BG --output=avail /home | tail -1 | tr -dc 0-9; }
check_disk() { local f; f=$(free_gb); if [ "$f" -lt 3 ]; then echo "ABORT: only ${f}G free on /home" >&2; exit 3; fi; }
load1() { cut -d' ' -f1 /proc/loadavg; }

TMP="$(mktemp -d "${TMPDIR:-/tmp}/k10sbench-k10s.XXXXXX")"
NODE_PIDS=()
cleanup_nodes() {
  local p
  for p in "${NODE_PIDS[@]:-}"; do if [ -n "$p" ]; then kill -CONT "$p" 2>/dev/null || true; kill -KILL "$p" 2>/dev/null || true; fi; done
  for p in "${NODE_PIDS[@]:-}"; do if [ -n "$p" ]; then wait "$p" 2>/dev/null || true; fi; done
  NODE_PIDS=()
}
trap 'cleanup_nodes; rm -rf "$TMP"' EXIT

USE_SCOPE=0
if [ "${NO_SCOPE:-0}" != 1 ] && systemd-run --user --scope --quiet -p MemoryAccounting=yes true >/dev/null 2>&1; then USE_SCOPE=1; fi

# Copy the k10s binary (built by run-all.sh / build_k10s) and compile the modules once per run.
prepare_k10s() {
  [ -x "$K10S_BUILT" ] || { echo "ABORT: $K10S_BUILT missing (CARGO_TARGET_DIR=target/node cargo build --release -p k10s)" >&2; exit 2; }
  cp -f "$K10S_BUILT" "$K10S.tmp" && mv -f "$K10S.tmp" "$K10S"
  # provenance of the copied binary (the working tree may change while the suite runs)
  printf '{"source":"%s","source_mtime":"%s","copied_at":"%s","git_rev":"%s","crates_dirty_at_copy":%s,"sha256":"%s"}\n' \
    "$K10S_BUILT" "$(date -r "$K10S_BUILT" +%Y-%m-%dT%H:%M:%S%z)" "$(date +%Y-%m-%dT%H:%M:%S%z)" "$(git -C "$REPO" rev-parse --short HEAD)" \
    "$( [ -n "$(git -C "$REPO" status --porcelain -- crates)" ] && echo true || echo false)" "$(sha256sum "$K10S" | cut -c1-16)" > "$K10S.info"
  # sources edited after the build (the binary may not match the working tree)
  python3 - "$K10S.info" "$(find "$REPO/crates" -name '*.rs' -newer "$K10S_BUILT" | wc -l)" <<'PY'
import json,sys; i=json.load(open(sys.argv[1])); i["crates_rs_files_newer_than_binary"]=int(sys.argv[2]); json.dump(i,open(sys.argv[1],"w"))
PY
  local w
  for w in echo chain compute counter hello; do "$K10S" build "$REPO/examples/$w.ten" -o "$CACHE/mod/$w.k10" 2>/dev/null; done
  for w in hold crash version; do "$K10S" build "$KD/ten/$w.ten" -o "$CACHE/mod/$w.k10" 2>/dev/null; done
  # examples/chain.ten leaked one heap allocation per request (see ten/chain.ten): use the bench copy until the example
  # itself calls heap_reset()
  if ! grep -q 'heap_reset()' "$REPO/examples/chain.ten"; then
    mv "$CACHE/mod/chain.k10" "$CACHE/mod/chain-example.k10"
    "$K10S" build "$KD/ten/chain.ten" -o "$CACHE/mod/chain.k10" 2>/dev/null
    echo bench > "$CACHE/mod/chain.source"
  else
    echo examples > "$CACHE/mod/chain.source"
  fi
}
[ -x "$K10S" ] && [ -f "$CACHE/mod/hold.k10" ] || prepare_k10s
CLUSTER_KEY="$CACHE/cluster.key"                     # cluster key shared by every benchmark node
[ -f "$CLUSTER_KEY" ] || "$K10S" keygen -o "$CLUSTER_KEY" 2>/dev/null
export K10S_ADMIN_TOKEN_FILE="$CACHE/admin.key"      # admin key of every benchmark node (k10s apply, k10s.py sign with it)
[ -f "$K10S_ADMIN_TOKEN_FILE" ] || "$K10S" keygen -o "$K10S_ADMIN_TOKEN_FILE" 2>/dev/null

# start_node <tag> <http_port> [k10s node flags...] -> sets LAST_PID (not via $(...): the node must be a child of this
# shell). Fresh, empty data dir per start unless KEEP_DATA=1.
start_node() {
  local tag=$1 port=$2; shift 2
  local data="$CACHE/data/$tag"
  [ "${KEEP_DATA:-0}" = 1 ] || rm -rf "$data"
  mkdir -p "$data"
  local cmd=("$K10S" node --http "127.0.0.1:$port" --data "$data" --name "$tag" --admin-token-file "$K10S_ADMIN_TOKEN_FILE" "$@")
  # the binary's page cache is charged to the cgroup that faults it in first: evict it so a node's working set includes
  # its own text pages, like a kind node's does (pages still mapped by a running node stay shared, as between kind nodes)
  $PY fadvise-drop "$K10S" 2>/dev/null || true
  if [ "$USE_SCOPE" = 1 ]; then
    systemd-run --user --scope --quiet --unit="k10sbench-$tag-$$-$RANDOM" -p MemoryAccounting=yes -- "${cmd[@]}" \
      >"$CACHE/logs/$tag.log" 2>&1 &
  else
    "${cmd[@]}" >"$CACHE/logs/$tag.log" 2>&1 &
  fi
  local pid=$!
  NODE_PIDS+=("$pid")
  local t0; t0=$(now_ms)
  until curl -s -o /dev/null --max-time 1 "http://127.0.0.1:$port/_k10s/health"; do
    kill -0 "$pid" 2>/dev/null || { cat "$CACHE/logs/$tag.log" >&2; echo "ABORT: node $tag died" >&2; exit 1; }
    [ $(( $(now_ms) - t0 )) -gt 20000 ] && { echo "ABORT: node $tag did not come up" >&2; exit 1; }
    sleep 0.02
  done
  LAST_PID=$pid
}
stop_node() { # stop_node <pid> [signal]: SIGTERM (graceful) by default; waits (SIGKILL after 15 s)
  local pid=$1 sig=${2:-TERM}
  kill -CONT "$pid" 2>/dev/null || true
  kill -"$sig" "$pid" 2>/dev/null || true
  ( sleep 15; kill -KILL "$pid" 2>/dev/null ) >/dev/null 2>&1 &
  local guard=$!
  wait "$pid" 2>/dev/null || true      # also reaps a node killed earlier (a zombie still passes `kill -0`)
  kill "$guard" 2>/dev/null || true; wait "$guard" 2>/dev/null || true
  local a=() p; for p in "${NODE_PIDS[@]}"; do if [ -n "$p" ] && [ "$p" != "$pid" ]; then a+=("$p"); fi; done
  NODE_PIDS=(${a[@]+"${a[@]}"})
}

# Cluster of N nodes (default 3) on localhost: CL_PIDS[i], CL_HTTP[i] (host:port), CL_GOSSIP[i].
CL_PIDS=(); CL_HTTP=(); CL_GOSSIP=()
cluster_port() { echo $(( BASE_PORT + $1 + $2 )); }  # cluster_port <offset 101|201|301> <i>
start_cluster() { # start_cluster <tag> [n] [extra flags...]
  local tag=$1 n=${2:-3}; shift; [ $# -gt 0 ] && shift
  CL_PIDS=(); CL_HTTP=(); CL_GOSSIP=()
  local i join=""
  for i in $(seq 0 $(( n - 1 ))); do
    local hp gp rp; hp=$(cluster_port 101 "$i"); gp=$(cluster_port 201 "$i"); rp=$(cluster_port 301 "$i")
    local args=(--shards "$CLUSTER_SHARDS" --gossip "127.0.0.1:$gp" --rpc "127.0.0.1:$rp" --cluster-key-file "$CLUSTER_KEY" "$@")
    [ -n "$join" ] && args+=(--join "$join")
    start_node "$tag-$i" "$hp" "${args[@]}"; CL_PIDS+=("$LAST_PID")
    CL_HTTP+=("127.0.0.1:$hp"); CL_GOSSIP+=("127.0.0.1:$gp")
    join="127.0.0.1:$(cluster_port 201 0)"
  done
  wait_members "$n"
}
wait_members() { # every node sees n alive members
  local n=$1 t0 a; t0=$(now_ms)
  for a in "${CL_HTTP[@]}"; do
    until [ "$($PY status "$a" | python3 -c 'import json,sys; d=json.load(sys.stdin); print(sum(1 for m in d.get("cluster",{}).get("members",[]) if m.get("state")=="alive"))' 2>/dev/null)" = "$n" ]; do
      [ $(( $(now_ms) - t0 )) -gt 30000 ] && { echo "ABORT: cluster did not converge" >&2; exit 1; }
      sleep 0.05
    done
  done
}
stop_cluster() { local p; for p in "${CL_PIDS[@]}"; do stop_node "$p" KILL; done; CL_PIDS=(); CL_HTTP=(); CL_GOSSIP=(); }
pids_csv() { local IFS=,; echo "$*"; }

# apply_text <addr> <manifest-text>: writes the manifest next to the compiled modules and applies it
apply_text() {
  local addr=$1 f="$CACHE/mod/m-$$-$RANDOM.k10s"
  printf '%s' "$2" > "$f"
  "$K10S" apply "$f" --node "$addr" >/dev/null
  rm -f "$f"
}
REF_MANIFEST='service echo    { code = "echo.k10" memory = 64KiB }
service chain   { code = "chain.k10" memory = 64KiB config = "echo" calls = ["echo"] }
service compute { code = "compute.k10" memory = 64KiB cpu_ms = 60000 }
service counter { code = "counter.k10" memory = 64KiB }
'
# one request per reference service through every given address (one warm instance each)
warm_refs() { local a; for a in "$@"; do for s in echo chain "compute?n=1" counter; do curl -s -o /dev/null --max-time 5 "http://$a/$s"; done; done; }

# write_result <name>: wraps $TMP/runs.jsonl with the environment into $RESULTS/<name>.json
write_result() {
  $PY env "$K10S" > "$TMP/env.json"
  python3 - "$TMP/env.json" <<PY
import json,os,sys; e=json.load(open(sys.argv[1])); e.update({"shards_single": $SHARDS, "shards_cluster_node": $CLUSTER_SHARDS, "cgroup_scopes": bool($USE_SCOPE)})
src="$CACHE/mod/chain.source"; e["chain_module"]=(open(src).read().strip() if os.path.exists(src) else "bench") + " chain.ten"
json.dump(e,open(sys.argv[1],"w"))
PY
  $PY wrap "$1" "$TMP/env.json" "$TMP/runs.jsonl" > "$RESULTS/$1.json"
  log "wrote $RESULTS/$1.json"
}
# add_fields <json-file> key=value...: merges scalar fields (numbers or strings) into a JSON object file, prints it
add_fields() {
  python3 - "$@" <<'PY'
import json,sys
o=json.load(open(sys.argv[1]))
for kv in sys.argv[2:]:
    k,v=kv.split("=",1)
    try: v=json.loads(v)
    except Exception: pass
    o[k]=v
print(json.dumps(o))
PY
}
noisy_flag() { python3 -c "import sys; a,b=float(sys.argv[1]),float(sys.argv[2]); print('true' if max(a,b) > $NOISY_LOAD else 'false')" "$1" "$2"; }
