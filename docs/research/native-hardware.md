# k10-metal on physical hardware (Tier 2): research report

Answers [TASK-native-hardware.md](../TASK-native-hardware.md). Date: 2026-10-19.
Companion: [TASK-universal-os.md](../TASK-universal-os.md) (Tier 1, Linux as a driver layer). The decisions are in §11.

## How to read this report

Every claim carries a label:

| Label | Meaning |
|---|---|
| **[measured]** | Measured or run on the dev machine during this research, by a prototype or a count. |
| **[code]** | Read from the k10s sources (file and line counts from the main working tree, 2026-10-19). |
| **[doc]** | Vendor or official documentation, a spec, or upstream source code (URL given). |
| **[survey/data]** | A measured third-party dataset or analyst figure (URL given). |
| **[paper]** | A peer-reviewed paper (URL given). |
| **[blog/forum]** | A secondary source (URL given). |
| **[estimate]** | My inference. Effort figures are always estimates. |

**Test conditions.**
- **Host:** Intel i7-7820HQ (4 cores / 8 threads), Linux 6.1, KVM, QEMU 7.2.22. QEMU 10.0.2 was obtained without root (§8).
- **Load:** other agents ran builds and benchmarks throughout. The load average was **14–20** on 8 threads during every timing below. Timings are medians of interleaved runs and are noisy. Firmware times are in seconds and dominate anyway.

**Prototypes.**
- **Where they live:** throwaway code on the local branch `research/native-hardware`, worktree `/home/qgerard/dev/k10s-research/native-hardware/wt`. It is never merged or pushed. Commits:

  | Commit | Contents |
  |---|---|
  | a7fcd78 | snapshot of the working tree |
  | ae48aa8 | UEFI loader + ACPI |
  | 1e9cee4 | virtio 1.x |
  | 78e6297 | NVMe |
  | 8d4d350 | arm64 |
  | 4210ca0 | Intel GbE |

- **Reproduction:** [native-hardware/experiments.sh](native-hardware/experiments.sh), [native-hardware/bootcmp.py](native-hardware/bootcmp.py) (raw data in `bootcmp-results.json`) and [native-hardware/get-qemu10.sh](native-hardware/get-qemu10.sh).

---

## 0. Summary and recommendations

1. **x86 boot: keep PVH and add a ~500-line UEFI loader that hands the unchanged kernel a PVH `start_info`.** [measured]
   - The prototype `k10boot.efi` (551 lines, no dependencies) boots the unmodified PVH kernel under OVMF on `pc` and `q35`, both from `-kernel` and from a FAT ESP (`\EFI\BOOT\BOOTX64.EFI`).
   - It passes the ACPI RSDP in `start_info.rsdp_paddr` and adds ~7 KB to the image.
   - Kernel-side boot is unchanged (3.3–5 ms entry → ready). OVMF itself takes 3.3–4.1 s, against 42–58 ms for SeaBIOS/qboot.
   - **Also add a Multiboot2 entry shim** (~150 lines [estimate]) so GRUB or Limine can boot the kernel from **legacy BIOS**. Why it is needed:
     - AWS x86 AMIs default to legacy BIOS. https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/ami-boot.html [doc]
     - DigitalOcean custom images must boot BIOS. https://docs.digitalocean.com/products/custom-images/details/limits/ [doc]
     - Azure Gen1 VMs boot BIOS. https://learn.microsoft.com/azure/virtual-machines/generation-2 [doc]
   - **Do not adopt the Limine protocol.** It requires a higher-half kernel. [doc]
2. **Secure Boot: ship unsigned at first.** [measured + doc]
   - Our unsigned loader is refused ("Access Denied") under OVMF with Microsoft keys enrolled. [measured]
   - Getting a Microsoft-signed shim takes a 2–3-month shim review, an EV certificate, the 2023 UEFI CA (the 2011 CA expired 2026-06-27), and justification for a non-GRUB design that runs a ring-0 JIT. [doc]
   - Document instead how users disable Secure Boot or enroll their own keys (sbctl). Clouds do not enforce Secure Boot by default, except Azure's new Trusted Launch default, which can be opted out of. [doc]
3. **Platform discovery: table-only ACPI, no AML interpreter.** [measured]
   - Prototype: 324 lines, parsed in 9–21 µs. It reads MADT (CPU count and APIC IDs replace QEMU's fw_cfg), FADT, MCFG, HPET and SPCR.
   - **ACPI S5 power-off works without AML.** `\_S5` is found by a DSDT byte-pattern scan, and QEMU powers off in 0.2 s.
   - The PM1a control port differs between firmwares (0x604 vs 0xb004), so it must come from the FADT, never be hard-coded. [measured]
   - Add **uACPI** (MIT) only when laptops or hotplug matter, and those belong to Tier 1. [doc]
4. **Clouds and VMs first: modern virtio, then NVMe.**
   - Prototype virtio 1.x transport: 209 new lines plus 67 changed. It passes the whole `test.sh` JIT suite (164/164) on a **modern-only** NIC. [measured]
   - Prototype NVMe: 219 lines. Identify, I/O queue pair, and 4 KiB write/read/verify all work. [measured]
   - **Both need MMIO mapping above 4 GiB.** OVMF placed both 64-bit BARs at **448 GiB** (0x70_0000_0000), and the kernel maps only 4 GiB today. [measured]
5. **Coverage: 10 drivers cover most targets** (§4.4) [survey/data + estimate]. The ten:
   - virtio (pci modern/legacy + mmio; net, blk, scsi)
   - NVMe
   - AHCI
   - Intel GbE (e1000e / igb / igc)
   - Realtek r8169-class
   - AWS ENA
   - Hyper-V VMBus + netvsc/storvsc
   - GCP gVNIC
   - Broadcom tg3 (BCM5719/5720)
   - Pi GENET / RP1-GEM + SDHCI

   What they cover:
   - About **80% of PC Ethernet controller entries** and **89% of PC storage-controller entries** in the 12-month linux-hardware.org data. [survey/data]
   - NIC and disk on **AWS, Azure and GCP** (63% of the cloud market, Synergy Q2 2026). [survey/data]
   - Every KVM/virtio provider and microVM.
   - The 1 GbE default port of mainstream Dell/HPE/Lenovo/Supermicro servers.
   - Pi 4 and Pi 5.
6. **One Intel GbE driver covers three chip generations under QEMU.** [measured]
   - The prototype is 239 lines and polled, and serves 82540EM, 82574L and 82576.
   - QEMU's `igb` model forced two real-driver behaviours: a PHY autonegotiation restart via MDIC, and advanced RX descriptors.
   - The ~50% Realtek class and the I219 PCH quirks have **no QEMU model**, so they need real hardware.
7. **arm64: one `Image` (with a PE/COFF header later) for Pi firmware, U-Boot and UEFI; accept both DT and ACPI.**
   - The arch-layer sketch is 907 lines: boot, EL2→EL1, FDT, MMU, GICv2/v3, one-shot timer, PSCI or spin-table SMP, I-cache maintenance. [measured]
   - It runs on QEMU `virt` (GICv2 and GICv3, 4 CPUs) and on QEMU `raspi4b` (GIC-400, spin table, 4 CPUs). [measured]
   - A full arm64 arch layer is ~2.5–3k lines [estimate]. The x86 equivalent is ~2.1k lines [code].
8. **arm64 JIT: a second backend of ~3k lines, about 6–10 weeks.** [estimate]
   - Today's backend is 3,635 lines: an 879-line encoder and a 1,871-line function compiler [code].
   - k10s masking needs no guard regions. On arm64 it is `and` + register-offset `ldr`, the same instruction count as x86.
   - JIT code needs DC CVAU / IC IVAU + DSB/ISB after writing (exercised in the sketch). [measured]
9. **Leave to Tier 1** (§10):
   - Wi-Fi, GPUs, laptops (AML, EC, power states), Pi 3 and anything USB-attached.
   - 10/25/100G NICs and hardware RAID until someone needs them. VMD.
   - 32-bit x86 and armv7.
   - Driver isolation as TIR workloads (§5.2) is attractive for portability, but needs an IOMMU to be meaningful. Design for it now, build it later.
10. **The kernel has real-hardware blockers independent of drivers** (§1.2). Examples:
    - Only 4 GiB is mapped, while the frame allocator hands out all RAM.
    - 64-entry memory map (OVMF produced 134–143 UEFI descriptors).
    - fw_cfg CPU count and an 8-CPU limit.
    - xAPIC only; a firmware x2APIC handover would #GP.
    - COM1 only; `isa-debug-exit` for exit/panic; static IP 10.0.2.15.

    They are milestone M1's first work items.

**Roadmap** (§9): M1 UEFI/BIOS + ACPI + modern virtio + platform fixes → M2 NVMe + durable KV → M3 cloud NICs (ENA, gVNIC, VMBus) → M4 curated x86 bare metal (Intel GbE, Realtek, AHCI, tg3) → M5 arm64 arch layer (interpreter) → M6 arm64 JIT → M7 Raspberry Pi 4/5.

---

## 1. Where k10-metal stands

### 1.1 Hardware assumptions in the code [code]

| Area | Today (file) | Real-hardware problem |
|---|---|---|
| Boot | PVH 32-bit entry, `XEN_ELFNOTE_PHYS32_ENTRY` (boot.rs, 161 lines); `pvh::parse` panics without the PVH magic (pvh.rs) | PCs and servers boot UEFI or BIOS; clouds boot only via firmware from a disk image (§3.1) |
| Page tables | Identity map of the first **4 GiB** with 2 MiB pages; top 512 MiB uncached (boot.rs) | RAM and 64-bit BARs above 4 GiB are unmapped (§1.2) |
| Memory map | E820 from PVH, at most `MAX_MEMMAP = 64` entries (pvh.rs) | UEFI maps have 100+ descriptors; must be coalesced [measured: 134–143 → 16–17] |
| CPUs | Count from QEMU fw_cfg key 0x05; `MAX_CPUS = 8` (smp.rs, percpu.rs); INIT/SIPI broadcast "all excluding self" | No fw_cfg on hardware; servers have 16–256 threads; broadcast also wakes CPUs the firmware disabled → use MADT APIC IDs and targeted SIPIs |
| APIC | xAPIC MMIO; `init_local` clears IA32_APIC_BASE bit 10 (apic.rs) | Firmware may hand over in x2APIC mode (APIC IDs > 255 force it). x2APIC→xAPIC is an illegal transition → #GP |
| Timer | TSC from kvmclock, else CPUID 0x15, else 10 ms PIT calibration (time.rs) | AMD has no leaf 0x15; PIT may be gated; need ACPI PM timer or HPET (FADT/HPET give both) and an invariant-TSC check |
| PCI | Ports 0xCF8/0xCFC; `pci::find` scans **bus 0 only**; BAR assignment 32-bit only (pci.rs) | Real NICs sit behind root ports (bus ≥ 1); extended config and segments need ECAM (MCFG) |
| NIC | Legacy virtio-net only (`disable-modern=on`) | See §4 |
| Storage | None; KV in memory | §6.4 |
| Console | COM1 0x3F8 (serial.rs) | Many PCs have no serial; Dell iDRAC SOL is usually COM2; use SPCR / GOP / network (§6.2) |
| Power-off / panic | `isa-debug-exit` port 0xF4 (arch/mod.rs) | ACPI S5 / reset register (prototype done) |
| Network config | Static 10.0.2.15/24, gw 10.0.2.2 (net.rs) | DHCP (smoltcp has a DHCPv4 socket) or a command-line static IP |

### 1.2 Blockers found while prototyping

- **High memory.**
  - What the code does [code]: `frames::init` frees every E820 RAM frame (mem/frames.rs), but boot.rs maps only [0, 4 GiB).
  - Consequence [estimate, from the code; not triggered in tests]: on any machine with more than ~3.5 GiB of RAM, the first allocation above 4 GiB page-faults.
  - Fix: map all RAM at boot, with 1 GiB pages where available.
- **64-bit BARs.**
  - What happened [measured]: OVMF put the modern virtio BAR4 at `0x70_0000_0000` and the NVMe BAR0 at `0x70_0000_4000`.
  - The prototype adds `paging::map_mmio` (37 lines): uncached 1 GiB pages under PML4[0].
  - A 32-bit MSI-X BAR at `0xC000_1000` landed in the region boot.rs maps write-back. Under KVM, EPT forces UC for MMIO, so it works. On hardware this relies on the firmware MTRRs marking the hole UC. Map every BAR UC explicitly [estimate].
- **Memory-map size.** [measured] 134–143 UEFI descriptors coalesce into 16–17 E820 entries. The loader coalesces, so the kernel's 64-entry limit holds for OVMF. Real firmware maps can be larger, so raise the limit anyway.

---

## 2. Experiments

### 2.1 Summary of the prototypes [measured]

Line counts are physical lines including comments.

| Prototype | Lines | Result | Notes |
|---|---|---|---|
| UEFI → PVH loader `k10boot.efi` (`research/uefi-pvh`) | 551 (500 code) | Unmodified PVH kernel boots under OVMF `pc`/`q35` from `-kernel` and from a FAT ESP; SMP, virtio, HTTP all work | ~25 hand-written UEFI definitions, no crates. Embeds kernel + bundle; reads the command line from LoadOptions. RSDP from the EFI config table; GOP mode query (headless runs report "no GOP"). Leaves long mode via compatibility mode, then CR0.PG=0, then EFER.LME=0, and jumps to the PVH entry |
| ACPI static tables (`acpi.rs`) | 324 | MADT CPU count and APIC IDs, IOAPIC/ISO, FADT, MCFG, HPET, SPCR; S5 power-off by pattern | QEMU exits 0 after 0.19–0.21 s with no `isa-debug-exit` device |
| virtio 1.x PCI transport (`virtio_modern.rs`) | 209 new + 67 changed + 37 paging | Transitional and modern-only (`1af4:1041`) devices on PVH and UEFI; `test.sh` 164/164 (JIT) | Same split-ring code as legacy; 12-byte header; per-queue notify; FEATURES_OK; BAR above 4 GiB |
| NVMe probe (`nvme.rs`) | 219 | Identify controller + namespace, create I/O CQ/SQ, 4 KiB write/read/verify | Polled; one PRP per page. Pattern visible in the host image at LBA 8 |
| Intel GbE (`intel_gbe.rs`) | 239 | 82540EM (`e1000`), 82574L (`e1000e`), 82576 (`igb`, QEMU 10): reset, MAC, rings, ARP round trip with slirp | igb needed an MDIC autoneg restart (link up after ~495 ms) and advanced RX descriptors |
| arm64 sketch (`research/arm64-hello`) | 907 (787 code) | QEMU `virt` GICv2 + GICv3, EL1 and EL2 entry, 4 CPUs via PSCI hvc/smc; `raspi4b`, 4 CPUs via spin table, GIC-400 | 37 KB flat Image. Linux arm64 Image header; FDT parser; 1 GiB-block MMU; JIT-style code write + I-cache maintenance; PSCI SYSTEM_OFF |
| Secure Boot check | — | OVMF secboot + Microsoft keys: `failed to load Boot0001 … Access Denied` | Unsigned binaries need user action |

- **Kernel image growth** for ACPI + virtio 1.x + NVMe + GbE + `map_mmio`: ELF 697,224 → 717,392 B (+20 KB); loaded 670,650 → 688,506 B. [measured]
- **Legacy path:** the legacy-virtio `test.sh` JIT suite also passes on the same prototype kernel, 164/164. [measured]
- **Pace:** these ~2,450 lines took one working session to write. They are polled, single-queue happy paths. Production drivers with interrupts, error handling, multi-queue, link changes and hardware errata are typically **3–5× larger** (compare the OpenBSD sizes in §4.3) [estimate].

### 2.2 Boot time: PVH vs UEFI [measured]

`bootcmp.py`, 7 interleaved runs per configuration, `-m 64M -smp 1`, legacy virtio-net, load average 14.3–15.8.
- "wall" = QEMU process start → `k10-metal ready` on the serial console.
- "kernel" = kmain → ready, from the kernel.
- "vm→kmain" = kvmclock from VM creation to kernel entry, i.e. firmware + loader time.

| Path | wall median (min) | kernel | vm→kmain |
|---|---|---|---|
| PVH, SeaBIOS (`pc`) | 121 ms (97) | 5.1 ms | 58 ms |
| PVH, qboot (`pc`) | 114 ms (92) | 5.0 ms | 43 ms |
| UEFI, OVMF `-kernel k10boot.efi` (`pc`) | 3,594 ms (3,440) | 3.5 ms | 3,533 ms |
| UEFI, OVMF `-kernel` (`q35`) | 3,520 ms (3,479) | 3.3 ms | 3,465 ms |
| UEFI, OVMF 4M pflash + vars, boot from ESP via BdsDxe (`q35`) | 4,177 ms (3,992) | 3.4 ms | 4,105 ms |

- **Firmware variants.** OVMF time does not change with `-boot menu=off,splash-time=0`, the 4M CODE+VARS build or `-smp 4`: 3.3–3.9 s. It is firmware work, not a timeout. [measured]
- **Loader cost.** 50–66 Mcycles (17–23 ms at 2.9 GHz) with the verbose console on. GetMemoryMap + ExitBootServices alone is 32–39 Mcycles (11–13 ms). [measured]
- **Reading [estimate]:**
  - Kernel-side boot is unaffected by the boot path.
  - On real servers, POST takes tens of seconds, so firmware dominates and the kernel's ~3 ms is noise.
  - Fast boot matters only for VMs/microVMs, which keep PVH: QEMU `-kernel`, Firecracker, Cloud Hypervisor.

### 2.3 ACPI on QEMU firmwares [measured]

| Firmware / machine | Tables | PM1a_CNT | Reset register | MCFG |
|---|---|---|---|---|
| SeaBIOS `pc` (PVH) | FACP APIC HPET WAET (RSDT, rev 0) | 0x604 | none | — |
| SeaBIOS `q35` (PVH) | + MCFG | 0x604 | I/O 0xCF9 ← 0x0F | 0xB000_0000 bus 0–255 |
| OVMF `pc` (UEFI, XSDT rev 2) | FACP APIC HPET WAET | **0xB004** | none | — |
| OVMF `q35` | + MCFG | 0x604 | I/O 0xCF9 | 0xB000_0000 |

All four boots also showed:
- `\_S5` = (0, 0), found by the pattern scan.
- MADT with 4 enabled LAPICs (IDs 0–3) and 1 IOAPIC; the 8259 flag was set on `pc`.

### 2.4 arm64 sketch details [measured]

Runs on QEMU 10.0.2 TCG (no KVM for aarch64 on this host, so times are not meaningful).
- **`virt`, ELF boot:**
  - QEMU puts the DTB at the start of RAM (0x4000_0000). The image must be linked ≥ 2 MiB above it, or the DTB is silently not loaded.
  - With the Image header, QEMU passes x0 = DTB (0x4400_0000).
  - `virtualization=on` enters at EL2. The sketch drops to EL1 (HCR_EL2.RW, CNTHCTL_EL2, SPSR/ELR), and the DT then advertises PSCI via `smc`.
- **`raspi4b`:**
  - An ELF is entered at **EL3** with no firmware stub. A flat `kernel8.img` with the Image header is entered at EL2 like the real firmware, with x0 = 0x100.
  - Cores 1–3 are released by writing `secondary_entry` to 0xE0/0xE8/0xF0, then `dc civac` + `sev`.
  - The GIC-400 sits at 0xFF84_1000/0xFF84_2000 and the PL011 at 0xFE20_1000.
- **JIT test:** `movz x0,#42; ret` written to a data page, then DC CVAU + DSB ISH + IC IVAU + DSB ISH + ISB, then called: returns 42.
- **Sizes per component** (physical lines):

  | boot.rs (entry, EL drop, vectors) | fdt.rs | main.rs (bring-up / tests) | gic.rs (v2 + v3) | mmu.rs | smp.rs (PSCI + spin table) | timer.rs | uart.rs |
  |---|---|---|---|---|---|---|---|
  | 181 | 191 | 206 | 122 | 76 | 49 | 36 | 35 |

---

## 3. Boot and firmware (questions 1–4)

### 3.1 x86 boot path (Q1)

| Option | What it gives | Cost for k10-metal | Verdict |
|---|---|---|---|
| **Own UEFI loader → PVH handover** (prototype) | Runs on any UEFI PC, server or cloud. Memory map, RSDP, GOP and command line come from firmware. One PE file, signable | 551 lines [measured], no kernel change | **Primary path** |
| Kernel as its own PE/EFI stub | Same, one file | Kernel must be relocatable or loaded at a fixed address; mixes boot code into the kernel | Not needed: the loader can embed the kernel (prototype) or read it from the ESP |
| Multiboot2 (GRUB, Limine) | BIOS boot from disk. Without the EFI tags: 32-bit protected mode, like PVH. https://www.gnu.org/software/grub/manual/multiboot2/multiboot.html [doc] | ~150 lines: an MB2 header + a 32-bit shim converting the MB2 info (memory map, modules, cmdline, ACPI RSDP tags 14/15) into a PVH `start_info` [estimate] | **Secondary path**, for BIOS-only targets |
| Limine protocol | BIOS + UEFI, x86-64/aarch64/riscv64, BSD-2, SMP handoff, HHDM | Executable must be **higher-half** (≥ 0xffffffff80000000) and pointers are HHDM-offset. https://github.com/limine-bootloader/limine-protocol/blob/trunk/PROTOCOL.md [doc] | No. Use Limine as a *bootloader* via its Multiboot2 support if wanted |
| PVH (today) | QEMU `-kernel`, Firecracker, Cloud Hypervisor | — | **Keep**, for fast VM boot |

**Who needs which path:**
- **AWS:** UEFI and legacy BIOS on all Nitro instances (except bare metal, DL1, G4ad, P4, u-*tb1, VT1). Default: UEFI on Graviton, legacy BIOS on Intel/AMD. The AMI flag can be `uefi`, `legacy-bios` or `uefi-preferred`. https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/ami-boot.html [doc]
- **GCP:** imports need an MBR/hybrid image; Shielded VMs run Google's UEFI. https://docs.cloud.google.com/compute/docs/import/import-existing-image [doc]
- **Azure:** Gen1 is BIOS, Gen2 is UEFI, and v6 sizes are Gen2-only. [doc]
- **DigitalOcean:** BIOS only. [doc]
- **No public cloud boots a customer kernel directly** (PVH / `-kernel`) [estimate: not stated anywhere, inferred from the boot-mode docs].
- **So:** UEFI covers Graviton, GCP Shielded, Azure Gen2 and `uefi` AMIs. BIOS (Multiboot2 via GRUB/Limine on the disk image) covers the rest.
- **Legacy BIOS on real hardware:** Intel planned "Class 3 UEFI" (no CSM) by 2020 (https://www.phoronix.com/news/Intel-Legacy-BIOS-EOL-2020 [blog/forum]). HPE Gen11 still documents legacy options [doc]. BIOS matters mainly for clouds and older servers.

**Loader gotchas, seen or handled in the prototype [measured]:**
- Claim the kernel range with `AllocatePages(AllocateAddress)`; 1 MiB was free under OVMF.
- Keep the stub, GDT and `start_info` below 4 GiB.
- Clear CR4.PCIDE before clearing CR0.PG.
- Coalesce the memory map.
- Retry ExitBootServices if the map key goes stale (0 retries were needed).
- The AP trampoline page 0x8000 is not reserved by the loader yet. It should be allocated with `AllocatePages(AllocateAddress)` too [estimate].

### 3.2 Secure Boot (Q1)

**Measured:** OVMF with Secure Boot on and Microsoft keys enrolled refuses `k10boot.efi` ("Access Denied").

**Paths to a signature:**
- **Microsoft-signed shim [doc]:**
  - https://github.com/rhboot/shim-review: reproducible build from shim 16.x, SBAT, NX, a lockdown-enforcing kernel, HSM-held keys, stated wait "2 to 3 months". Reviewers want "strong justification" for anything other than GRUB2/systemd-boot.
  - Microsoft side: EV code-signing certificate and Partner Center. https://learn.microsoft.com/en-us/windows-hardware/drivers/dashboard/file-signing-reqs
  - Since 27 Jun 2026, new signatures use only the "Microsoft UEFI CA 2023". Machines whose db lacks it refuse them. https://wiki.almalinux.org/documentation/secure-boot-2023-certificates.html
  - Secured-core PCs ship with the third-party CA disabled. https://www.theregister.com/2022/07/11/lenovo_secured_core/ [blog/forum]
  - Expected scrutiny: a kernel that JIT-compiles into ring 0 is the opposite of what lockdown policies expect [estimate].
- **User-owned keys:**
  - `sbctl enroll-keys -m` in Setup Mode (keep the Microsoft certs, or option ROMs may stop working). https://github.com/Foxboron/sbctl [doc]
  - Or a MOK under a distro shim [estimate].
- **Clouds:**
  - AWS: Secure Boot is off, and the AMI is in Setup Mode by default. https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/how-uefi-secure-boot-works.html [doc]
  - GCP: not enabled by default. https://docs.cloud.google.com/compute/shielded-vm/docs/modifying-shielded-vm [doc]
  - Azure: "Trusted Launch as default" (Secure Boot + vTPM for new Gen2 VMs) since July 2026, with opt-out. https://mc.merill.net/message/AZ568600 [survey/data]
  - Hetzner Cloud: no Secure Boot. [doc]

**Recommendation:**
- Unsigned by default.
- An `sbctl`-based enrollment guide for bare metal.
- Opt out of Trusted Launch on Azure.
- Revisit signing only if a customer requires it. Cost: ~US$300–900/yr for the EV certificate, plus 2–3+ months of shim review [estimate].

### 3.3 ARM boot (Q2)

**Raspberry Pi firmware [doc]:**
- **Kernel files:** `kernel8.img` on Pi 3/4; `kernel_2712.img`, falling back to `kernel8.img`, on Pi 5. `arm_64bit=1` (default on Pi 4, mandatory on Pi 5). https://www.raspberrypi.com/documentation/computers/config_txt.html
- **Load address:** header kernels load at 0x200000 (honouring text_offset); flat images at 0x80000. https://github.com/raspberrypi/firmware/issues/1415 [blog/forum, RPi engineers]
- **Entry state:** x0 = DTB, EL2, MMU off. https://docs.kernel.org/arch/arm64/booting.html
- **Pi 3/4 secondaries:** spin table at 0xd8/0xe0/0xe8/0xf0. https://raw.githubusercontent.com/raspberrypi/tools/master/armstubs/armstub8.S
- **Pi 5:** TF-A BL31 provides PSCI. https://tf-a.docs.trustedfirmware.org/en/latest/plat/rpi5.html
- **Measured** on QEMU `raspi4b`: firmware-style EL2 entry and spin-table release work with the sketch.

**UEFI on Pi:**
- **Pi 4:** pftf/RPi4 is still released (v1.53, 2026-08). ACPI by default; 3 GB RAM limit by default (Broadcom DMA bug); GENET supported. https://github.com/pftf/RPi4 [doc]
- **Pi 5:** worproject/rpi5-uefi is **archived** and has no RP1 Ethernet. https://github.com/worproject/rpi5-uefi [doc]
- **U-Boot on Pi 5:** serial and SD only; the PCIe series was landing in mid-2026; no RP1 Ethernet driver. https://ratatoskr.run/u-boot/2026/06/17071941/t [blog/forum]

**Servers:** Arm SystemReady (SRS 3.0) = BSA + SBSA + SBBR = UEFI + ACPI. SystemReady Devicetree = EBBR (UEFI subset + DT). https://uefi.org/sites/default/files/resources/15.%20Integrate%20Arm%20SystemReady_Arm_Wang_FINAL.pdf [doc] Graviton boots UEFI by default. [doc]

**Recommendation (single path):**
- One arm64 **Linux-format `Image`** (header measured working) **with a PE/COFF header** (EFI stub; to do).
- It boots raw from Pi firmware (the best path on Pi 5), from U-Boot `booti`/`bootefi`, and from UEFI on SystemReady servers, Graviton and pftf Pi 4.
- The kernel accepts both **DT** (Pi firmware, U-Boot) and **ACPI** (servers, pftf) from x0 or the EFI configuration table.

### 3.4 ACPI without AML (Q3)

**Table subset to parse** (https://uefi.org/specs/ACPI/6.5/ [doc]):

| Table | What k10-metal needs from it | Where |
|---|---|---|
| RSDP / XSDT | entry point | [prototype] |
| MADT | LAPIC/x2APIC/IOAPIC/ISO; GICC/GICD/GICR/ITS on arm64 | [prototype, x86 entries] |
| MCFG | ECAM | [prototype] |
| FADT | reset register, PM1 control, PM timer, SCI, `HW_REDUCED`, `SMI_CMD`/`ACPI_ENABLE`, `IAPC_BOOT_ARCH` | [prototype] |
| HPET | timer fallback | [prototype] |
| SPCR | console port (rev 4: subtype, GAS, baud). https://learn.microsoft.com/en-us/windows-hardware/drivers/bringup/serial-port-console-redirection-table | [prototype: presence + base] |
| SRAT/SLIT | NUMA | — |
| DMAR / IVRS / IORT | IOMMU | — |
| GTDT | arm64 timers, SBSA watchdog | — |

Prior art: Theseus parses RSDP/RSDT/MADT/FADT/HPET/DMAR/WAET in ~2.0k lines of Rust (https://github.com/theseus-os/Theseus [doc]); our subset is 324 lines.

**When AML is unavoidable [doc + estimate]:**
- **S5:**
  - SLP_TYP lives in `\_S5`. The byte-pattern scan works on QEMU [measured] and is what iPXE `acpipwr.c` and Minix do.
  - Caveat: skipping `_PTS` can hang some machines. https://wiki.osdev.org/Shutdown [blog/forum]
  - Fallbacks: the reset register, then 0xCF9, then the BMC.
- **PCI INTx routing** (`_PRT`): avoid it by using **MSI/MSI-X only**. Every device in our top 10 supports MSI-X.
- **`_OSC`:** native hotplug, AER and PME stay with the firmware. Fine for us.
- **Laptops, batteries, EC, thermal zones, GPIO/I2C devices on ARM ACPI:** Tier 1.

**Small AML options [doc]:**
- **uACPI:** MIT, ~35k lines C, 54 kernel hooks, barebones and reduced-hardware modes, actively maintained. https://github.com/uACPI/uACPI
- **LAI:** MIT, ~10.7k lines, superseded in practice. https://github.com/managarm/lai
- **Rust `acpi` 6.x:** MIT/Apache, AML merged in; still lists unimplemented features and "panic-free interpretation" as a TODO. https://github.com/rust-osdev/acpi
- **ACPICA:** BSD/GPL dual, ~100k lines.
- **Pick uACPI** if AML is ever needed.

### 3.5 Devicetree subset (Q4)

**Needed nodes and properties:**
- `/memory`, `/reserved-memory`, `/chosen` (`stdout-path`, `bootargs`).
- `/cpus/cpu@N`: `reg` = MPIDR; `enable-method` = `psci` or `spin-table` + `cpu-release-addr`; `/psci` `method`.
- GIC: `arm,gic-400` / `arm,cortex-a15-gic` (v2) or `arm,gic-v3` (+ ITS).
- `arm,armv8-timer`: PPI 30 for the EL1 physical timer.
- UART: `arm,pl011`, `brcm,bcm2835-aux-uart` or `ns16550a`.
- **`ranges` / `dma-ranges` translation.**

**Measured:** the sketch's 191-line FDT reader handles everything above except `ranges`. Pi device trees describe peripherals at bus addresses under `/soc` (0x7e… → 0xfe… on Pi 4), so a real port needs `ranges` and `dma-ranges` (~100 lines [estimate]).

**Pi specifics [doc]:**
- The PCIe host bridges (`brcm,bcm2711-pcie`, `brcm,bcm2712-pcie`) need a brcmstb driver (Linux 2,216 lines; U-Boot 575; OpenBSD 976 [measured counts]), not generic ECAM.
- Pi 3 has **no GIC** (`bcm2836-l1-intc`).
- Crates: `fdt` (MPL-2.0), `fdt-rs` (MIT). Our own reader is small enough to keep.

---

## 4. Hardware coverage (questions 5–7)

### 4.1 What each device class contains

**Clouds.** Synergy Q2 2026: AWS 28%, Microsoft 20%, Google 15%. https://www.srgresearch.com/articles/q2-cloud-market-passes-143-billion-highest-growth-rate-in-eight-years [survey/data]

| Platform | Guest NIC | Guest disk | Source |
|---|---|---|---|
| AWS Nitro (x86 + Graviton) | ENA (1d0f:0ec2/1ec2/ec20/ec21); a usable driver needs LLQ + write-combining BAR (DPDK: disabling LLQ = "huge performance degradation") | NVMe (EBS and instance store) | https://docs.aws.amazon.com/ec2/latest/instancetypes/ec2-nitro-instances.html , https://github.com/amzn/amzn-drivers/blob/master/kernel/linux/ena/README.rst , https://doc.dpdk.org/guides/nics/ena.html [doc] |
| AWS Xen (T2, D2, I3, X1, older) | netfront or 82599 VF (ixgbevf) | blkfront | https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/enhanced-networking.html [doc] |
| GCP gen 3+ (C3, C4, N4, H3, …) | **gVNIC only** (1ae0:0042) | **NVMe only** | https://docs.cloud.google.com/compute/docs/networking/using-gvnic , https://docs.cloud.google.com/compute/docs/images/building-custom-os [doc] |
| GCP gen 1/2 (N1, N2, E2, C2, …) | virtio-net **1af4:1000 (legacy/transitional)** | virtio-scsi (NVMe on T2A/M3/Confidential) | same [doc] |
| GCP bare metal | Intel IDPF | NVMe | https://docs.cloud.google.com/compute/docs/instances/bare-metal-instances [doc] |
| Azure (all) | Hyper-V **VMBus netvsc** always. Accelerated Networking adds an SR-IOV VF (ConnectX mlx4/mlx5 or MANA) bonded to netvsc and enumerated through hv_pci over VMBus | storvsc (SCSI) up to v5; **NVMe only** from v6 (Gen2) | https://learn.microsoft.com/en-us/azure/virtual-network/accelerated-networking-how-it-works , https://learn.microsoft.com/en-us/azure/virtual-machines/nvme-overview [doc] |
| OCI | virtio (paravirtualized default), VFIO VF, "accelerated virtio" | paravirtualized / iSCSI | https://docs.oracle.com/en-us/iaas/Content/Compute/Tasks/edit-launch-options.htm [doc] |
| Hetzner Cloud, Vultr | **modern-only virtio 1af4:1041**, virtio-scsi | | https://wiki.gentoo.org/wiki/Hetzner_Cloud_(ARM64) , https://openbenchmarking.org/system/2304197-EDGE-V28248818/Intel%20Core%20-%20bochs-drmdrmfb%20-%20Vultr%20VC2/lspci [survey/data] |
| DigitalOcean, Linode, Scaleway, OVH | virtio (transitional likely) | virtio-blk/scsi | [doc/estimate] |
| Firecracker | virtio-**mmio v2** (modern) by default; virtio-pci (modern, MSI-X only) opt-in since 1.13 | virtio-blk | https://github.com/firecracker-microvm/firecracker/blob/main/CHANGELOG.md [doc] |
| Cloud Hypervisor | virtio-pci modern only | virtio-blk | https://github.com/cloud-hypervisor/cloud-hypervisor/blob/main/docs/device_model.md [doc] |
| QEMU microvm | virtio-mmio, `force-legacy` = true by default | | https://www.qemu.org/docs/master/system/i386/microvm.html [doc] |

QEMU puts a virtio-pci device into modern-only mode automatically when it sits behind a PCIe port; on the root bus it is transitional (hw/virtio/virtio-pci.c) [doc]. **Implement modern first; keep legacy (already written) for GCP gen 1/2 and microvm.**

**Rack servers.** Server takeaway [estimate from the docs above]: the 1 GbE port present everywhere is **BCM5719/5720 (tg3)**, then **I350 (igb)**. 10/25G is a four-way split: bnxt, ice/i40e, mlx5, ixgbe.

| OEM / generation | Default or common 1 GbE | Common 10/25G | Storage → Linux driver |
|---|---|---|---|
| HPE DL360/380 Gen10 | 331i = **BCM5719** (tg3) on some models, or FlexibleLOM I350 | BCM57414/57416 (bnxt), X710 (i40e), CX-4 Lx (mlx5) | Embedded SATA = **AHCI** (S100i SW RAID is Windows-only); Smart Array P408i etc. = **smartpqi**; NS204i NVMe boot |
| HPE Gen10 Plus / Gen11 | No embedded NIC; OCP3: BCM5719 4x1G, I350-T4 | BCM57416/57414/57504, E810 (ice), CX-6 Lx | AHCI/VROC; MR216i/416i (megaraid_sas) [estimate]; SR932i (smartpqi) |
| Dell 14G (R640/R740) | rNDC: **BCM5720**, I350 | BCM57412, X710/XXV710, 82599 | PERC H730P/H740P = **megaraid_sas**; HBA330 = mpt3sas; BOSS-S1 = Marvell 88SE9230 = **AHCI** |
| Dell 15G/16G (R650/R760) | optional LOM 2x1G **BCM5720** + OCP 3.0 | Broadcom, Intel, Mellanox OCP | H755 (megaraid_sas); **H965i = mpi3mr**; BOSS-N1 = **NVMe** |
| Dell 17G (R670/R770) | no LOM, 2x OCP | 1–400G, BlueField-3 | H965i/H975i (mpi3mr), BOSS-N1 |
| Lenovo SR630/650 V2/V3 | no onboard data NIC; OCP: **5719**, I350 | 57416/57414/57454/57504, E810, CX-6 Lx | Onboard SATA/NVMe with VROC (AHCI/NVMe underneath); RAID 5xx/9xx (megaraid_sas), x350 (smartpqi) |
| Supermicro | H12/H13 (EPYC): **BCM5720**; X11: X722 (i40e); X12: X550 (ixgbe); X13: BCM57416 | — | C6xx SATA (AHCI), NVMe |

Server sources [doc]: DL380 Gen10 QuickSpecs https://www.hpe.com/psnow/doc/a00008180enw ; DL380 Gen11 QuickSpecs https://indico.psi.ch/event/15441/attachments/26613/49651/HPE%20ProLiant%20DL380%20Gen11-QuickSpecs.pdf ; HPE tg3 package listing 331i https://downloads.linux.hpe.com/downloads/ServicePackforProLiant/RedHatEnterpriseServer/6.6Server/i686/current/hp-tg3-3.122g-2.src.txt ; Dell R650 / R660 technical guides https://i.dell.com/sites/csdocuments/Product_Docs/en/poweredge-r650-technical-guide.pdf , https://www.delltechnologies.com/asset/en-us/products/servers/technical-support/poweredge-r660-technical-guide.pdf ; Lenovo SR650 V3 https://lenovopress.lenovo.com/lp1601-thinksystem-sr650-v3-server ; Supermicro H12SSL-i https://www.supermicro.com/en/products/motherboard/H12SSL-i ; PCI IDs via https://admin.pci-ids.ucw.cz/read/PC/1000/00a5 and https://cateee.net/lkddb/web-lkddb/SCSI_MPI3MR.html [survey/data].

**Server storage market** [survey/data]: Trendfocus 3Q24 enterprise SSDs were NVMe ≈ 77% of units and ≈ 90% of capacity. https://www.storagenewsletter.com/2024/11/12/ssd-unit-shipments-decreased-by-8-q-q-to-62-443-million-in-3q24/

**Desktops, workstations and mini-PCs** [survey/data]. linux-hardware.org, last 12 months (n = 45,608 controller entries; self-selected Linux users, so DIY and older PCs are over-represented). https://linux-hardware.org/?view=net_ethernet_model&period=-2

| Ethernet driver family | all | desktops | mini PCs |
|---|---|---|---|
| r8169-class Realtek (RTL8111/8168, RTL8125 2.5G, RTL8126 5G, 810x) | **51.0%** | 57.2% | 49.1% |
| e1000e (I217/I218/I219 PCH, 82574, …) | **18.9%** | 16.9% | 21.7% |
| igc (I225/I226) | 3.6% | 6.3% | 7.1% |
| igb (I210/I211/I350) | 3.0% | 4.9% | 0.8% |
| tg3 | 3.7% | 1.4% | 7.8% |
| Atheros (alx/atl1c) | 3.0% | 2.0% | 0 |
| USB (r8152, ASIX) | 6.5% | 2.3% | 4.7% |

- **Business OEM desktops** (Lenovo M70q/M90q Gen 4/5, Dell OptiPlex Micro 7020, HP Elite Mini 800 G9 / Pro SFF 400 G9) use **I219-LM** (e1000e), often 8086:1a1c; RTL8125BGS is an option. **NUC 13 Pro / N100 boxes** use **I226** (igc). [doc] e.g. https://psref.lenovo.com/syspool/Sys/PDF/ThinkCentre/ThinkCentre_M70q_Gen_5/ThinkCentre_M70q_Gen_5_Spec.pdf , https://ubuntu.com/certified/202304-31496
- **Storage controllers, 12 months:** SATA/AHCI 48.9%, NVMe 39.9%, RAID-mode 6.7% (Intel VMD 2.9%, RST SATA RAID ~5%), IDE 3.9%. With VMD enabled, NVMe drives are hidden behind the VMD device. https://linux-hardware.org/?view=storage_kind&period=-2 [survey/data]
- **Drives, 12 months:** NVMe 40.3%, SATA SSD 30.3%, HDD 23.1%. [survey/data]

**Raspberry Pi** [doc]. Sales: 7.6M boards/modules in 2025 (7.0M in 2024); >77M cumulative; H1 2026: 4.2M. https://www.investegate.co.uk/announcement/rns/raspberry-pi-holdings-wi---rpi/fy-2025-results/9499135

| Model | Ethernet | Storage | Other |
|---|---|---|---|
| Pi 3 B / B+ | LAN9514 / LAN7515 **over USB** (DWC2) | SD (sdhost / Arasan) | No GIC |
| Pi 4 / 400 / CM4 | **GENET v5** (`brcm,bcm2711-genet-v5`) + BCM54213PE PHY | EMMC2 (`sdhci-iproc`) | USB via VL805 xHCI behind brcmstb PCIe; firmware-load mailbox |
| Pi 5 / CM5 | **RP1 Cadence GEM** (`raspberrypi,rp1-gem`, macb) behind PCIe x4 | `sdhci-brcmstb`; NVMe via the M.2 HAT (PCIe Gen2 x1) | RP1 also carries USB (2x DWC3) and the GPIO UARTs; mainline RP1 Ethernet still in review in late 2025 |

Sources: https://github.com/torvalds/linux/tree/master/arch/arm64/boot/dts/broadcom , https://datasheets.raspberrypi.com/rp1/rp1-peripherals.pdf

### 4.2 Driver sizes [measured]

**Method.** Code lines (tokei: non-blank, non-comment; C, headers and Rust only). Commits counted: Linux 7.3 c03c1a14507d (2026-10-18), FreeBSD f19009020a33, OpenBSD 77f6ee04c2b5, plus Redox, Hermit, Theseus, rcore virtio-drivers, iPXE, U-Boot and DPDK at their 2026-10 heads. Clones and the counting script are in `/home/qgerard/dev/k10s-research/native-hardware/ref/`, raw table in `counts.tsv`.

**"Small ref"** is the best existing small driver. iPXE and U-Boot drivers are polled, single-queue and lean on firmware state, so they are lower bounds.

| Driver | Linux | FreeBSD | OpenBSD | Small ref | k10 prototype |
|---|---|---|---|---|---|
| virtio core + pci + net | 3,152 + 2,482 + 5,494 | 1,392 + 2,536 + 4,370 | 932 + 1,049 + 1,712 | virtio-drivers (Rust) queue 1,027 + transport 1,770 + net 473; U-Boot whole virtio 2,786 | legacy 443 + modern 209 |
| virtio-blk | 1,304 | 1,457 | 607 | virtio-drivers 622 | — |
| NVMe | 8,410 (core+pci) | 7,114 | **2,345** | Redox nvmed 1,051; sDDF 1,718 | **219** |
| AHCI | 4,542 + libata 12,421 | 3,651 | 3,251 | U-Boot 722; Redox 968 | — |
| e1000e (I219 PCH) | 19,782 (ich8lan 3,909) | em/igb dir 33,049 | em 14,601 | iPXE intel 933 (8254x … I219, I225) | **239** (82540/82574/82576) |
| igb | 19,586 | (shared) | (em) | iPXE intel 933 | (above) |
| igc | 12,577 | 8,524 | 6,065 | iPXE intel | — |
| r8169 (8111/8125/8126) | 6,028 | re 3,858 | re 2,803 + rge 4,290 | U-Boot 844; Redox 369 | — |
| tg3 (5719/5720) | 16,909 | bge 7,427 | bge **5,863** | iPXE 7,437 | — |
| AWS ENA | 9,778 | 12,466 | — | iPXE **1,267**; DPDK 10,296 | — |
| GCP gve | 10,822 | 6,681 | — | iPXE **1,543** | — |
| Hyper-V VMBus + netvsc + storvsc | 5,663 + 6,320 + 1,433 | 5,959 + 7,977 + 1,883 | **1,954 + 1,680 + 729** | iPXE netvsc+vmbus 1,810 | — |
| Azure MANA | 7,129 (+1,781) | 6,458 | — | DPDK 4,221 | — |
| i40e / ice | 38,871 / 82,137 | 30,379 / 57,399 | 9,629 / 36,583 | iPXE intelxl 1,522 / ice 844 + 1,522 | — |
| bnxt_en | 31,626 | 51,872 (34k HSI headers) | 3,509 .c + 28,753 HSI | iPXE 2,390 + HSI | — |
| mlx5 | 56,491 (top level) | 44,704 | **mcx 7,463** | Theseus (Rust) 3,100 | — |
| GENET (Pi 4) | 4,704 | 1,625 | bse 995 | U-Boot **529**; sDDF 822 | — |
| Cadence GEM (Pi 5) | 6,382 | cgem 1,837 | cad 1,633 | iPXE **441**; Hermit 646 | — |
| brcmstb PCIe (Pi 4/5) | 1,607 | 537 | 976 | U-Boot 575 | — |
| SDHCI + glue | 4,407 + mmc 17,433 | 2,675 | sdhc 1,677 + sdmmc 2,737 | U-Boot 849 + 857 | — |
| xHCI + USB core | 12,848 + 18,155 | 4,368 | 2,999 + 4,711 | iPXE 2,535 + 1,269 | — |
| megaraid_sas / smartpqi / mpt3sas | 14,479 / 11,756 / 31,971 | 9,860 / 12,165 / 20,401 | mfii 3,154 / — / mpii 4,275 | — | — |

**Firmware blobs at runtime** [measured: grep of `MODULE_FIRMWARE`/`request_firmware` in Linux 7.3, cross-checked with BSD drivers]:
- **None needed:** e1000/e1000e/igb/igc, ixgbe, GENET, macb, ENA, gve, MANA, netvsc, vmxnet3, NVMe, AHCI, megaraid_sas, mpt3sas, SDHCI.
- **tg3:** BCM5719/5720 need **none**. Only 5701 A0, 5703–5705 firmware-TSO and 57766 do.
- **Optional:**
  - r8169 `rtl_nic/*.fw` PHY/MCU patches. Linux continues if the load fails; the BSDs compile the tables in.
  - ice DDP package. Without it the driver runs in "Safe Mode" with 1 TX/RX queue and no RSS.
  - i40e DDP profiles.
- **bnxt and mlx5:** firmware lives on the NIC, and the driver speaks HWRM or the command queue.
- **Pi 4:** the VL805 xHCI firmware is loaded by the VideoCore through a mailbox call (`RPI_FIRMWARE_NOTIFY_XHCI_RESET`).
- **Pi 5:** RP1 runs from ROM.

**Licensing:**
- **Read only (GPL):** Linux (GPL-2.0), U-Boot (GPL-2.0+), and iPXE tg3/bnxt/golan (GPL2-only or later).
- **Usable as references (BSD-family):**
  - OpenBSD: mostly ISC; em/ix/igc BSD-3; bge/re BSD-4.
  - FreeBSD: BSD-2/3, bge/re BSD-4.
  - Redox, Theseus, rcore virtio-drivers, ixy.rs: MIT. Hermit: MIT/Apache.
  - Asterinas, `fdt`: MPL-2.0. Unikraft, DPDK: BSD-3.
  - sDDF drivers: BSD-2.
- **Practical rule:** write from datasheets and the BSD/MIT drivers, cite them, and never paste GPL code. Realtek datasheets are under NDA, so the BSD re/rge drivers are the practical reference.

### 4.3 Coverage matrix

Sizes as Linux / best small reference. "QEMU" = can the driver be developed against an emulated model.

| Device class | NIC | Storage | Drivers | Size (Linux / small) | Firmware | QEMU |
|---|---|---|---|---|---|---|
| KVM VMs, small clouds, OCI-PV, Proxmox | virtio-net modern + legacy | virtio-blk / virtio-scsi | virtio | ~11k / ~3k | — | **yes** [measured] |
| Firecracker / CH / microvm | virtio-mmio v2 / v1, virtio-pci | virtio-blk | virtio-mmio | 576 / 436 | — | yes (microvm) |
| GCP gen 1/2 | virtio-net legacy | virtio-scsi | virtio-scsi | — | — | yes |
| GCP gen 3+ | gVNIC | NVMe | gve, nvme | 10.8k / 1.5k; 8.4k / 1.05k | — | NVMe only |
| AWS Nitro x86 + Graviton | ENA (LLQ) | NVMe | ena, nvme | 9.8k / 1.3k | — | NVMe only |
| Azure (all, v6) | VMBus netvsc (+ MANA / mlx5 VF) | storvsc / NVMe (over hv_pci) | vmbus, hvn, hvs | 13.4k / 4.4k (OpenBSD) | — | **no** (QEMU vmbus-bridge has no backends) |
| VMware | vmxnet3 | pvscsi | vmxnet3 | 6.2k / 1.85k | — | yes |
| Business desktop / NUC | **I219-LM** (e1000e PCH), I226 (igc) | NVMe, AHCI | e1000e, igc | 19.8k / 0.9k | — | partial (82574 ≠ I219) [measured] |
| Consumer PC / mini-PC | **RTL8111/8125/8126** | NVMe, AHCI | r8169-class | 6.0k / 0.84k | optional | **no** (only rtl8139) |
| Rack server, 1G | **BCM5719/5720** (tg3), I350 (igb) | AHCI / NVMe; RAID: megaraid_sas, smartpqi, mpi3mr | tg3, igb | 16.9k / 5.9k; igb in Intel family | — | igb yes (QEMU ≥ 8) [measured] |
| Rack server, 10/25G+ | bnxt, ice/i40e, mlx5, ixgbe | — | — | 31–154k / 2.4–7.5k | ice DDP optional | no |
| Pi 4 / CM4 | GENET | SD (EMMC2) | genet, sdhci-iproc | 4.7k / 0.5k; SDHCI ~1.7k | — | **no** (raspi4b lacks GENET/PCIe); SD yes |
| Pi 5 / CM5 | RP1 GEM | SD (sdhci-brcmstb), NVMe HAT | brcmstb PCIe, RP1, macb, sdhci | ~20k / ~4k | — | no Pi 5 machine; GEM via Xilinx machines |
| Pi 3 | USB LAN9514 / LAN7515 | SD | dwc2 + USB core + smsc95xx/lan78xx | ~38k / ~6k | — | partial (raspi3b dwc2, no LAN chip) → **Tier 1** |

### 4.4 Top 10 drivers (Q7)

Ranked by coverage ÷ effort. Effort is for a production-quality driver with interrupts, MSI-X and error paths, from the small references ×1.5–3 [estimate]. "k10 lines" is the estimated driver size in k10's own Rust.

| # | Driver | Covers | k10 lines | Effort | Test |
|---|---|---|---|---|---|
| 1 | **virtio** (modern pci + legacy + mmio v1/v2; net, blk, scsi) | All KVM/virtio clouds and hypervisors, GCP gen 1/2, microVMs | ~2.5–3k (≈700 exist) | 3–4 wk | QEMU |
| 2 | **NVMe** (MSI-X, multi-queue later) | AWS (all disks), GCP gen 3+, Azure v6, 40% of PC controllers, ~77% of enterprise SSD units, Pi 5 HAT | ~1.2–1.8k (219 exist) | 2–3 wk | QEMU |
| 3 | **AHCI** (+ minimal ATA) | 49% of PC controllers, server embedded SATA, Dell BOSS-S1/S2 | ~1.2–1.5k | 2–3 wk | QEMU (ich9-ahci) |
| 4 | **Intel GbE family** (e1000e incl. I219 PCH, igb, igc) | ~19% + 3% + 4% of PC NICs; business desktops; I350 server option | ~2.5–4k (239 exist) | 4–6 wk | QEMU partial + real I219/I226/I350 |
| 5 | **Realtek r8169-class** (8111/8168/8125/8126) | ~51% of PC NICs | ~2–3k | 4–6 wk (59 chip revisions) | real hardware only |
| 6 | **AWS ENA** (admin queue, AENQ, LLQ + WC) | AWS Nitro (28% of cloud) | ~2–3k | 4–5 wk | EC2 only |
| 7 | **Hyper-V VMBus + netvsc + storvsc** | All of Azure (20%); on-prem Hyper-V | ~4–5k | 6–8 wk | Azure / Hyper-V only |
| 8 | **gVNIC** (GQI or DQO) | GCP gen 3+ (most new GCP) | ~2–3k | 4–5 wk | GCP only |
| 9 | **tg3** (BCM5719/5720 only) | 1G LOM/OCP on Dell/HPE/Lenovo/Supermicro AMD; ~4% of PCs | ~4–5k | 5–7 wk (errata) | real server |
| 10 | **Pi set:** GENET + SDHCI (Pi 4); brcmstb PCIe + RP1 + GEM (Pi 5) | Pi 4/5/CM4/CM5 (7.6M units/yr, all models) | ~3–5k | 6–9 wk | real Pi; arm64 port first |

**Share covered by the ten (a qualitative union of the classes) [survey/data + estimate]:**
- **PC Ethernet:** r8169 51.0 + e1000e 18.9 + igc 3.6 + igb 3.0 + tg3 3.7 = **≈80%** of the 12-month controller entries.
- **PC storage:** AHCI 48.9 + NVMe 39.9 = **≈89%** of controller entries (VMD and RAID mode excluded).
- **Clouds:** NIC and disk on AWS + Azure + GCP (**63%** of the market by revenue), plus all KVM/virtio providers and microVMs.
- **Rack servers:** the 1 GbE port on all four OEMs' current generations, and every embedded/boot storage path (AHCI, NVMe). Not covered: 10/25G-only configurations and hardware-RAID data disks, unless the controller is in HBA/AHCI/NVMe mode.

**Next tier (on demand):** ixgbe/i40e/ice (iPXE does i40e + ice in ~2.4k without DDP), bnxt, mlx5 (OpenBSD mcx 7.5k; also the Azure VF), megaraid_sas (mfii 3.2k), mpi3mr, smartpqi (~12k, HPE only), xHCI + USB (5–8k), MANA.

---

## 5. Alternatives to writing every driver (questions 8–10)

### 5.1 Driver reuse (Q8)

| Option | Footprint | Licence | Status | Verdict |
|---|---|---|---|---|
| NetBSD rump kernels / rumprun | NetBSD kernel subsystems + libc glue; MBs | BSD | rumprun's last push was 2020-05 (unmaintained); in-tree rump lives on. https://github.com/rumpkernel/rumprun [doc] | No: too large, wrong model (POSIX glue), dead integration project |
| LKL (Linux as a library) | Linux core + drivers | **GPL-2.0**. Linking it into Apache-2.0 k10-metal makes the combined binary GPL-2.0, which Apache-2.0 is incompatible with. https://www.apache.org/licenses/GPL-compatibility.html [doc] | Out of tree, active | No |
| DDE / Genode `dde_linux` | per-driver Linux emulation + generated stubs | GPL (Linux code) | Active in Genode. https://genode.org/documentation/release-notes/21.08 [doc] | No (GPL, heavy C) |
| Driver VM ("Tier 1 inside Tier 2") | Linux guest + virtio back-ends | GPL stays inside the VM | seL4 does this: "a few days" per device class plus tuning. https://trustworthy.systems/projects/drivers [doc] | Not now. k10-metal would need a hypervisor (VT-x/EL2), and a machine that needs this is better served by booting Tier 1 directly |
| Virtio front-ends to a host | Only virtio in k10-metal | — | Already how VMs work | **Yes, implicitly:** every cloud or hypervisor host is our driver VM |

### 5.2 Drivers as isolated k10s workloads (Q9)

**Feasibility.** A TIR module can only touch its own slot and call granted host functions. A driver workload needs:
1. **MMIO capability.** Host functions `mmio_read{32,64}(bar, off)` / `mmio_write…`, bounds-checked against the granted BAR. Each costs one host call. The JIT host-call path is fast: `hello` with one host call costs 0.17 µs of CPU in total [code: RESULTS.md §6].
2. **DMA memory.** Slots are already **physically contiguous, identity-mapped frames** (`2^k + 4096` bytes) [code]. Rings and buffers can live inside the driver's slot, and the bus address is slot base + offset. A `dma_region()` host call returns that base.
3. **Interrupts and fences.** MSI-X vectors become wake-ups of a driver invocation (k10's run-to-completion and preemption model fits polled drivers). A `fence()` host call, needed on arm64 for device ordering (DMB OSH).
4. **Networking glue.** The driver hands frames to the in-kernel smoltcp through a shared-ring host API. Same-node calls are already a `memcpy` [code: DESIGN.md §2.6].

**DMA safety.**
- Without an IOMMU, a buggy driver can program the device to DMA anywhere. Memory isolation then covers only the CPU side, so fault isolation is **not** achieved.
- With an IOMMU (VT-d DMAR, AMD-Vi IVRS, Arm SMMUv3 IORT), each device's requester ID is given a domain that maps only its driver's slot.
- Cost of a minimal IOMMU driver: Asterinas VT-d is 2,287 lines of Rust (MPL-2.0), Fuchsia's Arm SMMU ~5.1k C++, Linux arm-smmu-v3 5,667. QEMU emulates `intel-iommu`, `amd-iommu`, `virtio-iommu` and `virt,iommu=smmuv3` [doc].
- Servers with more than 255 threads need VT-d interrupt remapping for x2APIC anyway [estimate].

**Is it worth it? [estimate]**
- **Benefits:**
  - (a) **Portability:** TIR is architecture-neutral, so one driver binary runs on x86-64 and arm64, and is distributed like any blob through gossip.
  - (b) A driver fault becomes a trapped invocation instead of a kernel crash.
  - (c) It matches the seL4 sDDF / Fuchsia / Redox model. sDDF shows small user-level drivers can be fast: ~65% CPU at 1 GbE line rate on Arm, Linux CPU-bound at ~500 Mb/s. https://trustworthy.systems/projects/drivers [doc]
- **Costs:**
  - A host-call per register access.
  - A TIR driver language surface: Ten is C-like enough.
  - An IOMMU driver per architecture before the safety claim is true.
- **Recommendation:**
  - Keep the first-tier drivers (virtio, NVMe, AHCI, Intel, Realtek) in Rust in the kernel. They are on the boot and performance path.
  - Define the in-kernel driver interface now (`NetDevice` / `BlockDevice` traits, DMA allocation from slot-like contiguous regions), so that a "TIR driver host" can implement the same traits later.
  - Build the IOMMU support in M4, then pilot one long-tail driver as a TIR workload.

### 5.3 Where Tier 1 is simply better (Q10)

- Wi-Fi (firmware blobs, 802.11 MAC/MLME, supplicant, regulatory database).
- GPUs and display beyond a GOP framebuffer.
- Laptops: AML-heavy power management, EC, battery, lid, s2idle, Thunderbolt, I219 ULP quirks.
- USB-attached anything: Pi 3 Ethernet, RTL8153/AX88179 dongles (6.5% of PC NIC entries).
- Exotic or legacy NICs: Atheros, Aquantia, Marvell, Solarflare.
- Hardware RAID data paths (smartpqi, megaraid_sas, mpi3mr), until there is demand.
- Intel VMD / RST RAID mode (~8% of PC controllers).
- 10/25/100G NICs at first (each 2.4–7.5k lines minimum, complex firmware interfaces, no QEMU models).
- 32-bit x86 and armv7: k10-metal is long-mode only, and the JIT is 64-bit.
- Any machine whose devices are not on the Tier 2 list.

**Suggested glue [estimate]:** the UEFI loader can enumerate PCI through the firmware's PCI I/O protocol before ExitBootServices. With a "supported devices" table, it could boot k10-metal when the NIC (and disk, if needed) is supported, and chain-load the Tier 1 Linux image otherwise. One installed image then covers both tiers: decided (§11), layout in universal-os.md §8.4.

---

## 6. Platform services (questions 11–13)

### 6.1 IOMMU, microcode, power, thermal, watchdog, RTC, entropy (Q11)

| Service | Finding | Recommendation |
|---|---|---|
| IOMMU | DMAR/IVRS/IORT tables; minimal DMA remapping is 2–5k lines (§5.2). Firmware may hand over with VT-d off | VT-d in M4; required for TIR drivers and >255-thread x2APIC servers |
| Microcode | BIOS is Intel's preferred loader; OS early-load is optional. Lost on reset. Redistribution in binary form is allowed. https://github.com/intel/Intel-Linux-Processor-Microcode-Data-Files , https://docs.kernel.org/arch/x86/microcode.html [doc] | Rely on firmware updates; optional early load from the boot bundle later |
| CPU frequency | Once firmware enables HWP it cannot be disabled until reset; otherwise the CPU stays at the BIOS P-state (often no turbo). https://docs.kernel.org/admin-guide/pm/intel_pstate.html [doc] | ~50 lines: enable HWP (IA32_PM_ENABLE, HWP_REQUEST/EPP) if present |
| Idle | `hlt` today. MWAIT C-states without `_CST` use per-model tables (intel_idle). https://docs.kernel.org/admin-guide/pm/intel_idle.html [doc] | Later, for power |
| Thermal / fans | Throttling is autonomous; server fans belong to the BMC (iLO/iDRAC thermal profiles). https://community.hpe.com/t5/proliant-servers-ml-dl-sl/new-ml350-gen-10-server-unusable-ilo-and-fan-speed-problem/m-p/7234233 [blog/forum] | Nothing to do |
| Watchdog | ACPI WDAT (register actions, no AML) https://lwn.net/Articles/701235/ [blog/forum]; IPMI BMC watchdog over KCS; SBSA generic watchdog (GTDT); bcm2835 PM watchdog on Pi | Yes for unattended nodes: IPMI/WDAT on servers, PM watchdog on Pi (~200–300 lines each [estimate]) |
| RTC | CMOS on x86 (implemented); Pi 3/4 have none; Pi 5 RTC only through the firmware mailbox [doc] | SNTP client or time from cluster peers (gossip carries wall-clock incarnations already) |
| Entropy | RDRAND/RDSEED on x86 (used for the TCP seed). Arm RNDR is optional (v8.5); Pi 4 (A72) and Pi 5 (A76) lack it. https://github.com/rust-random/getrandom/pull/494 [doc] | Pi: iproc rng200 (Linux 245 lines); VMs: virtio-rng |

### 6.2 Console without a serial port (Q12)

- **UEFI GOP framebuffer.**
  - The loader already queries GOP (resolution, stride, framebuffer base) [measured; headless runs had none].
  - The linear framebuffer stays valid after ExitBootServices, but the mode cannot be changed. https://uefi.org/specs/UEFI/2.10/12_Protocols_Console_Support.html [doc]
  - A text console with an 8×16 font is ~300 lines plus a 4 KB font [estimate]. Pass the GOP info through a k10 extension of `start_info`.
  - It is the only local console on desktops and mini-PCs.
- **Network console.**
  - k10-metal already exposes `/_k10s/status` and `/_k10s/logs/<service>` over HTTP [code].
  - Add a netconsole-style UDP log stream to a configured collector or to cluster peers, starting before the cluster joins. https://docs.kernel.org/networking/netconsole.html [doc]
- **BMC serial-over-LAN.**
  - Dell iDRAC "console redirection via COM2" puts the virtual port at COM2/0x2F8. https://lists.proxmox.com/pipermail/pve-user/2022-July/016799.html [blog/forum]
  - HPE iLO Virtual Serial Port is assigned to COM1 or COM2 in RBSU. https://www.golinuxcloud.com/capture-serial-console-dump-hpe-ilo4-vsp/ [blog/forum]
  - AWS serial console: `ttyS0` on Nitro. [doc]
  - **Read SPCR** (prototype parses it) and allow `k10.console=` instead of hard-coding COM1.

### 6.3 Remote management

- For the "very simple system" goal, the admin plane is the cluster itself: apply, status and logs through any node.
- Gossip and peer links are authenticated and encrypted with the shared cluster key, and admin requests are signed [code: metal/README.md]. On physical LANs the remaining gap is that every key holder is trusted (no per-node identity yet).

### 6.4 Persistent storage (Q13)

**What exists [code].** The hosted node already has the format we want (crates/k10-node/src/kv.rs, 671 lines):
- Per-service append-only log `len | crc32 | op | klen | key | value`.
- In-memory map rebuilt by replay; a torn tail is truncated.
- Group commit (flusher, ~2 ms window).
- Compaction when the log exceeds 2× the live size.

**Native design [estimate]:**
- **Layout:** GPT, with a k10s data partition (own type GUID) next to the ESP. GPT parsing is ~100 lines.
- **No filesystem in the kernel:** a FAT or ext4 writer is 1–10k+ lines and adds failure modes.
- **On-disk format:** one **segmented log** on the raw partition. Records are the hosted format plus a namespace (service) id and a sequence number; fixed-size segments (e.g. 8 MiB); a superblock pair (A/B, CRC) pointing to the live segments; and NVMe Flush (or FUA writes) for durability.
- **Boot:** replay rebuilds the per-service maps.
- **Compaction:** copy live keys of the oldest segments forward, then free them. Same contract as hosted: asynchronous commit with a few-ms window.
- **Code reuse:** extract the record codec (CRC-32, encode/decode) into a `no_std` crate shared by both nodes, so the two formats cannot drift.
- **Updates without FAT writes:**
  - Keep two raw kernel slots (A/B) on the data disk.
  - The UEFI loader reads the newest valid slot (hash/signature check) through the firmware's Block I/O protocol before ExitBootServices, and falls back to the embedded kernel.
  - k10-metal writes only raw blocks.
- **Pi:**
  - SD-card wear argues for larger, aligned segment writes and less frequent compaction.
  - The NVMe HAT on Pi 5 is better.
  - With no disk, KV stays in memory as today.

---

## 7. arm64 native port (questions 14–15)

### 7.1 Arch layer size (Q14)

**x86 today [code; physical lines]:**

| boot.rs | pvh.rs | smp.rs | percpu.rs | time.rs | serial.rs | coro.rs | sync.rs | arch/ (mod, apic, gdt, idt, paging) | Total |
|---|---|---|---|---|---|---|---|---|---|
| 161 | 113 | 170 | 95 | 314 | 85 | 260 | 82 | 127 + 144 + 89 + 230 + 200 = 790 | **≈2.1k** |

**arm64 sketch [measured]:** 907 lines for boot (incl. vectors and the EL2 drop), FDT, MMU (1 GiB blocks), GICv2 + GICv3, generic timer, PSCI + spin table and PL011. It runs on `virt` and `raspi4b`.

**Missing for a port [estimate]:**

| Item | Lines | Notes |
|---|---|---|
| 4 KiB page tables with W^X sealing | ~200 | |
| Per-CPU state via TPIDR_EL1 | ~80 | |
| Coroutine switch (x19–x28, fp, lr, sp) | ~80 | |
| IPIs via SGIs | ~40 | |
| GICv3 ITS (MSI-X on servers) or GICv2m | ~300–400 | |
| ACPI arm64 tables (MADT GICC/GICD/GICR, GTDT, SPCR, MCFG, IORT) on top of acpi.rs | ~250 | |
| `ranges`/`dma-ranges` | ~100 | |
| Cache maintenance for non-coherent DMA | ~60 | **Pi devices are not cache-coherent with the CPU; server PCIe is** |
| Barrier audit of the rings | — | x86 TSO hides missing `dma_wmb` (DMB OSHST) |
| PCIe ECAM generic host | ~150 | brcmstb on Pi separately, §4 |
| PSCI SYSTEM_OFF/RESET | done | |
| **Total arm64 arch layer** | **≈2.5–3k** | **5–7 weeks** with QEMU `virt` + AAVMF as the test bed |

**Good news [doc]:** arm64 broadcast TLB invalidation (`TLBI …IS`) makes cross-CPU W^X changes simpler than x86's lazy-generation scheme in paging.rs.

### 7.2 arm64 JIT backend (Q15)

**Today [code]:** `crates/k10-jit/src` is 3,635 lines:

| x64.rs (encoder) | codegen/func.rs | codegen/mod.rs | blob.rs | abi.rs | exec.rs | helpers.rs | cpu.rs | lib.rs |
|---|---|---|---|---|---|---|---|---|
| 879 | 1,871 | 225 | 206 | 141 | 169 | 57 | 45 | 42 |

Tests are 3.5k lines (diff, ops, robust, isolation, red team).

**New code for aarch64 [estimate]:**
- An A64 encoder (~900–1,100 lines). Fixed-width encodings are simpler, but logical-immediate and wide-constant materialisation are fiddly.
- A function compiler (~1,800–2,200 lines). func.rs is built around x86 two-address forms, flags and r13/r15 pinning, so its structure ports but its code does not.
- Invoke/thunk glue (~150 lines). abi.rs, blob.rs and helpers.rs are reused.
- All existing tests reused, with differential testing against the interpreter.
- **Total ≈3k new lines, 6–10 weeks.**
- Comparable: Wasmtime's Winch aarch64 MacroAssembler is 5.5k lines (x64 7.7k) on ~16k shared lines, and took about 15 months from tracking issue to complete. https://bytecodealliance.org/articles/winch-aarch64-support [doc]

**SFI on arm64:**
- **The k10 scheme.** Masking `base + ((addr + off) & mask)` with no guard regions becomes `add t, a, #off; and t, t, mask; ldr x, [base, t]`, the same instruction count as x86's `lea; and; mov [r15+…]`. Pinned registers (e.g. x27 base, x28 mask) mirror r15/r13.
- **LFI:**
  - LFI uses 4 GiB-aligned sandboxes with 80 KiB guards and `ldr rt, [x21, wN, uxtw]` (one instruction). Overhead is 7% on SPEC 2017 (6% after optimisations). https://www.scs.stanford.edu/~zyedidia/docs/papers/lfi.pdf [paper]
  - LLVM 22 ships an LFI mode. https://releases.llvm.org/22.1.0/docs/LFI.html [doc]
  - Its `uxtw` form needs 4 GiB slots, while k10 uses power-of-two slots of any size. Use it only as an optimisation for large slots [estimate].
- **Risk:**
  - Two 2026 Wasmtime aarch64 sandbox escapes were bounds-check lowering bugs: Cranelift CVE-2026-34971 and Winch CVE-2026-34987. https://security.julialang.org/advisories/JLSEC-2026-1360/ , https://security.julialang.org/advisories/JLSEC-2026-1361/ [doc]
  - Add an **independent machine-code verifier** (LFI-style) for both backends. research.md §"risks" already recommends it.
- **I-cache maintenance [measured in the sketch]:**
  - Required: DC CVAU per D-line, DSB ISH, IC IVAU per I-line, DSB ISH, ISB. Line sizes come from CTR_EL0.
  - Can be skipped when CTR_EL0.IDC/DIC say so (Neoverse N1 has errata). https://lkml.iu.edu/hypermail/linux/kernel/1802.2/04001.html [blog/forum]
  - Do it once per sealed module, as the x86 sealing does.
- **Other:**
  - JIT preemption check: `ldr t,[ctx,#limit]; cmp sp, t; b.lo slow` works with SP.
  - 16-byte SP alignment is architectural.
  - FP: the kernel stays soft-float (`aarch64-unknown-none-softfloat`, as in the sketch), and JIT code uses NEON/FP with CPACR_EL1.FPEN enabled, exactly the x86 SSE arrangement.
- **Order:** the arm64 JIT is shared with Tier 1's hosted arm64 node (Graviton, Ampere, Pi on Linux). Build it **once**, test it hosted under Linux (Tier 1) first, then in metal.

---

## 8. Testing without real hardware (Q16)

**What emulation covers [measured unless noted]:**

| Need | Tool | Status here |
|---|---|---|
| UEFI x86 | OVMF in `/usr/share/OVMF` (incl. 4M, secboot and MS-keys variants) | Works: boots, Secure Boot refusal reproduced |
| Legacy BIOS | SeaBIOS, qboot | Works (PVH) |
| Modern virtio-pci, virtio-mmio | QEMU 7.2 (`*-non-transitional`, microvm) | Modern-pci tested; mmio not yet |
| NVMe, AHCI, xHCI, SDHCI-PCI, vmxnet3 | QEMU 7.2 | NVMe tested |
| e1000 / e1000e (82574L) | QEMU 7.2 | Tested |
| igb (82576) | QEMU ≥ 8.0 | Tested on QEMU 10 |
| IOMMU | `intel-iommu`, `amd-iommu`, `virtio-iommu`, arm `smmuv3` | Available, untested |
| arm64 `virt`, `sbsa-ref` (e1000e, AHCI, xHCI, GICv3, SBSA watchdog), `raspi3b`, `raspi4b` (QEMU ≥ 9.0) | **QEMU 10.0.2 rootless** | virt and raspi4b tested |
| aarch64 UEFI | AAVMF / `qemu-efi-aarch64` 2022.11, extracted alongside | Available, untested |
| Cadence GEM (Pi 5 MAC) | Xilinx machines in qemu-system-aarch64 [doc] | Untested |

**Rootless QEMU 10 (`get-qemu10.sh`) [measured]:**
- `apt-get download` from bookworm-backports + `dpkg -x`: 277 MB after trimming.
- Two gotchas:
  1. Set `QEMU_MODULE_DIR` to the extracted module directory, or QEMU 10 loads the system's 7.2 modules and aborts.
  2. Use `-display none -audio none`, or it loads GUI/audio modules that were not extracted.

**What QEMU cannot model [doc]:**
- **NICs:** I219 PCH MAC/PHY (QEMU's e1000e is the 82574L), I225/I226, all Realtek gigabit parts, tg3, bnxt, ixgbe/i40e/ice, mlx5, ENA, gVNIC, MANA, netvsc/storvsc.
- **Raspberry Pi:** raspi4b has no GENET and no PCIe, so no VL805 xHCI or NVMe. There is no Pi 5 machine. https://www.qemu.org/docs/master/system/arm/raspi.html
- **Firmware behaviour:** x2APIC handover, large memory maps, real SMM/ACPI quirks, MTRR/PAT surprises.

**Hardware the user should provide** (in priority order) [estimate]:
1. **A business mini-PC with Intel I219-LM**, e.g. a Lenovo M70q/M90q or Dell OptiPlex Micro (used units are cheap). Covers UEFI, GOP-only console, NVMe, e1000e PCH.
2. **A consumer mini-PC or desktop with Realtek RTL8125/RTL8111 and a SATA disk.** Covers r8169-class and AHCI.
3. **An N100 box with Intel I226-V.** Covers igc.
4. **A used rack server with a BMC:** Dell R640/R650 or HPE DL360 Gen10. Covers BCM5720/5719 (tg3), I350, iDRAC/iLO SOL on COM2, >8 CPUs, x2APIC, NUMA, PERC in HBA mode, BOSS.
5. **Raspberry Pi 4 (4 GB) and Pi 5,** plus a 3.3 V USB-serial adapter and the Pi 5 debug-UART cable.
6. **Cloud accounts:**
   - AWS: one Nitro x86 (BIOS and UEFI AMIs) and one Graviton.
   - GCP: N2 (virtio) and C3 (gVNIC + NVMe).
   - Azure: Dsv5 (storvsc) and Dsv6 (NVMe + MANA).
   - Use the serial consoles for bring-up.

---

## 9. Roadmap

Efforts are for one experienced engineer, in weeks, and are [estimate]s. They grow with real-hardware debugging.

**M1: boots anywhere UEFI or BIOS, as a VM or cloud guest with virtio (4–6 weeks).**
- **Boot:**
  - Production UEFI loader: kernel and bundle from the ESP, command line from LoadOptions or a config file, GOP info, AP trampoline reservation.
  - Multiboot2 shim for BIOS via GRUB/Limine. Keep PVH.
  - Tier selector (universal-os.md §8.4): `tier=auto|linux|metal`, PCI enumeration against the supported-device table, chain-load of the Tier 1 UKI, boot-counted fallback to `linux`.
- **Platform:**
  - Map all RAM and every BAR (UC).
  - Coalesced memory map with no 64-entry cap.
  - ACPI tables: MADT CPUs with targeted SIPI, `MAX_CPUS` ≥ 64, x2APIC (MSR interface, accept x2APIC handover), FADT reset/S5, SPCR console, HPET/PM-timer fallback.
  - PCI: ECAM full scan, 64-bit BARs.
- **Devices:** virtio 1.x net + blk + mmio v2, behind `NetDevice`/`BlockDevice` traits (queue count, DMA from slot-like contiguous regions) that a TIR driver host can implement later.
- **Network:** DHCPv4 and a static IP from `k10s.conf` on the ESP or the command line; IPv4 only. Remove the QEMU-only exits.
- **Test plan:**
  - `test.sh` and `cluster-test.sh` under SeaBIOS PVH, OVMF `pc`/`q35` (`-kernel` and ESP), modern-only and legacy NICs, microvm (mmio), and Firecracker + Cloud Hypervisor if installable without root.
  - One real VM at Hetzner or Vultr (modern virtio) and at DigitalOcean (BIOS).
- **Exit criteria:**
  - The same image boots on all of the above.
  - Joins a hosted cluster.
  - Serves `test.sh` 100%.
  - Powers off and reboots without `isa-debug-exit`.
  - Boots with 16 GiB of RAM and 32 vCPUs under QEMU.

**M2: durable storage (3–4 weeks).**
- Work:
  - NVMe production driver (MSI-X, queue per CPU later), virtio-blk, AHCI.
  - GPT and the `K10S-DATA` partition (own type GUID); other partitions are left alone.
  - Segmented KV log sharing the hosted record codec (`no_std` crate); Tier 1 switches `K10S-DATA` from ext4 to the same log (`O_DIRECT` + `fdatasync`).
  - Raw A/B kernel slots in `K10S-DATA`, read by the loader.
- Test plan: QEMU NVMe/AHCI/virtio-blk, plus a crash-injection test (kill QEMU during writes, then verify replay).
- Exit criteria: KV survives reboot and crash with the hosted durability contract; a self-update from the cluster switches the A/B slot; a machine switched between tiers keeps its KV state.

**M3: big-cloud NICs (12–16 weeks, in market order).**
- Work:
  - **ENA** (with LLQ) + NVMe on AWS: 4–5 weeks.
  - **VMBus + netvsc + storvsc** on Azure: 6–8 weeks. It works even on MANA hosts at near line rate. [doc]
  - **gVNIC** on GCP gen 3+: 4–5 weeks.
- Test plan: on the clouds only (serial console). Keep a soak test of 24 h per cloud.
- Exit criteria: boots from a custom image on each cloud, joins a cluster spanning clouds, passes the load and failover parts of `test.sh` remotely.

**M4: curated x86 bare metal (12–16 weeks).**
- Work:
  - Intel GbE family (I219 PCH incl. ME/ULP handling, igb, igc).
  - Realtek r8169-class (8111/8125/8126).
  - tg3 (5719/5720).
  - GOP text console.
  - UDP log console.
  - Watchdog (IPMI/WDAT).
  - HWP enable.
  - IOMMU (VT-d) DMA remapping.
- Test plan: the reference machines of §8 (1–4), plus a hardware CI rack: PXE or USB boot, power cycling through smart plugs or the BMC.
- Exit criteria: each reference machine boots from USB/NVMe, gets DHCP, joins the cluster, serves at line rate for 24 h, survives cable unplug/replug, and reboots cleanly.

**M5: arm64 arch layer, interpreter engine (5–7 weeks).**
- Work: Image + PE/COFF header, DT and ACPI, W^X MMU, GICv3 + ITS / GICv2m, timer, PSCI, virtio on PCI-ECAM and MMIO, coroutine switch.
- Test plan: QEMU `virt` (GICv2/v3), `sbsa-ref` and AAVMF; then a Graviton metal or VM via UEFI.
- Exit criteria: `test.sh` passes with `k10.engine=interp` on `virt` and joins a mixed x86/arm64 cluster.

**M6: arm64 JIT (6–10 weeks, shared with Tier 1).**
- Work: A64 encoder + function compiler, machine-code verifier (both arches).
- Test plan: the k10-jit diff, ops, robust, isolation and red-team suites on aarch64 hosted (Tier 1: Graviton or Pi on Linux); then metal.
- Exit criteria: all suites pass, red-team escapes are contained, and the JIT speed-up is comparable to x86 (≥ 7× over the interpreter on `sha`/`nbody`/`sort`).

**M7: Raspberry Pi 4 and 5 (8–12 weeks).**
- Work:
  - Pi firmware boot.
  - Mailbox, rng200, PM watchdog.
  - GENET + EMMC2 SDHCI (Pi 4).
  - brcmstb PCIe + RP1 + GEM + sdhci-brcmstb (Pi 5).
  - Non-coherent DMA.
  - `dma-ranges` (Pi 4 3 GB DMA limit).
- Test plan: real boards with a serial console. QEMU raspi4b for boot, SMP and SD only.
- Exit criteria: boots from SD, serves, and joins a cluster on Pi 4 and Pi 5. On 4 GB/8 GB boards, DMA stays within the limits.

**Optional on demand:** 10/25G NICs (ice without DDP, bnxt, mlx5/mcx-style) with RSS and per-CPU queues, megaraid_sas/mpi3mr/smartpqi, xHCI + USB mass storage, a pilot long-tail driver as a TIR workload behind the M4 IOMMU.

**Total to "clouds + curated x86 + Pi + arm64 JIT":** ≈ 50–70 engineer-weeks [estimate].

---

## 10. What Tier 2 should NOT attempt

| Do not attempt | Reason |
|---|---|
| Wi-Fi, Bluetooth | Firmware blobs, 802.11 stack, supplicant, regulatory compliance; Tier 1 has all of it |
| GPUs, display beyond a GOP framebuffer | Enormous drivers and blobs; no workload needs them |
| Laptops | AML-heavy (EC, battery, lid, s2idle), I219 ULP quirks, Thunderbolt/USB-C |
| Pi 3 (and Pi 1/2/Zero) | Ethernet only over USB: DWC2 + USB core + LAN95xx/78xx (~38k Linux lines, ~6k minimum); no GIC; 1 GB RAM |
| USB as a device class (NIC dongles, mass storage) | A USB stack is 5–8k lines minimum before the first device. Only revisit xHCI for Pi 4/5 USB boot if needed |
| Hardware RAID data paths (smartpqi, megaraid_sas, mpi3mr), Intel VMD/RST | Large, vendor-specific; ask for HBA/AHCI/NVMe mode or use Tier 1 |
| 10/25/100G NICs in the first round | Many vendors, complex firmware interfaces (HWRM, mlx5 command queue, DDP), no emulation; Tier 1 until a customer needs one |
| 32-bit x86 / armv7 | k10-metal is long-mode only; JIT and slots are 64-bit; Tier 1 + interpreter |
| A general AML interpreter, ACPI hotplug, suspend/resume | Not needed for servers or nodes that run 24/7 |
| Linux driver reuse inside the kernel (LKL, DDE) | GPL incompatibility with Apache-2.0 linking; footprint |
| Secure Boot signing via Microsoft before there is demand | Months of process, recurring audits, JIT-in-ring-0 scrutiny |

---

## 11. Risks and decisions

**Risks [estimate]:**
1. **QEMU passing ≠ hardware passing.**
   - The Intel prototype already hit model-specific behaviour (igb PHY and descriptor format).
   - Real I219, Realtek (59 revisions) and tg3 parts have errata the emulators lack.
   - Budget for a hardware CI rack, and for drivers being permanent maintenance.
2. **Firmware variance:** memory maps, x2APIC handover, MTRR/PAT, ACPI quirks, `_PTS`-dependent S5, broken SPCR tables.
3. **Cloud drift:**
   - ENA requires LLQ for performance.
   - Azure is moving to MANA and NVMe, with Accelerated Networking VFs revoked during host servicing (fallback to netvsc).
   - GCP gen 3+ is gVNIC/NVMe only.
   - Each needs continuous updates.
4. **Weak memory ordering on arm64:** device rings written assuming x86 TSO, plus non-coherent DMA on the Pi.
5. **Security on physical networks:** gossip and peer links are authenticated and encrypted and admin requests are signed, but with one shared cluster key (no per-node identity) and an HTTP admin API with no TLS.
6. **JIT trust:** a second backend doubles the compiler TCB; mitigate with a machine-code verifier.

**Decisions** (summary of both tiers in REPORT.md §9):
1. **Secure Boot:** not required in v1. k10-metal and the loader ship unsigned; the docs explain how to turn Secure Boot off or sign with an own key and enroll it (sbctl, MOK). No own Microsoft-signed shim.
2. **Legacy BIOS:** in scope. M1 adds the Multiboot2 shim and GRUB/Limine on the image.
3. **Storage ownership:** k10-metal writes only its own GPT partitions (`K10S-DATA` with its own type GUID, next to the ESP) and leaves other partitions alone. The Tier 1 image builder writes the ESP for both tiers; k10-metal never writes FAT (raw A/B kernel slots in `K10S-DATA`).
4. **Addressing:** DHCPv4 by default and a static IP from `k10s.conf` on the ESP or the command line; IPv4 only on Tier 2 (Tier 1 gets IPv6 from kernel SLAAC).
5. **Cluster authentication and encryption:** in place: gossip and peer links authenticated and encrypted with the shared, rotatable cluster key, signed admin requests. OS images are signed by the k10s release key (universal-os.md §10).
6. **Driver interfaces:** `NetDevice`/`BlockDevice` traits with DMA from slot-like regions from M1; first-tier drivers stay in Rust in the kernel; IOMMU (VT-d) in M4, then a TIR-driver pilot on demand.
7. **Priority:** x86 first. The milestones run in order M1 → M7: clouds and curated x86 before arm64 and the Pi (Pi users get Tier 1 at U3).
8. **One universal image:** yes. The loader picks the tier from the PCI device list (`tier=auto|linux|metal`) with the layout of universal-os.md §8.4.
9. **Multi-queue NICs:** not in Tier 2 v1. One queue, network I/O on CPU 0 (enough for 1 GbE); RSS and per-CPU queues come with 10G+ NICs on demand.
10. **Out of Tier 2:** Wi-Fi (not supported in either tier) and 32-bit targets (Tier 1 best effort, interpreter only).

---

## 12. Reproduction and sources

**Scripts** (in `docs/research/native-hardware/`):
- `experiments.sh <step>`. Steps: build, boot-times, acpi, modern-virtio, nvme, gbe, uefi-all, secureboot, arm64.
- `bootcmp.py`: interleaved boot timing.
- `bootcmp-results.json`: the raw §2.2 data.
- `get-qemu10.sh`: rootless QEMU 10 + AAVMF.

**Prototype code:**
- Branch `research/native-hardware`, commits ae48aa8 … 531b9ef (local only).
- Full prototype `test.sh` runs: `NICDEV=virtio-net-pci-non-transitional ENGINES=jit ./test.sh` → 164 passed, 0 failed; the same with legacy virtio → 164/164.

**Driver line counts:** clones and `counts.tsv` in `/home/qgerard/dev/k10s-research/native-hardware/ref/` (commits in §4.2).

**Market surveys** (full text with every source URL, gathered 2026-10-19):
- `/home/qgerard/dev/k10s-research/native-hardware/cloud-devices-survey.md`
- `chip-prevalence-survey.md`
- `server-controllers-survey.md`

The main URLs are inline above.
