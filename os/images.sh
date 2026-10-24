#!/usr/bin/env bash
# Builds the x86-64 disk images of a release (docs/OS.md) into OUT (default: target/os/images); V is the k10s version:
#   k10s-os-V-x86_64-vm.img.xz      QEMU/KVM and cloud guests: every driver built in, serial console
#   k10s-os-V-x86_64-metal.img.xz   PCs and servers: signed driver modules, NIC firmware, screen console
#   k10s-os-V-x86_64-{vm,metal}.efi the signed UKIs of the images (slot A): OS updates (`os` in k10s apply), PXE
#                                   boot or another UEFI loader
#   SHA256SUMS.os
# Both images boot on BIOS and UEFI. The uncompressed images stay in target/os/k10s-os-x86_64-{vm,metal}.img.
# K10S_BIN: the static musl k10s to ship (default: built here). K10S_IMAGE_KEY: the signing key file (the release
# key in releases; default: a throwaway key made here, target/os/image-test.key, which nodes trust only when its
# public key is pinned with os_key). Fails when a compressed image exceeds its budget (vm 5 MB, metal 40 MB).
set -euo pipefail
cd "$(dirname "$0")/.."
out=${1:-target/os/images}
cache=${K10S_OS_CACHE:-target/os}
VM_CMDLINE="console=ttyS0,115200 quiet panic=5"
METAL_CMDLINE="console=ttyS0,115200 console=tty0 quiet panic=5"
export SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH:-$(git log -1 --format=%ct)}
mkdir -p "$cache"
if [[ -z ${K10S_BIN:-} ]]; then
    cargo build --release --locked -p k10s --target x86_64-unknown-linux-musl
    K10S_BIN=$cache/k10s
    strip --strip-debug -o "$K10S_BIN" "${CARGO_TARGET_DIR:-target}/x86_64-unknown-linux-musl/release/k10s"
fi
V=$("$K10S_BIN" --version | awk '{print $2}')
V=${V%%+*}
key=${K10S_IMAGE_KEY:-$cache/image-test.key}
if [[ -z ${K10S_IMAGE_KEY:-} && ! -f $key ]]; then
    "$K10S_BIN" keygen -o "$key"
fi
echo "images.sh: signing with the key of $("$K10S_BIN" image pubkey "$key")" >&2
sign=(--sign "$key" --k10s "$K10S_BIN" --version "$V")
vm=$(os/kbuild.sh vm)
metal=$(os/kbuild.sh metal)
boot=$(os/bootfiles.sh)
fw=$(os/firmware.sh)
python3 os/mkinitramfs.py "$cache/initrd-vm.cpio.zst" --init "$K10S_BIN"
python3 os/mkinitramfs.py "$cache/initrd-metal.cpio.zst" --init "$K10S_BIN" --kmods "$metal"/modroot/lib/modules/* --mod '*'
mkdir -p "$out"
rm -f "$out"/k10s-os-* "$out/SHA256SUMS.os"
python3 os/mkimage.py "$cache/k10s-os-x86_64-vm.img" --kernel "$vm/arch/x86/boot/bzImage" --initrd "$cache/initrd-vm.cpio.zst" \
    --cmdline "$VM_CMDLINE" --boot "$boot" "${sign[@]}" --profile vm --uki "$out/k10s-os-$V-x86_64-vm.efi"
python3 os/mkimage.py "$cache/k10s-os-x86_64-metal.img" --kernel "$metal/arch/x86/boot/bzImage" \
    --initrd "$cache/initrd-metal.cpio.zst" --initrd "$fw" --cmdline "$METAL_CMDLINE" --boot "$boot" "${sign[@]}" --profile metal \
    --uki "$out/k10s-os-$V-x86_64-metal.efi"
for p in vm:5000000 metal:40000000; do
    img=$out/k10s-os-$V-x86_64-${p%:*}.img.xz
    xz -9 -T0 -c "$cache/k10s-os-x86_64-${p%:*}.img" >"$img"
    size=$(stat -c %s "$img")
    echo "images.sh: $img: $size bytes" >&2
    if ((size >= ${p#*:})); then
        echo "images.sh: $img exceeds ${p#*:} bytes" >&2
        exit 1
    fi
done
(cd "$out" && sha256sum -- k10s-os-* >SHA256SUMS.os)
