#!/usr/bin/env bash
# Rootless disk image for the k10s universal image (no loop devices, no mount):
#   GPT: p1 BIOS-boot (1 MiB, GRUB core.img, x86 only) | p2 ESP/FAT32 "K10S-BOOT" | p3 ext4 "K10S-DATA" (empty)
#   ESP: systemd-boot as the removable-media loader (BOOTX64.EFI / BOOTAA64.EFI) + /loader/entries/k10s.conf,
#        /boot/grub/grub.cfg for legacy BIOS (x86), /k10s/{vmlinuz,initrd}, /k10s.conf (node config, editable on any PC)
#   MBR: GRUB i386-pc boot.img, its core.img in p1 (x86 only), sectors patched by hand (no grub-bios-setup needed)
# Usage: mkdisk.sh ARCH(x86_64|aarch64) KERNEL INITRD "CMDLINE" OUT.img [SIZE_MiB]
set -euo pipefail
UOS=${UOS:-/home/qgerard/dev/k10s-research/universal-os}
B=$UOS/boot-debs/x
arch=$1 kernel=$2 initrd=$3 cmdline=$4 out=$5 size=${6:-96}
tmp=$(mktemp -d "$UOS/runs/mkdisk.XXXX")
export SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH:-0} MTOOLS_SKIP_CHECK=1
esp_mib=$(( size - 2 - 16 ))   # 1 MiB alignment gap + 1 MiB BIOS boot ... 16 MiB data partition
rm -f "$out"; truncate -s ${size}M "$out"
/sbin/sfdisk -q "$out" <<EOF
label: gpt
label-id: 6B10C5A0-0000-4000-8000-000000000001
start=2048, size=2048, type=21686148-6449-6E6F-744E-656564454649, name="bios-boot", uuid=6B10C5A0-0000-4000-8000-000000000011
start=4096, size=$(( esp_mib * 2048 )), type=C12A7328-F81F-11D2-BA4B-00A0C93EC93B, name="K10S-BOOT", uuid=6B10C5A0-0000-4000-8000-000000000012
start=$(( 4096 + esp_mib * 2048 )), type=0FC63DAF-8483-4772-8E79-3D69D8477DE4, name="K10S-DATA", uuid=6B10C5A0-0000-4000-8000-000000000013
EOF
# ESP (FAT32), filled with mtools
/sbin/mkfs.fat -F 32 -n K10S-BOOT -i 6B10C5A0 -C "$tmp/esp.img" $(( esp_mib * 1024 )) >/dev/null
m() { mmd -i "$tmp/esp.img" "$@"; }
c() { mcopy -i "$tmp/esp.img" -m "$@"; }
m ::/EFI ::/EFI/BOOT ::/loader ::/loader/entries ::/k10s ::/boot ::/boot/grub
if [ "$arch" = x86_64 ]; then efi=BOOTX64.EFI sdb=systemd-bootx64.efi; else efi=BOOTAA64.EFI sdb=systemd-bootaa64.efi; fi
c "$B/usr/lib/systemd/boot/efi/$sdb" ::/EFI/BOOT/$efi
printf 'timeout 0\ndefault k10s.conf\nconsole-mode keep\n' > "$tmp/loader.conf"; c "$tmp/loader.conf" ::/loader/loader.conf
printf 'title k10s\nlinux /k10s/vmlinuz\ninitrd /k10s/initrd\noptions %s\n' "$cmdline" > "$tmp/k10s.conf"
c "$tmp/k10s.conf" ::/loader/entries/k10s.conf
printf 'set timeout=0\nlinux /k10s/vmlinuz %s\ninitrd /k10s/initrd\nboot\n' "$cmdline" > "$tmp/grub.cfg"
c "$tmp/grub.cfg" ::/boot/grub/grub.cfg
printf '# k10s node configuration (k10s.KEY=VALUE on the kernel command line wins)\n# join=10.0.0.10:7946\n' > "$tmp/k10s-node.conf"
c "$tmp/k10s-node.conf" ::/k10s.conf
c "$kernel" ::/k10s/vmlinuz
c "$initrd" ::/k10s/initrd
dd if="$tmp/esp.img" of="$out" bs=1M seek=2 conv=notrunc status=none
# data partition: empty ext4, deterministic
data_mib=$(( size - 2 - esp_mib - 1 ))
mkdir -p "$tmp/empty"
/sbin/mkfs.ext4 -q -L K10S-DATA -U 6b10c5a0-0000-4000-8000-0000000000da -E hash_seed=6b10c5a0-0000-4000-8000-0000000000db \
  -d "$tmp/empty" "$tmp/data.img" ${data_mib}M
dd if="$tmp/data.img" of="$out" bs=1M seek=$(( 2 + esp_mib )) conv=notrunc status=none
# legacy BIOS (x86): GRUB core.img in p1 (LBA 2048), boot.img in the MBR's first 440 bytes
if [ "$arch" = x86_64 ]; then
  G=$B/usr/lib/grub/i386-pc
  grub-mkimage -d "$G" -O i386-pc -o "$tmp/core.img" -p '(hd0,gpt2)/boot/grub' biosdisk part_gpt fat normal linux configfile >/dev/null
  python3 - "$out" "$G/boot.img" "$tmp/core.img" <<'PY'
import struct, sys
disk, boot, core = sys.argv[1], open(sys.argv[2], "rb").read(), bytearray(open(sys.argv[3], "rb").read())
CORE_LBA = 2048
# diskboot.img's blocklist (last 12 bytes of core.img's first sector): start LBA of the rest of core.img
struct.pack_into("<Q", core, 0x200 - 12, CORE_LBA + 1)
mbr = bytearray(boot[:440])
struct.pack_into("<Q", mbr, 0x5C, CORE_LBA)  # GRUB_BOOT_MACHINE_KERNEL_SECTOR
with open(disk, "r+b") as f:
    f.seek(0); f.write(mbr)
    f.seek(CORE_LBA * 512); f.write(core)
print(f"mkdisk: core.img {len(core)} bytes at LBA {CORE_LBA}", file=sys.stderr)
PY
fi
rm -rf "$tmp"
echo "mkdisk: $out ($size MiB, ESP $esp_mib MiB, $arch)" >&2
