#!/usr/bin/env bash
# End-to-end test: a k10-metal guest as a member of a hosted k10s cluster.
#   2 hosted `k10s node` processes (Linux) + 1 k10-metal guest (QEMU/KVM, slirp user networking with hostfwd for
#   its gossip UDP, rpc TCP and http TCP ports; it advertises 127.0.0.1:<forwarded ports>).
#   1. membership: all three see 3 live members (guest joins through a hosted seed);
#   2. apply the test bundle through a hosted node → all three fetch/apply the same version, compute the same
#      placement, and the guest hosts some services;
#   3. every service answers through every node's ingress (guest-placed services through hosted ingress and vice
#      versa; `call` across nodes);
#   4. kill the guest (SIGKILL of QEMU) → the hosted nodes declare it dead and serve its services (probe loop through
#      a hosted ingress measures the outage);
#   5. restart the guest (same node id) with the cluster key and a next key (key rotation) → it rejoins the nodes
#      that hold only the current key, with a higher incarnation, fetches the desired state, gets its services back,
#      and serves through its ingress again;
#   6. apply a new version through the guest → the hosted nodes fetch it from the guest and apply it;
#   7. refused peers: a hosted node with another cluster key joining through the guest (its gossip is dropped, it
#      never becomes a member), raw connections to the guest's peer port that never authenticate (at most 16 are kept
#      in their handshake, oldest evicted, all closed after the 5 s handshake timeout), and a key holder that asks for
#      the manifest without ever reading the replies (tools/meshpeer: far more than the guest's RAM): the guest closes
#      that link at its cap and keeps serving.
#   Keys (`k10s keygen -o`, mode 0600): a shared cluster key (guest: fw_cfg opt/k10s/cluster_key), a next key (guest:
#   opt/k10s/cluster_next_key), a shared admin key (hosted: --admin-token-file; guest: opt/k10s/admin_key); every
#   admin request is signed for the receiving node process's audience (tools/admin_auth.py, bench/k10s/k10s.py); one
#   signed for another node, or for the guest before its restart, is refused.
#   8. KV on the guest's disk (NVMe, a GPT with K10S-DATA): 12 replicas of the kv service, some placed on the
#      guest, are written through n1; the guest is killed (a power cut) and restarted: once it hosts them again,
#      every value is there (the guest's from K10S-DATA);
#   9. power off through the admin API: the guest leaves the cluster (the hosted nodes see it leave, not die) and
#      powers off through ACPI (QEMU exits 0).
# Environment: BASE (port base, default 19400), K10S (hosted binary, default ../target/metal-host/release/k10s, built
# with `CARGO_TARGET_DIR=target/metal-host cargo build --release -p k10s`), ENGINE (jit|interp, guest), PROBE_MS
# (100), SUSPICION_MULT (2), MEM (guest RAM, 128M), SMP (guest CPUs, 1), PLATFORM and ACCEL (qemu.sh), CLUSTER_OUT
# (work directory, default out/cluster-test; the hosted nodes keep their data there).
set -uo pipefail
cd "$(dirname "$0")"
. ./qemu.sh
PLATFORM="${PLATFORM:-pc}"
ACCEL_ARGS=$(k10_accel) || exit 1
MKBUNDLE="${CARGO_TARGET_DIR:-target}/release/mkbundle"
MESHPEER="${CARGO_TARGET_DIR:-target}/release/meshpeer"
BASE="${BASE:-19400}"
PROBE_MS="${PROBE_MS:-100}"
SUSP="${SUSPICION_MULT:-2}"
MEM="${MEM:-128M}"
ENGINE="${ENGINE:-jit}"
SMP="${SMP:-1}"
OUT="${CLUSTER_OUT:-out/cluster-test}"
rm -rf "$OUT"
mkdir -p "$OUT"
if [[ -z "${K10S:-}" ]]; then
    K10S=../target/metal-host/release/k10s
    (cd .. && CARGO_TARGET_DIR=target/metal-host cargo build --release -q -p k10s) || exit 1
fi
[[ -f out/k10-metal.elf ]] || ./build.sh >/dev/null
cargo build --release -q -p meshpeer || exit 1
for k in cluster cluster-next wrong admin; do
    "$K10S" keygen -o "$OUT/$k.key" 2>/dev/null || exit 1
done
export K10S_ADMIN_TOKEN_FILE=$(realpath "$OUT")/admin.key
# sign PORT METHOD TARGET [BODY|@FILE]: Authorization header value for an admin request to the node at
# 127.0.0.1:PORT (signed for its audience, /_k10s/audience).
sign() {
    local port=$1
    shift
    tools/admin_auth.py "127.0.0.1:$port" "$@"
}

pass=0
fail=0
ok() { echo "  ok   $*"; pass=$((pass + 1)); }
ko() { echo "  FAIL $*"; fail=$((fail + 1)); }
now_ms() { date +%s%3N; }

H1=$((BASE + 1)) G1=$((BASE + 101)) R1=$((BASE + 201))
H2=$((BASE + 2)) G2=$((BASE + 102)) R2=$((BASE + 202))
H3=$((BASE + 3)) G3=$((BASE + 103)) R3=$((BASE + 203))
GUEST_ID=00000000000000a1
PIDS=()
QPID=
cleanup() {
    [[ -n "$QPID" ]] && kill "$QPID" 2>/dev/null
    for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null; done
    wait 2>/dev/null
}
trap cleanup EXIT

hosted() { # name http gossip rpc [join [cluster key file]]
    local a=(node --name "$1" --http "127.0.0.1:$2" --gossip "127.0.0.1:$3" --rpc "127.0.0.1:$4" --data "$OUT/$1"
        --mem 64MiB --shards 2 --probe-ms "$PROBE_MS" --suspicion-mult "$SUSP" --cluster-key-file "${6:-$OUT/cluster.key}"
        --admin-token-file "$OUT/admin.key")
    [[ -n "${5:-}" ]] && a+=(--join "$5")
    "$K10S" "${a[@]}" 2> "$OUT/$1.log" &
    PIDS+=($!)
}

guest() { # serial log [next]: secrets as fw_cfg files; `next`: also the next cluster key (rotation)
    GLOG=$1
    local fw=(-fw_cfg "name=opt/k10s/cluster_key,file=$OUT/cluster.key" -fw_cfg "name=opt/k10s/admin_key,file=$OUT/admin.key")
    [[ "${2:-}" == next ]] && fw+=(-fw_cfg "name=opt/k10s/cluster_next_key,file=$OUT/cluster-next.key")
    # shellcheck disable=SC2046,SC2086
    qemu-system-x86_64 $ACCEL_ARGS $(k10_machine "$PLATFORM") -m "$MEM" -smp "$SMP" -nodefaults -display none -no-reboot \
        -drive "file=$OUT/guest-disk.img,if=none,id=d0,format=raw" -device nvme,drive=d0,serial=k10s \
        -netdev "user,id=n0,hostfwd=tcp:127.0.0.1:$H3-:80,hostfwd=udp:127.0.0.1:$G3-:7946,hostfwd=tcp:127.0.0.1:$R3-:7947" \
        $(k10_nic "$PLATFORM" n0) -kernel out/k10-metal.elf -serial "file:$1" "${fw[@]}" \
        -append "k10.engine=$ENGINE k10.join=127.0.0.1:$G1,127.0.0.1:$G2 k10.gossip=127.0.0.1:$G3 k10.rpc=127.0.0.1:$R3 k10.http=127.0.0.1:$H3 k10.node_id=$GUEST_ID k10.probe_ms=$PROBE_MS k10.suspicion_mult=$SUSP k10.mem=67108864" &
    QPID=$!
}

# st PORT PYEXPR: evaluates PYEXPR on the node's /_k10s/status JSON (`d`, cluster part `c`; signed request);
# prints the result.
st() {
    python3 -c "
import json, sys
sys.path.insert(0, '../bench/k10s')
import k10s
try:
    d = json.loads(k10s.http_get('127.0.0.1:$1', '/_k10s/status', 2)[1])
except Exception:
    print(''); sys.exit()
c = d.get('cluster', {})
print($2)" 2>/dev/null
}

# wait_for TIMEOUT_S CMD...: polls until CMD succeeds; prints elapsed ms.
wait_for() {
    local t=$1
    shift
    local t0
    t0=$(now_ms)
    while (($(now_ms) - t0 < t * 1000)); do
        if "$@"; then
            echo $(($(now_ms) - t0))
            return 0
        fi
        sleep 0.02
    done
    echo -1
    return 1
}

live_is() { # PORT N
    [[ "$(st "$1" "c.get('live_members')")" == "$2" ]]
}
all_live() { live_is "$H1" "$1" && live_is "$H2" "$1" && live_is "$H3" "$1"; }
applied_v() { st "$1" "(c.get('applied') or {}).get('version')"; }
all_applied() { [[ "$(applied_v "$H1")" == "$1" && "$(applied_v "$H2")" == "$1" && "$(applied_v "$H3")" == "$1" ]]; }

# expect NAME URL STATUS BODY_REGEX
expect() {
    local body code
    body=$(curl -sS -m 5 -o - -w '\n%{http_code}' "$2" 2>&1)
    code=${body##*$'\n'}
    body=${body%$'\n'*}
    body=${body%$'\n'}
    if [[ "$code" == "$3" && "$body" =~ $4 ]]; then
        ok "$1 ($code)"
    else
        ko "$1: got $code '${body:0:100}', want $3 /$4/"
    fi
}

# Deterministic services: name, path, regex.
SERVICES=(
    "hello|/hello/|^hello from k10-metal$"
    "echo|/echo/x?y=1|^/x\\?y=1$"
    "sha|/sha/?n=100|^[0-9a-f]{64}$"
    "compute|/compute?n=1000|^[0-9]+$"
    "nbody|/nbody/?n=10|^-0\\.169"
    "config|/config|^greeting=hello from the manifest$"
    "chain|/chain|^chain -> ping from chain$"
    "time|/time|^1[0-9]{12} [0-9]+$"
    "random|/random|^[0-9]+$"
    "instcount|/instcount|^10[0-9][0-9]$"
)

serve_all() { # label
    for port in "$H1" "$H2" "$H3"; do
        for s in "${SERVICES[@]}"; do
            IFS='|' read -r name path re <<< "$s"
            expect "$1: $name via :$port" "http://127.0.0.1:$port$path" 200 "$re"
        done
    done
}

echo "== start: 2 hosted nodes + 1 k10-metal guest (engine $ENGINE, $SMP CPU(s), probe ${PROBE_MS} ms, suspicion x$SUSP)"
hosted n1 "$H1" "$G1" "$R1"
sleep 0.3
hosted n2 "$H2" "$G2" "$R2" "127.0.0.1:$G1"
t=$(wait_for 10 eval 'live_is $H1 2 && live_is $H2 2') && ok "hosted nodes converged (${t} ms)" || ko "hosted nodes did not converge"
T0=$(now_ms)
python3 -c 'import sys; sys.path.insert(0, "tools"); import storage_test; storage_test.gpt_disk(sys.argv[1], 128, 1, 64)' "$OUT/guest-disk.img"
guest "$OUT/guest.log"
t=$(wait_for 10 grep -qs "k10-metal ready" "$OUT/guest.log") && ok "guest booted: $(grep 'k10-metal ready' "$OUT/guest.log")" || ko "guest boot"
t=$(wait_for 15 all_live 3)
JOIN_MS=$(($(now_ms) - T0))
[[ $t != -1 ]] && ok "all three see 3 live members (${JOIN_MS} ms after QEMU start)" || ko "membership did not converge: $(st $H1 "c.get('live_members')") / $(st $H2 "c.get('live_members')") / $(st $H3 "c.get('live_members')")"
GID=$(st "$H3" "c.get('id')")
[[ "$GID" == "$GUEST_ID" ]] && ok "guest id $GID" || ko "guest id '$GID'"
INC1=$(st "$H1" "[m['incarnation'] for m in c['members'] if m['id']=='$GUEST_ID'][0]")
links=$(st "$H3" "sorted(l['state'] for l in c['links'])")
[[ "$links" == "['up', 'up']" ]] && ok "guest peer links: $links" || ko "guest peer links: $links"
ids=$(for p in "$H1" "$H2" "$H3"; do st "$p" "c['auth'].get('key_id')"; done | sort -u)
[[ $(echo "$ids" | wc -l) == 1 && ${#ids} == 16 ]] && ok "same cluster key id on all three ($ids)" || ko "key ids: $ids"
KEY_ID=$ids
code=$(curl -s -m 5 -o /dev/null -w '%{http_code}' "127.0.0.1:$H3/_k10s/status")$(curl -s -m 5 -o /dev/null -w '%{http_code}' "127.0.0.1:$H3/_k10s/metrics")
[[ "$code" == 401401 ]] && ok "guest admin API unsigned (status, metrics) -> 401" || ko "guest status/metrics unsigned: $code"
code=$(curl -s -m 5 -o /dev/null -w '%{http_code}' "127.0.0.1:$H1/_k10s/status")
[[ "$code" == 401 ]] && ok "hosted admin API unsigned -> 401" || ko "hosted status unsigned: $code"
a1=$(curl -s -m 5 "127.0.0.1:$H1/_k10s/audience") a3=$(curl -s -m 5 "127.0.0.1:$H3/_k10s/audience")
[[ "$a3" =~ ^[0-9a-f]{32}$ && "$a1" =~ ^[0-9a-f]{32}$ && "$a1" != "$a3" ]] && ok "audiences open and distinct (n1 $a1, guest $a3)" || ko "audiences: n1 '$a1', guest '$a3'"
r=$(curl -s -m 5 -H "Authorization: $(sign "$H1" GET /_k10s/status)" "127.0.0.1:$H3/_k10s/status")
[[ "$r" == *"another node"* ]] && ok "guest refuses a request signed for n1 (same admin key)" || ko "request signed for n1 sent to the guest: '${r:0:80}'"
r=$(curl -s -m 5 -H "Authorization: $(sign "$H3" GET /_k10s/status)" "127.0.0.1:$H1/_k10s/status")
[[ "$r" == *"another node"* ]] && ok "n1 refuses a request signed for the guest" || ko "request signed for the guest sent to n1: '${r:0:80}'"

echo "== apply through hosted node n1"
"$MKBUNDLE" -o "$OUT/bundle.k10b" --version 0 2>/dev/null
T0=$(now_ms)
r=$(curl -s -m 10 -H "Authorization: $(sign "$H1" POST /_k10s/apply "@$OUT/bundle.k10b")" --data-binary "@$OUT/bundle.k10b" "127.0.0.1:$H1/_k10s/apply")
V=$(python3 -c "import json,sys; print(json.loads(sys.argv[1])['version'])" "$r" 2>/dev/null)
[[ -n "$V" ]] && ok "apply via n1 -> version $V" || ko "apply via n1: $r"
t=$(wait_for 10 all_applied "$V")
APPLY_MS=$(($(now_ms) - T0))
[[ $t != -1 ]] && ok "all three applied v$V (${APPLY_MS} ms after the apply request)" || ko "not applied everywhere: $(applied_v $H1) $(applied_v $H2) $(applied_v $H3)"
h1=$(st "$H1" "(c.get('applied') or {}).get('hash')") h3=$(st "$H3" "(c.get('applied') or {}).get('hash')")
[[ -n "$h1" && "$h1" == "$h3" ]] && ok "same manifest hash everywhere (${h1:0:12})" || ko "hash mismatch '$h1' / '$h3'"
p1=$(st "$H1" "sorted(c.get('placement',{}).items())") p2=$(st "$H2" "sorted(c.get('placement',{}).items())") p3=$(st "$H3" "sorted(c.get('placement',{}).items())")
[[ -n "$p1" && "$p1" == "$p2" && "$p1" == "$p3" ]] && ok "same placement computed on all three" || ko "placements differ"
GSVC=$(st "$H3" "' '.join(s['name'] for s in d['services'] if s.get('placement') == 'local')")
HSVC=$(st "$H1" "' '.join(s for s, n in c.get('placement',{}).items() if '$GUEST_ID' not in n)")
[[ -n "$GSVC" ]] && ok "services placed on the guest: $GSVC" || ko "no service placed on the guest"
echo "    services placed on hosted nodes: $HSVC"

echo "== every service through every ingress"
serve_all "converged"
PROBE_HOST=$(st "$H1" "c['placement']['probe'][0]")
echo "== calls allowlist (probe may call hello only; enforced by probe's node: $PROBE_HOST, guest is $GUEST_ID)"
for port in "$H1" "$H2" "$H3"; do
    expect "probe -> hello (granted) via :$port" "http://127.0.0.1:$port/probe/hello/" 200 '^probe -> hello from k10-metal$'
    expect "probe -> echo (denied) via :$port" "http://127.0.0.1:$port/probe/echo/x" 502 '^call failed: -1$'
    expect "probe -> nosuch (unknown) via :$port" "http://127.0.0.1:$port/probe/nosuch" 502 '^call failed: -1$'
done
head -c 102400 /dev/zero | tr '\0' x > "$OUT/100k.txt"
for port in "$H1" "$H2" "$H3"; do
    n=$(curl -s -m 10 --data-binary "@$OUT/100k.txt" "127.0.0.1:$port/echo/" | wc -c)
    [[ "$n" == 102400 ]] && ok "echo of a 100 KiB body via :$port (echo on $(st "$H1" "c['placement']['echo'][0][-4:]"))" || ko "100 KiB echo via :$port: $n bytes"
done
calls=$(st "$H3" "c['stats']['served_for_peers']")
fw=$(st "$H3" "c['stats']['forwarded']")
((calls > 0 && fw > 0)) && ok "guest served $calls call(s) for peers and forwarded $fw" || ko "guest served $calls / forwarded $fw"
GTARGET=$(echo "$GSVC" | tr ' ' '\n' | grep -E '^(hello|echo|sha|compute|nbody|config|chain|time|random)$' | head -1)
[[ -n "$GTARGET" ]] || GTARGET=hello
case $GTARGET in
    echo) GPATH="/echo/x" ;;
    sha) GPATH="/sha/?n=10" ;;
    compute) GPATH="/compute?n=10" ;;
    nbody) GPATH="/nbody/?n=1" ;;
    hello) GPATH="/hello/" ;;
    *) GPATH="/$GTARGET" ;;
esac

echo "== kill the guest (SIGKILL QEMU) while probing $GPATH through n1 every 10 ms"
python3 - "$H1" "$GPATH" > "$OUT/probe.json" <<'EOF' &
import json, sys, time, urllib.request
port, path = sys.argv[1], sys.argv[2]
t_end = time.time() + 8
res = []
while time.time() < t_end:
    t = time.time()
    try:
        code = urllib.request.urlopen(f"http://127.0.0.1:{port}{path}", timeout=2).status
    except Exception as e:
        code = getattr(e, "code", 0) or 0
    res.append((t, code))
    time.sleep(0.01)
print(json.dumps(res))
EOF
PROBE=$!
EARLIER=$(sign "$H3" GET /_k10s/status)
sleep 1
KILL_T=$(python3 -c "import time; print(time.time())")
kill -9 "$QPID"
wait "$QPID" 2>/dev/null
QPID=
t=$(wait_for 15 eval "[[ \"\$(st $H1 \"[m['state'] for m in c['members'] if m['id']=='$GUEST_ID'][0]\")\" == dead ]]")
[[ $t != -1 ]] && ok "n1 declared the guest dead ${t} ms after the kill" || ko "guest not declared dead"
t=$(wait_for 10 eval "live_is $H1 2 && live_is $H2 2")
wait $PROBE
python3 - "$OUT/probe.json" "$KILL_T" <<'EOF'
import json, sys
res = json.load(open(sys.argv[1])); kill = float(sys.argv[2])
before = [c for t, c in res if t < kill]
after = [(t, c) for t, c in res if t >= kill]
fails = [(t, c) for t, c in after if c != 200]
first_ok = next((t for t, c in after if c == 200 and (not fails or t > fails[-1][0])), None)
print(f"    probes: {len(before)} before the kill (all 200: {all(c == 200 for c in before)}), {len(after)} after, "
      f"{len(fails)} failed; last failure {((fails[-1][0] - kill) * 1000 if fails else 0):.0f} ms after the kill")
EOF
sleep 0.3
p1=$(st "$H1" "sorted(c.get('placement',{}).items())")
[[ "$p1" != *"$GUEST_ID"* ]] && ok "guest services re-placed on hosted nodes" || ko "placement still references the guest"
for s in "${SERVICES[@]}"; do
    IFS='|' read -r name path re <<< "$s"
    [[ " $GSVC " == *" $name "* ]] || continue
    expect "after failover: $name via n1" "http://127.0.0.1:$H1$path" 200 "$re"
    expect "after failover: $name via n2" "http://127.0.0.1:$H2$path" 200 "$re"
done

echo "== restart the guest (same node id, current + next cluster key; the hosted nodes hold only the current one)"
T0=$(now_ms)
guest "$OUT/guest2.log" next
t=$(wait_for 15 all_live 3)
REJOIN_MS=$(($(now_ms) - T0))
[[ $t != -1 ]] && ok "guest rejoined: 3 live members everywhere (${REJOIN_MS} ms after QEMU start)" || ko "guest did not rejoin"
INC2=$(st "$H1" "[m['incarnation'] for m in c['members'] if m['id']=='$GUEST_ID'][0]")
((INC2 > INC1)) && ok "higher incarnation ($INC1 -> $INC2)" || ko "incarnation $INC1 -> $INC2"
r=$(curl -s -m 5 -H "Authorization: $EARLIER" "127.0.0.1:$H3/_k10s/status")
[[ "$r" == *"earlier start"* ]] && ok "restarted guest refuses a request signed before the restart" || ko "request signed before the restart: '${r:0:80}'"
auth=$(st "$H3" "c['auth']['key_id'] + ' ' + str(c['auth']['next_key_id'])")
[[ "${auth% *}" == "$KEY_ID" && "${auth#* }" =~ ^[0-9a-f]{16}$ && "${auth#* }" != "$KEY_ID" ]] && ok "guest key ids: current $KEY_ID, next ${auth#* }" || ko "guest key ids: '$auth'"
t=$(wait_for 10 eval "[[ \$(applied_v $H3) == $V ]]")
RESYNC_MS=$(($(now_ms) - T0))
[[ $t != -1 ]] && ok "rejoined guest fetched and applied v$V (${RESYNC_MS} ms after QEMU start)" || ko "rejoined guest did not apply v$V"
t=$(wait_for 5 eval "[[ \"\$(st $H1 \"sorted(c.get('placement',{}).items())\")\" == *$GUEST_ID* ]]")
GSVC2=$(st "$H3" "' '.join(s['name'] for s in d['services'] if s.get('placement') == 'local')")
[[ "$GSVC2" == "$GSVC" ]] && ok "guest hosts its services again: $GSVC2" || ko "guest services after rejoin: '$GSVC2' (before: '$GSVC')"
serve_all "rejoined"

echo "== apply through the guest (operator apply on the k10-metal node)"
"$MKBUNDLE" -o "$OUT/bundle2.k10b" --version 0 --services hello,echo,sha,chain,config,compute 2>/dev/null
r=$(curl -s -m 10 -H "Authorization: $(sign "$H3" POST /_k10s/apply "@$OUT/bundle2.k10b")" --data-binary "@$OUT/bundle2.k10b" "127.0.0.1:$H3/_k10s/apply")
V2=$(python3 -c "import json,sys; print(json.loads(sys.argv[1])['version'])" "$r" 2>/dev/null)
[[ "$V2" == $((V + 1)) ]] && ok "apply via the guest -> version $V2 (highest known + 1)" || ko "apply via the guest: $r"
t=$(wait_for 10 all_applied "$V2") && ok "hosted nodes fetched v$V2 from the guest and applied it (${t} ms)" || ko "v$V2 not applied everywhere"
for port in "$H1" "$H2" "$H3"; do
    expect "v$V2: sha via :$port" "http://127.0.0.1:$port/sha/?n=100" 200 '^[0-9a-f]{64}$'
    expect "v$V2: chain via :$port" "http://127.0.0.1:$port/chain" 200 '^chain -> ping from chain$'
    expect "v$V2: removed service via :$port" "http://127.0.0.1:$port/nbody/" 404 ''
done

rej=$(st "$H1" "c['stats']['gossip_rejected'] + c['stats']['link_auth_failures']")$(st "$H3" "c['stats']['gossip_rejected'] + c['stats']['link_auth_failures']")
[[ "$rej" == 00 ]] && ok "no authentication failures between members" || ko "authentication failures (n1, guest): $rej"

echo "== KV on the guest's K10S-DATA through a power cut (12 kv replicas, some on the guest)"
"$MKBUNDLE" -o "$OUT/kv.k10b" --version 0 --replicate kv:12 2>/dev/null
r=$(curl -s -m 10 -H "Authorization: $(sign "$H1" POST /_k10s/apply "@$OUT/kv.k10b")" --data-binary "@$OUT/kv.k10b" "127.0.0.1:$H1/_k10s/apply")
V3=$(python3 -c "import json,sys; print(json.loads(sys.argv[1])['version'])" "$r" 2>/dev/null)
t=$(wait_for 10 all_applied "$V3") && ok "kv replicas applied as v$V3 (${t} ms)" || ko "v$V3 not applied everywhere: $r"
KVG=$(st "$H3" "' '.join(s['name'] for s in d['services'] if s.get('placement') == 'local')")
[[ -n "$KVG" ]] && ok "kv replicas on the guest: $KVG" || ko "no kv replica placed on the guest"
n=0
for i in $(seq 0 11); do
    [[ $(curl -s -m 5 -o /dev/null -w '%{http_code}' -X PUT --data-binary "value $i" "127.0.0.1:$H1/kv-$i/x") == 200 ]] && n=$((n + 1))
done
[[ $n == 12 ]] && ok "12 values written through n1" || ko "$n of 12 values written"
# The group commit's window is 2 ms: then the power goes.
sleep 0.3
kill -9 "$QPID"
wait "$QPID" 2>/dev/null
QPID=
t=$(wait_for 15 eval "[[ \"\$(st $H1 \"[m['state'] for m in c['members'] if m['id']=='$GUEST_ID'][0]\")\" == dead ]]")
guest "$OUT/guest3.log" next
t=$(wait_for 15 all_live 3) && ok "guest back after the power cut (${t} ms)" || ko "guest did not come back"
grep -aq "storage: K10S-DATA on nvme" "$OUT/guest3.log" && ok "guest: $(grep -a "storage: K10S-DATA on" "$OUT/guest3.log" | tr -d '\r')" || ko "guest opened no store"
t=$(wait_for 15 eval "[[ \"\$(st $H3 \"' '.join(s['name'] for s in d['services'] if s.get('placement') == 'local')\")\" == \"$KVG\" ]]") \
    && ok "guest hosts its kv replicas again" || ko "guest kv replicas after the cut: '$(st "$H3" "' '.join(s['name'] for s in d['services'] if s.get('placement') == 'local')")'"
kv_all() {
    local i
    for i in $(seq 0 11); do
        [[ $(curl -s -m 5 "127.0.0.1:$H1/kv-$i/x") == "value $i" ]] || return 1
    done
}
t=$(wait_for 15 kv_all) && ok "all 12 values read back through n1 after the guest's power cut" || ko "values after the power cut: $(for i in $(seq 0 11); do curl -s -m 5 "127.0.0.1:$H1/kv-$i/x"; echo -n '|'; done)"

echo "== refused peers"
H4=$((BASE + 4)) G4=$((BASE + 104)) R4=$((BASE + 204))
hosted n4 "$H4" "$G4" "$R4" "127.0.0.1:$G3" "$OUT/wrong.key"
rejected() { st "$H3" "c['stats']['gossip_rejected']"; }
t=$(wait_for 5 eval 'r=$(rejected); ((${r:-0} > 0))') && ok "guest dropped gossip from a node with another cluster key ($(rejected) datagrams)" || ko "guest did not reject the wrong-key node's gossip"
sleep 1
n4=$(st "$H4" "c.get('live_members')") n3=$(st "$H3" "c.get('live_members')")
[[ "$n4" == 1 && "$n3" == 3 ]] && ok "wrong-key node stays alone; the guest still sees 3 live members" || ko "membership with a wrong-key node: n4 $n4, guest $n3"
kill "${PIDS[-1]}" 2>/dev/null
F0=$(st "$H3" "c['stats']['link_auth_failures']")
# One connection to the guest's peer port sends garbage, then 20 never authenticate (held 8 s).
python3 - "$R3" <<'PY' &
import socket, sys, time
port = int(sys.argv[1])
bad = socket.create_connection(("127.0.0.1", port), timeout=2)
bad.sendall(b"\xff" * 64)
time.sleep(0.2)
socks = []
for _ in range(20):
    socks.append(socket.create_connection(("127.0.0.1", port), timeout=2))
    time.sleep(0.02)
time.sleep(8)
PY
RAW=$!
handshaking() { st "$H3" "sum(1 for l in c['links'] if l['state'] == 'handshake')"; }
sleep 1.7
n=$(handshaking)
[[ "$n" == 16 ]] && ok "at most 16 unauthenticated links kept in their handshake ($n; oldest evicted)" || ko "links in their handshake: $n (want 16)"
t=$(wait_for 6 eval '[[ $(handshaking) == 0 ]]') && ok "unauthenticated links closed by the handshake timeout" || ko "links still in their handshake: $(handshaking)"
F1=$(st "$H3" "c['stats']['link_auth_failures']")
((F1 - F0 >= 21)) && ok "guest counted $((F1 - F0)) refused peer links (1 garbage, 4 evicted, 16 timed out)" || ko "refused peer links counted: $F0 -> $F1"
wait $RAW
n3=$(st "$H3" "c.get('live_members')")
[[ "$n3" == 3 ]] && ok "cluster unaffected (3 live members)" || ko "live members after refused peers: $n3"
O0=$(st "$H3" "c['stats']['links_overflowed']")
r=$(timeout 300 "$MESHPEER" "127.0.0.1:$R3" "$OUT/cluster.key" 2000000)
sent=${r#sent=} && sent=${sent%% *}
r2=$(st "$H3" "*(c['stats']['links_overflowed'], d['mem']['peak_used'] >> 20, d['mem']['total_ram'] >> 20, len(c['links']))")
read -r O1 peak ram nlinks <<< "$r2"
if ((${O1:-0} == O0 + 1 && ${sent:-2000000} < 2000000)); then
    ok "guest closed a non-reading key holder's link at its cap after $sent manifest requests ($r; peak $peak of $ram MiB)"
else
    ko "non-reading peer: '$r', links_overflowed $O0 -> $O1"
fi
[[ "$nlinks" == 2 && "$(st "$H3" "c.get('live_members')")" == 3 ]] && ok "guest keeps its 2 member links and 3 live members" || ko "after the non-reading peer: $r2"
kv_all && ok "all 12 kv values read through n1 afterwards (the guest's replicas through its links)" || ko "kv values after the non-reading peer"
echo "== power off the guest through its admin API (it leaves the cluster first)"
r=$(curl -s -m 5 -o /dev/null -w '%{http_code}' -H "Authorization: $(sign "$H3" POST /_k10s/power off)" --data-binary off "127.0.0.1:$H3/_k10s/power")
[[ "$r" == 202 ]] && ok "signed POST /_k10s/power off -> 202" || ko "power off request: $r"
left() { [[ "$(st "$H1" "[m['state'] for m in c['members'] if m['id'] == '$GUEST_ID']")" == "['left']" ]]; }
t=$(wait_for 5 left) && ok "n1 sees the guest as left, not dead ($t ms)" || ko "guest state on n1: $(st "$H1" "[m['state'] for m in c['members'] if m['id'] == '$GUEST_ID']")"
st_q=124
for _ in $(seq 1 200); do
    kill -0 "$QPID" 2>/dev/null || { wait "$QPID"; st_q=$?; break; }
    sleep 0.05
done
[[ $st_q == 0 ]] && grep -aq "k10-metal: power off" "$GLOG" && ok "guest powered off through ACPI (QEMU exit 0)" || ko "guest power off: QEMU exit $st_q"
QPID=
grep -aq "PANIC\|CPU exception" "$OUT"/guest*.log && ko "kernel panic/exception in guest logs" || ok "no panic or CPU exception in guest logs"
echo "== timings: join ${JOIN_MS} ms, apply->all applied ${APPLY_MS} ms, rejoin ${REJOIN_MS} ms, rejoin->applied ${RESYNC_MS} ms"
echo "== $pass passed, $fail failed"
[[ $fail == 0 ]]
