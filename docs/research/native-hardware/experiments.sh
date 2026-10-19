#!/usr/bin/env bash
# Reproduces the measured experiments of docs/research/native-hardware.md.
#
# The prototypes are throwaway code on the local branch `research/native-hardware` (never merged or pushed), checked
# out as a worktree. Its first commit is a snapshot of the main working tree on 2026-10-19; the later commits are
# the experiments:
#   research/uefi-pvh/        UEFI application that boots the unmodified PVH kernel (x86_64-unknown-uefi)
#   research/arm64-hello/     aarch64 arch-layer sketch (aarch64-unknown-none-softfloat)
#   metal/kernel/src/{acpi,virtio_modern,nvme,intel_gbe}.rs + small hooks in main.rs, smp.rs, pvh.rs, paging.rs
#
#   WT=/path/to/worktree R=/path/to/scratch ./experiments.sh <step>
# Steps: build | boot-times | acpi | modern-virtio | nvme | gbe | uefi-all | secureboot | arm64
# Needs: QEMU 7.2 + OVMF (system), QEMU 10 from ./get-qemu10.sh for igb and aarch64, rustup targets
# x86_64-unknown-none, x86_64-unknown-uefi, aarch64-unknown-none-softfloat. No root.
set -euo pipefail
WT="${WT:-$HOME/dev/k10s-research/native-hardware/wt}"
R="${R:-$HOME/dev/k10s-research/native-hardware}"
Q10="${Q10:-$R/tools/qemu10}"
HERE="$(cd "$(dirname "$0")" && pwd)"
K="$WT/metal/out"
q10x=(env QEMU_MODULE_DIR="$Q10/usr/lib/x86_64-linux-gnu/qemu" "$Q10/usr/bin/qemu-system-x86_64" -L "$Q10/usr/share/qemu")
q10a=(env QEMU_MODULE_DIR="$Q10/usr/lib/x86_64-linux-gnu/qemu" "$Q10/usr/bin/qemu-system-aarch64" -L "$Q10/usr/share/qemu")
base=(-enable-kvm -cpu host -m 64M -nodefaults -display none -serial stdio -no-reboot
      -device isa-debug-exit,iobase=0xf4,iosize=0x04)
nic=(-netdev user,id=n0 -device virtio-net-pci,disable-modern=on,netdev=n0,romfile=)
efi() { echo "$WT/research/uefi-pvh/target/x86_64-unknown-uefi/release/k10boot.efi"; }

case "${1:-}" in
build)
    (cd "$WT/metal" && ./build.sh && cp target/x86_64-unknown-none/release/k10-metal out/k10-metal-proto.elf)
    (cd "$WT/research/uefi-pvh" && K10_KERNEL="$K/k10-metal-proto.elf" K10_BUNDLE="$K/bundle.k10b" K10_CMDLINE="" \
        cargo build --release --target x86_64-unknown-uefi)
    mkdir -p "$R/esp/EFI/BOOT" "$R/fw" && cp "$(efi)" "$R/esp/EFI/BOOT/BOOTX64.EFI"
    cp /usr/share/OVMF/OVMF_VARS_4M.fd "$R/fw/vars.fd"
    (cd "$WT/research/arm64-hello" && cargo build --release && cargo build --release --features rpi4 --target-dir target-rpi4)
    ;;
boot-times) # PVH (SeaBIOS, qboot) vs UEFI (OVMF, -kernel and ESP), 7 interleaved runs each
    c="-enable-kvm -cpu host -m 64M -smp 1 -nodefaults -display none -serial stdio -no-reboot -device isa-debug-exit,iobase=0xf4,iosize=0x04 -netdev user,id=n0 -device virtio-net-pci,disable-modern=on,netdev=n0,romfile="
    python3 "$HERE/bootcmp.py" 7 \
        pvh_seabios_pc "qemu-system-x86_64 -M pc $c -kernel $K/k10-metal.elf -initrd $K/bundle.k10b" \
        pvh_qboot_pc "qemu-system-x86_64 -M pc $c -bios /usr/share/qemu/qboot.rom -kernel $K/k10-metal.elf -initrd $K/bundle.k10b" \
        uefi_kernel_pc "qemu-system-x86_64 -M pc $c -bios /usr/share/ovmf/OVMF.fd -kernel $(efi)" \
        uefi_kernel_q35 "qemu-system-x86_64 -M q35 $c -bios /usr/share/ovmf/OVMF.fd -kernel $(efi)" \
        uefi_esp_q35_pflash "qemu-system-x86_64 -M q35 $c -drive if=pflash,format=raw,readonly=on,file=/usr/share/OVMF/OVMF_CODE_4M.fd -drive if=pflash,format=raw,file=$R/fw/vars.fd -drive if=none,id=esp,format=raw,readonly=on,file=fat:$R/esp -device virtio-blk-pci,drive=esp"
    ;;
acpi) # MADT CPU count, FADT, MCFG, S5 power-off without AML (QEMU exits 0 with no isa-debug-exit device)
    for m in pc q35; do
        timeout 20 qemu-system-x86_64 -M $m "${base[@]}" -smp 4 "${nic[@]}" -kernel "$K/k10-metal-proto.elf" \
            -initrd "$K/bundle.k10b" -append "k10.verbose k10.acpi_off_on_ready" | grep -aE 'acpi|smp|ready' || true
    done
    ;;
modern-virtio) # virtio 1.x transport (transitional forced to modern, then modern-only), PVH
    for d in "virtio-net-pci|k10.virtio=modern" "virtio-net-pci-non-transitional|k10.verbose"; do
        timeout 15 qemu-system-x86_64 -M q35 "${base[@]}" -netdev user,id=n0 -device "${d%%|*},netdev=n0,romfile=" \
            -kernel "$K/k10-metal-proto.elf" -initrd "$K/bundle.k10b" -append "${d##*|} k10.exit_on_ready" | grep -aE 'net:|ready' || true
    done
    echo "full e2e suite on a modern-only NIC: (cd $WT/metal && NICDEV=virtio-net-pci-non-transitional ENGINES=jit ./test.sh)"
    ;;
nvme)
    truncate -s 64M "$R/nvme.img"
    timeout 20 qemu-system-x86_64 -M q35 "${base[@]}" "${nic[@]}" -drive "file=$R/nvme.img,if=none,id=nv0,format=raw" \
        -device nvme,serial=k10test,drive=nv0 -kernel "$K/k10-metal-proto.elf" -initrd "$K/bundle.k10b" \
        -append "k10.nvme_probe k10.exit_on_ready" | grep -aE 'nvme|ready' || true
    ;;
gbe) # one polled Intel driver for 82540EM / 82574L / 82576 (igb needs QEMU >= 8)
    for d in e1000 e1000e igb; do
        timeout 20 "${q10x[@]}" -M q35 "${base[@]}" "${nic[@]}" -netdev user,id=n1 -device "$d,netdev=n1,romfile=" \
            -kernel "$K/k10-metal-proto.elf" -initrd "$K/bundle.k10b" -append "k10.gbe_probe k10.exit_on_ready" | grep -a gbe || true
    done
    ;;
uefi-all) # OVMF q35: UEFI loader + ACPI (4 CPUs) + modern-only virtio-net and NVMe with 64-bit BARs above 4 GiB
    truncate -s 64M "$R/nvme.img"
    qemu-system-x86_64 -M q35 -enable-kvm -cpu host -m 512M -smp 4 -nodefaults -display none -serial stdio -no-reboot \
        -bios /usr/share/ovmf/OVMF.fd -netdev user,id=n0,hostfwd=tcp:127.0.0.1:18095-:80 \
        -device virtio-net-pci-non-transitional,netdev=n0,romfile= -drive "file=$R/nvme.img,if=none,id=nv0,format=raw" \
        -device nvme,serial=k10test,drive=nv0 -kernel "$(efi)" -append "k10.verbose k10.nvme_probe" &
    pid=$!; sleep 8; curl -s localhost:18095/hello/; kill $pid
    ;;
secureboot) # Secure Boot with Microsoft keys enrolled refuses the unsigned loader ("Access Denied")
    cp /usr/share/OVMF/OVMF_VARS_4M.ms.fd "$R/fw/vars-ms.fd"
    timeout 40 qemu-system-x86_64 -machine q35,smm=on -global driver=cfi.pflash01,property=secure,value=on -enable-kvm \
        -cpu host -m 128M -nodefaults -display none -serial stdio -no-reboot \
        -drive if=pflash,format=raw,unit=0,readonly=on,file=/usr/share/OVMF/OVMF_CODE_4M.secboot.fd \
        -drive if=pflash,format=raw,unit=1,file="$R/fw/vars-ms.fd" \
        -drive if=none,id=esp,format=raw,readonly=on,file=fat:"$R/esp" -device virtio-blk-pci,drive=esp | grep -a BdsDxe || true
    ;;
arm64) # virt GICv2/GICv3, EL2 entry, then the Raspberry Pi 4 machine (spin table, GIC-400)
    E="$WT/research/arm64-hello/target/aarch64-unknown-none-softfloat/release/hello"
    for m in virt,gic-version=2 virt,gic-version=3 virt,gic-version=3,virtualization=on; do
        timeout 30 "${q10a[@]}" -M $m -cpu cortex-a72 -smp 4 -m 128M -nodefaults -display none -serial stdio -audio none -kernel "$E"
    done
    # raspi4b wants a flat Image (QEMU then enters at EL2 like the Pi firmware does); objcopy needs the toolchain's libLLVM.
    T="$(rustc --print sysroot)"; LD_LIBRARY_PATH="$T/lib" "$T/lib/rustlib/x86_64-unknown-linux-gnu/bin/rust-objcopy" -O binary \
        "$WT/research/arm64-hello/target-rpi4/aarch64-unknown-none-softfloat/release/hello" "$R/kernel8.img"
    timeout 20 "${q10a[@]}" -M raspi4b -nodefaults -display none -serial stdio -audio none -kernel "$R/kernel8.img" || true
    ;;
*) sed -n 2,16p "$0"; exit 1 ;;
esac
