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
#   secureboot              the vm image on OVMF with Secure Boot on and Microsoft's keys (docs/OS.md "Secure Boot"):
#                           refused as released; serves with Secure Boot off, with a MOK under Debian's shim and with
#                           own keys; under own keys a re-signed update commits, an update not signed for Secure Boot is
#                           refused by the firmware and rolled back
# --arch aarch64 (the arm64 kernel and images of os/images.sh --arch aarch64, on qemu-system-aarch64 virt, TCG unless
# the host is arm64 with KVM):
#   boot-arm64              kernel + initramfs (devicetree): boot → apply → serve → power key → boot again → state kept
#   image-arm64             the aarch64-vm image on AAVMF (UEFI, ACPI), as image-uefi
#   update-arm64            A/B update of the aarch64-vm image (systemd-boot), as update-uefi
#   pi                      the Pi image behind a model of the Pi firmware (autoboot.txt, tryboot): K10S-BOOT and
#                           K10S-DATA by label on MBR, A/B update with a power cut at every step, a panicking image
#                           (3 tries), a never-committing one
#   pi3                     the same on QEMU raspi3b (a Raspberry Pi 3: its devicetree and SDHOST, no RTC), QEMU's USB
#                           CDC NIC on the dwc2 bus standing in for the LAN9514; each run ends in a power cut
#   mixed                   a Pi joins a cluster whose first member is an x86-64 k10s on the host (K10S_HOST_X86)
#   secureboot-arm64        the aarch64-vm image on AAVMF with Secure Boot on and Microsoft's keys, as secureboot
# --arch i686 (the i386 kernel and the i686-metal image, on qemu-system-i386 with an Atom N270, KVM on x86-64 hosts):
#   boot-i686               kernel + initramfs (virtio, signed modules): boot → apply → serve → power button → again
#   image-i686              the image (console on serial for the test) on SeaBIOS: MBR, IDE disk, RTL8139 (8139too):
#                           K10S-DATA grown, apply, serve, state kept
#   update-i686             A/B update through GRUB (BIOS only, no systemd-boot) on IDE and e1000, as update-bios
# --arch armv7 (the armv7 kernel and the armv7-vm image, on qemu-system-arm virt, Cortex-A15, TCG):
#   boot-armv7              kernel + initramfs (devicetree): boot → apply → serve → power key → again
#   image-armv7             the armv7-vm image on AAVMF32 (UEFI), as image-uefi
#   update-armv7            A/B update of the armv7-vm image (systemd-boot), as update-uefi
# usage: os/test.sh [--arch x86_64|aarch64|i686|armv7] [TEST...]   (default: all of the arch). K10S_BIN: the static
# musl k10s of the arch to test (default: built here); K10S_HOST_BIN: a k10s the host runs (keys, apply, signing;
# default K10S_BIN when it is the host's arch); K10S_HOST_X86: the x86-64 k10s of `mixed` (default K10S_HOST_BIN).
# Needs the musl target, qemu-system-x86_64, -i386, -aarch64 or -arm (KVM when /dev/kvm is usable), OVMF, AAVMF or
# AAVMF32, python3, zstd, xz, mkfs.fat, mtools, mkfs.ext4, sfdisk, dpkg-deb, depmod, dtc (aarch64) and the kernel
# build tools of os/kbuild.sh (a cross compiler for kernels of another architecture); secureboot also sbsign,
# virt-fw-vars, openssl and the firmware builds with Microsoft's keys (OVMF_*_4M.ms.fd, AAVMF_*.ms.fd).
set -euo pipefail
cd "$(dirname "$0")/.."
arch=x86_64
if [[ ${1:-} == --arch ]]; then
    arch=$2
    shift 2
fi
tests=("$@")
if [[ ${#tests[@]} == 0 ]]; then
    case $arch in
    x86_64) tests=(boot-debian boot-k10s dhcp image-bios image-uefi image-metal unsigned boottime repro update-bios update-uefi rollout secureboot) ;;
    aarch64) tests=(boot-arm64 image-arm64 update-arm64 pi pi3 mixed secureboot-arm64) ;;
    i686) tests=(boot-i686 image-i686 update-i686) ;;
    armv7) tests=(boot-armv7 image-armv7 update-armv7) ;;
    *) echo "os/test.sh: unknown arch $arch" >&2; exit 2 ;;
    esac
fi
out=${K10S_OS_CACHE:-target/os}
mkdir -p "$out"
export SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH:-$(git log -1 --format=%ct)}

k10s=${K10S_BIN:-}
if [[ -z $k10s ]]; then
    triple=$arch-unknown-linux-musl
    [[ $arch != armv7 ]] || triple=armv7-unknown-linux-musleabihf
    cargo build --release --locked -p k10s --target "$triple"
    k10s=$out/k10s-$arch
    strip --strip-debug -o "$k10s" "${CARGO_TARGET_DIR:-target}/$triple/release/k10s"
fi
host=${K10S_HOST_BIN:-}
if [[ -z $host ]]; then
    [[ $(uname -m) == "$arch" ]] || { echo "os/test.sh: set K10S_HOST_BIN to a k10s for this $(uname -m) host" >&2; exit 2; }
    host=$k10s
fi
has() { for t in "$@"; do [[ " ${tests[*]} " == *" $t "* ]] && return 0; done; return 1; }
if has boot-arm64 image-arm64 update-arm64 pi pi3 mixed secureboot-arm64; then
    [[ $arch == aarch64 ]] || { echo "os/test.sh: arm64 tests need --arch aarch64" >&2; exit 2; }
    K10S_BIN=$k10s K10S_HOST_BIN=$host os/images.sh --arch aarch64 "$out/images"
    arm=$(os/kbuild.sh arm64)
    image_key=$out/image-test.key
    [[ -f $image_key ]] || "$host" keygen -o "$image_key"
    arm_upd=(--arch aarch64 --image-key "$image_key" --boot "$(os/bootfiles.sh aarch64)" --k10s "$host" --examples examples)
fi

if has boot-i686 image-i686 update-i686; then
    [[ $arch == i686 ]] || { echo "os/test.sh: i686 tests need --arch i686" >&2; exit 2; }
    K10S_BIN=$k10s K10S_HOST_BIN=$host os/images.sh --arch i686 "$out/images"
    i386=$(os/kbuild.sh i386)
    image_key=$out/image-test.key
    [[ -f $image_key ]] || "$host" keygen -o "$image_key"
    # The image with the console on serial last (the release image ends with the screen), slot A signed by the test key.
    python3 os/mkimage.py "$out/i686-serial.img" --arch i686 --kernel "$i386/arch/x86/boot/bzImage" \
        --initrd "$out/initrd-i686.cpio.zst" --initrd "$(os/firmware.sh)" --cmdline "console=tty0 console=ttyS0,115200 quiet panic=5 k10s.bootwait=30000" \
        --boot "$(os/bootfiles.sh i686)" --sign "$image_key" --k10s "$host" --profile metal --uki "$out/i686-serial.efi"
fi
if has boot-armv7 image-armv7 update-armv7; then
    [[ $arch == armv7 ]] || { echo "os/test.sh: armv7 tests need --arch armv7" >&2; exit 2; }
    K10S_BIN=$k10s K10S_HOST_BIN=$host os/images.sh --arch armv7 "$out/images"
    armv7=$(os/kbuild.sh armv7)
    image_key=$out/image-test.key
    [[ -f $image_key ]] || "$host" keygen -o "$image_key"
fi
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
if has image-bios image-uefi image-metal unsigned boottime update-bios update-uefi rollout secureboot; then
    K10S_BIN=$k10s os/images.sh --arch x86_64 "$out/images"
    metal=$(os/kbuild.sh metal)
    kmods=$(echo "$metal"/modroot/lib/modules/*)
fi
if has update-bios update-uefi rollout; then
    image_key=$out/image-test.key
    [[ -f $image_key ]] || "$k10s" keygen -o "$image_key"
    upd=(--image "$out/k10s-os-x86_64-vm.img" --uki "$(echo "$out"/images/k10s-os-*-x86_64-vm.efi)" --image-key "$image_key"
        --boot "$(os/bootfiles.sh)" --k10s "$k10s" --examples examples)
fi
if has secureboot secureboot-arm64; then
    image_key=$out/image-test.key
    [[ -f $image_key ]] || "$host" keygen -o "$image_key"
    sb=(--image "$out/k10s-os-$arch-vm.img" --uki "$(echo "$out"/images/k10s-os-*-"$arch"-vm.efi)" --image-key "$image_key"
        --shim "$(os/bootfiles.sh "shim-$arch")" --k10s "$host" --examples examples)
fi
if has image-metal; then
    python3 os/mkimage.py "$out/metal-serial.img" --kernel "$metal/arch/x86/boot/bzImage" --initrd "$out/initrd-metal.cpio.zst" \
        --initrd "$(os/firmware.sh)" --cmdline "console=tty0 console=ttyS0,115200 quiet panic=5 k10s.bootwait=30000" --boot "$(os/bootfiles.sh)"
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
    boot-arm64)
        python3 os/test.py boot --arch aarch64 --kernel "$arm/arch/arm64/boot/Image" --initrd "$out/initrd-aarch64.cpio.zst" \
            --k10s "$host" --examples examples --work "$work"
        ;;
    image-arm64) python3 os/test.py image --arch aarch64 --uefi --image "$out/k10s-os-aarch64-vm.img" --k10s "$host" --examples examples --work "$work" ;;
    update-arm64)
        python3 os/test.py update --uefi "${arm_upd[@]}" --image "$out/k10s-os-aarch64-vm.img" \
            --uki "$(echo "$out"/images/k10s-os-*-aarch64-vm.efi)" --work "$work"
        ;;
    pi | pi3)
        python3 os/test.py "$t" "${arm_upd[@]}" --image "$out/k10s-os-aarch64-pi.img" --uki "$(echo "$out"/images/k10s-os-*-aarch64-pi.fat)" \
            --work "$work"
        ;;
    boot-i686)
        python3 os/test.py boot --arch i686 --kernel "$i386/arch/x86/boot/bzImage" --initrd "$out/initrd-i686.cpio.zst" \
            --k10s "$host" --examples examples --work "$work"
        ;;
    image-i686) python3 os/test.py image --arch i686 --bios --image "$out/i686-serial.img" --k10s "$host" --examples examples --work "$work" ;;
    update-i686)
        python3 os/test.py update --arch i686 --bios --image "$out/i686-serial.img" --uki "$out/i686-serial.efi" --image-key "$image_key" \
            --boot "$(os/bootfiles.sh i686)" --k10s "$host" --examples examples --work "$work"
        ;;
    boot-armv7)
        python3 os/test.py boot --arch armv7 --kernel "$armv7/arch/arm/boot/zImage" --initrd "$out/initrd-armv7.cpio.zst" \
            --k10s "$host" --examples examples --work "$work"
        ;;
    image-armv7) python3 os/test.py image --arch armv7 --uefi --image "$out/k10s-os-armv7-vm.img" --k10s "$host" --examples examples --work "$work" ;;
    update-armv7)
        python3 os/test.py update --arch armv7 --uefi --image "$out/k10s-os-armv7-vm.img" --uki "$(echo "$out"/images/k10s-os-*-armv7-vm.efi)" \
            --image-key "$image_key" --boot "$(os/bootfiles.sh armv7)" --k10s "$host" --examples examples --work "$work"
        ;;
    mixed)
        python3 os/test.py mixed --image "$out/k10s-os-aarch64-pi.img" --k10s "$host" --host-k10s "${K10S_HOST_X86:-$host}" \
            --examples examples --work "$work"
        ;;
    secureboot) python3 os/test.py secureboot "${sb[@]}" --work "$work" ;;
    secureboot-arm64) python3 os/test.py secureboot --arch aarch64 "${sb[@]}" --work "$work" ;;
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
