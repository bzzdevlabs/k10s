#!/usr/bin/env bash
# Artifact sizes (cf. k8s/sizes.sh): .ten sources and compiled .k10 (TIR) modules vs container images; JIT machine
# code per module (from /_k10s/status after deploying the module alone); the k10s binary (as built, and stripped) vs
# kubelet/containerd/...; the node's data directory after deploying the reference services and serving requests.
source "$(dirname "$0")/lib.sh"
: > "$TMP/runs.jsonl"
A="127.0.0.1:$BASE_PORT"
start_node sizes "$BASE_PORT" --shards 1; P=$LAST_PID
echo '{}' > "$TMP/jit.json"
for w in hello echo chain compute counter; do
  apply_text "$A" "service $w { code = \"$w.k10\" memory = 64KiB }"
  $PY status "$A" | python3 -c "
import json,sys; d=json.load(sys.stdin); j=json.load(open('$TMP/jit.json')); j['$w']=d['code']; json.dump(j,open('$TMP/jit.json','w'))"
done
apply_text "$A" "$REF_MANIFEST"
for _ in $(seq 100); do warm_refs "$A"; done
sleep 3   # KV group commit
stop_node "$P"
strip -o "$TMP/k10s.stripped" "$K10S" 2>/dev/null || cp "$K10S" "$TMP/k10s.stripped"
python3 - "$REPO" "$CACHE" "$K10S" "$TMP/k10s.stripped" "$TMP/jit.json" >> "$TMP/runs.jsonl" <<'PY'
import json,os,subprocess,sys
repo,cache,k10s,stripped,jit=sys.argv[1:6]
def du(p):
    return sum(os.path.getsize(os.path.join(d,f)) for d,_,fs in os.walk(p) for f in fs)
out={"modules_k10_bytes":{w:os.path.getsize(f"{cache}/mod/{w}.k10") for w in ("hello","echo","chain","compute","counter")},
     "sources_ten_bytes":{w:os.path.getsize(f"{repo}/examples/{w}.ten") for w in ("hello","echo","chain","compute","counter")},
     "jit_code":json.load(open(jit)),
     "k10s_binary_bytes":os.path.getsize(k10s),"k10s_binary_stripped_bytes":os.path.getsize(stripped),
     "k10s_binary_dynamic_deps":subprocess.run(["ldd",k10s],capture_output=True,text=True).stdout.split("\n"),
     "data_dir_after_refs_bytes":du(f"{cache}/data/sizes"),
     "data_dir_files":{os.path.relpath(os.path.join(d,f),f"{cache}/data/sizes"):os.path.getsize(os.path.join(d,f)) for d,_,fs in os.walk(f"{cache}/data/sizes") for f in fs}}
print(json.dumps(out))
PY
write_result sizes
