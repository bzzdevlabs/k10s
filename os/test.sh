#!/usr/bin/env bash
# PID 1 mode and the disk images under QEMU (docs/OS.md), no root. Builds the static k10s, the k10s kernels
# (os/kbuild.sh), the pinned Debian kernel (os/debian-kernel.sh), their initramfs images and the release disk images
# (os/images.sh), then runs os/test.py:
#   boot-debian, boot-k10s  kernel + initramfs: boot → apply → serve → power button → boot again → state kept
#   dhcp                    renew, rebind, NAK → a new address (k10s kernel)
#   image-bios, image-uefi  the vm image on SeaBIOS and OVMF: K10S-DATA grown to the disk, apply, serve, state kept
#   image-metal             the metal image (console on serial instead of the screen) on OVMF q35: NVMe, signed
#                           e1000e, Intel TCO watchdog
#   unsigned                the metal kernel refuses a module whose signature is stripped
#   boottime                the vm image on BIOS: QEMU start → first HTTP 200, median of 5 below 1 s
#   repro                   the vm image built twice is bit-identical
#   update-bios, update-uefi  A/B update with a power cut at every step, an image that panics, one that never
#                           commits (single node; the update images are signed with a test key pinned by os_key)
#   rollout                 20 BIOS VMs: a bad image stops after the first wave, a good one rolls out in waves
# usage: os/test.sh [TEST...]   (default: all). K10S_BIN: the static musl k10s to test (default: built here).
# Needs the musl target, qemu-system-x86_64 (KVM when /dev/kvm is usable), OVMF, python3, zstd, xz, mkfs.fat, mtools,
# mkfs.ext4, sfdisk, objcopy, dpkg-deb, depmod and the kernel build tools of os/kbuild.sh.
set -euo pipefail
cd "$(dirname "$0")/.."
tests=("$@")
[[ ${#tests[@]} -gt 0 ]] || tests=(boot-debian boot-k10s dhcp image-bios image-uefi image-metal unsigned boottime repro update-bios update-uefi rollout)
out=${K10S_OS_CACHE:-target/os}
mkdir -p "$out"
export SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH:-$(git log -1 --format=%ct)}

k10s=${K10S_BIN:-}
if [[ -z $k10s ]]; then
    cargo build --release --locked -p k10s --target x86_64-unknown-linux-musl
    k10s=$out/k10s
    strip --strip-debug -o "$k10s" "${CARGO_TARGET_DIR:-target}/x86_64-unknown-linux-musl/release/k10s"
fi
has() { for t in "$@"; do [[ " ${tests[*]} " == *" $t "* ]] && return 0; done; return 1; }

if has boot-k10s dhcp repro; then
    bz=$(os/kbuild.sh vm)/arch/x86/boot/bzImage
    python3 os/mkinitramfs.py "$out/k10s-vm.cpio.zst" --init "$k10s"
fi
if has boot-debian; then
    deb=$(os/debian-kernel.sh)
    python3 os/mkinitramfs.py "$out/debian-vm.cpio.zst" --init "$k10s" --kmods "$deb"/lib/modules/* \
        --mod 'drivers/net/virtio_net.ko*' --mod 'drivers/block/virtio_blk.ko*' --mod 'drivers/char/hw_random/virtio-rng.ko*' \
        --mod 'fs/ext4/ext4.ko*' --mod 'crypto/crc32c*' --mod 'arch/x86/crypto/crc32c*' --mod 'fs/fat/vfat.ko*' \
        --mod 'fs/nls/nls_cp437.ko*' --mod 'fs/nls/nls_iso8859-1.ko*' --mod 'drivers/acpi/button.ko*' \
        --mod 'drivers/input/evdev.ko*' --mod 'drivers/watchdog/i6300esb.ko*'
fi
if has image-bios image-uefi image-metal unsigned boottime update-bios update-uefi rollout; then
    K10S_BIN=$k10s os/images.sh "$out/images"
    metal=$(os/kbuild.sh metal)
    kmods=$(echo "$metal"/modroot/lib/modules/*)
fi
if has update-bios update-uefi rollout; then
    image_key=$out/image-test.key
    [[ -f $image_key ]] || "$k10s" keygen -o "$image_key"
    upd=(--image "$out/k10s-os-x86_64-vm.img" --uki "$(echo "$out"/images/k10s-os-*-x86_64-vm.efi)" --image-key "$image_key"
        --boot "$(os/bootfiles.sh)" --k10s "$k10s" --examples examples)
fi
if has image-metal; then
    python3 os/mkimage.py "$out/metal-serial.img" --kernel "$metal/arch/x86/boot/bzImage" --initrd "$out/initrd-metal.cpio.zst" \
        --initrd "$(os/firmware.sh)" --cmdline "console=tty0 console=ttyS0,115200 quiet panic=5" --boot "$(os/bootfiles.sh)"
fi

for t in "${tests[@]}"; do
    echo "=== os/test.sh $t" >&2
    work=$out/test-$t
    rm -rf "$work" && mkdir -p "$work"
    case $t in
    boot-debian) python3 os/test.py boot --kernel "$deb/vmlinuz" --initrd "$out/debian-vm.cpio.zst" --k10s "$k10s" --examples examples --work "$work" ;;
    boot-k10s) python3 os/test.py boot --kernel "$bz" --initrd "$out/k10s-vm.cpio.zst" --k10s "$k10s" --examples examples --work "$work" ;;
    dhcp) python3 os/test.py dhcp --kernel "$bz" --initrd "$out/k10s-vm.cpio.zst" --work "$work" ;;
    image-bios) python3 os/test.py image --bios --image "$out/k10s-os-x86_64-vm.img" --k10s "$k10s" --examples examples --work "$work" ;;
    image-uefi) python3 os/test.py image --uefi --image "$out/k10s-os-x86_64-vm.img" --k10s "$k10s" --examples examples --work "$work" ;;
    image-metal) python3 os/test.py image --uefi --metal --image "$out/metal-serial.img" --k10s "$k10s" --examples examples --work "$work" ;;
    unsigned) python3 os/test.py unsigned --kernel "$metal/arch/x86/boot/bzImage" --kmods "$kmods" --k10s "$k10s" --work "$work" ;;
    boottime) python3 os/test.py boottime --image "$out/k10s-os-x86_64-vm.img" --work "$work" ;;
    update-bios) python3 os/test.py update --bios "${upd[@]}" --work "$work" ;;
    update-uefi) python3 os/test.py update --uefi "${upd[@]}" --work "$work" ;;
    rollout) python3 os/test.py rollout "${upd[@]}" --work "$work" ;;
    repro)
        boot=$(os/bootfiles.sh)
        for i in 1 2; do
            python3 os/mkimage.py "$work/vm$i.img" --kernel "$bz" --initrd "$out/k10s-vm.cpio.zst" --cmdline "console=ttyS0 panic=5" \
                --boot "$boot" --uki "$work/vm$i.efi"
        done
        cmp "$work/vm1.img" "$work/vm2.img" && cmp "$work/vm1.efi" "$work/vm2.efi"
        echo "test.sh: repro: ok" >&2
        ;;
    *) echo "os/test.sh: unknown test $t" >&2; exit 2 ;;
    esac
done
