# QEMU arguments shared by run.sh, test.sh and cluster-test.sh (sourced, not run).
#   ACCEL      kvm | tcg (default: kvm when /dev/kvm is usable, else tcg)
#   PLATFORM   the machine, firmware and NIC:
#     pc          i440fx + SeaBIOS, PVH -kernel, virtio-net-pci transitional (driven as virtio 1.x)  [default]
#     pc-legacy   the same NIC with disable-modern=on (legacy virtio 0.9.5, I/O BAR)
#     pc-legacy-split  legacy without ANY_LAYOUT (the header in a descriptor of its own)
#     pc-modern   the same NIC with disable-legacy=on (modern-only 1af4:1041)
#     q35         q35 + SeaBIOS (ECAM), the NIC modern-only behind a PCIe root port
#     microvm     QEMU microvm (qboot, hardware-reduced ACPI), virtio-mmio version 1 (microvm's default)
#     microvm-v2  microvm with virtio-mmio version 2 (modern, as Firecracker)
#   BIOS       firmware image for -bios (e.g. /usr/share/qemu/qboot.rom)

k10_accel() {
    local a=${ACCEL:-}
    if [[ -z $a ]]; then
        if [[ -r /dev/kvm && -w /dev/kvm ]]; then a=kvm; else a=tcg; fi
    fi
    case $a in
    kvm) echo "-enable-kvm -cpu host" ;;
    tcg) echo "-accel tcg,thread=multi -cpu max" ;;
    *) echo "qemu.sh: ACCEL=$a: want kvm or tcg" >&2; return 1 ;;
    esac
}

# k10_machine PLATFORM: the -M option (and machine-wide settings).
k10_machine() {
    case $1 in
    pc | pc-legacy | pc-legacy-split | pc-modern) echo "-M pc" ;;
    q35) echo "-M q35 -device pcie-root-port,id=rp0,chassis=1 -device pcie-root-port,id=rp1,chassis=2" ;;
    microvm) echo "-M microvm" ;;
    microvm-v2) echo "-M microvm -global virtio-mmio.force-legacy=false" ;;
    *) echo "qemu.sh: PLATFORM=$1: want pc, pc-legacy, pc-legacy-split, pc-modern, q35, microvm or microvm-v2" >&2; return 1 ;;
    esac
}

# k10_nic PLATFORM NETDEV: the NIC on netdev NETDEV.
k10_nic() {
    case $1 in
    pc) echo "-device virtio-net-pci,netdev=$2,romfile=" ;;
    pc-legacy) echo "-device virtio-net-pci,disable-modern=on,netdev=$2,romfile=" ;;
    pc-legacy-split) echo "-device virtio-net-pci,disable-modern=on,any_layout=off,netdev=$2,romfile=" ;;
    pc-modern) echo "-device virtio-net-pci,disable-legacy=on,netdev=$2,romfile=" ;;
    q35) echo "-device virtio-net-pci,netdev=$2,bus=rp0,romfile=" ;;
    microvm | microvm-v2) echo "-device virtio-net-device,netdev=$2" ;;
    esac
}

# k10_disk PLATFORM FILE: a virtio-blk disk backed by FILE (raw).
k10_disk() {
    local d="-drive file=$2,if=none,id=d0,format=raw,cache=writethrough"
    case $1 in
    pc | pc-modern) echo "$d -device virtio-blk-pci,drive=d0" ;;
    pc-legacy | pc-legacy-split) echo "$d -device virtio-blk-pci,disable-modern=on,drive=d0" ;;
    q35) echo "$d -device virtio-blk-pci,drive=d0,bus=rp1" ;;
    microvm | microvm-v2) echo "$d -device virtio-blk-device,drive=d0" ;;
    esac
}
