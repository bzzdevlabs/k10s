#!/usr/bin/env bash
# Builds the Linux comparison guest (no root needed): the host's own kernel (/boot/vmlinuz-$(uname -r)) booted by
# QEMU's Linux loader with a tiny initramfs whose /init is tools/linux-guest (static musl binary):
#   out/linux/initramfs-boot.cpio  /init prints "linux-guest ready" and powers off      (k10mode=boot)
#   out/linux/initramfs-http.cpio  + virtio-net modules; /init serves HTTP on :80       (k10mode=http)
# Then `tools/measure.py linux` measures boot time / minimum RAM, and `tools/measure.py http` uses the HTTP guest.
set -euo pipefail
cd "$(dirname "$0")/.."
K=$(uname -r)
M=/lib/modules/$K/kernel
[[ -r /boot/vmlinuz-$K ]] || { echo "need a readable /boot/vmlinuz-$K"; exit 1; }
cargo build --release -p linux-guest --target x86_64-unknown-linux-musl
mkdir -p out/linux
strip -o out/linux/init target/x86_64-unknown-linux-musl/release/linux-guest
python3 tools/mkinitramfs.py out/linux/initramfs-boot.cpio out/linux/init
# Load order follows modules.dep (virtio_net needs net_failover/failover and the virtio core + PCI transport).
python3 tools/mkinitramfs.py out/linux/initramfs-http.cpio out/linux/init \
    $M/drivers/virtio/virtio.ko $M/drivers/virtio/virtio_ring.ko \
    $M/drivers/virtio/virtio_pci_legacy_dev.ko $M/drivers/virtio/virtio_pci_modern_dev.ko \
    $M/drivers/virtio/virtio_pci.ko $M/net/core/failover.ko $M/drivers/net/net_failover.ko \
    $M/drivers/net/virtio_net.ko
echo "boot it: qemu-system-x86_64 -M pc -enable-kvm -cpu host -m 128M -nodefaults -display none -serial stdio \\"
echo "  -kernel /boot/vmlinuz-$K -initrd out/linux/initramfs-boot.cpio -append 'console=ttyS0 quiet panic=-1 k10mode=boot'"
