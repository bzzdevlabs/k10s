#!/usr/bin/env bash
# Rebuilds every x86 image used by final-bench.sh from bin/ and the kernels in img/.
set -euo pipefail
cd "$(dirname "$0")/.."
Q="console=ttyS0 quiet panic=-1"
scripts/initramfs-deb.sh qemu img/deb-qemu.cpio.zst bin/k10s-pid1-x86_64
scripts/initramfs-deb.sh x86-universal img/deb-x86u.cpio.zst bin/k10s-pid1-x86_64
python3 scripts/mkinitramfs.py img/min-pid1.cpio.zst --init bin/k10s-pid1-x86_64
python3 scripts/mkinitramfs.py img/min-unmod.cpio.zst --init k10s-x86_64
python3 scripts/mkinitramfs.py img/i386-pid1.cpio.zst --init bin/k10s-pid1-i686
scripts/mkdisk.sh x86_64 /boot/vmlinuz-6.1.0-53-amd64 img/deb-qemu.cpio.zst "$Q" img/disk-deb-x86_64.img 96
scripts/mkdisk.sh x86_64 img/k10s-x86_64-bzImage img/min-pid1.cpio.zst "$Q" img/disk-min-x86_64.img 64
scripts/mkuki.sh img/k10s-x86_64-bzImage img/min-pid1.cpio.zst "$Q" img/k10s-x86_64.efi
T=$(mktemp -d runs/esp.XXXX); export MTOOLS_SKIP_CHECK=1
truncate -s 34M $T/disk.img
printf 'label: gpt\nstart=2048, type=C12A7328-F81F-11D2-BA4B-00A0C93EC93B, name="K10S-BOOT"\n' | /sbin/sfdisk -q $T/disk.img
/sbin/mkfs.fat -F 32 -n K10S-BOOT -C $T/esp.img $((33*1024)) >/dev/null
mmd -i $T/esp.img ::/EFI ::/EFI/BOOT && mcopy -i $T/esp.img img/k10s-x86_64.efi ::/EFI/BOOT/BOOTX64.EFI
dd if=$T/esp.img of=$T/disk.img bs=1M seek=1 conv=notrunc status=none && mv $T/disk.img img/disk-uki-x86_64.img && rm -rf $T
