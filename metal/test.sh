#!/usr/bin/env bash
# End-to-end test of k10-metal under QEMU/KVM:
#   1. in-kernel self tests (k10.selftest → isa-debug-exit status 33);
#   2. boot with the test bundle, wait for the "ready" serial line, check every service over HTTP
#      (status codes and bodies), traps → 500 + instance replaced, KV, instance state vs fresh, service calls;
#   3. isolation: CPU-bound infinite loops (spin) running while other services keep answering;
#   4. redeploy over HTTP (POST /_k10s/apply) and check the new service set;
#   5. keep-alive load with bench/bin/loadgen (if present): no errors;
#   6. the same functional checks with a tiny guest (MEM_SMALL, default 8M).
# Environment: TEST_PORT (default 18090), MEM (64M), MEM_SMALL (8M), BIOS (firmware, empty = SeaBIOS).
set -uo pipefail
cd "$(dirname "$0")"
PORT="${TEST_PORT:-18090}"
MEM="${MEM:-64M}"
MEM_SMALL="${MEM_SMALL:-8M}"
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
    local a=(-M pc -enable-kvm -cpu host -m "$1" -smp 1 -nodefaults -display none -no-reboot
        -device isa-debug-exit,iobase=0xf4,iosize=0x04
        -netdev "user,id=n0,hostfwd=tcp:127.0.0.1:$PORT-:80"
        -device virtio-net-pci,disable-modern=on,netdev=n0,romfile=
        -kernel out/k10-metal.elf)
    [[ -n "${BIOS:-}" ]] && a+=(-bios "$BIOS")
    echo "${a[@]}"
}

QPID=
boot() { # $1 = mem, $2 = bundle, $3 = log
    rm -f "$3"
    # shellcheck disable=SC2046
    qemu-system-x86_64 $(qemu_args "$1") -initrd "$2" -serial "file:$3" &
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
}

echo "== selftest"
# shellcheck disable=SC2046
timeout 30 qemu-system-x86_64 $(qemu_args "$MEM") -serial "file:$OUT/selftest.log" -append k10.selftest
st=$?
grep -E "  (ok|FAIL|info) " "$OUT/selftest.log" | sed 's/^/    /'
[[ $st == 33 ]] && ok "selftest (exit $st)" || ko "selftest (exit $st)"

echo "== functional ($MEM)"
if boot "$MEM" out/bundle.k10b "$OUT/serial.log"; then
    ok "boot: $(grep 'k10-metal ready' "$OUT/serial.log")"
    functional

    echo "== isolation: 4 infinite loops (100 ms CPU budget each) vs. hello"
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

    echo "== redeploy over HTTP"
    target/release/mkbundle -o "$OUT/v2.k10b" --version 2 --services hello,echo 2>/dev/null
    expect "apply v2" "$U/_k10s/apply" 200 '"version":2,"services":2' -X POST --data-binary "@$OUT/v2.k10b" -H 'Content-Type: application/octet-stream'
    expect "hello after apply" "$U/hello/" 200 '^hello from k10-metal$'
    expect "counter removed" "$U/counter" 404 'no such service'
    expect "older version refused" "$U/_k10s/apply" 400 'older' -X POST --data-binary @out/bundle.k10b
    expect "garbage refused" "$U/_k10s/apply" 400 'apply failed' -X POST --data-binary 'not a bundle'
    target/release/mkbundle -o "$OUT/v3.k10b" --version 3 2>/dev/null
    expect "apply v3 (all services back)" "$U/_k10s/apply" 200 '"version":3,"services":14' -X POST --data-binary "@$OUT/v3.k10b"
    expect "counter state kept across redeploys" "$U/counter" 200 '^3$'

    echo "== call waiting for a busy callee (max_instances = 1)"
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
    expect "apply v5 (back to all services)" "$U/_k10s/apply" 200 '"version":5,"services":14' -X POST --data-binary "@$OUT/v5.k10b"

    if [[ -x $LOADGEN ]]; then
        echo "== keep-alive load (loadgen, 64 connections, 3 s)"
        r=$($LOADGEN -addr "127.0.0.1:$PORT" -path /hello/ -conns 64 -duration 3s -warmup 500ms 2>/dev/null)
        echo "    $r" | cut -c1-300
        errs=$(python3 -c "import json,sys; d=json.loads(sys.argv[1]); print(sum(d.get('errors',{}).values()) if isinstance(d.get('errors'),dict) else d.get('errors',0))" "$r" 2>/dev/null || echo "?")
        [[ "$errs" == 0 ]] && ok "loadgen: no errors" || ko "loadgen errors: $errs"
        r=$($LOADGEN -addr "127.0.0.1:$PORT" -path /chain -conns 16 -duration 2s -warmup 300ms 2>/dev/null)
        errs=$(python3 -c "import json,sys; d=json.loads(sys.argv[1]); print(sum(d.get('errors',{}).values()) if isinstance(d.get('errors'),dict) else d.get('errors',0))" "$r" 2>/dev/null || echo "?")
        [[ "$errs" == 0 ]] && ok "loadgen chain (service calls under load): no errors" || ko "loadgen chain errors: $errs"
    fi
    expect "still healthy" "$U/_k10s/status" 200 '"5xx":[0-9]+'
else
    ko "boot (no ready line):"
    tail -5 "$OUT/serial.log"
fi
shutdown

echo "== functional ($MEM_SMALL guest)"
if boot "$MEM_SMALL" out/bundle.k10b "$OUT/serial-small.log"; then
    ok "boot: $(grep 'k10-metal ready' "$OUT/serial-small.log")"
    functional
    if [[ -x $LOADGEN ]]; then
        r=$($LOADGEN -addr "127.0.0.1:$PORT" -path /hello/ -conns 16 -duration 2s -warmup 300ms 2>/dev/null)
        errs=$(python3 -c "import json,sys; d=json.loads(sys.argv[1]); print(sum(d.get('errors',{}).values()) if isinstance(d.get('errors'),dict) else d.get('errors',0))" "$r" 2>/dev/null || echo "?")
        [[ "$errs" == 0 ]] && ok "loadgen 16 conns on $MEM_SMALL: no errors" || ko "loadgen errors on $MEM_SMALL: $errs"
    fi
else
    ko "boot $MEM_SMALL (no ready line)"
    tail -5 "$OUT/serial-small.log"
fi
shutdown
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
