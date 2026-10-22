#!/usr/bin/env bash
# PID 1 mode under QEMU (docs/OS.md), no root: builds the static k10s, the k10s kernel (os/kbuild.sh), the pinned
# Debian kernel (os/debian-kernel.sh) and their initramfs images, then runs os/test.py:
#   boot on both kernels (boot → apply → serve → power button → boot again → state kept → power button),
#   dhcp on the k10s kernel (renew, rebind, NAK → new address).
# usage: os/test.sh [boot-debian] [boot-k10s] [dhcp]   (default: all three)
# Needs the musl target, qemu-system-x86_64 (KVM when /dev/kvm is usable), python3, zstd, mkfs.fat, mtools,
# mkfs.ext4, sfdisk, dpkg-deb, depmod and the kernel build tools of os/kbuild.sh.
set -euo pipefail
cd "$(dirname "$0")/.."
tests=("$@")
[[ ${#tests[@]} -gt 0 ]] || tests=(boot-debian boot-k10s dhcp)
out=${K10S_OS_CACHE:-target/os}
mkdir -p "$out"

cargo build --release --locked -p k10s --target x86_64-unknown-linux-musl
k10s=$out/k10s
strip --strip-debug -o "$k10s" "${CARGO_TARGET_DIR:-target}/x86_64-unknown-linux-musl/release/k10s"

if [[ " ${tests[*]} " == *" boot-k10s "* || " ${tests[*]} " == *" dhcp "* ]]; then
    bz=$(os/kbuild.sh)
    python3 os/mkinitramfs.py "$out/k10s-vm.cpio.zst" --init "$k10s"
fi
if [[ " ${tests[*]} " == *" boot-debian "* ]]; then
    deb=$(os/debian-kernel.sh)
    python3 os/mkinitramfs.py "$out/debian-vm.cpio.zst" --init "$k10s" --kmods "$deb"/lib/modules/* \
        --mod 'drivers/net/virtio_net.ko*' --mod 'drivers/block/virtio_blk.ko*' --mod 'drivers/char/hw_random/virtio-rng.ko*' \
        --mod 'fs/ext4/ext4.ko*' --mod 'crypto/crc32c*' --mod 'arch/x86/crypto/crc32c*' --mod 'fs/fat/vfat.ko*' \
        --mod 'fs/nls/nls_cp437.ko*' --mod 'fs/nls/nls_iso8859-1.ko*' --mod 'drivers/acpi/button.ko*' \
        --mod 'drivers/input/evdev.ko*' --mod 'drivers/watchdog/i6300esb.ko*'
fi

for t in "${tests[@]}"; do
    echo "=== os/test.sh $t" >&2
    work=$out/test-$t
    rm -rf "$work" && mkdir -p "$work"
    case $t in
    boot-debian) python3 os/test.py boot --kernel "$deb/vmlinuz" --initrd "$out/debian-vm.cpio.zst" --k10s "$k10s" --examples examples --work "$work" ;;
    boot-k10s) python3 os/test.py boot --kernel "$bz" --initrd "$out/k10s-vm.cpio.zst" --k10s "$k10s" --examples examples --work "$work" ;;
    dhcp) python3 os/test.py dhcp --kernel "$bz" --initrd "$out/k10s-vm.cpio.zst" --work "$work" ;;
    *) echo "os/test.sh: unknown test $t" >&2; exit 2 ;;
    esac
done
