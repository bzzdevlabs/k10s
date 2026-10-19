#!/usr/bin/env bash
# Boots k10-metal in QEMU/KVM with the test bundle and an HTTP host forward.
#   ./run.sh [bundle]           (default out/bundle.k10b)
# Environment:
#   PORT=8090        host port forwarded to guest :80 (slirp hostfwd)
#   BIND=127.0.0.1   host address the forward listens on (use BIND= for all interfaces, i.e. tcp::8090-:80)
#   MEM=64M          guest RAM
#   SMP=1            CPUs
#   BIOS=            firmware: empty = SeaBIOS (QEMU default), or e.g. /usr/share/qemu/qboot.rom (faster boot)
#   APPEND="k10.verbose"   kernel command line (k10.verbose, k10.selftest, k10.exit_on_ready, k10.spin_us=N,
#                          k10.engine=jit|interp, k10.smp=N, cluster mode: see README.md)
#   KERNEL=out/k10-metal.elf
set -euo pipefail
cd "$(dirname "$0")"
BUNDLE="${1:-out/bundle.k10b}"
PORT="${PORT:-8090}"
BIND="${BIND-127.0.0.1}"
MEM="${MEM:-64M}"
SMP="${SMP:-1}"
KERNEL="${KERNEL:-out/k10-metal.elf}"
APPEND="${APPEND-k10.verbose}"
[[ -f "$KERNEL" ]] || ./build.sh
args=(
    -M pc -enable-kvm -cpu host -m "$MEM" -smp "$SMP"
    -nodefaults -display none -serial stdio -no-reboot
    -device isa-debug-exit,iobase=0xf4,iosize=0x04
    -netdev "user,id=n0,hostfwd=tcp:${BIND}:${PORT}-:80"
    -device virtio-net-pci,disable-modern=on,netdev=n0,romfile=
    -kernel "$KERNEL" -append "$APPEND"
)
[[ -n "${BIOS:-}" ]] && args+=(-bios "$BIOS")
[[ -n "$BUNDLE" && "$BUNDLE" != none ]] && args+=(-initrd "$BUNDLE")
exec qemu-system-x86_64 "${args[@]}"
