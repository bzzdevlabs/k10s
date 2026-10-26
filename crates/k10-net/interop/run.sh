#!/usr/bin/env bash
# Interop of k10-net with Linux's TCP/IP stack, without privileges: builds the guest's /init (static musl), an
# initramfs with the host kernel's virtio-net modules, and runs tests/interop.rs, which boots the host's own
# kernel in QEMU on a UDP socket netdev and drives k10-net with raw Ethernet frames from the test process.
set -euo pipefail
cd "$(dirname "$0")"
K=$(uname -r)
M=/lib/modules/$K/kernel
[[ -r /boot/vmlinuz-$K ]] || { echo "need a readable /boot/vmlinuz-$K"; exit 1; }
cargo build --release --manifest-path guest/Cargo.toml --target x86_64-unknown-linux-musl
mkdir -p out
strip -o out/init guest/target/x86_64-unknown-linux-musl/release/k10-net-guest
# Load order follows modules.dep (virtio_net needs net_failover/failover and the virtio core + PCI transport).
python3 ../../../metal/tools/mkinitramfs.py out/initramfs.cpio out/init \
    $M/drivers/virtio/virtio.ko $M/drivers/virtio/virtio_ring.ko \
    $M/drivers/virtio/virtio_pci_legacy_dev.ko $M/drivers/virtio/virtio_pci_modern_dev.ko \
    $M/drivers/virtio/virtio_pci.ko $M/net/core/failover.ko $M/drivers/net/net_failover.ko \
    $M/drivers/net/virtio_net.ko
K10_NET_KERNEL=/boot/vmlinuz-$K K10_NET_INITRAMFS=$PWD/out/initramfs.cpio \
    cargo test -p k10-net --test interop -- --ignored --nocapture "$@"
