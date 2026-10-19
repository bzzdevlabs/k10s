#!/usr/bin/env bash
# Unified kernel image (one PE file: systemd-stub + kernel + initramfs + cmdline), assembled with objcopy, no root.
# A UKI can be started by any UEFI loader with LoadImage/StartImage (e.g. a Tier 2 loader falling back to Tier 1),
# by systemd-boot (/EFI/Linux/*.efi) or by the firmware itself as \EFI\BOOT\BOOTX64.EFI.
#   mkuki.sh KERNEL INITRD "CMDLINE" OUT.efi
set -euo pipefail
UOS=${UOS:-/home/qgerard/dev/k10s-research/universal-os}
k=$1 i=$2 c=$3 out=$4
t=$(mktemp -d "$UOS/runs/uki.XXXX")
printf '%s' "$c" > "$t/cmdline"
printf 'NAME="k10s"\nID=k10s\nVERSION_ID=research\n' > "$t/osrel"
objcopy \
  --add-section .osrel="$t/osrel" --change-section-vma .osrel=0x20000 \
  --add-section .cmdline="$t/cmdline" --change-section-vma .cmdline=0x30000 \
  --add-section .linux="$k" --change-section-vma .linux=0x2000000 \
  --add-section .initrd="$i" --change-section-vma .initrd=0x3000000 \
  "$UOS/boot-debs/x/usr/lib/systemd/boot/efi/linuxx64.efi.stub" "$out"
rm -rf "$t"
ls -l "$out" >&2
