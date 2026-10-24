#!/usr/bin/env bash
# Builds the disk images of a release (docs/OS.md) into OUT (default: target/os/images); V is the k10s version.
#   os/images.sh [--arch x86_64|aarch64|i686|armv7] [OUT]
# x86_64 (both images boot on BIOS and UEFI, Tier 1 or the native kernel: /k10s/tier, docs/OS.md "Tiers"):
#   k10s-os-V-x86_64-vm.img.xz      QEMU/KVM and cloud guests: every driver built in, serial console
#   k10s-os-V-x86_64-metal.img.xz   PCs and servers: signed driver modules, NIC firmware, screen console
#   k10s-os-V-x86_64-{vm,metal}.efi the signed UKIs of the images (slot A): OS updates (`os` in k10s apply), PXE
#                                   boot or another UEFI loader
#   k10s-os-V-x86_64-native.elf     the native kernel (metal/) as both images carry it in /k10s/metal.elf, signed
#                                   (profile native); k10boot, the images' UEFI loader, verifies it
# aarch64 (one kernel, os/kbuild.sh arm64):
#   k10s-os-V-aarch64-vm.img.xz     QEMU virt and arm64 cloud guests (UEFI), serial console; its UKI
#                                   k10s-os-V-aarch64-vm.efi
#   k10s-os-V-aarch64-pi.img.xz     Raspberry Pi 3 B/B+, 4 and 5 (SD card, USB or NVMe); its signed slot image
#                                   k10s-os-V-aarch64-pi.fat (OS updates)
# i686 (os/kbuild.sh i386; best effort, interpreter only):
#   k10s-os-V-i686-metal.img.xz     old PCs (BIOS, MBR disk): signed driver modules, NIC firmware, screen console; its
#                                   signed UKI k10s-os-V-i686-metal.efi (OS updates)
# armv7 (os/kbuild.sh armv7; best effort, interpreter only):
#   k10s-os-V-armv7-vm.img.xz       QEMU virt and other 32-bit ARM UEFI machines, serial console; its UKI
#                                   k10s-os-V-armv7-vm.efi
#   SHA256SUMS.os-ARCH
# The uncompressed images stay in target/os/k10s-os-ARCH-PROFILE.img. K10S_BIN: the static musl k10s of the arch to
# ship (default: built here); K10S_HOST_BIN: the same release of k10s for this host, which signs (default: K10S_BIN);
# K10S_METAL_DIR (x86_64): a directory with the native kernel k10-metal.elf and k10boot.efi, such as the release's
# metal archive (default: built here by metal/build.sh).
# K10S_IMAGE_KEY: the signing key file (the release key in releases; default: a throwaway
# key made here, target/os/image-test.key, which nodes trust only when its public key is pinned with os_key). Fails
# when a compressed image exceeds its budget (x86_64: vm 5.5 MB, metal 40 MB; aarch64: vm 6 MB, pi 8 MB; i686: metal
# 40 MB; armv7: vm 6 MB).
set -euo pipefail
cd "$(dirname "$0")/.."
arch=x86_64
if [[ ${1:-} == --arch ]]; then
    arch=$2
    shift 2
fi
case $arch in
x86_64 | aarch64 | i686) triple=$arch-unknown-linux-musl ;;
armv7) triple=armv7-unknown-linux-musleabihf ;;
*) echo "images.sh: unknown arch $arch" >&2; exit 2 ;;
esac
out=${1:-target/os/images}
cache=${K10S_OS_CACHE:-target/os}
VM_CMDLINE="console=ttyS0,115200 quiet panic=5"
# Machines that boot from USB disks or SD cards, which appear after init has started: init waits up to 30 s for
# K10S-BOOT.
METAL_CMDLINE="console=ttyS0,115200 console=tty0 quiet panic=5 k10s.bootwait=30000"
ARM_VM_CMDLINE="console=ttyAMA0,115200 quiet panic=5"
# Pi 3 and 4: the mini UART on GPIO 14/15 (ttyS1); Pi 5: the debug UART connector (ttyAMA10).
PI_CMDLINE="console=ttyS1,115200 console=ttyAMA10,115200 quiet panic=5 k10s.bootwait=30000"
export SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH:-$(git log -1 --format=%ct)}
mkdir -p "$cache"
if [[ -z ${K10S_BIN:-} ]]; then
    cargo build --release --locked -p k10s --target "$triple"
    K10S_BIN=$cache/k10s-$arch
    strip --strip-debug -o "$K10S_BIN" "${CARGO_TARGET_DIR:-target}/$triple/release/k10s"
fi
host=${K10S_HOST_BIN:-$K10S_BIN}
V=$("$host" --version | awk '{print $2}')
V=${V%%+*}
key=${K10S_IMAGE_KEY:-$cache/image-test.key}
if [[ -z ${K10S_IMAGE_KEY:-} && ! -f $key ]]; then
    "$host" keygen -o "$key"
fi
echo "images.sh: signing with the key of $("$host" image pubkey "$key")" >&2
sign=(--sign "$key" --k10s "$host" --version "$V" --arch "$arch")
boot=$(os/bootfiles.sh "$arch")
mkdir -p "$out"
rm -f "$out"/k10s-os-*-"$arch"-* "$out/SHA256SUMS.os-$arch"
if [[ $arch == x86_64 ]]; then
    metal_dir=${K10S_METAL_DIR:-}
    if [[ -z $metal_dir ]]; then
        metal/build.sh >&2
        metal_dir=metal/out
    fi
    native=$out/k10s-os-$V-x86_64-native.elf
    "$host" image sign --key "$key" --arch x86_64 --profile native --version "$V" "$metal_dir/k10-metal.elf" "$native"
    tier2=(--k10boot "$metal_dir/k10boot.efi" --metal "$native")
    vm=$(os/kbuild.sh vm)
    metal=$(os/kbuild.sh metal)
    fw=$(os/firmware.sh)
    python3 os/mkinitramfs.py "$cache/initrd-vm.cpio.zst" --init "$K10S_BIN"
    python3 os/mkinitramfs.py "$cache/initrd-metal.cpio.zst" --init "$K10S_BIN" --kmods "$metal"/modroot/lib/modules/* --mod '*'
    python3 os/mkimage.py "$cache/k10s-os-x86_64-vm.img" --kernel "$vm/arch/x86/boot/bzImage" --initrd "$cache/initrd-vm.cpio.zst" \
        --cmdline "$VM_CMDLINE" --boot "$boot" "${sign[@]}" --profile vm --uki "$out/k10s-os-$V-x86_64-vm.efi" "${tier2[@]}"
    python3 os/mkimage.py "$cache/k10s-os-x86_64-metal.img" --kernel "$metal/arch/x86/boot/bzImage" \
        --initrd "$cache/initrd-metal.cpio.zst" --initrd "$fw" --cmdline "$METAL_CMDLINE" --boot "$boot" "${sign[@]}" --profile metal \
        --uki "$out/k10s-os-$V-x86_64-metal.efi" "${tier2[@]}"
    budgets=(vm:5500000 metal:40000000)
elif [[ $arch == i686 ]]; then
    kernel=$(os/kbuild.sh i386)
    python3 os/mkinitramfs.py "$cache/initrd-i686.cpio.zst" --init "$K10S_BIN" --kmods "$kernel"/modroot/lib/modules/* --mod '*'
    python3 os/mkimage.py "$cache/k10s-os-i686-metal.img" --arch i686 --kernel "$kernel/arch/x86/boot/bzImage" \
        --initrd "$cache/initrd-i686.cpio.zst" --initrd "$(os/firmware.sh)" --cmdline "$METAL_CMDLINE" --boot "$boot" "${sign[@]}" \
        --profile metal --uki "$out/k10s-os-$V-i686-metal.efi"
    budgets=(metal:40000000)
elif [[ $arch == armv7 ]]; then
    kernel=$(os/kbuild.sh armv7)
    python3 os/mkinitramfs.py "$cache/initrd-armv7.cpio.zst" --init "$K10S_BIN"
    python3 os/mkimage.py "$cache/k10s-os-armv7-vm.img" --arch armv7 --kernel "$kernel/arch/arm/boot/zImage" \
        --initrd "$cache/initrd-armv7.cpio.zst" --cmdline "$ARM_VM_CMDLINE" --boot "$boot" "${sign[@]}" --profile vm \
        --uki "$out/k10s-os-$V-armv7-vm.efi"
    budgets=(vm:6000000)
else
    kernel=$(os/kbuild.sh arm64)
    python3 os/mkinitramfs.py "$cache/initrd-aarch64.cpio.zst" --init "$K10S_BIN"
    python3 os/mkimage.py "$cache/k10s-os-aarch64-vm.img" --kernel "$kernel/arch/arm64/boot/Image" \
        --initrd "$cache/initrd-aarch64.cpio.zst" --cmdline "$ARM_VM_CMDLINE" --boot "$boot" "${sign[@]}" --profile vm \
        --uki "$out/k10s-os-$V-aarch64-vm.efi"
    python3 os/mkimage.py "$cache/k10s-os-aarch64-pi.img" --pi --kernel "$kernel/arch/arm64/boot/Image.gz" \
        --dtbs "$kernel/arch/arm64/boot/dts/broadcom" --initrd "$cache/initrd-aarch64.cpio.zst" --cmdline "$PI_CMDLINE" \
        --boot "$boot" "${sign[@]}" --profile pi --uki "$out/k10s-os-$V-aarch64-pi.fat"
    budgets=(vm:6000000 pi:8000000)
fi
for p in "${budgets[@]}"; do
    img=$out/k10s-os-$V-$arch-${p%:*}.img.xz
    xz -9 -T0 -c "$cache/k10s-os-$arch-${p%:*}.img" >"$img"
    size=$(stat -c %s "$img")
    echo "images.sh: $img: $size bytes" >&2
    if ((size >= ${p#*:})); then
        echo "images.sh: $img exceeds ${p#*:} bytes" >&2
        exit 1
    fi
done
(cd "$out" && sha256sum -- k10s-os-*-"$arch"-* >"SHA256SUMS.os-$arch")
