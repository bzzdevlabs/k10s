#!/usr/bin/env bash
# Boots k10-metal in QEMU (KVM, else TCG) with the test bundle and an HTTP host forward.
#   ./run.sh [bundle]           (default out/bundle.k10b)
# Environment:
#   PORT=8090        host port forwarded to guest :80 (slirp hostfwd)
#   BIND=127.0.0.1   host address the forward listens on (use BIND= for all interfaces, i.e. tcp::8090-:80)
#   MEM=64M          guest RAM
#   SMP=1            CPUs
#   PLATFORM=pc      machine and NIC (qemu.sh: pc, pc-legacy, pc-legacy-split, pc-modern, q35, microvm, microvm-v2); ACCEL=kvm|tcg
#   BIOS=            firmware: empty = SeaBIOS (QEMU default), or e.g. /usr/share/qemu/qboot.rom (faster boot)
#   DISK=FILE        a raw virtio-blk disk
#   APPEND="k10.verbose"   kernel command line (k10.verbose, k10.selftest, k10.exit_on_ready, k10.spin_us=N,
#                          k10.engine=jit|interp, k10.smp=N, k10.ip=A.B.C.D/P[,GW], k10.console=..., cluster mode:
#                          see README.md)
#   ADMIN_KEY=FILE         admin key (`k10s keygen -o FILE`), passed as fw_cfg opt/k10s/admin_key: admin requests
#                          signed with it are served (`k10s status --node 127.0.0.1:$PORT --admin-token-file FILE`)
#   CLUSTER_KEY=FILE, CLUSTER_NEXT_KEY=FILE   cluster keys, passed as fw_cfg opt/k10s/cluster_key / cluster_next_key
#   KERNEL=out/k10-metal.elf
# The guest powers off (POST /_k10s/power, or k10.exit_on_ready) through ACPI; QEMU then exits.
set -euo pipefail
cd "$(dirname "$0")"
. ./qemu.sh
BUNDLE="${1:-out/bundle.k10b}"
PORT="${PORT:-8090}"
BIND="${BIND-127.0.0.1}"
MEM="${MEM:-64M}"
SMP="${SMP:-1}"
PLATFORM="${PLATFORM:-pc}"
KERNEL="${KERNEL:-out/k10-metal.elf}"
APPEND="${APPEND-k10.verbose}"
[[ -f "$KERNEL" ]] || ./build.sh
# shellcheck disable=SC2207
args=(
    $(k10_accel) $(k10_machine "$PLATFORM") -m "$MEM" -smp "$SMP"
    -nodefaults -display none -serial stdio -no-reboot
    -netdev "user,id=n0,hostfwd=tcp:${BIND}:${PORT}-:80"
    $(k10_nic "$PLATFORM" n0)
    -kernel "$KERNEL" -append "$APPEND"
)
# shellcheck disable=SC2207
[[ -n "${DISK:-}" ]] && args+=($(k10_disk "$PLATFORM" "$DISK"))
[[ -n "${BIOS:-}" ]] && args+=(-bios "$BIOS")
[[ -n "${ADMIN_KEY:-}" ]] && args+=(-fw_cfg "name=opt/k10s/admin_key,file=$ADMIN_KEY")
[[ -n "${CLUSTER_KEY:-}" ]] && args+=(-fw_cfg "name=opt/k10s/cluster_key,file=$CLUSTER_KEY")
[[ -n "${CLUSTER_NEXT_KEY:-}" ]] && args+=(-fw_cfg "name=opt/k10s/cluster_next_key,file=$CLUSTER_NEXT_KEY")
[[ -n "$BUNDLE" && "$BUNDLE" != none ]] && args+=(-initrd "$BUNDLE")
exec qemu-system-x86_64 "${args[@]}"
