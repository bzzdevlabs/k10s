#!/usr/bin/env bash
# k10-metal on Firecracker and Cloud Hypervisor (PVH boot, KVM): for each VMM
#   1. the in-kernel self tests with a virtio-blk disk (Firecracker: virtio-mmio; Cloud Hypervisor: virtio-pci
#      modern behind ECAM), then a power off that ends the VMM with status 0 (Cloud Hypervisor: ACPI S5 through the
#      sleep control register; Firecracker has no ACPI power off, so a reset, which ends it);
#   2. a boot with the test bundle and a static address on a tap device: the services answer over HTTP;
#   3. a reset: a configuration error reboots the guest after k10.panic=1 s (Firecracker exits on it, Cloud
#      Hypervisor boots the kernel again).
# These VMMs have no fw_cfg, so no admin key reaches the guest (its admin API stays closed, 403).
# Needs /dev/kvm and a tap device TAP (default k10tap0) owned by this user with HOST_IP (default 172.30.0.1/24), e.g.
#   sudo ip tuntap add dev k10tap0 mode tap user "$USER" && sudo ip addr add 172.30.0.1/24 dev k10tap0 &&
#   sudo ip link set k10tap0 up
# Environment: VMMS (default "firecracker cloud-hypervisor"), VMM_DIR (binaries, default target/vmm, see
# tools/get-vmms.sh), TAP, GUEST_IP (172.30.0.2), HOST_IP.
set -uo pipefail
cd "$(dirname "$0")"
VMMS="${VMMS:-firecracker cloud-hypervisor}"
VMM_DIR="${VMM_DIR:-target/vmm}"
TAP="${TAP:-k10tap0}"
GUEST_IP="${GUEST_IP:-172.30.0.2}"
HOST_IP="${HOST_IP:-172.30.0.1/24}"
OUT=out/vmm-test
rm -rf "$OUT"
mkdir -p "$OUT"
[[ -f out/k10-metal.elf && -f out/bundle.k10b ]] || ./build.sh >/dev/null
U="http://$GUEST_IP"
NET="k10.ip=$GUEST_IP/24,${HOST_IP%/*}"

pass=0
fail=0
ok() { echo "  ok   $*"; pass=$((pass + 1)); }
ko() { echo "  FAIL $*"; fail=$((fail + 1)); }

VPID=
stop() { [[ -n "$VPID" ]] && kill "$VPID" 2>/dev/null && wait "$VPID" 2>/dev/null; VPID=; }
trap stop EXIT

# start VMM LOG CMDLINE [initrd] [disk]: the VMM in the background, serial console to LOG.
start() {
    local vmm=$1 log=$2 cmdline=$3 initrd=${4:-} disk=${5:-}
    rm -f "$log"
    case $vmm in
    firecracker)
        local cfg=$OUT/fc.json api=$OUT/fc.sock
        rm -f "$api"
        python3 - "$cfg" "$cmdline" "$initrd" "$disk" "$TAP" <<'PY'
import json, os, sys
cfg, cmdline, initrd, disk, tap = sys.argv[1:]
c = {"boot-source": {"kernel_image_path": os.path.abspath("out/k10-metal.elf"), "boot_args": cmdline},
     "machine-config": {"vcpu_count": 2, "mem_size_mib": 128},
     "network-interfaces": [{"iface_id": "eth0", "host_dev_name": tap, "guest_mac": "52:54:00:12:34:56"}],
     "drives": []}
if initrd:
    c["boot-source"]["initrd_path"] = os.path.abspath(initrd)
if disk:
    c["drives"].append({"drive_id": "d0", "path_on_host": os.path.abspath(disk), "is_root_device": False, "is_read_only": False})
json.dump(c, open(cfg, "w"))
PY
        "$VMM_DIR/firecracker" --api-sock "$api" --config-file "$cfg" > "$log" 2>&1 &
        ;;
    cloud-hypervisor)
        local a=(--kernel out/k10-metal.elf --cmdline "$cmdline" --cpus boot=2 --memory size=128M --serial tty --console off
            --net "tap=$TAP,mac=52:54:00:12:34:56")
        [[ -n $initrd ]] && a+=(--initramfs "$initrd")
        [[ -n $disk ]] && a+=(--disk "path=$disk")
        "$VMM_DIR/cloud-hypervisor" "${a[@]}" > "$log" 2>&1 < /dev/null &
        ;;
    esac
    VPID=$!
}

# wait_line LOG REGEX [SECONDS] [COUNT]
wait_line() {
    local i n
    for ((i = 0; i < ${3:-30} * 20; i++)); do
        n=$(grep -acE "$2" "$1" 2>/dev/null)
        ((${n:-0} >= ${4:-1})) && return 0
        kill -0 "$VPID" 2>/dev/null || return 1
        sleep 0.05
    done
    return 1
}

# wait_exit SECONDS: the VMM's exit status (124: still running).
wait_exit() {
    local i st
    for ((i = 0; i < $1 * 20; i++)); do
        if ! kill -0 "$VPID" 2>/dev/null; then
            wait "$VPID"
            st=$?
            VPID=
            return $st
        fi
        sleep 0.05
    done
    return 124
}

expect() { # NAME URL STATUS REGEX [curl args]
    local name=$1 url=$2 st=$3 re=$4 body code
    shift 4
    body=$(curl -sS -m 10 -o - -w '\n%{http_code}' "$@" "$url" 2>&1)
    code=${body##*$'\n'}
    body=${body%$'\n'*}
    body=${body%$'\n'}
    if [[ "$code" == "$st" && "$body" =~ $re ]]; then ok "$name ($code)"; else ko "$name: got $code '${body:0:120}', want $st /$re/"; fi
}

for vmm in $VMMS; do
    if [[ ! -x "$VMM_DIR/$vmm" ]]; then
        ko "$vmm: no binary in $VMM_DIR (tools/get-vmms.sh)"
        continue
    fi
    echo "== $vmm: self test, power off"
    disk=$OUT/disk-$vmm.img
    python3 -c 'import sys; open(sys.argv[1], "wb").write(bytes(512) + b"k10s disk test".ljust(512, b"\0") + bytes(2 << 20))' "$disk"
    log=$OUT/selftest-$vmm.log
    start "$vmm" "$log" "k10.selftest" "" "$disk"
    wait_exit 180
    st=$?
    grep -aE "  (ok|FAIL|info) |^acpi:|^disk:|^net:|^smp:" "$log" | sed 's/^/    /'
    grep -aq "k10-metal selftest: passed" "$log" && grep -aq "k10-metal: power off" "$log" && [[ $st == 0 ]] \
        && ok "$vmm: self test passed, powered off (exit 0)" || ko "$vmm: self test: exit $st, $(grep -a "selftest:" "$log" | tail -1)"
    python3 -c 'import sys; d = open(sys.argv[1], "rb").read(); sys.exit(d[1024:1044] != b"written by k10-metal")' "$disk" \
        && ok "$vmm: the disk write reached the host" || ko "$vmm: disk blocks 2-9 not written"
    stop

    echo "== $vmm: serve (static $GUEST_IP)"
    log=$OUT/serve-$vmm.log
    start "$vmm" "$log" "k10.verbose $NET" out/bundle.k10b
    if wait_line "$log" "k10-metal ready" 30; then
        ok "boot: $(grep -a 'k10-metal ready' "$log")"
        grep -a "^net: \|^acpi: " "$log" | sed 's/^/    /'
        expect "hello" "$U/hello/" 200 '^hello from k10-metal$'
        expect "echo body" "$U/echo/x" 200 '^some body bytes$' -X POST --data-binary 'some body bytes'
        expect "compute" "$U/compute?n=100000" 200 '^1120606191$'
        expect "counter 1" "$U/counter" 200 '^1$'
        expect "counter 2 (kv)" "$U/counter" 200 '^2$'
        expect "chain (call)" "$U/chain" 200 '^chain -> ping from chain$'
        expect "sha (Ten, JIT)" "$U/sha/?n=1000" 200 '^cc2cbdc008d39ef7d46c8f4bd213c400cf60059acb1f0b409f7116b2912f2c9a$'
        expect "trap -> 500" "$U/trap" 500 'Unreachable'
        expect "health" "$U/_k10s/health" 200 '^ok$'
        expect "admin API closed without an admin key" "$U/_k10s/status" 403 'no admin key'
    else
        ko "$vmm: no ready line"
        tail -20 "$log"
    fi
    stop

    echo "== $vmm: reset (a bad k10.ip, k10.panic=1)"
    log=$OUT/reset-$vmm.log
    start "$vmm" "$log" "k10.ip=bad k10.panic=1"
    if [[ $vmm == firecracker ]]; then
        wait_exit 60
        st=$?
        grep -aq "rebooting in 1 s" "$log" && [[ $st == 0 ]] && ok "$vmm: reset ends the VM (exit 0)" || ko "$vmm: reset: exit $st, $(tail -2 "$log")"
    else
        wait_line "$log" "rebooting in 1 s" 60 2 && ok "$vmm: reset reboots the guest (second boot)" || ko "$vmm: reset: $(tail -3 "$log")"
    fi
    stop
done
grep -aq "PANIC\|CPU exception" "$OUT"/*.log && ko "kernel panic/exception in the logs" || ok "no panic or CPU exception in the logs"
echo "== $pass passed, $fail failed"
[[ $fail == 0 ]]
