#!/usr/bin/env bash
# Builds initramfs images for a Debian kernel's module tree with k10s as /init.
#   initramfs-deb.sh PROFILE OUT INIT_BINARY [KMODS_DIR]
# Profiles: qemu (virtio only), x86-universal (every Ethernet + USB-net + storage + input + watchdog),
#           arm64-qemu, arm64-universal.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
profile=$1 out=$2 init=$3 K=${4:-/lib/modules/$(uname -r)}
common=(--mod 'drivers/net/virtio_net.ko' --mod 'drivers/virtio/*' --mod 'drivers/block/virtio_blk.ko'
        --mod 'drivers/char/hw_random/*' --mod 'fs/ext4/ext4.ko' --mod 'crypto/crc32c*' --mod 'arch/*/crypto/crc32c*' --mod 'fs/fat/*' --mod 'fs/nls/nls_cp437.ko' --mod 'fs/nls/nls_ascii.ko' --mod 'fs/nls/nls_utf8.ko' --mod 'fs/nls/nls_iso8859-1.ko' --mod 'drivers/input/evdev.ko')
case $profile in
  qemu) mods=("${common[@]}" --mod 'drivers/acpi/button.ko' --mod 'drivers/watchdog/i6300esb.ko') ;;
  x86-universal) mods=("${common[@]}" --mod 'drivers/acpi/button.ko' --mod 'drivers/watchdog/*'
        --mod 'drivers/net/ethernet/*' --mod 'drivers/net/usb/*' --mod 'drivers/net/phy/*' --mod 'drivers/net/mdio/*'
        --mod 'drivers/ata/*' --mod 'drivers/nvme/host/*' --mod 'drivers/scsi/sd_mod.ko' --mod 'drivers/scsi/virtio_scsi.ko'
        --mod 'drivers/usb/host/*' --mod 'drivers/usb/storage/*' --mod 'drivers/mmc/*' --mod 'drivers/hid/*'
        --mod 'drivers/input/keyboard/atkbd.ko' --mod 'drivers/rtc/*') ;;
  arm64-qemu) mods=("${common[@]}" --mod 'drivers/input/keyboard/gpio_keys.ko' --mod 'drivers/gpio/gpio-pl061.ko') ;;
  arm64-universal) mods=("${common[@]}" --mod 'drivers/watchdog/*' --mod 'drivers/net/ethernet/*'
        --mod 'drivers/net/usb/*' --mod 'drivers/net/phy/*' --mod 'drivers/net/mdio/*' --mod 'drivers/ata/*'
        --mod 'drivers/nvme/host/*' --mod 'drivers/scsi/sd_mod.ko' --mod 'drivers/usb/*' --mod 'drivers/mmc/*'
        --mod 'drivers/hid/*' --mod 'drivers/rtc/*' --mod 'drivers/gpio/*' --mod 'drivers/clk/*' --mod 'drivers/pinctrl/*'
        --mod 'drivers/regulator/*' --mod 'drivers/phy/*' --mod 'drivers/reset/*' --mod 'drivers/soc/*') ;;
  *) echo "unknown profile $profile"; exit 2 ;;
esac
python3 "$here/mkinitramfs.py" "$out" --init "$init" --kmods "$K" "${mods[@]}" 2> >(grep -v "no module matches" >&2)
