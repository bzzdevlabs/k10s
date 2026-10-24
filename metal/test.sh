#!/usr/bin/env bash
# End-to-end test of k10-metal under QEMU (KVM, or TCG where there is no KVM):
#   1. in-kernel self tests (k10.selftest: the kernel prints its verdict and powers off through ACPI; QEMU must
#      exit 0), both engines, ACPI tables, page attributes, a virtio-blk disk (the host checks what it wrote),
#      fw_cfg, the admin gate;
#   then, for each engine (ENGINES, default "jit interp"; the JIT is the default, `k10.engine=interp` selects the
#   interpreter), on PLATFORM (qemu.sh; default pc):
#   2. boot with the test bundle, wait for the "ready" serial line, check every service over HTTP
#      (status codes and bodies), traps → 500 + instance replaced, KV, instance state vs fresh, service calls,
#      the Ten-compiled services (SHA-256 chain, f64 n-body, merge sort in 2 MiB, KV/JSON REST API);
#   3. isolation: CPU-bound infinite loops (spin) running while other services keep answering;
#   4. redeploy over HTTP (POST /_k10s/apply signed with the admin key) and check the new service set;
#   5. keep-alive load with bench/bin/loadgen (if present): no errors;
#   6. the same functional checks with a tiny guest (MEM_SMALL, default 8M).
#   Admin API: requests signed (K10s-HMAC) with an admin key passed as fw_cfg `opt/k10s/admin_key`, for the guest's
#   per-boot audience; unsigned, wrongly signed, replayed, body-mismatched requests and requests signed for an earlier
#   boot are refused, health and audience are open, and a guest without an admin key serves health only.
#   Then (PLATFORMS, default every platform of qemu.sh): on each machine a self test with a disk, then a boot that
#   serves, reboots on a signed `POST /_k10s/power` (`reboot`: the firmware runs again, the kernel boots a second
#   time) and powers off on another (`off`: QEMU exits 0) — no isa-debug-exit device anywhere; the console moved
#   to COM2 by an ACPI SPCR table; a static address (k10.ip); the PM timer and PIT clock paths (no HPET); DHCP
#   renew, rebind and NAK against a scripted server (tools/dhcp_test.py); 16 GiB and 32 vCPUs (BIG=0 skips);
#   embedded bundle, qboot firmware, no panic in any serial log.
# Environment: TEST_PORT (default 18090), MEM (64M), MEM_SMALL (8M), BIOS (firmware, empty = SeaBIOS), ENGINES,
# PLATFORM, PLATFORMS, ACCEL (qemu.sh), EXTRA_CMDLINE (appended to the engine suites' kernel command line),
# BOOT_WAIT (seconds for the ready line, default 30), HOG_MS (latency bound during the spin storm; default 50 with
# KVM, 1000 with TCG), K10S (hosted binary for `k10s keygen`, default ../target/metal-host/release/k10s, built
# with `CARGO_TARGET_DIR=target/metal-host cargo build --release -p k10s`).
set -uo pipefail
cd "$(dirname "$0")"
. ./qemu.sh
PORT="${TEST_PORT:-18090}"
MEM="${MEM:-64M}"
MEM_SMALL="${MEM_SMALL:-8M}"
ENGINES="${ENGINES:-jit interp}"
PLATFORM="${PLATFORM:-pc}"
PLATFORMS="${PLATFORMS:-pc pc-legacy pc-legacy-split pc-modern q35 microvm microvm-v2}"
BOOT_WAIT="${BOOT_WAIT:-30}"
ACCEL_ARGS=$(k10_accel) || exit 1
[[ $ACCEL_ARGS == *kvm* ]] && HOG_MS="${HOG_MS:-50}" || HOG_MS="${HOG_MS:-1000}"
# Also run the JIT suite on this many CPUs (0 = skip).
SMP="${SMP:-4}"
NCPU=1
LOADGEN=../bench/bin/loadgen
MKBUNDLE="${CARGO_TARGET_DIR:-target}/release/mkbundle"
OUT=out/test
mkdir -p "$OUT"
[[ -f out/k10-metal.elf && -f out/bundle.k10b ]] || ./build.sh >/dev/null
if [[ -z "${K10S:-}" ]]; then
    K10S=../target/metal-host/release/k10s
    (cd .. && CARGO_TARGET_DIR=target/metal-host cargo build --release -q -p k10s) || exit 1
fi

# Admin keys (`k10s keygen`, mode 0600): the guest's (fw_cfg opt/k10s/admin_key) and another one.
rm -f "$OUT/admin.key" "$OUT/wrong.key"
"$K10S" keygen -o "$OUT/admin.key" 2>/dev/null && "$K10S" keygen -o "$OUT/wrong.key" 2>/dev/null || exit 1
export K10S_ADMIN_TOKEN_FILE=$OUT/admin.key
# sign METHOD TARGET [BODY|@FILE]: Authorization header value for an admin request to the running guest (signed for
# its audience, /_k10s/audience).
sign() { tools/admin_auth.py "127.0.0.1:$PORT" "$@"; }
# A request signed for the previous guest boot (replayed to the next one).
EARLIER=

pass=0
fail=0
ok() { echo "  ok   $*"; pass=$((pass + 1)); }
ko() { echo "  FAIL $*"; fail=$((fail + 1)); }
U="http://127.0.0.1:$PORT"

qemu_args() { # $1 = mem, $2 = platform (default PLATFORM); NO_REBOOT=0: let the guest reboot
    local p=${2:-$PLATFORM}
    local a=($ACCEL_ARGS $(k10_machine "$p") -m "$1" -smp "$NCPU" -nodefaults -display none
        -netdev "user,id=n0,hostfwd=tcp:127.0.0.1:$PORT-:80" $(k10_nic "$p" n0)
        -kernel out/k10-metal.elf)
    [[ ${NO_REBOOT:-1} == 1 ]] && a+=(-no-reboot)
    [[ -n "${BIOS:-}" ]] && a+=(-bios "$BIOS")
    echo "${a[@]}"
}

# wait_ready LOG [N]: until LOG has N (default 1) complete ready lines (the console writes 16 bytes at a time, so
# a line counts once its \r is there), or QEMU is gone, or BOOT_WAIT passes.
wait_ready() {
    local i n
    for ((i = 0; i < BOOT_WAIT * 20; i++)); do
        n=$(grep -ac "k10-metal ready .*"$'\r' "$1" 2>/dev/null)
        ((${n:-0} >= ${2:-1})) && return 0
        kill -0 "$QPID" 2>/dev/null || break
        sleep 0.05
    done
    return 1
}

QPID=
boot() { # $1 = mem, $2 = bundle, $3 = log, $4 = kernel command line; NO_ADMIN_KEY=1: no admin key; QEMU_EXTRA: more args
    rm -f "$3"
    local fw=(-fw_cfg "name=opt/k10s/admin_key,file=$OUT/admin.key")
    [[ -n "${NO_ADMIN_KEY:-}" ]] && fw=()
    # shellcheck disable=SC2046,SC2086
    qemu-system-x86_64 $(qemu_args "$1" "${BOOT_PLATFORM:-}") "${fw[@]}" -initrd "$2" -serial "file:$3" ${QEMU_EXTRA:-} -append "${4:-}" &
    QPID=$!
    wait_ready "$3"
}
# wait_exit TIMEOUT_S: QEMU's exit status once it is gone (124 if it is still running after TIMEOUT_S).
wait_exit() {
    local i
    for ((i = 0; i < $1 * 20; i++)); do
        kill -0 "$QPID" 2>/dev/null || { wait "$QPID"; local st=$?; QPID=; return $st; }
        sleep 0.05
    done
    return 124
}
shutdown() { [[ -n "$QPID" ]] && kill "$QPID" 2>/dev/null && wait "$QPID" 2>/dev/null; QPID=; }
trap shutdown EXIT

# expect NAME URL EXPECTED_STATUS EXPECTED_BODY_REGEX|len=N [curl args...]; /_k10s/ requests are signed.
expect() {
    local name=$1 url=$2 st=$3 re=$4
    shift 4
    local body code
    if [[ "$url" == */_k10s/* ]]; then
        local m=GET data= i
        for ((i = 1; i <= $#; i++)); do
            case "${!i}" in
                -X) i=$((i + 1)); m=${!i} ;;
                --data-binary) i=$((i + 1)); data=${!i}; [[ $m == GET ]] && m=POST ;;
            esac
        done
        set -- -H "Authorization: $(sign "$m" "${url#"$U"}" "$data")" "$@"
    fi
    body=$(curl -sS -m 5 -o - -w '\n%{http_code}' "$@" "$url" 2>&1)
    code=${body##*$'\n'}
    body=${body%$'\n'*}
    body=${body%$'\n'}
    local match=0
    if [[ "$re" == len=* ]]; then
        [[ ${#body} == "${re#len=}" ]] && match=1
    elif [[ "$body" =~ $re ]]; then
        match=1
    fi
    if [[ "$code" == "$st" && $match == 1 ]]; then
        ok "$name ($code)"
    else
        ko "$name: got $code '${body:0:120}', want $st /$re/"
    fi
}

functional() {
    expect "hello" "$U/hello/" 200 '^hello from k10-metal$'
    expect "echo body (POST)" "$U/echo/x" 200 '^some body bytes$' -X POST --data-binary 'some body bytes'
    expect "echo path + query" "$U/echo/a/b?c=d" 200 '^/a/b\?c=d$'
    expect "echo 100 KiB body" "$U/echo" 200 'len=102400' -X POST --data-binary "$(head -c 102400 /dev/zero | tr '\0' x)"
    expect "compute" "$U/compute?n=100000" 200 '^1120606191$'
    expect "counter 1" "$U/counter" 200 '^1$'
    expect "counter 2 (kv persisted)" "$U/counter" 200 '^2$'
    expect "chain (call echo)" "$U/chain" 200 '^chain -> ping from chain$'
    # `calls` allowlist: probe may call hello only; anything else fails like an unknown service.
    expect "probe -> hello (granted)" "$U/probe/hello/" 200 '^probe -> hello from k10-metal$'
    expect "probe -> echo (denied: exists, not granted)" "$U/probe/echo/x" 502 '^call failed: -1$'
    expect "probe -> probe (denied: self)" "$U/probe/probe/hello/" 502 '^call failed: -1$'
    expect "probe -> nosuch (unknown)" "$U/probe/nosuch" 502 '^call failed: -1$'
    expect "trap -> 500" "$U/trap" 500 'Unreachable'
    expect "instcount (init ran)" "$U/instcount" 200 '^1001$'
    expect "instcount (instance state kept)" "$U/instcount" 200 '^1002$'
    expect "freshcount" "$U/freshcount" 200 '^1001$'
    expect "freshcount (snapshot restored)" "$U/freshcount" 200 '^1001$'
    expect "config" "$U/config" 200 '^greeting=hello from the manifest$'
    expect "log" "$U/log" 200 '^logged$'
    expect "logs endpoint" "$U/_k10s/logs/log" 200 'hello from the log service'
    expect "time" "$U/time" 200 '^1[0-9]{12} [0-9]+$'
    expect "random" "$U/random" 200 '^[0-9]+$'
    expect "unknown service" "$U/nope" 404 'no such service'
    expect "spin -> budget trap" "$U/spin" 500 'Interrupted'
    expect "status (signed)" "$U/_k10s/status" 200 '"services":\['
    local code a
    code=$(curl -s -m 5 -o /dev/null -w '%{http_code}' "$U/_k10s/health")
    [[ "$code" == 200 ]] && ok "health unsigned -> 200" || ko "health unsigned: $code"
    a=$(curl -s -m 5 "$U/_k10s/audience")
    [[ "$a" =~ ^[0-9a-f]{32}$ ]] && ok "audience unsigned -> $a" || ko "audience: '$a'"
    a=$(curl -s -m 5 -D - -o /dev/null "$U/_k10s/status" | tr -d '\r')
    [[ "$a" == "HTTP/1.1 401"* && "$a" == *'WWW-Authenticate: K10s-HMAC realm="k10s"'* ]] && ok "status unsigned -> 401, K10s-HMAC challenge" || ko "status unsigned: ${a:0:80}"
    code=$(curl -s -m 5 -o /dev/null -w '%{http_code}' "$U/_k10s/metrics")
    [[ "$code" == 401 ]] && ok "metrics unsigned -> 401" || ko "metrics unsigned: $code"
    code=$(curl -s -m 5 -o /dev/null -w '%{http_code}' "$U/_k10s/bench/10/hello/")
    [[ "$code" == 401 ]] && ok "bench unsigned -> 401" || ko "bench unsigned: $code"
    code=$(curl -s -m 5 -o /dev/null -w '%{http_code}' -H "Authorization: $(K10S_ADMIN_TOKEN_FILE=$OUT/wrong.key sign POST /_k10s/apply x)" --data-binary x "$U/_k10s/apply")
    [[ "$code" == 401 ]] && ok "apply signed with another key -> 401" || ko "apply signed with another key: $code"
    a=$(sign GET /_k10s/status)
    code=$(curl -s -m 5 -o /dev/null -w '%{http_code}' -H "Authorization: $a" "$U/_k10s/status")$(curl -s -m 5 -o /dev/null -w '%{http_code}' -H "Authorization: $a" "$U/_k10s/status")
    [[ "$code" == 200401 ]] && ok "replayed signed request -> 401" || ko "signed request then its replay: $code"
    code=$(curl -s -m 5 -o /dev/null -w '%{http_code}' -H "Authorization: $(sign POST /_k10s/apply x)" --data-binary y "$U/_k10s/apply")
    [[ "$code" == 401 ]] && ok "apply whose body differs from the signed one -> 401" || ko "apply with another body: $code"

    # Services compiled from Ten (examples/*.ten) with k10-lang.
    expect "sha (Ten: SHA-256 chain x1000)" "$U/sha/?n=1000" 200 '^cc2cbdc008d39ef7d46c8f4bd213c400cf60059acb1f0b409f7116b2912f2c9a$'
    expect "nbody (Ten: f64)" "$U/nbody/?n=1000" 200 '^-0\.169075164.-0\.169087605$'
    expect "sort (Ten: 2 MiB instance)" "$U/sort/?n=50000" 200 '^n=50000 checksum=6500369350017099242$'
    expect "todo create (Ten: KV + JSON)" "$U/todo/" 201 '^\{"id":1,"title":"buy milk"\}$' -X POST --data-binary 'buy milk'
    expect "todo update (PUT)" "$U/todo/1" 200 '^\{"id":1,"title":"buy oat milk"\}$' -X PUT --data-binary 'buy oat milk'
    expect "todo list" "$U/todo/" 200 '^\[\{"id":1,"title":"buy oat milk"\}\]$'
    expect "todo delete" "$U/todo/1" 200 '^\{"deleted":1\}$' -X DELETE
    expect "todo gone" "$U/todo/1" 404 'not found'
    # Atomic KV (ids from kv_add, index updated with kv_cas): concurrent creates get distinct ids, none is lost.
    local pids=() i n
    for i in $(seq 1 16); do
        curl -s -m 5 -o /dev/null -X POST --data-binary "c$i" "$U/todo/" &
        pids+=($!)
    done
    wait "${pids[@]}"
    n=$(curl -s -m 5 "$U/todo/" | python3 -c 'import json,sys; l=json.load(sys.stdin); print(len(l), len({x["id"] for x in l}))' 2>&1)
    [[ "$n" == "16 16" ]] && ok "todo: 16 concurrent creates -> 16 distinct ids, all listed" || ko "todo concurrent creates: '$n'"
}

# disk_image FILE: 2 MiB raw disk, "k10s disk test" in block 1 (the self test reads it, then writes blocks 2-9).
disk_image() { python3 -c 'import sys; open(sys.argv[1], "wb").write(bytes(512) + b"k10s disk test".ljust(512, b"\0") + bytes(2 << 20))' "$1"; }
# disk_written FILE: the self test's write reached the host file (flushed before the power off).
disk_written() { python3 -c 'import sys; d = open(sys.argv[1], "rb").read(); sys.exit(d[1024:1044] != b"written by k10-metal")' "$1"; }

# selftest NAME PLATFORM [qemu args...]: the in-kernel self tests with a disk; the kernel powers off through ACPI.
selftest() {
    local name=$1 p=$2 log="$OUT/selftest-$1.log" disk="$OUT/disk-$1.img" st
    shift 2
    disk_image "$disk"
    rm -f "$log"
    # shellcheck disable=SC2046
    timeout 300 qemu-system-x86_64 $(qemu_args "$MEM" "$p") $(k10_disk "$p" "$disk") \
        -fw_cfg "name=opt/k10s/selftest,string=k10s fw_cfg selftest" -serial "file:$log" "$@" -append k10.selftest
    st=$?
    grep -aE "  (ok|FAIL|info) " "$log" | sed 's/^/    /'
    [[ $st == 0 ]] && grep -aq "k10-metal selftest: passed" "$log" && grep -aq "k10-metal: power off" "$log" \
        && ok "selftest $name: passed, ACPI power off (QEMU exit 0)" || ko "selftest $name (exit $st): $(grep -a "selftest:" "$log" | tail -1)"
    disk_written "$disk" && ok "selftest $name: the disk write reached the host" || ko "selftest $name: disk blocks 2-9 not written"
}

echo "== selftest"
selftest "$PLATFORM" "$PLATFORM"
grep -aq "ok   fw_cfg: file opt/k10s/selftest read" "$OUT/selftest-$PLATFORM.log" && ok "selftest read a fw_cfg file" || ko "selftest did not read the fw_cfg file"

echo "== admin API without an admin key"
if NO_ADMIN_KEY=1 boot "$MEM" out/bundle.k10b "$OUT/serial-nokey.log"; then
    code=$(curl -s -m 5 -o /dev/null -w '%{http_code}' "$U/_k10s/status")$(curl -s -m 5 -o /dev/null -w '%{http_code}' "$U/_k10s/audience")
    [[ "$code" == 403403 ]] && ok "status and audience refused (no admin key) -> 403" || ko "status/audience without an admin key: $code"
    code=$(curl -s -m 5 -o /dev/null -w '%{http_code}' "$U/_k10s/health")
    [[ "$code" == 200 ]] && ok "health served without an admin key" || ko "health without an admin key: $code"
    expect "services served without an admin key" "$U/hello/" 200 '^hello from k10-metal$'
else
    ko "boot without an admin key (no ready line)"
fi
shutdown

engine_suite() { # $1 = engine (jit | interp); NCPU = CPUs
local E=$1 CMD="k10.engine=$1${EXTRA_CMDLINE:+ $EXTRA_CMDLINE}"
((NCPU > 1)) && E="$1 smp$NCPU"
echo "== [$E] functional ($MEM)"
if boot "$MEM" out/bundle.k10b "$OUT/serial-${E// /-}.log" "$CMD"; then
    ok "boot: $(grep 'k10-metal ready' "$OUT/serial-${E// /-}.log")"
    expect "engine is $1" "$U/_k10s/status" 200 "\"engine\":\"$1\".*\"name\":\"sha\",[^}]*\"engine\":\"$1\""
    functional

    echo "== [$E] isolation: 4 infinite loops (100 ms CPU budget each) vs. hello"
    spins=()
    for _ in 1 2 3 4; do
        curl -s -m 10 -o /dev/null "$U/spin" &
        spins+=($!)
    done
    sleep 0.02
    worst=0
    n=0
    while ((n < 50)); do
        t=$(curl -s -m 5 -o /dev/null -w '%{time_total}' "$U/hello/")
        ms=$(python3 -c "print(int(float('$t')*1000))")
        ((ms > worst)) && worst=$ms
        n=$((n + 1))
    done
    wait "${spins[@]}"
    ((worst < HOG_MS)) && ok "50 hello requests during the spin storm, worst ${worst} ms" || ko "hello worst latency ${worst} ms during spins (bound $HOG_MS ms)"
    expect "spin traps counted" "$U/_k10s/status" 200 '"name":"spin"[^}]*"interrupted":5'

    echo "== [$E] redeploy over HTTP"
    "$MKBUNDLE" -o "$OUT/v2.k10b" --version 2 --services hello,echo 2>/dev/null
    expect "apply v2" "$U/_k10s/apply" 200 '"version":2,"services":2' -X POST --data-binary "@$OUT/v2.k10b" -H 'Content-Type: application/octet-stream'
    expect "hello after apply" "$U/hello/" 200 '^hello from k10-metal$'
    expect "counter removed" "$U/counter" 404 'no such service'
    expect "older version refused" "$U/_k10s/apply" 400 'older' -X POST --data-binary @out/bundle.k10b
    expect "garbage refused" "$U/_k10s/apply" 400 'apply failed' -X POST --data-binary 'not a bundle'
    "$MKBUNDLE" -o "$OUT/v3.k10b" --version 3 2>/dev/null
    expect "apply v3 (all services back)" "$U/_k10s/apply" 200 '"version":3,"services":19' -X POST --data-binary "@$OUT/v3.k10b"
    expect "counter state kept across redeploys" "$U/counter" 200 '^3$'

    echo "== [$E] call waiting for a busy callee (max_instances = 1)"
    "$MKBUNDLE" -o "$OUT/v4.k10b" --version 4 --services hello,spin,callspin --max-instances 1 2>/dev/null
    expect "apply v4" "$U/_k10s/apply" 200 '"version":4,"services":3' -X POST --data-binary "@$OUT/v4.k10b"
    c1=$(mktemp) c2=$(mktemp)
    curl -s -m 10 -w ' %{http_code} %{time_total}' "$U/callspin" > "$c1" &
    p1=$!
    sleep 0.02
    curl -s -m 10 -w ' %{http_code} %{time_total}' "$U/callspin" > "$c2" &
    p2=$!
    wait $p1 $p2
    r1=$(cat "$c1") r2=$(cat "$c2")
    rm -f "$c1" "$c2"
    if [[ "$r1" == "call failed: -4 502 "* && "$r2" == "call failed: -4 502 "* ]]; then
        ok "two callers of a 1-instance CPU hog: both get -4 (callee trapped): '$r1' / '$r2'"
    else
        ko "callspin: '$r1' / '$r2'"
    fi
    expect "spin instance replaced after traps" "$U/_k10s/status" 200 '"name":"spin"[^}]*"instances":0[^}]*"interrupted":2'
    "$MKBUNDLE" -o "$OUT/v5.k10b" --version 5 2>/dev/null
    expect "apply v5 (back to all services)" "$U/_k10s/apply" 200 '"version":5,"services":19' -X POST --data-binary "@$OUT/v5.k10b"

    if [[ -x $LOADGEN ]]; then
        echo "== [$E] keep-alive load (loadgen, 64 connections, 3 s)"
        r=$($LOADGEN -addr "127.0.0.1:$PORT" -path /hello/ -conns 64 -duration 3s -warmup 500ms 2>/dev/null)
        echo "    $(python3 -c "import json,sys; d=json.loads(sys.argv[1]); print('rps %.0f p50 %s us p99 %s us' % (d['rps'], d['latency_us']['p50'], d['latency_us']['p99']))" "$r" 2>/dev/null)"
        errs=$(python3 -c "import json,sys; d=json.loads(sys.argv[1]); print(sum(d.get('errors',{}).values()) if isinstance(d.get('errors'),dict) else d.get('errors',0))" "$r" 2>/dev/null || echo "?")
        [[ "$errs" == 0 ]] && ok "loadgen: no errors" || ko "loadgen errors: $errs"
        r=$($LOADGEN -addr "127.0.0.1:$PORT" -path /chain -conns 16 -duration 2s -warmup 300ms 2>/dev/null)
        errs=$(python3 -c "import json,sys; d=json.loads(sys.argv[1]); print(sum(d.get('errors',{}).values()) if isinstance(d.get('errors'),dict) else d.get('errors',0))" "$r" 2>/dev/null || echo "?")
        [[ "$errs" == 0 ]] && ok "loadgen chain (service calls under load): no errors" || ko "loadgen chain errors: $errs"
    fi
    expect "still healthy" "$U/_k10s/status" 200 '"5xx":[0-9]+'
    EARLIER=$(sign GET /_k10s/status)
    if ((NCPU > 1)); then
        busy=$(curl -s -H "Authorization: $(sign GET /_k10s/status)" "$U/_k10s/status" | python3 -c "import json,sys; d=json.load(sys.stdin); print(sum(1 for c in d['cpus'] if c['tasks_run'] > 0), len(d['cpus']))")
        [[ "${busy% *}" -ge 2 && "${busy#* }" == "$NCPU" ]] && ok "$NCPU CPUs online, ${busy% *} ran invocations" || ko "CPUs that ran invocations / online: $busy"
    fi
else
    ko "boot (no ready line):"
    tail -5 "$OUT/serial-${E// /-}.log"
fi
shutdown

echo "== [$E] functional ($MEM_SMALL guest)"
if boot "$MEM_SMALL" out/bundle.k10b "$OUT/serial-small-${E// /-}.log" "$CMD"; then
    ok "boot: $(grep 'k10-metal ready' "$OUT/serial-small-${E// /-}.log")"
    a=$(curl -s -m 5 -H "Authorization: $EARLIER" "$U/_k10s/status")
    [[ "$a" == *"another node or an earlier start"* ]] && ok "request signed for the previous boot -> refused" || ko "request signed for the previous boot: '${a:0:80}'"
    functional
    if [[ -x $LOADGEN ]]; then
        r=$($LOADGEN -addr "127.0.0.1:$PORT" -path /hello/ -conns 16 -duration 2s -warmup 300ms 2>/dev/null)
        errs=$(python3 -c "import json,sys; d=json.loads(sys.argv[1]); print(sum(d.get('errors',{}).values()) if isinstance(d.get('errors'),dict) else d.get('errors',0))" "$r" 2>/dev/null || echo "?")
        [[ "$errs" == 0 ]] && ok "loadgen 16 conns on $MEM_SMALL: no errors" || ko "loadgen errors on $MEM_SMALL: $errs"
    fi
else
    ko "boot $MEM_SMALL (no ready line)"
    tail -5 "$OUT/serial-small-${E// /-}.log"
fi
shutdown
}

for e in $ENGINES; do
    engine_suite "$e"
done
if ((SMP > 1)); then
    NCPU=$SMP
    engine_suite jit
    NCPU=1
fi
if [[ -f out/k10-metal-embedded.elf ]]; then
    echo "== embedded bundle (no -initrd)"
    rm -f "$OUT/serial-embedded.log"
    # shellcheck disable=SC2046
    qemu-system-x86_64 $(qemu_args "$MEM" | sed 's|out/k10-metal.elf|out/k10-metal-embedded.elf|') -serial "file:$OUT/serial-embedded.log" &
    QPID=$!
    wait_ready "$OUT/serial-embedded.log"
    grep -q "src=embedded" "$OUT/serial-embedded.log" && ok "boot from embedded bundle" || ko "embedded boot: $(cat "$OUT/serial-embedded.log")"
    expect "hello (embedded)" "$U/hello/" 200 '^hello from k10-metal$'
    shutdown
fi
if [[ -f /usr/share/qemu/qboot.rom ]]; then
    echo "== qboot firmware (kernel assigns PCI BARs itself)"
    BIOS=/usr/share/qemu/qboot.rom boot "$MEM" out/bundle.k10b "$OUT/serial-qboot.log" && ok "boot with qboot" || ko "boot with qboot"
    expect "hello (qboot)" "$U/hello/" 200 '^hello from k10-metal$'
    expect "chain (qboot)" "$U/chain" 200 '^chain -> ping from chain$'
    shutdown
fi

# power_cycle NAME LOG: the running guest (started with NO_REBOOT=0) serves, reboots on a signed request (the
# firmware and the kernel run again), serves again and powers off on another (QEMU exits 0).
power_cycle() {
    local name=$1 log=$2 st
    expect "hello ($name)" "$U/hello/" 200 '^hello from k10-metal$'
    expect "sha ($name)" "$U/sha/?n=1000" 200 '^cc2cbdc008d39ef7d46c8f4bd213c400cf60059acb1f0b409f7116b2912f2c9a$'
    expect "power: bad body refused ($name)" "$U/_k10s/power" 400 'want the body' -X POST --data-binary halt
    expect "power: reboot ($name)" "$U/_k10s/power" 202 '^rebooting$' -X POST --data-binary reboot
    wait_ready "$log" 2 && ok "$name: rebooted ($(grep -ac "k10-metal: reboot" "$log") reboot line, second ready line)" || ko "$name: no second boot"
    expect "hello after the reboot ($name)" "$U/hello/" 200 '^hello from k10-metal$'
    expect "power: off ($name)" "$U/_k10s/power" 202 '^powering off$' -X POST --data-binary off
    wait_exit 60
    st=$?
    [[ $st == 0 ]] && grep -aq "k10-metal: power off" "$log" && ok "$name: powered off through ACPI (QEMU exit 0)" || ko "$name: power off: QEMU exit $st"
    QPID=
}

for p in $PLATFORMS; do
    echo "== platform $p"
    selftest "$p" "$p"
    if NO_REBOOT=0 BOOT_PLATFORM=$p boot "$MEM" out/bundle.k10b "$OUT/serial-platform-$p.log" "k10.verbose"; then
        ok "boot $p: $(grep -a 'k10-metal ready' "$OUT/serial-platform-$p.log" | head -1)"
        grep -a "^net: \|^acpi: \|^boot phases" "$OUT/serial-platform-$p.log" | head -4 | sed 's/^/    /'
        power_cycle "$p" "$OUT/serial-platform-$p.log"
    else
        ko "boot $p (no ready line)"
        tail -5 "$OUT/serial-platform-$p.log"
    fi
    shutdown
done

echo "== console moved to COM2 by an ACPI SPCR table"
# SPCR body (after the 36-byte header QEMU builds): 16550, I/O port 0x2F8 (8-bit), IRQ 3, 115200 baud, 8N1.
python3 -c '
import struct, sys
b = bytes([0, 0, 0, 0]) + struct.pack("<BBBBQ", 1, 8, 0, 1, 0x2F8) + bytes([0, 3]) + struct.pack("<I", 0)
b += bytes([7, 0, 1, 0, 0, 0]) + struct.pack("<HH", 0xFFFF, 0xFFFF) + bytes(10)
open(sys.argv[1], "wb").write(b)' "$OUT/spcr.bin"
rm -f "$OUT/serial-com1.log" "$OUT/serial-com2.log"
# shellcheck disable=SC2046
timeout 120 qemu-system-x86_64 $(qemu_args "$MEM" pc) -acpitable "sig=SPCR,rev=2,oem_id=K10S,data=$OUT/spcr.bin" \
    -serial "file:$OUT/serial-com1.log" -serial "file:$OUT/serial-com2.log" -initrd out/bundle.k10b -append k10.exit_on_ready
st=$?
[[ $st == 0 ]] && grep -aq "k10-metal ready" "$OUT/serial-com2.log" && ! grep -aq "k10-metal ready" "$OUT/serial-com1.log" \
    && ok "SPCR: console on COM2, ACPI power off" || ko "SPCR console: exit $st, COM2: $(head -c 200 "$OUT/serial-com2.log")"

echo "== static address (k10.ip)"
if boot "$MEM" out/bundle.k10b "$OUT/serial-static.log" "k10.ip=10.0.2.15/24,10.0.2.2"; then
    grep -aq "ip=10.0.2.15" "$OUT/serial-static.log" && ! grep -aq "net: DHCP" "$OUT/serial-static.log" \
        && ok "static 10.0.2.15/24 via 10.0.2.2, no DHCP" || ko "static address: $(grep -a ready "$OUT/serial-static.log")"
    expect "hello (static address)" "$U/hello/" 200 '^hello from k10-metal$'
else
    ko "boot with a static address"
fi
shutdown
rm -f "$OUT/serial-badip.log"
# shellcheck disable=SC2046
timeout 60 qemu-system-x86_64 $(qemu_args "$MEM") -serial "file:$OUT/serial-badip.log" -append "k10.ip=10.0.2.15 k10.panic=1"
grep -aq "k10.ip=10.0.2.15: want" "$OUT/serial-badip.log" && grep -aq "rebooting in 1 s" "$OUT/serial-badip.log" \
    && ok "a bad k10.ip stops the boot, reboot after k10.panic=1 s" || ko "bad k10.ip: $(tail -2 "$OUT/serial-badip.log")"

echo "== clock without an HPET (ACPI PM timer, else CPUID or the PIT)"
cpu_nokvmclock=$([[ $ACCEL_ARGS == *kvm* ]] && echo "-cpu host,kvmclock=off" || echo "")
# shellcheck disable=SC2046,SC2086
timeout 120 qemu-system-x86_64 $(qemu_args "$MEM" pc) -machine hpet=off $cpu_nokvmclock -serial "file:$OUT/serial-nohpet.log" \
    -initrd out/bundle.k10b -append "k10.verbose k10.exit_on_ready"
st=$?
src=$(grep -ao "MHz ([A-Za-z0-9]*)" "$OUT/serial-nohpet.log" | head -1)
[[ $st == 0 && $src != *Hpet* && -n $src ]] && ok "no HPET: TSC $src, ready, powered off" || ko "no HPET: exit $st, $src"

if [[ -x tools/dhcp_test.py ]]; then
    echo "== DHCP renew, rebind, NAK (scripted server)"
    if ACCEL_ARGS="$ACCEL_ARGS" tools/dhcp_test.py --kernel out/k10-metal.elf --log "$OUT/serial-dhcp.log" > "$OUT/dhcp.out" 2>&1; then
        ok "DHCP: $(tail -1 "$OUT/dhcp.out")"
    else
        ko "DHCP: $(tail -3 "$OUT/dhcp.out")"
    fi
fi

if [[ ${BIG:-1} == 1 ]]; then
    echo "== 16 GiB of RAM, 32 vCPUs (q35)"
    # Anonymous memory, not reserved up front (MAP_NORESERVE): only what the guest touches is ever allocated, so this
    # runs on hosts with less than 16 GiB.
    if NCPU=32 BOOT_PLATFORM=q35 QEMU_EXTRA="-object memory-backend-ram,id=big,size=16G,reserve=off -machine memory-backend=big" \
        boot 16G out/bundle.k10b "$OUT/serial-big.log" "k10.engine=jit"; then
        r=$(grep -a "k10-metal ready" "$OUT/serial-big.log")
        [[ $r == *cpus=32* && $r =~ ram=(1677[0-9]{4})K ]] && ok "boot: $r" || ko "16G/32: $r"
        # 64 concurrent CPU-bound requests (SHA-256 chains): the dispatcher spreads them over the CPUs once it has
        # seen one run (a service without history counts as cheap and stays on CPU 0).
        curl -s -m 120 -o /dev/null "$U/sha/?n=20000"
        pids=()
        for i in $(seq 1 64); do
            curl -s -m 120 -o /dev/null "$U/sha/?n=20000" &
            pids+=($!)
        done
        wait "${pids[@]}"
        busy=$(curl -s -H "Authorization: $(sign GET /_k10s/status)" "$U/_k10s/status" | python3 -c "import json,sys; d=json.load(sys.stdin); print(sum(1 for c in d['cpus'] if c['tasks_run'] > 0), len(d['cpus']))")
        [[ "${busy% *}" -ge 16 && "${busy#* }" == 32 ]] && ok "32 CPUs online, ${busy% *} ran invocations" || ko "CPUs that ran invocations / online: $busy"
        expect "power: off (16G/32)" "$U/_k10s/power" 202 '^powering off$' -X POST --data-binary off
        wait_exit 60 && ok "16G/32: powered off" || ko "16G/32: power off"
        QPID=
    else
        ko "boot 16G/32 (no ready line)"
        tail -5 "$OUT/serial-big.log"
    fi
    shutdown
fi

grep -aq "PANIC\|CPU exception" "$OUT"/*.log && ko "kernel panic/exception in serial logs" || ok "no panic or CPU exception in serial logs"

echo "== $pass passed, $fail failed"
[[ $fail == 0 ]]
