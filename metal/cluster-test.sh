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
#   5. restart the guest (same node id) → it rejoins with a higher incarnation, fetches the desired state, gets its
#      services back, and serves through its ingress again;
#   6. apply a new version through the guest → the hosted nodes fetch it from the guest and apply it.
#   Shared cluster key (`k10s keygen`, guest: k10.cluster_key=); guest admin API token: k10.admin_token=.
# Environment: BASE (port base, default 19400), K10S (hosted binary, default ../target/node/release/k10s, built with
# `CARGO_TARGET_DIR=target/node cargo build --release -p k10s` if missing), ENGINE (jit|interp, guest), PROBE_MS (100),
# SUSPICION_MULT (2), MEM (guest RAM, 128M), SMP (guest CPUs, 1).
set -uo pipefail
cd "$(dirname "$0")"
BASE="${BASE:-19400}"
K10S="${K10S:-../target/node/release/k10s}"
PROBE_MS="${PROBE_MS:-100}"
SUSP="${SUSPICION_MULT:-2}"
MEM="${MEM:-128M}"
ENGINE="${ENGINE:-jit}"
SMP="${SMP:-1}"
OUT=out/cluster-test
rm -rf "$OUT"
mkdir -p "$OUT"
[[ -x "$K10S" ]] || (cd .. && CARGO_TARGET_DIR=target/node cargo build --release -p k10s) || exit 1
[[ -f out/k10-metal.elf ]] || ./build.sh >/dev/null
"$K10S" keygen -o "$OUT/cluster.key" 2>/dev/null || exit 1
KEY=$(cat "$OUT/cluster.key")
TOKEN=$(od -An -tx1 -N16 /dev/urandom | tr -d ' \n')
AUTH="Authorization: Bearer $TOKEN"

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

hosted() { # name http gossip rpc [join]
    local a=(node --name "$1" --http "127.0.0.1:$2" --gossip "127.0.0.1:$3" --rpc "127.0.0.1:$4" --data "$OUT/$1"
        --mem 64MiB --shards 2 --probe-ms "$PROBE_MS" --suspicion-mult "$SUSP" --cluster-key-file "$OUT/cluster.key")
    [[ -n "${5:-}" ]] && a+=(--join "$5")
    "$K10S" "${a[@]}" 2> "$OUT/$1.log" &
    PIDS+=($!)
}

guest() { # serial log
    # shellcheck disable=SC2054
    qemu-system-x86_64 -M pc -enable-kvm -cpu host -m "$MEM" -smp "$SMP" -nodefaults -display none -no-reboot \
        -device isa-debug-exit,iobase=0xf4,iosize=0x04 \
        -netdev "user,id=n0,hostfwd=tcp:127.0.0.1:$H3-:80,hostfwd=udp:127.0.0.1:$G3-:7946,hostfwd=tcp:127.0.0.1:$R3-:7947" \
        -device virtio-net-pci,disable-modern=on,netdev=n0,romfile= -kernel out/k10-metal.elf -serial "file:$1" \
        -append "k10.engine=$ENGINE k10.join=127.0.0.1:$G1,127.0.0.1:$G2 k10.gossip=127.0.0.1:$G3 k10.rpc=127.0.0.1:$R3 k10.http=127.0.0.1:$H3 k10.node_id=$GUEST_ID k10.probe_ms=$PROBE_MS k10.suspicion_mult=$SUSP k10.mem=67108864 k10.cluster_key=$KEY k10.admin_token=$TOKEN" &
    QPID=$!
}

# st PORT PYEXPR: evaluates PYEXPR on the node's /_k10s/status JSON (`d`, cluster part `c`); prints the result.
st() {
    curl -s -m 2 -H "$AUTH" "127.0.0.1:$1/_k10s/status" 2>/dev/null | python3 -c "
import json, sys
try:
    d = json.load(sys.stdin)
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
code=$(curl -s -m 5 -o /dev/null -w '%{http_code}' "127.0.0.1:$H3/_k10s/status")
[[ "$code" == 401 ]] && ok "guest admin API without the token -> 401" || ko "guest status without the token: $code"

echo "== apply through hosted node n1"
target/release/mkbundle -o "$OUT/bundle.k10b" --version 0 2>/dev/null
T0=$(now_ms)
r=$(curl -s -m 10 --data-binary "@$OUT/bundle.k10b" "127.0.0.1:$H1/_k10s/apply")
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

echo "== restart the guest (same node id)"
T0=$(now_ms)
guest "$OUT/guest2.log"
t=$(wait_for 15 all_live 3)
REJOIN_MS=$(($(now_ms) - T0))
[[ $t != -1 ]] && ok "guest rejoined: 3 live members everywhere (${REJOIN_MS} ms after QEMU start)" || ko "guest did not rejoin"
INC2=$(st "$H1" "[m['incarnation'] for m in c['members'] if m['id']=='$GUEST_ID'][0]")
((INC2 > INC1)) && ok "higher incarnation ($INC1 -> $INC2)" || ko "incarnation $INC1 -> $INC2"
t=$(wait_for 10 eval "[[ \$(applied_v $H3) == $V ]]")
RESYNC_MS=$(($(now_ms) - T0))
[[ $t != -1 ]] && ok "rejoined guest fetched and applied v$V (${RESYNC_MS} ms after QEMU start)" || ko "rejoined guest did not apply v$V"
t=$(wait_for 5 eval "[[ \"\$(st $H1 \"sorted(c.get('placement',{}).items())\")\" == *$GUEST_ID* ]]")
GSVC2=$(st "$H3" "' '.join(s['name'] for s in d['services'] if s.get('placement') == 'local')")
[[ "$GSVC2" == "$GSVC" ]] && ok "guest hosts its services again: $GSVC2" || ko "guest services after rejoin: '$GSVC2' (before: '$GSVC')"
serve_all "rejoined"

echo "== apply through the guest (operator apply on the k10-metal node)"
target/release/mkbundle -o "$OUT/bundle2.k10b" --version 0 --services hello,echo,sha,chain,config,compute 2>/dev/null
r=$(curl -s -m 10 -H "$AUTH" --data-binary "@$OUT/bundle2.k10b" "127.0.0.1:$H3/_k10s/apply")
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
grep -q "PANIC\|CPU exception" "$OUT"/guest*.log && ko "kernel panic/exception in guest logs" || ok "no panic or CPU exception in guest logs"
echo "== timings: join ${JOIN_MS} ms, apply->all applied ${APPLY_MS} ms, rejoin ${REJOIN_MS} ms, rejoin->applied ${RESYNC_MS} ms"
echo "== $pass passed, $fail failed"
[[ $fail == 0 ]]
