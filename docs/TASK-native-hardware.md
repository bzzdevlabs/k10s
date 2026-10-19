# Task: k10-metal on physical hardware (Tier 2 — native kernel, no Linux)

Status: **open — analysis & research**. Owner: unassigned.
Companion task: [TASK-universal-os.md](TASK-universal-os.md) (Tier 1: universal image with Linux as a driver layer).

## 1. Context
k10s is a ground-up replacement for Kubernetes (read [DESIGN.md](DESIGN.md) first). Workloads are TIR modules
(compiled from the Ten language), isolated by software fault isolation inside one address space, so a node needs from
the hardware only CPUs, memory, a timer, a NIC, a console and (optionally) storage.

The node exists in two forms:
- **Hosted**: `k10s node`, one Linux process ([NODE.md](NODE.md)). Runs on any x86-64 Linux machine today.
- **Native**: `k10-metal` ([../metal/README.md](../metal/README.md), [../metal/RESULTS.md](../metal/RESULTS.md)), the same
  runtime as its own kernel. Measured under QEMU/KVM: 694 KB image, ready ~3 ms after kernel entry (~95 ms including
  QEMU startup), serves HTTP in 3 MiB of RAM, JIT engine, SMP (up to 8 CPUs), full member of a hosted cluster.

The user's goal is to install a very simple system on **real physical devices of any kind** — Raspberry Pi, HP
servers, home workstations/desktops; amd64, i386, arm64 — and run everything. The agreed strategy has two tiers that
run the same workloads in the same cluster. This task is **Tier 2**: make the native kernel run on real hardware where
it pays off (VMs and clouds first, then a curated hardware list). Tier 1 covers "everything else".

## 2. Current state (verified facts)
`k10-metal` is a **VM guest only**. Its hardware assumptions:

| Area | Today | Problem on real hardware |
|---|---|---|
| Boot | PVH entry (`XEN_ELFNOTE_PHYS32_ENTRY`), works with QEMU / Firecracker / Cloud Hypervisor | PCs and servers boot via UEFI (or legacy BIOS); ARM boards via their own firmware (Pi) or UEFI/U-Boot |
| Platform discovery | CPU count from QEMU `fw_cfg`; no ACPI; xAPIC at the MSR base | Needs ACPI (MADT for CPUs/APIC IDs, MCFG for PCIe ECAM, FADT for reset/poweroff) on x86; devicetree on ARM boards |
| Interrupts | LAPIC (xAPIC MMIO) + MSI-X; legacy PIC masked | Firmware may hand over in x2APIC mode; ARM uses GIC |
| Timer | TSC from kvmclock, else CPUID 0x15, else 10 ms PIT; LAPIC one-shot | Mostly OK on x86 (HPET as extra source); ARM generic timer |
| NIC | Legacy virtio-net only (`disable-modern=on`) | Physical NICs need their own drivers; clouds need modern virtio (or ENA on AWS, gVNIC on GCP, MANA on Azure) |
| Storage | None (KV is in memory) | NVMe / AHCI / SD-eMMC drivers |
| Console | COM1 serial | Many desktops have no serial port: framebuffer (UEFI GOP) or network console |
| Arch | x86-64 only (kernel arch layer + JIT backend) | arm64 needs its own arch layer and JIT backend |

Dev machine constraints: x86-64 laptop, no root, QEMU 7.2 with KVM, only `qemu-system-x86_64`, `qemu-system-i386`
and `microvm` installed (no aarch64 emulator), user-mode networking only.

## 3. Goal of this task
Produce an **analysis and research report** that tells us how to take `k10-metal` to physical hardware, what it
costs, in which order, and what to deliberately leave to Tier 1. Small throwaway experiments/prototypes are welcome
(in a separate branch/worktree or the scratch area) to validate key claims; do not change the main code base.

## 4. Research questions
### 4.1 Boot and firmware
1. Best x86 boot path for real machines: native UEFI application (PE/COFF, `ExitBootServices`), an existing boot
   protocol through a bootloader (Limine, GRUB multiboot2), or both? Keep PVH for VMs. Secure Boot implications
   (shim/signing) and cost.
2. ARM boot: Raspberry Pi firmware (`kernel8.img` + devicetree + `config.txt`), UEFI on Pi (community firmware),
   U-Boot, and UEFI on arm64 servers (SBSA/SBBR). Which single path covers the most arm64 devices?
3. Minimum ACPI subset needed without an AML interpreter (MADT, MCFG, FADT, HPET, SRAT/SLIT for NUMA?). When is AML
   unavoidable (power-off, some interrupt routing), and what are the small-AML options?
4. Devicetree parsing on ARM: minimum subset for CPUs, memory, GIC, timer, UART, PCIe/Ethernet.

### 4.2 Hardware coverage (the core question)
5. Build a **coverage matrix** of NIC and storage chips per target device class, with rough market share:
   - HP/Dell/Lenovo servers (e.g. Broadcom BCM5719/5720 `tg3`, Intel i350 `igb`, X710/E810 `i40e`/`ice`,
     Mellanox ConnectX-4/5/6 `mlx5`); RAID/HBA vs NVMe;
   - home desktops/workstations (Realtek RTL8111/8125 `r8169`, Intel I219/I225/I226 `e1000e`/`igc`; NVMe, AHCI);
   - Raspberry Pi 3 (USB-attached LAN9514/LAN7515 — needs a USB/xHCI/DWC2 stack), Pi 4 (Broadcom GENET),
     Pi 5 (RP1 south bridge, Cadence GEM `macb`); SD/eMMC;
   - clouds: modern virtio-net/blk, AWS ENA + NVMe, GCP gVNIC, Azure MANA/Mellanox; virtio-mmio for Firecracker/microvm.
6. For each driver: size and complexity estimate (reference the Linux driver and simpler implementations, e.g.
   FreeBSD/NetBSD, Redox, Hermit, Unikraft, Theseus, rust-osdev projects), firmware blobs required, licensing
   constraints (GPL code cannot be copied into this Apache-2.0 code base).
7. Which minimal set of drivers covers the largest share of target devices (a "top 10 drivers" list)?

### 4.3 Alternatives to writing every driver
8. Driver reuse: NetBSD rump kernels, LKL (Linux Kernel Library), DDE/DDEKit, running unmodified drivers in a
   driver VM (Tier 1 inside Tier 2), or virtio front-ends to a host. Footprint, licensing, effort.
9. **Drivers as isolated k10s workloads**: could a driver be a TIR module granted narrow MMIO/DMA/interrupt
   capabilities through the host API (user-space-driver style, like seL4/Fuchsia/Redox), so drivers are fault-isolated
   and portable? What does DMA safety require (IOMMU/VT-d/SMMU)? Is it worth it?
10. Where is Tier 1 simply the better answer (Wi-Fi, GPUs, exotic NICs, laptops)?

### 4.4 Platform services
11. IOMMU use (DMA protection), microcode loading, CPU frequency/power states, thermal, fans (servers mostly delegate
    to the BMC — confirm), watchdogs, RTC, entropy (RDRAND/RNDR vs others).
12. Console and remote management without serial: UEFI GOP framebuffer text console, netconsole, BMC serial-over-LAN.
13. Persistent storage design for the native kernel (raw append-only log on NVMe vs a filesystem) and how it backs the
    KV store.

### 4.5 arm64 native port
14. Size of an arm64 arch layer (boot, MMU, GIC, generic timer, exceptions, SMP via PSCI) relative to the x86 one in
    `metal/kernel/src/arch`.
15. Requirements for an arm64 JIT backend (see `crates/k10-jit`; SFI masking on arm64 — research reports ~7% overhead
    for LFI-style isolation, see [research.md](research.md)).

### 4.6 Testing strategy without real hardware
16. How far QEMU can emulate the targets: OVMF for UEFI (installed?), `-device e1000e`, `-device igb` (QEMU ≥ 8),
    `nvme`, `ahci`, modern virtio, `qemu-system-aarch64 -M virt` / `raspi3b` / `raspi4b` (QEMU ≥ 9). What must be
    installed, and can it be done without root? Which steps need real hardware (and which devices the user should
    provide)?

## 5. Deliverables
- `docs/research/native-hardware.md`: findings for every question above, with sources (URLs) and clear labels for
  estimates vs measured facts.
- A hardware coverage matrix (device class × chip × driver effort × testable in QEMU?).
- A recommended roadmap: ordered milestones (e.g. UEFI + ACPI + modern virtio → Intel NIC + NVMe → Pi 4/5 → arm64 JIT),
  each with effort estimate, test plan and exit criteria.
- A clear statement of what Tier 2 should NOT attempt (left to Tier 1), with reasons.
- Risks and open questions for the architect.

## 6. Constraints
- Do not modify existing crates or `metal/`; experiments go in a separate branch/worktree or the scratch area.
- No root on the dev machine; don't install system packages without asking.
- Keep k10s principles: compiler-enforced isolation, no central control plane, minimal footprint, Apache-2.0.
