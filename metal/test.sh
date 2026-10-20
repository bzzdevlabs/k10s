#!/usr/bin/env bash
# End-to-end test of k10-metal under QEMU/KVM:
#   1. in-kernel self tests (k10.selftest → isa-debug-exit status 33), both engines;
#   then, for each engine (ENGINES, default "jit interp"; the JIT is the default, `k10.engine=interp` selects the
#   interpreter):
#   2. boot with the test bundle, wait for the "ready" serial line, check every service over HTTP
#      (status codes and bodies), traps → 500 + instance replaced, KV, instance state vs fresh, service calls,
#      the Ten-compiled services (SHA-256 chain, f64 n-body, merge sort in 2 MiB, KV/JSON REST API);
#   3. isolation: CPU-bound infinite loops (spin) running while other services keep answering;
#   4. redeploy over HTTP (POST /_k10s/apply) and check the new service set;
#   5. keep-alive load with bench/bin/loadgen (if present): no errors;
#   6. the same functional checks with a tiny guest (MEM_SMALL, default 8M).
#   Then: embedded bundle, qboot firmware, no panic in any serial log.
# Environment: TEST_PORT (default 18090), MEM (64M), MEM_SMALL (8M), BIOS (firmware, empty = SeaBIOS), ENGINES,
# EXTRA_CMDLINE (appended to the engine suites' kernel command line).
set -uo pipefail
cd "$(dirname "$0")"
PORT="${TEST_PORT:-18090}"
MEM="${MEM:-64M}"
MEM_SMALL="${MEM_SMALL:-8M}"
ENGINES="${ENGINES:-jit interp}"
# Also run the JIT suite on this many CPUs (0 = skip).
SMP="${SMP:-4}"
NCPU=1
LOADGEN=../bench/bin/loadgen
OUT=out/test
mkdir -p "$OUT"
[[ -f out/k10-metal.elf && -f out/bundle.k10b ]] || ./build.sh >/dev/null

pass=0
fail=0
ok() { echo "  ok   $*"; pass=$((pass + 1)); }
ko() { echo "  FAIL $*"; fail=$((fail + 1)); }
U="http://127.0.0.1:$PORT"

qemu_args() { # $1 = mem
    local a=(-M pc -enable-kvm -cpu host -m "$1" -smp "$NCPU" -nodefaults -display none -no-reboot
        -device isa-debug-exit,iobase=0xf4,iosize=0x04
        -netdev "user,id=n0,hostfwd=tcp:127.0.0.1:$PORT-:80"
        -device virtio-net-pci,disable-modern=on,netdev=n0,romfile=
        -kernel out/k10-metal.elf)
    [[ -n "${BIOS:-}" ]] && a+=(-bios "$BIOS")
    echo "${a[@]}"
}

QPID=
boot() { # $1 = mem, $2 = bundle, $3 = log, $4 = kernel command line
    rm -f "$3"
    # shellcheck disable=SC2046
    qemu-system-x86_64 $(qemu_args "$1") -initrd "$2" -serial "file:$3" -append "${4:-}" &
    QPID=$!
    for _ in $(seq 1 200); do
        grep -q "k10-metal ready" "$3" 2>/dev/null && return 0
        kill -0 $QPID 2>/dev/null || break
        sleep 0.05
    done
    return 1
}
shutdown() { [[ -n "$QPID" ]] && kill "$QPID" 2>/dev/null && wait "$QPID" 2>/dev/null; QPID=; }
trap shutdown EXIT

# expect NAME URL EXPECTED_STATUS EXPECTED_BODY_REGEX|len=N [curl args...]
expect() {
    local name=$1 url=$2 st=$3 re=$4
    shift 4
    local body code
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
    expect "status" "$U/_k10s/status" 200 '"services":\['
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

echo "== selftest"
# shellcheck disable=SC2046
timeout 30 qemu-system-x86_64 $(qemu_args "$MEM") -serial "file:$OUT/selftest.log" -append k10.selftest
st=$?
grep -E "  (ok|FAIL|info) " "$OUT/selftest.log" | sed 's/^/    /'
[[ $st == 33 ]] && ok "selftest (exit $st)" || ko "selftest (exit $st)"

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
    ((worst < 50)) && ok "50 hello requests during the spin storm, worst ${worst} ms" || ko "hello worst latency ${worst} ms during spins"
    expect "spin traps counted" "$U/_k10s/status" 200 '"name":"spin"[^}]*"interrupted":5'

    echo "== [$E] redeploy over HTTP"
    target/release/mkbundle -o "$OUT/v2.k10b" --version 2 --services hello,echo 2>/dev/null
    expect "apply v2" "$U/_k10s/apply" 200 '"version":2,"services":2' -X POST --data-binary "@$OUT/v2.k10b" -H 'Content-Type: application/octet-stream'
    expect "hello after apply" "$U/hello/" 200 '^hello from k10-metal$'
    expect "counter removed" "$U/counter" 404 'no such service'
    expect "older version refused" "$U/_k10s/apply" 400 'older' -X POST --data-binary @out/bundle.k10b
    expect "garbage refused" "$U/_k10s/apply" 400 'apply failed' -X POST --data-binary 'not a bundle'
    target/release/mkbundle -o "$OUT/v3.k10b" --version 3 2>/dev/null
    expect "apply v3 (all services back)" "$U/_k10s/apply" 200 '"version":3,"services":18' -X POST --data-binary "@$OUT/v3.k10b"
    expect "counter state kept across redeploys" "$U/counter" 200 '^3$'

    echo "== [$E] call waiting for a busy callee (max_instances = 1)"
    target/release/mkbundle -o "$OUT/v4.k10b" --version 4 --services hello,spin,callspin --max-instances 1 2>/dev/null
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
    target/release/mkbundle -o "$OUT/v5.k10b" --version 5 2>/dev/null
    expect "apply v5 (back to all services)" "$U/_k10s/apply" 200 '"version":5,"services":18' -X POST --data-binary "@$OUT/v5.k10b"

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
    if ((NCPU > 1)); then
        busy=$(curl -s "$U/_k10s/status" | python3 -c "import json,sys; d=json.load(sys.stdin); print(sum(1 for c in d['cpus'] if c['tasks_run'] > 0), len(d['cpus']))")
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
    for _ in $(seq 1 200); do grep -q "k10-metal ready" "$OUT/serial-embedded.log" 2>/dev/null && break; sleep 0.05; done
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
grep -q "PANIC\|CPU exception" "$OUT"/serial*.log && ko "kernel panic/exception in serial logs" || ok "no panic or CPU exception in serial logs"

echo "== $pass passed, $fail failed"
[[ $fail == 0 ]]
