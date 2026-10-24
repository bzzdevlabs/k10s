#!/usr/bin/env bash
# Test suites of CI and the release gate, same locally. usage: ci-test.sh SUITE...
#   unit            all crates, release mode, JIT then interpreter (node, CLI); no #[ignore]d tests, no campaigns
#   musl            the same on x86_64-unknown-linux-musl (the static release binary)
#   arm64           on an arm64 host: all crates but the x86-64 JIT, on the interpreter (the native backend there),
#                   for the host's gnu target and aarch64-unknown-linux-musl
#   campaign        chaos_short + soak_short_* on both backends (minutes long, noise-sensitive)
#   metal-selftest  kernel self tests (k10.selftest) under QEMU/KVM on 1 and 4 CPUs
#   metal           metal/test.sh end-to-end (TEST_PORT, default 18090)
#   dist            the k10s integration tests against the static k10s of dist/ (K10S_BIN), both backends
#   os              PID 1 mode under QEMU (os/test.sh): boot, apply, serve, power off on the Debian and k10s
#                   kernels; DHCP renew, rebind and address change; the x86-64 images, A/B updates, rollout
#   os-arm64        os/test.sh --arch aarch64: the arm64 kernel, the aarch64-vm and Pi images, their A/B updates, a Pi
#                   in a cluster with an x86-64 node (K10S_BIN, K10S_HOST_BIN as os/test.sh; on an x86-64 host both
#                   are built here, the aarch64 one linked by rust-lld)
# env: CARGO_TEST_ARGS (extra cargo test args), K10S_VERSION_FULL (metal-selftest checks the banner)
set -euo pipefail
cd "$(dirname "$0")/.."

CRATES=(-p k10-nospec -p k10-ir -p k10-host -p k10-mesh -p k10-lang -p k10-jit -p k10-node -p k10s)
ARM_CRATES=(-p k10-nospec -p k10-ir -p k10-host -p k10-mesh -p k10-lang -p k10-node -p k10s)
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

arm64() {
    [[ $(uname -m) == aarch64 ]] || { echo "ci-test.sh: arm64 runs on an arm64 host" >&2; return 1; }
    run cargo test --release --locked "${EXTRA[@]}" "${ARM_CRATES[@]}" -- "${SKIP[@]}"
    run cargo test --release --locked --target aarch64-unknown-linux-musl "${EXTRA[@]}" "${ARM_CRATES[@]}" -- "${SKIP[@]}"
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
    dist) dist ;;
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
    os) run os/test.sh ;;
    arm64) arm64 ;;
    os-arm64) os_arm64 ;;
    *)
        echo "ci-test.sh: unknown suite '$suite'" >&2
        exit 2
        ;;
    esac
done
