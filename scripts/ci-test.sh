#!/usr/bin/env bash
# Test suites of CI and the release gate, same locally. usage: ci-test.sh SUITE...
#   unit            all crates, release mode, JIT then interpreter (node, CLI); no #[ignore]d tests, no campaigns
#   musl            the same on x86_64-unknown-linux-musl (the static release binary)
#   campaign        chaos_short + soak_short_* on both backends (minutes long, noise-sensitive)
#   metal-selftest  kernel self tests (k10.selftest) under QEMU/KVM on 1 and 4 CPUs
#   metal           metal/test.sh end-to-end (TEST_PORT, default 18090)
# env: CARGO_TEST_ARGS (extra cargo test args), K10S_VERSION_FULL (metal-selftest checks the banner)
set -euo pipefail
cd "$(dirname "$0")/.."

CRATES=(-p k10-ir -p k10-mesh -p k10-lang -p k10-jit -p k10-node -p k10s)
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

campaign() {
    for b in jit interp; do
        run env K10S_BACKEND=$b cargo test --release --locked "${EXTRA[@]}" -p k10s --test chaos --test soak
    done
}

qemu_selftest() { # SMP
    local log=metal/out/selftest-smp$1.log st=0
    rm -f "$log"
    timeout 120 qemu-system-x86_64 -M pc -enable-kvm -cpu host -m 64M -smp "$1" -nodefaults -display none -no-reboot \
        -device isa-debug-exit,iobase=0xf4,iosize=0x04 \
        -netdev user,id=n0 -device virtio-net-pci,disable-modern=on,netdev=n0,romfile= \
        -kernel metal/out/k10-metal.elf -serial "file:$log" -append "k10.selftest" || st=$?
    sed 's/^/    /' "$log"
    if [[ -n ${K10S_VERSION_FULL:-} ]] && ! grep -qF "k10-metal $K10S_VERSION_FULL:" "$log"; then
        echo "metal selftest: the boot banner does not carry version $K10S_VERSION_FULL" >&2
        return 1
    fi
    # isa-debug-exit: 33 = (0x10 << 1) | 1 = success.
    [[ $st == 33 ]] || {
        echo "metal selftest (smp $1): exit status $st, want 33" >&2
        return 1
    }
}

for suite in "$@"; do
    echo "=== $suite"
    case $suite in
    unit) unit ;;
    musl) unit --target x86_64-unknown-linux-musl ;;
    campaign) campaign ;;
    metal-selftest)
        run env -u CARGO_TARGET_DIR metal/build.sh
        qemu_selftest 1
        qemu_selftest 4
        ;;
    metal)
        run env -u CARGO_TARGET_DIR EMBED=1 metal/build.sh
        [[ -x bench/bin/loadgen ]] || run env NO_DOCKER=1 bench/workloads/build.sh
        run env -u CARGO_TARGET_DIR metal/test.sh
        ;;
    *)
        echo "ci-test.sh: unknown suite '$suite'" >&2
        exit 2
        ;;
    esac
done
