#!/usr/bin/env bash
# Test suites of CI and the release gate, same locally. usage: ci-test.sh SUITE...
#   unit            all crates, release mode, JIT then interpreter (node, CLI); no #[ignore]d tests, no campaigns;
#                   the OCI tooling that needs no image or guest: bench/oci/table.py --check (docs/LX.md and the
#                   syscall table agree with the plan), os/tools/registry.py --self-test, lxtrace tracing a shell
#   musl            the same on x86_64-unknown-linux-musl (the static release binary)
#   arm64           on an arm64 host: the same (the AArch64 JIT, its encoder and SFI verifier tests included) for
#                   the host's gnu target and aarch64-unknown-linux-musl
#   i686            on an x86-64 host: all crates but the JIT for i686-unknown-linux-musl, run natively (interpreter;
#                   linked by rust-lld)
#   armv7           on an arm64 host: the same for armv7-unknown-linux-musleabihf, natively when the CPU runs AArch32
#                   code; otherwise under qemu-arm, the in-process suites only and without the seccomp tests (qemu-user
#                   implements no seccomp; os-legacy runs the armv7 node's filter under a real kernel) and the timing
#                   bounds
#   campaign        chaos_short + soak_short_* on both backends (minutes long, noise-sensitive)
#   metal-selftest  kernel self tests (k10.selftest) under QEMU/KVM: pc (1 and 4 CPUs, transitional and legacy
#                   virtio-net), q35 (ECAM, modern-only NIC), microvm (virtio-mmio), e1000e and igb NICs; each ends
#                   in an ACPI power off; the hardware watchdogs (i6300ESB, ICH9 TCO, ACPI WDAT) reset a kernel that
#                   hangs on purpose; the boot protocol's unit tests (metal/bootproto: k10s.conf, the tier file, boot
#                   counter, E820) and the drivers' register-model tests (metal/drivers: e1000e/I219, igb/igc,
#                   r8169/r8125, TCO/ESB/WDAT, control-method power buttons); the platform services
#                   (metal/tools/platform_test.py: SNTP and DNS on a scripted LAN, the clock stepped from 2001, the
#                   fixed-feature and GPE power buttons, single-node applies kept, IPv6 SLAAC)
#   metal           metal/test.sh and metal/cluster-test.sh end-to-end (TEST_PORT, default 18090)
#   metal-vmm       metal/vmm-test.sh: the kernel on Firecracker and Cloud Hypervisor (pinned downloads; sudo creates
#                   the tap device the guests use)
#   dist            the k10s integration tests against the static k10s of dist/ (K10S_BIN), both backends
#   os              PID 1 mode under QEMU (os/test.sh): boot, apply, serve, power off on the Debian and k10s
#                   kernels; DHCP renew, rebind and address change; the x86-64 images, A/B updates, rollout; the
#                   tiers of the x86-64 image (k10boot: native kernel, Tier 1, auto, fallback, signatures; Multiboot2)
#   os-arm64        os/test.sh --arch aarch64: the arm64 kernel, the aarch64-vm and Pi images (the Pi 3 on raspi3b),
#                   their A/B updates, a Pi in a cluster with an x86-64 node (K10S_BIN, K10S_HOST_BIN as os/test.sh; on
#                   an x86-64 host both are built here, the aarch64 one linked by rust-lld)
#   os-legacy       os/test.sh --arch i686 and --arch armv7: the i386 and armv7 kernels and images, their A/B updates
#                   (K10S_BIN_I686, K10S_BIN_ARMV7 and K10S_HOST_BIN, the x86-64 k10s, from the release; else built
#                   here, linked by rust-lld)
# env: CARGO_TEST_ARGS (extra cargo test args), K10S_VERSION_FULL (metal-selftest checks the banner)
set -euo pipefail
cd "$(dirname "$0")/.."

CRATES=(-p k10-nospec -p k10-ir -p k10-host -p k10-mesh -p k10-net -p k10-kvlog -p k10-lang -p k10-jit -p k10-node -p k10s)
# The 32-bit targets: no JIT; under qemu-user the crates whose tests run in-process.
I32_CRATES=(-p k10-nospec -p k10-ir -p k10-host -p k10-mesh -p k10-net -p k10-kvlog -p k10-lang -p k10-node -p k10s)
EMU_CRATES=(-p k10-nospec -p k10-ir -p k10-host -p k10-mesh -p k10-net -p k10-kvlog -p k10-lang -p k10-node)
# Campaign tests, skipped elsewhere (`--skip` matches test names in every test binary).
SKIP=(--skip chaos_short --skip soak_short)
read -r -a EXTRA <<<"${CARGO_TEST_ARGS:-}"

run() {
    echo "+ $*" >&2
    "$@"
}

unit() { # [--target TRIPLE]
    export K10_REQUIRE_OBJDUMP=1 # the encoder/SFI assurance tests must run, not skip
    objdump --version | head -1
    run env K10S_BACKEND=jit cargo test --release --locked "$@" "${EXTRA[@]}" "${CRATES[@]}" -- "${SKIP[@]}"
    run env K10S_BACKEND=interp cargo test --release --locked "$@" "${EXTRA[@]}" -p k10-node -p k10s -- "${SKIP[@]}"
}

oci_tools() {
    run python3 bench/oci/table.py --check
    run python3 -I os/tools/registry.py --self-test
    local t=$PWD/target/oci-lxtrace out
    (cd bench/oci/lxtrace && run env CARGO_TARGET_DIR="$t" cargo build --release --locked)
    out=$(mktemp -d)
    run "$t/release/lxtrace" trace "$out/trace.json" -- sh -c 'exec true'
    python3 -c 'import json, sys; d = json.load(open(sys.argv[1])); assert d["exit"] == 0 and d["programs"], d' \
        "$out/trace.json"
    rm -rf "$out"
}

arm64() {
    [[ $(uname -m) == aarch64 ]] || { echo "ci-test.sh: arm64 runs on an arm64 host" >&2; return 1; }
    unit
    unit --target aarch64-unknown-linux-musl
}

i686() {
    [[ $(uname -m) == x86_64 ]] || { echo "ci-test.sh: i686 runs on an x86-64 host" >&2; return 1; }
    export CARGO_TARGET_I686_UNKNOWN_LINUX_MUSL_LINKER=rust-lld
    run cargo test --release --locked --target i686-unknown-linux-musl "${EXTRA[@]}" "${I32_CRATES[@]}" -- "${SKIP[@]}"
}

armv7() {
    [[ $(uname -m) == aarch64 ]] || { echo "ci-test.sh: armv7 runs on an arm64 host" >&2; return 1; }
    local t=armv7-unknown-linux-musleabihf
    export CARGO_TARGET_ARMV7_UNKNOWN_LINUX_MUSLEABIHF_LINKER=rust-lld
    run cargo build --release --locked -p k10s --target "$t"
    if "${CARGO_TARGET_DIR:-target}/$t/release/k10s" --version; then
        run cargo test --release --locked --target "$t" "${EXTRA[@]}" "${I32_CRATES[@]}" -- "${SKIP[@]}"
    else
        echo "ci-test.sh: this CPU does not run AArch32 code: in-process suites under qemu-arm, without seccomp and timing tests" >&2
        run env CARGO_TARGET_ARMV7_UNKNOWN_LINUX_MUSLEABIHF_RUNNER=qemu-arm cargo test --release --locked --target "$t" \
            "${EXTRA[@]}" "${EMU_CRATES[@]}" -- "${SKIP[@]}" --skip sandbox:: --skip speed_
    fi
}

os_legacy() {
    local host=${K10S_HOST_BIN:-} i686=${K10S_BIN_I686:-} armv7=${K10S_BIN_ARMV7:-}
    if [[ -z $host || -z $i686 || -z $armv7 ]]; then
        export CARGO_TARGET_I686_UNKNOWN_LINUX_MUSL_LINKER=rust-lld CARGO_TARGET_ARMV7_UNKNOWN_LINUX_MUSLEABIHF_LINKER=rust-lld
        run cargo build --release --locked -p k10s --target x86_64-unknown-linux-musl --target i686-unknown-linux-musl \
            --target armv7-unknown-linux-musleabihf
        mkdir -p target/os
        strip --strip-debug -o target/os/k10s-x86_64 target/x86_64-unknown-linux-musl/release/k10s
        strip --strip-debug -o target/os/k10s-i686 target/i686-unknown-linux-musl/release/k10s
        arm-linux-gnueabihf-strip --strip-debug -o target/os/k10s-armv7 target/armv7-unknown-linux-musleabihf/release/k10s
        host=target/os/k10s-x86_64 i686=target/os/k10s-i686 armv7=target/os/k10s-armv7
    fi
    run env K10S_BIN="$i686" K10S_HOST_BIN="$host" os/test.sh --arch i686
    run env K10S_BIN="$armv7" K10S_HOST_BIN="$host" os/test.sh --arch armv7
}

os_arm64() {
    if [[ $(uname -m) != aarch64 && -z ${K10S_BIN:-} ]]; then
        export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld
        run cargo build --release --locked -p k10s --target aarch64-unknown-linux-musl --target x86_64-unknown-linux-musl
        mkdir -p target/os
        aarch64-linux-gnu-strip --strip-debug -o target/os/k10s-aarch64 target/aarch64-unknown-linux-musl/release/k10s
        strip --strip-debug -o target/os/k10s-x86_64 target/x86_64-unknown-linux-musl/release/k10s
        export K10S_BIN=target/os/k10s-aarch64 K10S_HOST_BIN=target/os/k10s-x86_64
    fi
    run os/test.sh --arch aarch64
}

dist() {
    local tgz=(dist/k10s-*-x86_64-unknown-linux-musl.tar.gz) tmp
    if [[ ${#tgz[@]} != 1 || ! -f ${tgz[0]} ]]; then
        echo "ci-test.sh: want one dist/k10s-*-x86_64-unknown-linux-musl.tar.gz (scripts/dist.sh)" >&2
        return 1
    fi
    tmp=$(mktemp -d)
    tar -xzf "${tgz[0]}" -C "$tmp" --strip-components 1
    "$tmp/k10s" --version
    for b in jit interp; do
        run env K10S_BIN="$tmp/k10s" K10S_BACKEND=$b cargo test --release --locked "${EXTRA[@]}" -p k10s --tests -- "${SKIP[@]}"
    done
    rm -rf "$tmp"
}

campaign() {
    for b in jit interp; do
        run env K10S_BACKEND=$b cargo test --release --locked "${EXTRA[@]}" -p k10s --test chaos --test soak
    done
}

qemu_selftest() { # SMP PLATFORM [QEMU ARGS...]
    # $1 = CPUs, $2 = platform (metal/qemu.sh), then more devices (disks). The kernel prints its verdict and powers off
    # through ACPI.
    local cpus=$1 p=$2 log=metal/out/selftest-$2-smp$1${3:+-disks}.log st=0
    shift 2
    rm -f "$log"
    # shellcheck disable=SC2046
    timeout 120 qemu-system-x86_64 $(k10_accel) $(k10_machine "$p") -m 64M -smp "$cpus" -nodefaults -display none -no-reboot \
        -netdev user,id=n0 $(k10_nic "$p" n0) "$@" \
        -kernel metal/out/k10-metal.elf -serial "file:$log" -append "k10.selftest" || st=$?
    set -- "$cpus" "$p"
    sed 's/^/    /' "$log"
    if [[ -n ${K10S_VERSION_FULL:-} ]] && ! grep -qF "k10-metal $K10S_VERSION_FULL:" "$log"; then
        echo "metal selftest: the boot banner does not carry version $K10S_VERSION_FULL" >&2
        return 1
    fi
    [[ $st == 0 ]] && grep -q "k10-metal selftest: passed" "$log" || {
        echo "metal selftest ($2, smp $1): exit status $st (want 0 after the ACPI power off), verdict: $(grep -a "k10-metal selftest" "$log")" >&2
        return 1
    }
}

qemu_watchdog() { # NAME PLATFORM SECONDS EXPECTED_NAME [qemu args]
    # The kernel arms the watchdog and hangs with interrupts off: the watchdog must reset it (QEMU exits: -no-reboot).
    local log=metal/out/watchdog-$1.log st=0
    rm -f "$log"
    # shellcheck disable=SC2046
    timeout 60 qemu-system-x86_64 $(k10_accel) $(k10_machine "$2") -m 64M -nodefaults -display none -no-reboot \
        -kernel metal/out/k10-metal.elf -serial "file:$log" "${@:5}" -append "k10.selftest=watchdog k10.watchdog=$3" || st=$?
    sed 's/^/    /' "$log"
    [[ $st == 0 ]] && grep -q "^watchdog: $4" "$log" && grep -q "watchdog selftest: hanging" "$log" || {
        echo "metal watchdog $1: exit status $st (want 0 after the watchdog's reset)" >&2
        return 1
    }
}

for suite in "$@"; do
    echo "=== $suite"
    case $suite in
    unit) unit; oci_tools ;;
    musl) unit --target x86_64-unknown-linux-musl ;;
    campaign) campaign ;;
    dist) dist ;;
    metal-selftest)
        run env -u CARGO_TARGET_DIR metal/build.sh
        (cd metal && run env -u CARGO_TARGET_DIR cargo test --release --locked -p k10-bootproto -p k10-drivers)
        . metal/qemu.sh
        qemu_selftest 1 pc
        qemu_selftest 4 pc
        qemu_selftest 4 pc-legacy
        qemu_selftest 4 q35
        qemu_selftest 2 microvm-v2
        # The NVMe and AHCI drivers (the self test writes, flushes and reads back every disk), then K10S-DATA on
        # virtio-blk, NVMe and AHCI through power cuts (metal/tools/storage_test.py).
        truncate -s 16M metal/out/nvme.img metal/out/sata.img
        qemu_selftest 2 q35 -drive file=metal/out/nvme.img,if=none,id=nv,format=raw -device nvme,drive=nv,serial=k10s \
            -drive file=metal/out/sata.img,if=none,id=sa,format=raw -device ide-hd,drive=sa,bus=ide.0
        grep -q "ok   nvme: 8 blocks written, flushed, read back" metal/out/selftest-q35-smp2-disks.log \
            && grep -q "ok   ahci: 8 blocks written, flushed, read back" metal/out/selftest-q35-smp2-disks.log \
            || { echo "metal selftest: the NVMe or AHCI disk was not exercised" >&2; exit 1; }
        run python3 metal/tools/storage_test.py --kernel metal/out/k10-metal.elf --bundle metal/out/bundle.k10b \
            --out metal/out --cycles 4
        qemu_selftest 2 e1000e
        qemu_selftest 2 igb
        metal/tools/wdat.py metal/out/wdat.bin
        qemu_watchdog esb pc 3 "i6300ESB" -device i6300esb
        qemu_watchdog tco q35 5 "Intel TCO (ICH/PCH, LPC)"
        qemu_watchdog wdat q35 5 "ACPI WDAT" -acpitable file=metal/out/wdat.bin
        run python3 metal/tools/platform_test.py --kernel metal/out/k10-metal.elf --bundle metal/out/bundle.k10b \
            --mkbundle metal/target/release/mkbundle --out metal/out
        ;;
    metal)
        run env -u CARGO_TARGET_DIR EMBED=1 metal/build.sh
        [[ -x bench/bin/loadgen ]] || run env NO_DOCKER=1 bench/workloads/build.sh
        run env -u CARGO_TARGET_DIR metal/test.sh
        run env -u CARGO_TARGET_DIR metal/cluster-test.sh
        ;;
    metal-vmm)
        run env -u CARGO_TARGET_DIR metal/build.sh
        vmm=$(metal/tools/get-vmms.sh "$PWD/target/vmm")
        # The guests' NIC: a tap owned by this user (creating it is the only step that needs root).
        if ! ip link show k10tap0 >/dev/null 2>&1; then
            run sudo ip tuntap add dev k10tap0 mode tap user "$(id -un)"
            run sudo ip addr add 172.30.0.1/24 dev k10tap0
            run sudo ip link set k10tap0 up
        fi
        run env VMM_DIR="$vmm" metal/vmm-test.sh
        ;;
    os) run os/test.sh ;;
    arm64) arm64 ;;
    i686) i686 ;;
    armv7) armv7 ;;
    os-arm64) os_arm64 ;;
    os-legacy) os_legacy ;;
    *)
        echo "ci-test.sh: unknown suite '$suite'" >&2
        exit 2
        ;;
    esac
done
