#!/usr/bin/env bash
# Builds the disk images of a release (docs/OS.md) into OUT (default: target/os/images); V is the k10s version.
#   os/images.sh [--arch x86_64|aarch64] [OUT]
# x86_64 (both images boot on BIOS and UEFI):
#   k10s-os-V-x86_64-vm.img.xz      QEMU/KVM and cloud guests: every driver built in, serial console
#   k10s-os-V-x86_64-metal.img.xz   PCs and servers: signed driver modules, NIC firmware, screen console
#   k10s-os-V-x86_64-{vm,metal}.efi the signed UKIs of the images (slot A): OS updates (`os` in k10s apply), PXE
#                                   boot or another UEFI loader
# aarch64 (one kernel, os/kbuild.sh arm64):
#   k10s-os-V-aarch64-vm.img.xz     QEMU virt and arm64 cloud guests (UEFI), serial console; its UKI
#                                   k10s-os-V-aarch64-vm.efi
#   k10s-os-V-aarch64-pi.img.xz     Raspberry Pi 4 and 5 (SD card, USB or NVMe); its signed slot image
#                                   k10s-os-V-aarch64-pi.fat (OS updates)
#   SHA256SUMS.os-ARCH
# The uncompressed images stay in target/os/k10s-os-ARCH-PROFILE.img. K10S_BIN: the static musl k10s of the arch to
# ship (default: built here); K10S_HOST_BIN: the same release of k10s for this host, which signs (default: K10S_BIN).
# K10S_IMAGE_KEY: the signing key file (the release key in releases; default: a throwaway
# key made here, target/os/image-test.key, which nodes trust only when its public key is pinned with os_key). Fails
# when a compressed image exceeds its budget (x86_64: vm 5 MB, metal 40 MB; aarch64: vm 6 MB, pi 8 MB).
set -euo pipefail
cd "$(dirname "$0")/.."
arch=x86_64
if [[ ${1:-} == --arch ]]; then
    arch=$2
    shift 2
fi
[[ $arch == x86_64 || $arch == aarch64 ]] || { echo "images.sh: unknown arch $arch" >&2; exit 2; }
out=${1:-target/os/images}
cache=${K10S_OS_CACHE:-target/os}
VM_CMDLINE="console=ttyS0,115200 quiet panic=5"
METAL_CMDLINE="console=ttyS0,115200 console=tty0 quiet panic=5"
ARM_VM_CMDLINE="console=ttyAMA0,115200 quiet panic=5"
# Pi 4: the mini UART on GPIO 14/15 (ttyS1); Pi 5: the debug UART connector (ttyAMA10).
PI_CMDLINE="console=ttyS1,115200 console=ttyAMA10,115200 quiet panic=5"
export SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH:-$(git log -1 --format=%ct)}
mkdir -p "$cache"
if [[ -z ${K10S_BIN:-} ]]; then
    cargo build --release --locked -p k10s --target "$arch-unknown-linux-musl"
    K10S_BIN=$cache/k10s-$arch
    strip --strip-debug -o "$K10S_BIN" "${CARGO_TARGET_DIR:-target}/$arch-unknown-linux-musl/release/k10s"
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
    vm=$(os/kbuild.sh vm)
    metal=$(os/kbuild.sh metal)
    fw=$(os/firmware.sh)
    python3 os/mkinitramfs.py "$cache/initrd-vm.cpio.zst" --init "$K10S_BIN"
    python3 os/mkinitramfs.py "$cache/initrd-metal.cpio.zst" --init "$K10S_BIN" --kmods "$metal"/modroot/lib/modules/* --mod '*'
    python3 os/mkimage.py "$cache/k10s-os-x86_64-vm.img" --kernel "$vm/arch/x86/boot/bzImage" --initrd "$cache/initrd-vm.cpio.zst" \
        --cmdline "$VM_CMDLINE" --boot "$boot" "${sign[@]}" --profile vm --uki "$out/k10s-os-$V-x86_64-vm.efi"
    python3 os/mkimage.py "$cache/k10s-os-x86_64-metal.img" --kernel "$metal/arch/x86/boot/bzImage" \
        --initrd "$cache/initrd-metal.cpio.zst" --initrd "$fw" --cmdline "$METAL_CMDLINE" --boot "$boot" "${sign[@]}" --profile metal \
        --uki "$out/k10s-os-$V-x86_64-metal.efi"
    budgets=(vm:5000000 metal:40000000)
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
