# k10-metal — the k10s node runtime as its own kernel

k10-metal boots the k10s node runtime directly on a (virtual) x86-64 machine, with no Linux underneath
(docs/DESIGN.md §2.9). Workloads are TIR modules, isolated by construction: masked memory accesses, structured
control flow, and a closed host API. So the kernel needs no processes, no system calls, no user/kernel boundary,
and no per-workload page tables. It runs one identity-mapped address space (all RAM, above 4 GiB too) on one or more
cores (up to 64, from the ACPI MADT), with a timer, a NIC, an optional disk and its consoles (serial, the screen of
a UEFI machine, UDP). It finds the platform in the ACPI tables, without an AML interpreter, powers off and reboots
through ACPI, and arms the machine's hardware watchdog while it runs.

It boots with PVH (`-kernel`) on QEMU (`pc` and `q35` with SeaBIOS or qboot, `microvm`), Firecracker and Cloud
Hypervisor, under KVM or TCG, and from the x86-64 disk images of `os/` through its UEFI loader k10boot or through
GRUB and Multiboot2 on BIOS ("Booting"; docs/OS.md, "Tiers"), from a disk or a USB stick. NICs are virtio (PCI modern
and legacy, MMIO) and the Ethernet controllers of PCs ("Real machines": Intel e1000e — 82574L, I217/I218, I219 —,
igb — 82576, I350, I210/I211 — and igc — I225/I226 —, Realtek RTL8111/8168/8411 and RTL8125), disks virtio-blk,
NVMe or AHCI. On a disk with a `K10S-DATA` partition it keeps the services' KV data, its node id and the
cluster's desired state, in the format Tier 1 uses (crates/k10-kvlog), and installs cluster-rolled kernel updates
into the partition's two kernel slots ("Storage", "Updates"). It deploys a `k10_mesh::Bundle` (manifest + TIR blobs) and serves HTTP/1.1 on port 80 with the same routing
and host API as the hosted node (docs/NODE.md, docs/HOSTAPI.md). Services run as
machine code from `k10-jit` (the default) or on the `k10-ir` interpreter. With `k10.join=…` the guest joins a k10s
cluster as a full member next to Linux-hosted `k10s node` processes: gossip membership, desired state fetched from
peers, the same placement, forwarding both ways.
Measurements: [RESULTS.md](RESULTS.md).

## Quick start

```sh
./build.sh                 # out/k10-metal.elf (kernel), out/k10boot.efi (UEFI loader), out/bundle.k10b (20 test
                           # services, 4 of them compiled from ../examples/*.ten); EMBED=1 also bakes the bundle in
k10s keygen -o admin.key   # admin key (mode 0600); without one the admin API serves only /_k10s/health
ADMIN_KEY=admin.key ./run.sh # QEMU (KVM, else TCG), serial on stdout, host 127.0.0.1:8090 -> guest :80 (PORT=, MEM=,
                           # BIOS=, PLATFORM=pc|pc-legacy|pc-legacy-split|pc-modern|q35|microvm|microvm-v2|e1000e|igb,
                           # DISK=raw file);
                           # the key goes in as fw_cfg opt/k10s/admin_key, never on the kernel command line
curl localhost:8090/hello/ # -> hello from k10-metal
curl 'localhost:8090/sha/?n=1000'                # Ten SHA-256 chain (examples/compute.ten), JIT-compiled
k10s status --json --node 127.0.0.1:8090 --admin-token-file admin.key   # signed admin request
export K10S_ADMIN_TOKEN_FILE=admin.key           # tools/admin_auth.py signs for curl:
curl -H "Authorization: $(tools/admin_auth.py 127.0.0.1:8090 GET '/_k10s/bench/100/sha/?n=1000')" \
     'localhost:8090/_k10s/bench/100/sha/?n=1000'  # per-invocation CPU time (JSON)
curl -H "Authorization: $(tools/admin_auth.py 127.0.0.1:8090 POST /_k10s/apply @out/bundle.k10b)" --data-binary @out/bundle.k10b \
     localhost:8090/_k10s/apply                  # redeploy
curl -H "Authorization: $(tools/admin_auth.py 127.0.0.1:8090 POST /_k10s/power reboot)" --data-binary reboot \
     localhost:8090/_k10s/power                  # reboot (body `off`: power off) through ACPI
SMP=4 ./run.sh             # four CPUs: CPU 0 runs the network, CPU-heavy requests are spread over all four
./test.sh                  # in-kernel self tests + end-to-end checks: JIT, interpreter, JIT on 4 CPUs, admin API signing,
                           # every QEMU platform and NIC (power off/reboot, disks), SPCR console, static address, DHCP
                           # renew, watchdogs, UDP console, 16 GiB + 32 vCPUs (TEST_PORT=18090, PLATFORM=, ACCEL=kvm|tcg)
./cluster-test.sh          # 2 hosted `k10s node` + 1 k10-metal guest: join, apply, routing, failover, rejoin with a next
                           # cluster key, KV on the guest's NVMe K10S-DATA through a power cut, refused peers (wrong
                           # key, unauthenticated links), leave + power off
tools/storage_test.py --kernel out/k10-metal.elf --bundle out/bundle.k10b   # K10S-DATA on virtio-blk, NVMe and AHCI:
                           # growth, power cuts during a write stream (prefix checks), clean power off
tools/platform_test.py --kernel out/k10-metal.elf   # SNTP and DNS on a scripted LAN, the clock stepped from 2001,
                           # the power button (fixed-feature, GPE), single-node applies kept, IPv6 (SLAAC, HTTP, ping)
./vmm-test.sh              # Firecracker and Cloud Hypervisor (KVM, a tap device; tools/get-vmms.sh downloads them)
tools/linux-compare.sh     # builds the Linux comparison guest (host kernel + tiny initramfs)
tools/measure.py           # measurements -> out/results/*.json (see RESULTS.md); `engines` = JIT vs interpreter,
                           # `smp` = throughput with -smp 1/2/4
tools/cluster_measure.py   # cluster measurements with a metal member -> out/results/cluster.json
```

Kernel command line (`APPEND=` for run.sh): `k10.verbose` (boot banner, ACPI summary, phases, apply log),
`k10.selftest` (run the in-kernel tests, print `k10-metal selftest: passed` or `FAILED`, power off;
`k10.selftest=watchdog` arms the watchdog and hangs, `k10.selftest=panic` panics once the UDP console works: test
modes),
`k10.exit_on_ready` (power off once ready), `k10.spin_us=N` (busy-poll window before halting, default 50),
`k10.engine=jit|interp` (default jit), `k10.smp=N` (use at most N CPUs), `k10.ip=A.B.C.D/PREFIX[,GATEWAY]` (static
IPv4; default DHCP, `k10.ip=dhcp`), `k10.ntp=HOST[:PORT],…|none` (SNTP servers; default the DHCP lease's, else
`pool.ntp.org`), `k10.dns=A.B.C.D,…` (name servers; default the lease's), `k10.console=ITEM[,ITEM…]` with at most one serial port among
`com1|com2|com3|com4|io:0xPORT|mmio:0xADDR|mmio32:0xADDR|none` (default: the ACPI SPCR table's UART, else COM1),
`udp:A.B.C.D[:PORT]` (the UDP log console, default port 6666) and `nofb` (no framebuffer console; it is on whenever
k10boot hands over the screen), `k10.watchdog=SECONDS|on|off` (the hardware watchdog's timeout, default 60; `off`
leaves it alone), `k10.panic=N` (seconds before a fatal error reboots, default 10, 0 = halt), `k10.apic=xapic` (stay in xAPIC mode where the firmware did not hand over in x2APIC mode), `k10.pci=full`
(probe every bus of the ECAM windows under a hypervisor too), `virtio_mmio.device=SIZE@BASE:IRQ` (virtio-mmio slots,
Linux syntax; also read from the ACPI DSDT),
`k10.jit_indirect=jump|lfence|retpoline` (Spectre v2 hardening of JIT indirect branches, default jump),
`k10.timer_res_us=N` (resolution of the guests' `now_ns`/`time_ms`, default 100, 0 = full; any other value needs
RDRAND for its key; SECURITY.md),
Cluster mode: `k10.join=HOST:PORT[,…]`, `k10.gossip=`, `k10.rpc=`, `k10.http=` (advertised addresses),
`k10.node_id=HEX`, `k10.probe_ms=N`, `k10.suspicion_mult=N`, `k10.mem=BYTES`, `k10.host_alias=IP`,
`k10.gossip_max_skew_s=N` (default 300, at least 1), and `k10.insecure_unauthenticated_cluster` when no cluster key
is given. Storage and updates: `k10.data=none` (no K10S-DATA: KV in memory), `k10.grow=0` (keep K10S-DATA's size),
`k10.os_commit_grace=S` (30), `k10.os_trial=S` (600). The advertised addresses default to the interface's (DHCP or static) address. An invalid `k10.spin_us`,
`k10.engine`, `k10.smp`, `k10.jit_indirect`, `k10.timer_res_us`, `k10.ip`, `k10.console`, `k10.watchdog`, `k10.panic`
or `k10.gossip_max_skew_s` or `k10.dns` value stops the boot.

Booted by k10boot or GRUB, the kernel also takes these from `k10s.conf` on the disk's `K10S-BOOT` ("Booting").

Secrets never go on the kernel command line (the host shows it in its process list). From a disk image they are the
key files that `k10s.conf` names (`admin-token-file=/boot/admin.key`, `cluster-key-file=`, `cluster-next-key-file=`),
which the loader hands over as modules; under QEMU's `-kernel` they are fw_cfg files,
`-fw_cfg name=opt/k10s/<name>,file=FILE` (run.sh: `ADMIN_KEY=`, `CLUSTER_KEY=`, `CLUSTER_NEXT_KEY=`). Each is created
with `k10s keygen -o FILE` (mode 0600). The kernel wipes the raw bytes after parsing; a malformed file stops the boot.
* `opt/k10s/admin_key`: the admin key (needs RDRAND, for the per-boot audience). Without it the admin API serves only
  `/_k10s/health` (403 otherwise).
* `opt/k10s/cluster_key`: the cluster key (the hosted nodes' `--cluster-key-file`); cluster mode needs it (and
  RDRAND) unless `k10.insecure_unauthenticated_cluster`.
* `opt/k10s/cluster_next_key`: the next cluster key while rotating (`--cluster-next-key-file` on hosted nodes).

The deployment bundle comes from QEMU's `-initrd` (PVH module 0), or from the loader (`/k10s/bundle.k10b` on
`K10S-BOOT`). If there is none, the kernel uses the bundle baked in at build time (`K10_EMBED_BUNDLE=path`). If neither
exists, the node starts empty and waits for `POST /_k10s/apply` (or, in a cluster, takes the cluster's desired state).
A single node with `K10S-DATA` keeps the last bundle it installed there (`bundle.k10b`, Tier 1's file: a machine that
changes tiers keeps it), written before the apply's 200: the next boot installs it first, and a boot bundle then goes
on top like an apply (an older manifest version is refused, the same one changes nothing).

## Layout

```
metal/                    its own cargo workspace (own target dir); path-depends on ../crates/k10-ir, k10-mesh, k10-jit
                          (kernel) and k10-lang (mkbundle)
  kernel/                 the kernel (x86_64-unknown-none, no_std + alloc), ~12k lines incl. comments and self tests
    linker.ld, build.rs   linked at 1 MiB, non-PIE (.cargo/config.toml), optional embedded bundle
    src/boot.rs           PVH ELF note + 32-bit entry → page tables → long mode → kmain; Multiboot2 header and shim
    src/arch/             port I/O, MSRs, per-CPU GDT+TSS (IST for #DF), IDT + asm entry stubs, local APIC (x2APIC or
                          xAPIC: timer, IPIs, INIT/STARTUP, NMI), I/O APIC routing, paging.rs (the identity map: all
                          RAM, uncached device ranges, NX, W^X for the kernel image and JIT code, TLB generations)
    src/acpi.rs           ACPI tables without AML: MADT, FADT, MCFG, HPET, SPCR, \_S5, virtio-mmio and power button
                          GPEs by DSDT/SSDT scan; MP table fallback
    src/power.rs          ACPI S5 power off, reset chain, the power button (SCI), the fatal-error path (stop the other
                          CPUs, reboot)
    src/percpu.rs         per-CPU state through the GS base (preemption request, idle flag, load, counters)
    src/smp.rs            application processors from the MADT: real-mode trampoline, targeted INIT-SIPI-SIPI, AP entry
    src/pvh.rs            hvm_start_info: E820 map (merged), command line, ACPI RSDP, named modules
    src/bootcfg.rs        k10s.conf → boot arguments, key files and the bundle from the loader's modules
    src/efi.rs            UEFI runtime services: clears k10boot's boot counter once the node is healthy (a trial
                          kernel: once it commits)
    src/storage.rs        K10S-DATA: GPT, growth, the k10_kvlog store, KV replay, asynchronous group commit,
                          checkpoints, the node's own files
    src/osupdate.rs       cluster-rolled kernel updates: the gossip report, install into a slot, trial commit/abandon
    src/fw_cfg.rs         QEMU fw_cfg over I/O ports: named files (secrets under -kernel)
    src/mem/              memory map (sorted, disjoint), frame allocator (bitmap), kernel heap, instance slots
    src/time.rs           TSC clock (kvmclock / CPUID 0x15 / HPET / PM timer / PIT), wall clock (kvmclock resync), guest
                          clock, preemption flag + JIT preemption word
    src/sntp.rs           SNTP client (without kvmclock): servers, DNS names, step or slew
    src/pci.rs            config space via ECAM (MCFG) or 0xCF8/0xCFC, bus scan, 64-bit BARs, MSI and MSI-X, bridges
    src/dev/              NetDevice and BlockDevice traits, DMA regions, device probe, virtio-mmio slots; nvme.rs
                          (NVMe), ahci.rs (AHCI SATA); nic.rs: the drivers of metal/drivers behind NetDevice
                          (registers, DMA, MSI/MSI-X)
    src/console/          the framebuffer text console (fb.rs, font.rs) and the UDP log console (udp.rs)
    src/watchdog.rs       the hardware watchdog: found at boot, armed by the main loop, petted every second
    src/virtio/           virtio transports (PCI modern + legacy, MMIO v1 + v2), split virtqueues, net and blk drivers
    src/net.rs            smoltcp interface (DHCPv4 or static, IPv6 SLAAC, DNS), listener pool, HTTP connections, UDP
                          + extra listeners + TCP dial, smoltcp glue for any NetDevice
    src/cluster.rs        cluster mode: gossip over UDP, peer links over TCP, desired-state fetch, placement, routing
    src/http.rs           HTTP/1.1 framing
    src/coro.rs           coroutines (context switch in asm), stack pool
    src/runtime/          services, instances, per-CPU schedulers + dispatch, host API, admin API; engine.rs (engine seam),
                          jit.rs (k10-jit: code memory, VmCtx, host thunks, interrupt hook)
    src/selftest.rs       in-kernel tests (k10.selftest)
  boot/                   k10boot, the UEFI loader and tier selector (x86_64-unknown-uefi): efi.rs (the UEFI
                          interfaces it uses, by hand), main.rs (tier, PCI scan, chain-load of systemd-boot),
                          native.rs (which kernel, signature, ELF, start_info, modules, boot counter,
                          ExitBootServices, jump), slots.rs (K10S-DATA's kernel slots through Block I/O)
  drivers/                k10-drivers (no_std + alloc, host tests against register models of the chips): the NIC
                          drivers (e1000e.rs, igc.rs, rtl8169.rs; ring.rs, the descriptor rings; ids.rs, the PCI
                          ids), the watchdog drivers (watchdog/: wdat.rs, itco.rs, esb.rs) and power_button.rs
                          (control-method power buttons' GPEs in AML)
  bootproto/              k10-bootproto (no_std, host unit tests): what k10boot, the Multiboot2 shim and the kernel
                          agree on: module names, framebuffer and EFI records, k10s.conf, the tier file, the boot
                          counter, E820; devices.rs, the supported-device table
  mkbundle/               host tool: test services built with k10_ir::builder and compiled from Ten → Bundle
  tools/                  boottime.py, measure.py, cluster_measure.py, linux-guest (comparison /init),
                          mkinitramfs.py, linux-compare.sh, admin_auth.py (signs admin requests for curl),
                          dhcp_test.py (DHCP client against a scripted server), get-vmms.sh (pinned Firecracker and
                          Cloud Hypervisor), storage_test.py (K10S-DATA through power cuts), wdat.py (an ACPI WDAT
                          table for q35's TCO watchdog), platform_test.py (SNTP, DNS, power button, kept applies,
                          IPv6: a scripted LAN over a QEMU socket netdev)
  build.sh run.sh test.sh cluster-test.sh vmm-test.sh qemu.sh (the QEMU platforms the scripts share)
```

## How it works

**Boot (PVH).** `qemu-system-x86_64 -kernel` loads an ELF64 directly if it carries an `XEN_ELFNOTE_PHYS32_ENTRY`
note. QEMU's `pvh.bin` option ROM then enters it in 32-bit protected mode, with paging off and `ebx` pointing at
`hvm_start_info`, which holds the E820 map, the command line and `-initrd` as module 0. Multiboot v1 was rejected
for two reasons. QEMU's Multiboot loader refuses ELF64 ("give a 32bit one"), so it would need a 32-bit wrapper or
the a.out kludge with a flat binary. And PVH is also what Firecracker and Cloud Hypervisor boot. The trampoline
(boot.rs) does the following:
1. Zeroes .bss.
2. Identity-maps 4 GiB with 2 MiB pages (PML4 → PDPT → 4 PDs). The top 512 MiB (MMIO, APIC) is uncached.
3. Enables PAE, long mode and paging (CR0.WP: read-only pages are read-only for the kernel too). It also enables
   SSE, for JIT code; the kernel itself is soft-float.
4. Loads a GDT, far-returns to 64-bit code, sets the stack and calls `kmain`.

The image works with SeaBIOS (QEMU's default), qboot (`BIOS=/usr/share/qemu/qboot.rom`, faster; it leaves PCI BARs
unprogrammed, so the PCI code assigns them itself), QEMU `microvm`, Firecracker and Cloud Hypervisor.

**Platform (acpi.rs).** Right after the memory map is up, the kernel reads the ACPI tables, with no AML interpreter:
the RSDP from `hvm_start_info` (else the EBDA and BIOS-area scan), then the XSDT (RSDT on revision 0) and
* MADT: the CPUs to start (enabled local APIC and x2APIC entries; disabled and hot-plug-only ones are never woken),
  the I/O APICs and the ISA interrupt overrides;
* FADT: PM1a/PM1b control and event blocks, the GPE blocks, SMI_CMD/ACPI_ENABLE, the PM timer, the reset register,
  the hardware-reduced flag and sleep control register — always the 64-bit (X_) form when present;
* DSDT and SSDTs, scanned byte by byte: `Name (_S5, Package () {a, b, ...})` for the S5 sleep types,
  `_HID "LNRO0005"` devices (virtio-mmio slots: the Memory32Fixed base and the Extended Interrupt of their `_CRS`),
  which is how QEMU microvm and Firecracker describe them, and the GPEs of control-method power buttons (a `_Lxx` or
  `_Exx` method of `\_GPE` that runs `Notify (B, 0x80)` for a device B whose `_HID` is PNP0C0C;
  `k10_drivers::power_button`);
* MCFG (PCI Express ECAM windows), HPET (the timer block), SPCR (the console UART: 16550 in I/O or memory space).
Without ACPI (QEMU `microvm,acpi=off`) an Intel MultiProcessor table gives the CPUs and I/O APICs, walked to its
length (QEMU leaves its entry count at 0). Tables with a bad checksum are used, as Linux does. Parsing takes a few
microseconds; `k10.verbose` prints a summary line.

**Power (power.rs).** Power off is ACPI S5: on hardware-reduced platforms (microvm, Cloud Hypervisor) SLP_TYP | SLP_EN
in the sleep control register; otherwise ACPI mode is entered if the firmware left SMM in charge (SMI_CMD ←
ACPI_ENABLE, up to 3 s for SCI_EN), every GPE is disabled (no wake-up), the PM1 status bits cleared, and SLP_TYP, then
SLP_TYP | SLP_EN, written to PM1a/PM1b control (the port differs between firmwares: 0x604 under SeaBIOS, 0xB004 under
OVMF `pc`). Firecracker has no ACPI power off; there a reset ends the VM, so power off resets. Reboot is the FADT
reset register, then the 8042 reset line, then port 0xCF9, then a triple fault. Both first stop the other CPUs with
an NMI. They are reached through `POST /_k10s/power` (body `off` or `reboot`; a signed admin request), which answers
202, leaves the cluster (gossip `Left`, so peers do not wait for failure detection), waits 300 ms and acts, through
`k10.exit_on_ready`, and after `k10.selftest`. The power button does the same as `POST /_k10s/power off` (Tier 1's
power button stops its node, which leaves the cluster, then powers off): the fixed-feature button (PM1 PWRBTN; the
FADT's PWR_BUTTON flag clear) and the GPEs of control-method buttons are the only SCI sources the kernel enables
(every other PM1 event and GPE off, statuses cleared); the SCI (FADT SCI_INT through the interrupt overrides; level,
active low by default) goes to CPU 0, whose handler clears the event (and disables a level-triggered GPE, whose method
would have cleared its source) and requests the power off; the console says `power: button pressed (fixed-feature)`
or `(GPE 0x1d)`. Not covered: hardware-reduced platforms (the Generic Event Device's `_EVT` reads a register through
AML: QEMU microvm, Cloud Hypervisor; Firecracker has no button), buttons whose notify is reached through other
methods or an embedded controller's query (`_Qxx`, laptops). A panic, a CPU exception or a configuration error prints its message,
stops the other CPUs and reboots after `k10.panic` seconds (default 10; 0 halts), so a boot-counting loader can fall
back to another image. There is no QEMU-specific exit device anywhere.

**Booting from a disk (k10boot, Multiboot2).** The x86-64 images of `os/` carry the kernel as `/k10s/metal.elf` on
their `K10S-BOOT` partition, signed as a k10s image (`k10s image sign --profile native`: the ELF followed by its
trailer, which ELF loaders ignore), and `/k10s/tier` says which tier boots (docs/OS.md, "Tiers"). PVH stays the one
entry interface; two loaders build its `start_info`:
* **k10boot** (`boot/`, UEFI, `\EFI\BOOT\BOOTX64.EFI`; with an SBAT section, so that shim starts it under Secure
  Boot) picks the kernel: the trial slot of `K10S-DATA` (read through the firmware's Block I/O, the disk of
  `K10S-BOOT` first) while the boot counter allows it, else the committed slot, else `/k10s/metal.elf` ("Updates").
  It verifies the signature (the release key and keys built in
  with `K10BOOT_KEYS`, plus `os_key` of k10s.conf when Secure Boot is off; a slot's image must also have the hash
  its superblock records), allocates the segments' physical range
  (`AllocatePages` at 1 MiB) and copies them, takes the firmware's memory map as E820 entries (merged), the ACPI
  RSDP from the configuration table, the command line from its load options or `metal_cmdline` of `/k10s/tier`, and
  reserves page 0x8000 (the application processors' trampoline, smp.rs; a firmware that keeps it is refused). Then
  `ExitBootServices` (the memory map taken again when its key went stale) and a stub copied below 4 GiB: off with
  CR4.PCIDE, a 32-bit code segment, paging and long mode off, CR4 cleared, `ebx` = `start_info`, jump to `_pvh_start`.
* **GRUB** on BIOS (`multiboot2` in the images' `grub.cfg` when `tier=metal`): the Multiboot2 header (in the first
  32 KiB of the file) asks for the command line, the modules (page-aligned), the memory map and the RSDP and makes
  `_mb2_start` the entry. That 32-bit shim converts GRUB's information into a `start_info` in `.mb2`, a section past
  `.bss` that `_pvh_start` does not clear (memory map entries copied, the RSDP copied out of GRUB's information,
  which becomes RAM), then jumps to `_pvh_start`.

Loaders name their PVH modules (`hvm_modlist_entry.cmdline_paddr`; `bootproto/src/lib.rs`): `k10s.bundle` (an unnamed
module is QEMU's `-initrd`), `k10s.conf`, a key file under the path k10s.conf gives it (`/boot/admin.key`),
`k10s.framebuffer` (the GOP mode: base, size, resolution, stride, pixel format), `k10s.efi` (the UEFI system
table, runtime services and their memory regions) and `k10s.image` (which kernel: K10S-BOOT's or a slot, on trial or
not, its hash, platform and version, the keys k10boot trusted, the K10S-DATA partition it read). Booted by a loader, the kernel takes secrets only from modules,
never from fw_cfg (whose I/O ports mean nothing on real machines), and k10s.conf, the configuration both tiers read,
sets the kernel's arguments whose meaning it shares (bootcfg.rs; the command line wins):

| k10s.conf | argument |
|---|---|
| `join`, `backend`, `jit-indirect`, `timer-res-us`, `idle-spin-us`, `probe-ms`, `suspicion-mult`, `gossip-max-skew` | `k10.join`, `k10.engine`, `k10.jit_indirect`, `k10.timer_res_us`, `k10.spin_us`, `k10.probe_ms`, `k10.suspicion_mult`, `k10.gossip_max_skew_s` |
| `insecure-unauthenticated-cluster` | `k10.insecure_unauthenticated_cluster` |
| any of `gossip`, `rpc`, `join`, `advertise` | `k10.cluster`; `k10.gossip`, `k10.rpc`, `k10.http`: the port of `gossip`, `rpc`, `http` (7946, 7947, 80) at `advertise`, else at the address given unless unspecified, else the node's own |
| `data` (`none`), `grow`, `os-commit-grace`, `os_trial` | `k10.data`, `k10.grow`, `k10.os_commit_grace`, `k10.os_trial` |

`k10s.KEY=VALUE` on the command line overrides the file, as on Tier 1. The kernel listens on 7946, 7947 and 80 in
any case; the addresses are what it gives its peers (forwarded ports under QEMU). Once the node is healthy (ready for
15 s, at least one frame received; a trial kernel: once it commits), efi.rs deletes k10boot's boot counter: a call to `SetVariable` on page tables of
its own (the first 4 GiB and the firmware's runtime regions identity-mapped, executable), switched in on CPU 0 with
interrupts off for the call only. Two counted native boots in a row that never get there make k10boot boot Tier 1
(docs/OS.md, "Tiers").

**Memory.** A bitmap frame allocator covers all E820 RAM above 1 MiB. It excludes the kernel image and the boot
module until the bundle has been decoded. Each allocation is tagged, so `/_k10s/status` reports where memory goes:
heap small/large, slots, stacks, net, kernel. There is no fixed heap region. The global allocator takes
power-of-two size classes up to 2 KiB from 4 KiB frames, and larger blocks directly from the frame allocator. So
the heap and the instance slots share all of RAM. An instance slot is `2^k + 4096` bytes of contiguous frames.
Slots are cached per size class for reuse. Instance creation, coroutine stacks and socket buffers are refused when
they would eat into a heap reserve (max(256 KiB, RAM/16)), and requests then queue instead.

The memory map is kept sorted and disjoint (the more restrictive type wins where firmware entries overlap), with
adjacent regions merged, so UEFI maps of 100+ descriptors take a few entries.

**The identity map (arch/paging.rs).** boot.rs maps the first 4 GiB with 2 MiB pages; right after the frame
allocator starts, the kernel rebuilds the map from the memory map. IA32_PAT is set to its power-on layout on every
CPU, so PCD|PWT always means uncached whatever the firmware programmed. Below 4 GiB, a 2 MiB page holding any memory
(RAM, ACPI tables, ACPI NVS) is write-back and every other one uncached: the PCI hole with every 32-bit BAR, ECAM,
the I/O APICs, the HPET and the local APIC. RAM above 4 GiB is mapped write-back with 1 GiB pages where the CPU has
them (2 MiB, and 4 KiB at unaligned edges, otherwise). Devices above 4 GiB (64-bit BARs; OVMF puts them at hundreds
of GiB) are mapped uncached by `map_mmio` when a driver claims them. Identity mapping limits physical addresses to
the 47-bit lower canonical half. EFER.NXE is enabled and everything is non-executable except the kernel's text.
The first 2 MiB (low memory and the kernel image) are split into
4 KiB pages: `.text` read+execute, `.rodata` read-only, the rest read+write, page 0 not present (null guard). JIT
code gets its own frames (tag `code`): written through the writable, non-executable identity mapping, then
*sealed* read+execute (the large page around them is split once, its page table kept) and never written again;
when the last service using the code is gone, the pages are unsealed and freed. So nothing is ever writable and
executable at the same time: not the kernel, not instance memory, not JIT code.

**Interrupts and time.** The IDT has handlers for every exception, which print a register dump (CR2 for page
faults) and take the fatal-error path (power.rs). The double fault, NMI and machine-check handlers use an IST stack.
The 8259 PICs are remapped and masked, and so is every I/O APIC pin until a driver routes one; devices use MSI-X, or
an I/O APIC line (virtio-mmio, edge-triggered, acknowledged at the device). The local APIC runs in x2APIC mode (MSR
interface, 32-bit APIC ids) under a hypervisor when the CPU has it, when the MADT lists APIC ids above 254, and always
when the firmware handed over in that mode (which cannot be left); otherwise, on bare metal without interrupt
remapping, in xAPIC mode, as Linux does (`k10.apic=xapic` keeps xAPIC wherever it can). An NMI that is not the
kernel's own stop signal is counted and reported, not fatal. The TSC frequency comes from kvmclock (exact, with no
calibration wait), else CPUID 0x15 when it states the crystal frequency, else 20 ms against the HPET, else the ACPI PM
timer, else CPUID 0x16's nominal frequency, else 20 ms of PIT channel 0 (read back by latch commands: microVMs have no
port 0x61 gate). A CPU without an invariant TSC is reported. Wall time comes from the kvmclock wall clock (re-read
every 10 s), else the CMOS RTC, which SNTP then corrects (sntp.rs, Tier 1's client: `k10.ntp` / k10s.conf `ntp`
servers, else the lease's (option 42), else `pool.ntp.org`, names through DNS; first query once the interface has an
address, then every 17 minutes, 1 s doubling to 64 s while none answers; offsets of 128 ms or more step the clock,
smaller ones are slewed at 500 ppm; a reply must come from the address asked and echo the request's random transmit
timestamp). With kvmclock the client stays off: the hypervisor's clock is followed. The LAPIC timer is one-shot and tickless: TSC-deadline mode, or count mode calibrated against the TSC. It is
armed only for the next deadline: the end of the running invocation's slice or budget, a TCP timer, or a queue
timeout. When idle the CPU `hlt`s, so an idle node takes no timer interrupts.

**PCI (pci.rs).** Configuration space goes through the MCFG's ECAM windows (every segment, the 4 KiB extended
space), else through ports 0xCF8/0xCFC (segment 0). One scan at boot follows PCI-PCI bridges from each segment's
first bus; on bare metal every other bus of the ECAM windows is probed too (other root buses of multi-socket
machines), under a hypervisor only with `k10.pci=full` (each absent function costs a VM exit there). BARs are 32- or
64-bit; a driver maps the ones it uses uncached; unprogrammed ones (qboot) get addresses in the hole below 4 GiB.
Enabling a device also enables memory, I/O and bus mastering on every bridge above it. MSI-X tables are programmed
masked-then-unmasked, MSI capabilities disabled while they change (one message), both to the boot CPU (an APIC id
above 255 would need interrupt remapping: the driver then polls).

**Devices (dev/, virtio/).** Drivers implement `NetDevice` (numbered queue pairs of receive and transmit halves,
interrupt switch, counters) or `BlockDevice` (logical block size, capacity, numbered request queues, asynchronous
`submit` with a tag and polled completions, `block_sync` for one request). DMA memory is a `DmaRegion`: contiguous,
zeroed, page-aligned frames, like an instance slot — the shape a driver written as a TIR module would get later.
The network stack and the disk users see only the traits. The first NIC is used, every disk is kept in `dev::BLOCK`
(storage.rs takes the one with K10S-DATA). NVMe controllers (class 01:08:02) get the admin queue and one I/O queue
pair (storage runs on CPU 0), the first active namespace (512-byte or 4 KiB blocks, no metadata), a PRP list page
per request slot, completions that raise MSI-X entry 0 on CPU 0 (vector `VEC_DISK`: it only wakes `hlt`), Flush
when the controller reports a volatile write cache, and a normal shutdown (CC.SHN) before power off and reboot. AHCI
controllers (01:06:01) are taken from the firmware (BIOS/OS handoff, HBA reset, staggered spin-up); each port with an
ATA disk is a device: READ/WRITE DMA EXT and FLUSH CACHE EXT (when its write cache is on) in up to 32 command slots,
which the HBA runs in order (no NCQ), one PRD entry per request, completions polled, a port restarted after a task
file error. virtio devices come from PCI (modern `1af4:1041`/`1af4:1042`,
transitional `1af4:1000`/`1af4:1001` driven as virtio 1.x when they offer the modern capabilities, legacy when they
do not) and from virtio-mmio slots (command line and DSDT; version 2, and version 1, which QEMU microvm defaults to).
The transports share split virtqueues: one zeroed region per queue (descriptors, available ring, used ring on the next
page — the legacy layout, valid for modern devices too). Modern devices negotiate VERSION_1 and FEATURES_OK and use
64-bit queue addresses; legacy queue sizes are the device's, modern ones are sized to the buffers.

**NIC (virtio/net.rs).** virtio-net negotiates MAC (and ANY_LAYOUT on legacy devices) only, so there is no checksum
or GSO offload and no mergeable buffers: every frame fits one 2 KiB buffer behind a 10-byte (legacy) or 12-byte
(VERSION_1) header. RX and TX buffers are bound to fixed descriptors, so nothing is allocated per packet. RX raises
MSI-X entry 0 on PCI, the slot's I/O APIC line on MMIO:

* Interrupts are suppressed while the kernel is busy. They are re-enabled, and the ring re-checked, just before
  `hlt`.
* TX completions are reaped lazily, without interrupts.
* Kicks are batched per main-loop iteration and skipped while the device sets `VRING_USED_F_NO_NOTIFY`.
* After activity the loop busy-polls for 50 µs before halting.

**Disk (virtio/blk.rs).** One request queue; each request is a fixed three-descriptor chain (header, data, status
byte) per slot, so nothing is allocated per request; FLUSH when the device has a write cache. The self test reads a
block the host wrote, writes, flushes and reads back 8 blocks, keeps four requests in flight and checks that a
request past the end is refused; test.sh then checks the written blocks in the host's image file.

**Real machines: NICs (metal/drivers, dev/nic.rs).** The Ethernet controllers of PCs have their drivers in
`k10-drivers`, written against small traits (registers, configuration space, delays, DMA memory) so that host tests
run them against register models of the chips; `dev/nic.rs` maps the memory BAR uncached, routes the interrupt and
puts each behind `NetDevice`. All use fixed-buffer descriptor rings like virtio-net's (buffer `i` is descriptor `i`,
2 KiB each, nothing allocated per packet; tail/doorbell writes batched per main-loop iteration), one queue pair, no
checksum or segmentation offload (smoltcp computes and checks checksums; offload would need per-frame status the
interface does not carry), the receive interrupt unmasked only when the kernel is about to halt, link state read
every 500 ms (`net: link up, 1000 Mb/s full duplex`, `net: link down`; when the link comes up DHCP starts over if it
has no lease yet or after an outage of 3 s or more, which may mean another network; shorter flaps keep the lease),
bus mastering turned on only after the device's reset:
* **e1000e** (82574L/82583V; I217/I218; I219 on 100-series and later PCHs; legacy descriptors, one MSI message): on
  the PCH parts the PHY is shared with the Management Engine and reached over MDIO only with the software
  semaphore. The driver takes the PHY out of Ultra Low Power (asking the ME through H2ME when its firmware is valid,
  else toggling LANPHYPC, clearing forced SMBus mode in the PHY and the MAC and the ULP configuration, then a PHY
  reset), checks the PHY answers (else forces the MAC into SMBus mode, toggles LANPHYPC unless the ME blocks PHY
  resets, releases SMBus mode again: the SMBus/MDIO handover), resets MAC and PHY, turns K1 off (Kumeran and PHY;
  Linux does at 1 Gb/s because of packet loss), clears the low-power link settings (PHY_CTRL) and, on the I219,
  flushes descriptor rings the firmware left running (DESC_RING_STATUS) before the reset and limits transmit
  requests (TARC0, Sunrise/Kaby Lake errata). The 82574 gets its PCIe completion errata bits (GCR, GCR2).
* **igb/igc** (82576, I350, I210/I211; I225/I226; advanced descriptors, MSI-X entry 0 for receive and link): reset
  (DEV_RST on igc), the PHY behind SW_FW_SYNC, Energy-Efficient Ethernet off (I225 link drops), the queue enable bits
  polled. QEMU's `igb` (82576) runs the shared path; the igc differences (interrupt registers at 0x1500, 2.5 Gb/s)
  are checked against a register model.
* **r8169** (RTL8111/8168/8411, RTL8125; Realtek descriptors with OWN/EOR bits, MSI-X or MSI): the chip version
  (XID) picks the PHY access (PHYAR, or the OCP window from the 8168G on), the PHY PLL is powered up (PMCH), the
  8125's newer transmit descriptor format is turned off and its address read from MAC0_BKP; short frames are padded
  in software. The per-revision PHY parameter tables and PHY firmware (`rtl_nic`) of Linux's driver are not applied
  ("Hardware bring-up").

The PCI ids are in `drivers/src/ids.rs` and, for k10boot's `tier=auto`, in `bootproto/src/devices.rs` (a test keeps
them equal).

**Consoles (console/).** Everything printed goes to the serial port and to:
* the **framebuffer console** when k10boot hands over the screen (module `k10s.framebuffer`, the GOP mode the
  firmware set): an 8×8 public-domain font (font8x8) scaled by an integer factor (2 at 1280×800: 80×40 characters
  of 16×20 pixels), light grey on black, the framebuffer mapped write-combining (PAT entry 1) and never read back:
  a copy of the text decides what to draw; past the last row the text jumps up by a quarter of the screen, and only
  changed cells are redrawn. The boot prints `console: framebuffer WxH, CxR characters of AxB pixels`.
* the **UDP log console** (`k10.console=udp:A.B.C.D[:PORT]`, default port 6666, like Linux's netconsole): every line
  from the moment the command line is read is kept in a 32 KiB ring (oldest bytes dropped, the loss reported) and
  sent from port 6665 as datagrams of whole lines once the interface has an address (`nc -klu 6666`, `socat -u
  UDP-RECV:6666 -`). A panic or fatal error on CPU 0 sends the rest straight through the NIC, with the headers of the
  last console datagram (so the collector's or gateway's MAC address is known).

**Watchdog (watchdog.rs, drivers/src/watchdog/).** The first of: an ACPI WDAT table (Microsoft's Watchdog Action
Table: the firmware's own description of its watchdog as register actions, run without AML; many Intel desktop
boards and servers have one), the Intel TCO timer (generation 2 through the LPC bridge: ICH6 to 9-series PCHs and
QEMU's q35, NO_REBOOT in GCS; generation 4 through the SMBus controller and the P2SB sideband on 100/200-series PCHs;
generation 6 through the SMBus controller, NO_REBOOT in TCO1_CNT, from the 300-series on), the i6300ESB. It is found
at boot (the line says if it reset the machine before this boot), armed when the main loop starts (60 s,
`k10.watchdog=`; `watchdog=0` in k10s.conf turns it off), petted by CPU 0's main loop once a second, disarmed before a
requested power off or reboot and before a halt (`k10.panic=0`). A kernel that hangs is reset; an unhealthy boot
counts against k10boot's native boots, so the machine falls back to Tier 1.

**TCP/IP and HTTP.** The interface address comes from DHCPv4 (smoltcp's client: renewal by unicast at T1,
rebinding by broadcast at T2, a NAK or the end of the lease starts over; a lost lease removes the address) or from
`k10.ip=` (static). IPv6 is configured as Tier 1's Linux kernel does: the link-local address from the MAC (EUI-64),
then SLAAC (router solicitations; the address of an advertised prefix, the router as default route; the console says
`net: IPv6 ADDR/64 (SLAAC)`). Listening sockets take both families, so the services answer on IPv6 too; echo
requests are answered in both. The NICs receive all multicast (IPv6 neighbor discovery). Names (the SNTP servers')
resolve through smoltcp's DNS client (A records) against `k10.dns`, else the lease's name servers (option 6). The DISCOVER goes out as soon as the NIC is up; a single node applies its bundle meanwhile and
waits up to 30 s for the lease before its ready line, a cluster member waits for it before it starts (it sends from
that address, and advertises it unless told otherwise). Ingress is
processed one packet at a time (`poll_ingress_single`), and the listening sockets are topped up after every
packet. A SYN burst therefore never meets an empty backlog, without keeping a large pool of idle sockets.
HTTP/1.1 supports keep-alive, pipelining (one request in flight per connection), Content-Length bodies and
`Expect: 100-continue`. Chunked request bodies get a 411.

**Runtime.** Routing and the host API follow the hosted node:
* `/<service>/<rest>` invokes the service. `req_path` sees `/<rest>` plus the `?query`.
* `/_k10s/*` needs a request signed with the admin key, except `/_k10s/health` and `/_k10s/audience`:
  `Authorization: K10s-HMAC a=…,t=…,n=…,h=…,sig=…` (`k10_mesh::admin_auth`, docs/NODE.md; `k10s status|apply|logs
  --node ADDR --admin-token-file FILE` or `tools/admin_auth.py ADDR …` sign). `a` is the guest's audience, 16 random
  bytes drawn at boot and served open as hex at `GET /_k10s/audience`: a request is accepted by one boot of one
  node only, at most once (nodes sharing the admin key, or the same guest after a restart, refuse it). The head is
  checked before the body is read (unsigned, wrong key or audience, more than 60 s from the guest's clock, replayed
  nonce → 401 with `WWW-Authenticate: K10s-HMAC`; too many requests in the window → 429), then the body against
  the hash it signed. That covers everything else under `/_k10s` (there is no separate admin listener).
* `/_k10s/status` returns JSON with memory per tag, counters, network and per-service stats.
* `/_k10s/apply` takes a bundle. `/_k10s/logs/<service>` returns the last 64 `log` lines.
* `/_k10s/bench/<n>/<service>/<rest>` runs `/<rest>` on `<service>` n times back to back in one task and answers
  the per-invocation CPU time (min, median, p90, mean; TSC time while the invocation runs, host calls included,
  network excluded) — how RESULTS.md compares the engines.
* All 19 host functions of docs/HOSTAPI.md are implemented (`k10_host`, shared with the hosted node; the selftest
  runs its conformance cases). `log` writes to the serial console and a ring buffer. `now_ns`/`time_ms` use the
  kernel's `GuestClock`, keyed by RDRAND at boot.
  The KV store is per service, in memory and, with `K10S-DATA`, on disk ("Storage"); it survives redeploys and
  reboots. Every `kv_*` call runs under the node lock, so `kv_add` and `kv_cas` are atomic across CPUs; a write that
  would eat into the heap reserve, or past what the store holds, traps `Host(3)`. `call` runs a local service in the caller's
  coroutine, or (cluster mode) forwards to the node it is placed on.

Apply validates every module, resolves imports by name and parameter count, and checks the `handle`/`init`
exports. Single-node: it runs `k10_mesh::place` over a one-member cluster (this node, with its free memory as
capacity); cluster mode: see below. Then it swaps the service table atomically: services whose spec and placement
did not change keep their runtime (warm instances, counters), prepared code is reused by code hash, in-flight
invocations finish on the old code and queued ones move to the new version.

**SMP (smp.rs, percpu.rs).** The boot CPU takes the CPUs the MADT lists as enabled (or the MP table's), up to 64 and
`k10.smp=N`, copies a 16→32→64-bit trampoline to page 0x8000 (made read+execute just for the bring-up; the APs take
their index with a `lock xadd` while paging is still off, so the page never needs to be writable for them), and sends
each AP its own INIT, then after 1 ms a STARTUP, and a second STARTUP to those not up 200 µs later — all APs at once,
and none the firmware did not list. Each AP loads the shared page tables (with NXE), gets its own stack, GDT + TSS +
IST stack, the shared IDT, the PAT, its local APIC (in the boot CPU's mode; TSC-deadline timer) and its per-CPU block
(GS base), and enters `runtime::worker_main`. Under TCG, 32 CPUs are online 5 ms after the first INIT. Locks (`IrqLock`) are spinlocks that also mask interrupts on the
holding CPU; per-CPU state (preemption request, JIT preemption word, coroutine bookkeeping, scheduler queues) needs
none.
* *Dispatch*: CPU 0 owns the NIC (its interrupt targets it), TCP/IP, HTTP parsing, the cluster layer and the admin API, and
  dispatches every invocation: requests of services whose average CPU time is under 20 µs stay on CPU 0 (a
  cross-CPU hand-off costs more) while its queue holds under 0.5 ms of them; others go to the least loaded CPU
  (CPU 0 counts one extra). A dispatched request lands in the target's inbox, plus a wake-up IPI if that CPU is
  halted (`idle` flag checked after the push; the CPU sets `idle`, re-checks its inbox with interrupts off, then
  `sti; hlt`). Results go back to CPU 0's outbox (HTTP replies, answers to peers, forwards), with an IPI if it is
  halted. A task never migrates; each CPU has its own run queues, time slices and preemption (its LAPIC timer).
* *Instances* are pooled per service and CPU (warm instances stay cache-local); `max_instances` is node-wide
  (atomic count), and a CPU at the limit takes an idle instance from another CPU's pool.
* *Shared state* (service table, KV, logs) sits behind one spinlock, held for lookups and copies only — never while
  TIR code runs. Code is shared read-only (`Arc<Prepared>`).
* *Page permissions*: sealing JIT code or splitting a large page bumps a generation; a CPU reloads CR3 before running
  JIT code when its TLB is older. With several CPUs, freed code frames are recycled for code only, so a stale
  read+execute TLB entry elsewhere can never cover a frame that became data.

**Scheduling.** Each in-flight invocation runs on a coroutine stack (32 KiB with the interpreter, 96 KiB with the
JIT, whose frames live there). The switch is six callee-saved registers plus `rsp`. A canary is checked at every
switch; the measured high-water mark of host code is under 3 KiB. Queued requests start one at a time just before
they run, so on a CPU a request that finishes within its slice hands its warm instance and stack straight to the
next one. Under steady load that means one instance per service and CPU and one stack, whatever the number of
connections.

The timer interrupt (each CPU's own LAPIC timer) requests preemption when the running invocation's 2 ms slice or its
`cpu_ms` budget ends: the interpreter polls the flag through `Host::interrupt` every 64 safepoints, JIT code sees its
stack limit change (see Engines). On a slice end the invocation yields to the "long" queue, which gets one slice per
scheduling round. On CPU 0, while a long task runs, RX interrupts are enabled and an arriving packet also requests
preemption, so network I/O and short requests wait microseconds, not
slices, behind any number of CPU hogs. On budget exhaustion the invocation traps `Interrupted`, the request gets a
500 and the instance is discarded.

A service-to-service `call` first checks the caller's manifest `calls` allowlist: a call it does not grant returns
-1 at once, like an unknown service, before any lookup or forwarding. The callee runs on one of its instances inside
the caller's coroutine. Nesting is
limited to depth 8. The callee gets its own budget, capped by the caller's remaining one. When the callee's
service is at `max_instances`, the caller yields until an instance is released (or -3 after 5 s). Callee traps
return -4. `fresh` services restore the post-`init` snapshot before each request.

**Engines (runtime/engine.rs, runtime/jit.rs).** A module is prepared once, at apply, into a `Prepared` shared (by
code hash) by every service deployed with the same blob and by all of their instances; a redeploy reuses unchanged
code. The engine is chosen at boot: the JIT unless `k10.engine=interp`. Both reach the host API through one trait,
`HostCalls` (`call(HostFn, args, mem)` and `safepoint()`), implemented once (runtime/host.rs): the invocation is a
`k10_host::Env`, and `k10_host::dispatch`, shared with the hosted node, runs the host functions.
* **Interpreter**: `k10_ir::Interp`, value stacks on the heap; polls the `time::PREEMPT` flag every 64 safepoints.
* **JIT** (docs/JIT-ABI.md): `k10_jit::compile_blob` (no_std) produces a position-independent blob; it is copied
  into sealed code frames (above) with `CodeBlob::write_to` and run with `BlobMeta::invoke` *on the coroutine
  stack*. The `VmCtx` lives in the invoking frame: `mem_base`/`mem_mask` = the slot (its 4 KiB tail covers the 8
  bytes the JIT may touch past `2^k`), `user` = the invocation's `HostCalls` and memory range, `host_fns` = one
  `extern "C"` thunk per host function (19 monomorphic thunks); a host `Err(trap)` becomes `trap_request`.
  `real_stack_limit` = coroutine stack bottom + canary + 32 KiB host reserve: every JIT function entry checks its
  frame against it, so runaway recursion traps `StackOverflow` (≈ 2000 frames of a small function in the 64 KiB
  left for JIT frames; JIT coroutine stacks are 96 KiB instead of 32) and host functions, nested service calls'
  host-side frames, the interrupt hook and interrupt frames always have ≥ 32 KiB.
* **JIT preemption**: while JIT code runs, `time::PREEMPT_WORD` points at its `VmCtx::stack_limit`. The timer
  interrupt (slice or budget end) and the RX interrupt while a CPU hog runs call `time::request_preempt()`, which
  sets the flag *and* stores `usize::MAX` there; the next safepoint (function entry, loop head) calls the interrupt
  hook, which restores the limit and runs the same `safepoint()` as the interpreter: `Interrupted` on budget
  exhaustion, else a yield to the scheduler with the JIT frames left on the suspended stack. Nested invocations
  (service calls) publish their own context and restore the caller's; a coroutine switch saves and restores the
  word (coro.rs), and a request pending when a word is published is forwarded to it, so no preemption is lost.
* JIT code uses SSE2 for f64; the kernel is soft-float, so interrupt handlers never touch vector state, and no
  vector value is live across a host call or safepoint (SysV caller-saved), so coroutine switches need not save it.
  MXCSR keeps its reset value. Code is compiled for this CPU (`CpuFeatures::detect`, asked once).

**Cluster mode (cluster.rs).** With `k10.join=` (or `k10.gossip=` / `k10.cluster`) the guest is a member of a k10s
cluster, speaking the hosted node's protocols (docs/NODE.md "Cluster mode", crates/k10-mesh/README.md):
* *Membership*: the sans-IO `k10_mesh::Gossip` (SWIM) over a smoltcp UDP socket (port 7946), clocked by the
  kernel's monotonic time and driven from the main loop (its next deadline bounds the idle `hlt`). The member
  advertises `k10.gossip`/`k10.rpc`/`k10.http` and a capacity of `k10.mem` (default: free memory at boot), with
  the wall clock as starting incarnation (a guest rebooted with the same `k10.node_id` refutes its old Dead record).
* *Authentication*: as on hosted nodes (docs/NODE.md "Cluster authentication"), with the same code
  (`k10_mesh::secure`): gossip datagrams sealed with the cluster key (opened with it or the next key; timestamps
  within `k10.gossip_max_skew_s` of the guest's wall clock) and `Auth`-handshaken, encrypted peer links. Handshake
  nonces and the gossip nonce prefix come from RDRAND: key mode refuses to start without it, a failed read stops
  cluster startup (gossip) or drops that link (nonce), never falling back to the TSC.
* *Wall clock*: kvmclock's wall clock at boot, advanced by the TSC and re-read from kvmclock every 10 s, so gossip
  and admin-request timestamps follow the host's clock over long uptimes (the monotonic clock is never stepped).
* *Peer links*: `k10_mesh::secure::Link` over smoltcp TCP (port 7947, plus outgoing connections): `Auth`, then
  `Hello` in the first sealed record, then sealed records. A peer that has not finished the handshake within 5 s is
  dropped, and at most 16 accepted links (two socket buffers each) may be in their handshake (the oldest is
  evicted). One link per peer pair (dial members with a larger id when they come up, others lazily; when both ends
  dialed, the connection dialed by the smaller id wins), requests multiplexed by id, write buffers flushed every
  main-loop iteration, 200 ms…5 s dial backoff, 1 s connect timeout.
* *Desired state*: a newer `DesiredState` from gossip is fetched (`GetManifest`, then the missing blobs with
  `GetBlob`, pipelined; the origin first, then the other live members; 10 s without progress → next one), checked
  with SHA-256 and kept in memory, and in `K10S-DATA` when there is one (`manifest.k10m` and `blobs/<sha256>`, as on
  a Tier 1 node): a rebooted node serves it before it rejoins, a cluster whose nodes all lost power serves again
  without an operator. `POST /_k10s/apply` on the guest works like on a hosted node: version
  0 = highest known + 1, install locally first (validates), then `set_desired`; peers fetch from the guest.
* *Placement*: `k10_mesh::place(manifest, live members)` on every desired-state change and, debounced 50 ms, on
  every change of the live member set. All services are installed (any node may run any service) with `Local` or
  `Remote(nodes)`; a service whose spec and placement did not change keeps its runtime. Each install also computes
  the per-service rendezvous ranking over all live members: the routing fallback order.
* *Routing*: requests for `Remote` services — HTTP ingress, and `call` from a local invocation (the coroutine
  waits on a `CallSlot`) — become `Forward`s: a placed replica (Alive, least outstanding), then down the ranking
  (this node included: then it runs here), then a Suspect replica; at most `replicas + 3` peers; re-sent only when
  known not to have run (bytes never handed to the TCP socket, or `NOT_HERE`). A peer's `Call` runs here whatever
  the placement (one hop at most) and without a `calls` check (the calling peer is trusted to have done it); an
  unknown service answers `NOT_HERE`.
* *Addresses*: without `k10.gossip`/`k10.rpc`/`k10.http` (or with a bare port), the member advertises the
  interface's address — the DHCP lease or `k10.ip` — with the default ports.
* *Leaving*: before a power off or reboot (`POST /_k10s/power`) the guest gossips `Left`, so the other members
  move its services at once instead of after failure detection.
* *NAT*: the guest is 10.0.2.15 behind QEMU's slirp. `hostfwd` rules expose its gossip (UDP), rpc and http ports
  on the host; it advertises those host addresses (`127.0.0.1:<port>`), and every loopback address it sends to —
  hosted members, or other guests, advertising `127.0.0.1:<port>` — is reached through the slirp host alias
  10.0.2.2 (`k10.host_alias`). Replies to datagram sources work because slirp maps them back.
* `/_k10s/status` gains the hosted node's `"cluster"` object (members, desired/applied, placement, links,
  `auth` with `key_id`/`next_key_id`, counters) and a `placement` per service.

```sh
# two hosted nodes on the host, one guest; the guest's forwarded ports are 19003 (http), 19103/udp, 19203
k10s keygen -o cluster.key; k10s keygen -o admin.key
k10s node --http 127.0.0.1:19001 --gossip 127.0.0.1:19101 --rpc 127.0.0.1:19201 --mem 64MiB --cluster-key-file cluster.key --admin-token-file admin.key &
k10s node --http 127.0.0.1:19002 --gossip 127.0.0.1:19102 --rpc 127.0.0.1:19202 --mem 64MiB --cluster-key-file cluster.key --admin-token-file admin.key --join 127.0.0.1:19101 &
qemu-system-x86_64 … -netdev user,id=n0,hostfwd=tcp:127.0.0.1:19003-:80,hostfwd=udp:127.0.0.1:19103-:7946,hostfwd=tcp:127.0.0.1:19203-:7947 \
    -fw_cfg name=opt/k10s/cluster_key,file=cluster.key -fw_cfg name=opt/k10s/admin_key,file=admin.key \
    -append "k10.join=127.0.0.1:19101 k10.gossip=127.0.0.1:19103 k10.rpc=127.0.0.1:19203 k10.http=127.0.0.1:19003 k10.mem=67108864"
k10s status --node 127.0.0.1:19003 --admin-token-file admin.key     # any member, signed with the shared admin key
```

## Hardware bring-up

A checklist for a desktop or mini-PC with a screen and keyboard (the first real machine: STATUS.md step 13c). The
native kernel boots from the `metal` image of a release, the same image as Tier 1 (docs/OS.md, "Tiers"); k10boot
falls back to Tier 1 by itself after two unhealthy native boots, so nothing here can leave the machine unbootable.

1. **Find the Ethernet controller.** The board's specification, or `lspci -nn | grep -i ethernet` from any Linux
   live USB, or let k10boot tell you: the image's `tier=auto` prints `tier auto: e1000e 8086:15bc at … is
   supported: native kernel` or `tier auto: no supported network adapter (10ec:8126 at …): linux` (from a USB stick
   it then boots Tier 1 anyway: `K10S-DATA`'s controller is USB, which the native kernel does not drive). Supported:
   `metal/drivers/src/ids.rs` (Intel 82574L, I217/I218, I219, 82576, I350, I210/I211, I225/I226; Realtek
   RTL8111/8168/8411, RTL8125). Others boot Tier 1 (its `metal` kernel has Linux's drivers and the NIC firmware).
2. **Firmware setup.** UEFI boot (CSM off: the screen console needs a UEFI GOP mode), Secure Boot off (or the
   image signed for it, docs/OS.md "Secure Boot"), USB first in the boot order, "fast boot" off so the firmware sees
   the stick. A BIOS-only machine boots the native kernel through GRUB with `tier=metal` (no screen console there:
   use the UDP console).
3. **Write the image** to a USB stick (every byte of it is replaced):
   ```sh
   gh release download --repo bzzdevlabs/k10s --pattern 'k10s-os-*-x86_64-metal.img.xz' --pattern SHA256SUMS
   sha256sum -c SHA256SUMS --ignore-missing
   xz -dc k10s-os-*-x86_64-metal.img.xz | sudo dd of=/dev/sdX bs=4M conv=fsync    # Linux
   xz -dc k10s-os-*-x86_64-metal.img.xz | sudo dd of=/dev/rdiskN bs=4m             # macOS (diskutil list)
   ```
4. **Configure it** on the stick's `K10S-BOOT` partition (FAT): an admin key and `k10s.conf` as for Tier 1
   (`k10s keygen -o admin.key`, copy it, `admin-token-file=/boot/admin.key`; `join=…` and `cluster-key-file=…` to
   join a cluster), then `/k10s/tier` (keep its first line, no blank lines, a newline at the end):
   ```
   tier=metal
   metal_cmdline=k10.verbose k10.panic=0 k10.console=udp:192.168.1.10
   ```
   `k10.verbose` prints the boot in detail, `k10.panic=0` keeps a fatal error on the screen instead of rebooting,
   `k10.console=udp:IP` also sends everything to another machine of the LAN (start `nc -klu 6666` or
   `socat -u UDP-RECV:6666 -` there first). Boot Tier 1 once first (`tier=linux`) if you want to see the machine
   serve with Linux's drivers before trying the native kernel.
5. **Boot.** The screen shows k10boot (`k10boot: tier metal`, `native boot 1 of 2 before Tier 1 is pinned`), then the
   kernel's framebuffer console: `console: framebuffer …`, the NIC (`e1000e: I219-V 8086:15bc, PHY id …, ME firmware
   present`, or `igc:`/`r8169:`), `watchdog: …`, `net: link up, 1000 Mb/s full duplex`, `net: DHCP …`,
   `k10-metal ready … ip=…`, `watchdog: armed, 60 s`. Then from another machine:
   ```sh
   curl http://IP/_k10s/health
   k10s status --node IP:80 --admin-token-file admin.key
   k10s apply examples/app.k10s --node IP:80 --admin-token-file admin.key && curl http://IP/hello/
   ```
   About 15 s after the first frame arrives the kernel clears k10boot's boot counter (`efi: healthy`).
6. **If it fails, capture** a photo of the screen (all of it: k10boot's lines and the kernel's), the UDP console's
   output, and the NIC's PCI id. What the lines mean:
   * Nothing after k10boot's `native boot`: no GOP mode (CSM on, or a GPU without UEFI support): the UDP console, or
     a serial port (`k10.console=com1,…`) if the board has one.
   * `no supported NIC` and a PCI list: the controller is not one of the supported ones (send the list).
   * The NIC line is an error (`PHY unreachable over MDIO`, `reset did not complete`, `registers read as all ones`)
     or the kernel stops after it: the bring-up sequence needs work on this chip (send the photo).
   * No `net: link up`: cable, switch port, or the PHY (I219/I225 power states, Realtek PHY firmware); a `link up`
     but no `DHCP` line: try a static address, `k10.ip=192.168.1.50/24,192.168.1.1`.
   * The screen freezes: the watchdog resets the machine after 60 s (if the chipset has one), and after two
     unhealthy boots k10boot boots Tier 1 until `/k10s/tier` is written again.
   * Back to Tier 1 any time: `tier=linux`.

What only real hardware tells (STATUS.md step 13c): every item of the drivers' "not done" lists — the I219's ULP
exit, SMBus/MDIO handover and ME interplay on a live ME, K1 off, the descriptor-ring flush; I225/I226 PHY and EEE
behaviour; Realtek revisions without their PHY tables and firmware; link up/down on a real cable; MSI delivery on
real chipsets; the GOP framebuffer's format and write-combining on real GPUs; the TCO generations 2/4/6 (NO_REBOOT
locks, the P2SB sideband) and firmware WDAT tables; booting the stick on real UEFI and BIOS firmware; the power
button on real firmware (the fixed-feature PWRBTN after the switch to ACPI mode, or the GPE of a control-method
button; `k10.verbose` prints which one is armed); SNTP from a real NTP pool and IPv6 from a real router.

## Dependencies and why

| crate | used by | why |
|---|---|---|
| `k10-ir` (path, no default features) | kernel, mkbundle | TIR types, decoder, validator, reference interpreter (execution engine), builder |
| `k10-jit` (path, no default features: `no_std + alloc`) | kernel | TIR → x86-64 compiler (`compile_blob`, `CodeBlob::write_to`, `BlobMeta::invoke`, `VmCtx`); the kernel provides executable memory |
| `k10-mesh` (path, no default features) | kernel, mkbundle, k10boot | `Bundle`, `Manifest`, `sha256`, `place()`, gossip state machine, peer-protocol framing; signed images (`osimage`, Ed25519) |
| `k10-bootproto` (path) | kernel, k10boot | the boot protocol between them and the supported-device table (`bootproto/`) |
| `k10-lang` (path) | mkbundle only | compiles the Ten example services (`examples/*.ten`) into the test bundle |
| `smoltcp` 0.14 (`alloc`, `medium-ethernet`, `proto-ipv4`, `proto-dhcpv4`, `socket-dhcpv4`, `socket-tcp`, `socket-udp`) | kernel | A correct TCP (retransmission, windows, RST/TIME-WAIT, congestion control) is the one part where writing our own would be large and risky. smoltcp is the standard `no_std` stack, 0BSD-licensed and dependency-light (`heapless`, `managed`, `byteorder`, `bitflags`, `cfg-if`). |
| `libc` | tools/linux-guest only | ioctl/finit_module for the Linux comparison `/init` (not part of the kernel) |

Deliberately *not* used:
* The `uefi` crate in k10boot. The UEFI interfaces it needs (boot and runtime services, five protocols) are about 400
  lines of declarations in `boot/src/efi.rs`; the loader's dependencies stay k10-bootproto and k10-mesh (image
  signatures, SHA-256).
* The `x86_64` crate. The GDT, IDT, APIC and port-I/O code is about 450 lines, and the interrupt entry stubs need
  assembly anyway (`extern "x86-interrupt"` is unstable).
* An allocator crate. The heap is about 140 lines with exact per-tag accounting and grows from the same frames as
  the instance slots, so there is no fixed heap size. That is what makes 3–4 MiB guests possible.

## Storage

`K10S-DATA` is the partition of a GPT disk with its type (`6b10c5a0-da7a-4000-8000-000000000001`) or its name, as
Tier 1 finds it; the images carry one (docs/OS.md). At boot storage.rs takes the disk that has it (the one k10boot
read, if several do), grows the partition to the end of the disk when it is the last one (backup GPT written and
flushed first, then the primary: the old table stays valid until the end; `k10.grow=0` keeps it), and opens the store
of `crates/k10-kvlog` on it, formatting a blank partition (anything else that is not a store is left alone, KV then
stays in memory). The store is the one Tier 1's node writes (docs/OS.md, "Data"), so a machine moved between tiers
keeps its KV data, its node id and the cluster's desired state:

* **Layout.** A superblock pair (alternate generations, each a sector-atomic write), two 16 MiB kernel slots, then a
  log of 4 MiB segments: namespaced records with the hosted node's record codec (`len | crc32 | op | klen | key |
  value`), each segment's checksums masked by its sequence number, segments linked by SEAL records.
* **Recovery.** Replay follows the segments from the superblock's oldest one and stops at the first record that does
  not check out; the unit where it stopped is written back unchanged followed by a SEAL to a segment with a fresh
  sequence number (reserved in the superblock beforehand), so nothing a crashed write left behind is ever read again.
  Each namespace comes back as a prefix of its writes, with every write of a batch whose flush completed.
* **Group commit.** KV writes go to per-namespace pending records under the node lock (as on hosted nodes). CPU 0's
  main loop opens a 2 ms window at the first one, then writes the batch (whole 4 KiB units, padded) and flushes it
  asynchronously through `BlockDevice` (MSI-X on NVMe wakes the idle CPU; otherwise the idle loop polls every 50 µs
  while a batch is in flight). An acknowledged write can be lost within that window plus the I/O time, the hosted
  node's contract. Power off and reboot drain it.
* **Checkpoints.** When the log is over twice the live data (and 1 MiB), or free segments run short, CPU 0 writes the
  whole live state into fresh segments, flushes, and points the superblock at them (synchronously; the old segments stay
  the store until then). About a quarter of the log may hold live data (the node's own files count); past it `kv_put`
  traps `Host(3)`. After a failed write the store is not durable until such a rewrite succeeds (retried with backoff);
  `/_k10s/status` reports `"storage":{"durable":…}`.
* **The node's own files** use Tier 1's names in the store's `.files` namespace: `node-id` (when `k10.node_id` is not
  given), `manifest.k10m` and `blobs/<sha256>` (cluster mode); the update agent keeps the images that failed in
  `.native`.

## Updates

Cluster-rolled OS updates (docs/OS.md, "Updates") reach native nodes through `K10S-DATA`'s kernel slots rather than
`K10S-BOOT`, which the native kernel never writes. k10boot hands over which kernel it booted (`k10s.image`); the node
reports it in its gossip record (platform `x86_64-native`, updatable when it has `K10S-DATA`), fetches and keeps the
image the manifest's OS plan gives its platform, and follows `k10_mesh::rollout` like a Tier 1 node: when its wave
comes it verifies the image against the keys k10boot trusted, writes it into the slot it does not run (data
flushed, then the superblock arms the trial), leaves the cluster and reboots. k10boot boots the trial while the boot
counter allows (two boots), else the committed slot. The trial commits (the boot counter cleared, then the
superblock) once the node holds and applied the cluster's desired state and, if it has peers, sees one, for
`k10.os_commit_grace` seconds; it is abandoned after `k10.os_trial` seconds, and when k10boot fell back. An abandoned
or refused image is recorded as failed, which halts the rollout cluster-wide. Power cuts at any step leave the
committed kernel booting or the armed trial being counted (`os/test.py update-native`). Updates need cluster mode
and k10boot (UEFI); GRUB's Multiboot2 path boots `/k10s/metal.elf` only.

## Limitations (v0)

* **SMP is simple**: CPU 0 does all network I/O (one NIC queue), dispatch is decided once per request (no work
  stealing or migration of queued work), at most 64 CPUs (a larger machine runs on 64; per-CPU state is static).
  TLB coherence is lazy (generation check before running JIT code) rather than shootdown IPIs. Interrupts go to CPU
  0 in physical destination mode without interrupt remapping (no IOMMU), so a boot CPU whose APIC id exceeds 255 gets
  no device interrupts (the NIC is then polled every millisecond).
* **No demand paging.** A slot is fully backed (64 KiB + 4 KiB for the test services). There are no CoW snapshots;
  `fresh` restores with `memcpy`.
* **No per-service memory reservations.** Instances take free frames first come, first served (hosted nodes reserve
  one instance per placed service, `docs/NODE.md`); placement, shared with hosted nodes, charges each replica
  `2^memory` + 4 KiB against the guest's capacity, so placed services fit, but a busy service can take the frames
  another one needs for its first instance.
* **Storage needs `K10S-DATA`.** Without it (or with `k10.data=none`) KV, the node id, the desired state and a
  single node's applied bundle live in memory: a rebooted node fetches the desired state again from its peers, a
  single node starts again from its boot bundle. Storage I/O and
  checkpoints run on CPU 0 (a checkpoint of a large store stalls its network loop meanwhile); NVMe uses one I/O
  queue, AHCI no NCQ. Volatile write caches, firmware quirks and power loss on real disks are untested (QEMU only).
* **Cluster mode.** A guest that is killed rather than powered off is detected as failed. KV is node-local, as on
  hosted nodes. The keys are files on `K10S-BOOT` (readable by whoever reads the disk, as on Tier
  1) or QEMU's fw_cfg (readable by whoever controls the hypervisor, as the guest's memory is); Firecracker and Cloud
  Hypervisor have neither, so no keys reach a guest there yet. No separate admin listener and no `/_k10s/metrics`.
  A member advertising the interface's address reboots when DHCP changes it (Tier 1 restarts its node), after
  leaving the cluster.
* **JIT code is page-granular per module** (W^X forbids appending to an executable page): a 300-byte module takes a
  4 KiB page; modules deployed several times share one copy by code hash.
* **No TLS; IPv6 by SLAAC only.** No DHCPv6 and no RDNSS (name servers come from DHCPv4 or `k10.dns`); names
  resolve to IPv4 addresses (A records) and cluster addresses are IPv4 (`advertise`, `join`); one SLAAC prefix;
  neighbor discovery without duplicate address detection; the UDP console is IPv4.
* **NICs: virtio, Intel e1000e/igb/igc, Realtek r8169-class; disks: virtio-blk, NVMe and AHCI.** Broadcom tg3, 10 GbE
  and cloud NICs later (M3, M4). One queue pair, no offloads, no jumbo frames, no VLANs. The Intel and Realtek
  drivers are tested under QEMU (82574L, 82576) and against register models (I219 with ULP and the ME, I225,
  RTL8168/8125), not yet on the chips themselves ("Hardware bring-up"). virtio-mmio interrupts are edge-triggered I/O
  APIC lines; PCI devices need MSI-X or MSI (INTx routing would need `_PRT`, which is AML): a device without them is
  polled.
* **ACPI without AML.** `_PTS`, `_PRT`, `_OSC` and every other method are never run: S5 works where `\_S5` is a plain
  package (QEMU's, and most firmware's); of the ACPI events only the power button is handled (fixed-feature, or a GPE
  whose own method notifies it: "Power"); devices described only in AML other than virtio-mmio slots are not found. Real-firmware quirks (x2APIC handover, broken tables,
  MTRR/PAT setups, `_PTS`-dependent S5) are untested (QEMU only).
* **Wall-clock accuracy.** Without kvmclock (bare metal), the wall clock starts from the CMOS RTC (whole seconds; the
  RTC is never written back) and runs on the TSC calibrated at boot (20 ms against the HPET or PM timer, tens of
  ppm) between SNTP corrections (every 17 minutes). SNTP is unauthenticated (as on Tier 1): an on-path attacker can
  move the clock.
* **Slow serial console.** Logging costs about 15 µs per byte under QEMU (one host `write()` per byte), so boot is
  quiet unless `k10.verbose` is given. The framebuffer console has no scrollback and no keyboard; the UDP console
  loses what a panic prints before its first datagram went out (no destination MAC address yet).
* **Watchdogs: WDAT, Intel TCO and i6300ESB only.** AMD (SP5100/FCH) and IPMI/BMC watchdogs are not driven; a
  machine without one of these runs without a watchdog (the boot says nothing unless `k10.verbose`).
* **No guard pages on coroutine stacks.** A canary is checked at every switch instead, and the call depth is
  bounded.
* **No guard pages around instance slots.** Slots are contiguous frames in the 2 MiB identity map; a guard would cost
  a page-table split and a frame per slot plus TLB shootdowns on every allocation (the kernel never revokes rights
  it handed out), and would only catch linear overruns, which masked SFI addressing cannot produce (the 4 KiB tail
  absorbs the 8-byte straddle). Hardware protection that does exist: slot memory is never executable (NX), JIT code
  and the kernel are W^X, page 0 is unmapped. There is no OS layer under the kernel: an SFI escape here reaches
  everything, which is why the JIT's output is verified independently (SECURITY.md, "Defense in depth").
* **BIOS boots are unchecked.** GRUB verifies no signature and k10boot's boot counter exists only on UEFI.

## Changes needed in other crates

None were required: `k10-ir`, `k10-jit` and `k10-mesh` work as is with `default-features = false`, and the
metal guest interoperates with hosted nodes built from the committed sources (the wire format is unchanged).
Note: cluster.rs is written against the `k10_mesh::wire` API of the current working tree (`Msg::Call::path` as raw
bytes, `Msg::encode` returning `Result`), which another change introduced after commit d95eaf6; against d95eaf6's
`wire.rs` (`path: &str`, infallible `encode`) two lines would need the old form. Nice to have:
* `k10_ir::init_memory` taking the data segments rather than a whole `Module` (we keep a data-only `Module`).
* A way to shrink or bound `InterpStack` capacity (we use one per invocation level instead of one per instance).
* `Bundle::decode` borrowing blobs instead of copying them (the boot module is copied once, then freed).
* `k10_jit`: a `CompileOptions` constructor from explicit `CpuFeatures` without calling `detect()` (we cache the
  default), and a way to pack several small modules into one executable page at load time.
* `k10_mesh::wire::FrameDecoder` has no `no_std` way to read in place other than `spare`/`commit`; we use `feed`
  (one copy per received segment), which is fine at peer-link rates.
