# Universal k10s OS image and multi-arch support (Tier 1: Linux as a driver layer): research report

Answers [TASK-universal-os.md](../TASK-universal-os.md). Date: 2026-10-19.
Companion: [native-hardware.md](native-hardware.md) (Tier 2, k10-metal on physical hardware). Where the two overlap
(arm64 JIT, Secure Boot, ESP layout, storage format) this report refers to it instead of repeating it, and §8.4
answers its open question 8 (one image, the loader picks the tier).

## How to read this report

Labels follow native-hardware.md:

| Label | Meaning |
|---|---|
| **[measured]** | Run on the dev machine during this research (prototype, script, count). |
| **[code]** | Read from the k10s sources. |
| **[doc]** | Official documentation, spec or upstream source (URL given). |
| **[survey/data]** | Third-party dataset or release asset sizes (URL given). |
| **[blog/forum]** | Secondary source (URL given). |
| **[estimate]** | My inference. Effort figures are always estimates. |

**Test conditions.** Intel i7-7820HQ (4C/8T), Linux 6.1 host, KVM, QEMU 7.2.22, no root. Other agents ran builds and
benchmarks throughout: **load average 11–18 on 8 threads** during every timing below (noted per table). Boot times are
medians of *interleaved* runs (every variant once per round), so load hits all variants alike. aarch64 guests run under
TCG emulation (no KVM for arm64 here): their times are functional evidence, not performance.

**Prototypes.**
- Branch `research/universal-os` (local, never merged or pushed), worktree
  `/home/qgerard/dev/k10s-research/universal-os/wt`, rebased on main `d4e67f3`:

  | Commit | Contents | Size |
  |---|---|---|
  | 0fb85da | hosted runtime on aarch64, armv7 (hard-float), i686, riscv64: per-arch context switch, x86_64-only JIT dependency, interpreter chosen automatically, 32-bit slot limits | 8 files, +331/−58 |
  | 21bf45f | `k10s` as PID 1 of a Linux image (`crates/k10s/src/pid1.rs`) | +1.1k lines incl. comments and tests |
  | 8a8b70d, 7b5283d | `k10s.prune_modules=1`; uevent hotplug; `k10s.netwait` | +100 |

  `git diff d4e67f3..research/universal-os`: 9 files, +1,546/−58; `pid1.rs` is 1,212 lines.

- Scripts that rebuild every image and rerun every measurement: [universal-os/](universal-os/) (copy the directory to
  `$UOS/scripts`, default `UOS=/home/qgerard/dev/k10s-research/universal-os`; §11 lists them).
- Tools obtained without root: `apt-get download` + `dpkg -x` (qemu-user-static, qemu-system-arm 7.2, AAVMF, flex,
  bison, libelf-dev, gcc-12-aarch64-linux-gnu + binutils, systemd-boot-efi amd64/arm64, grub-pc-bin, Debian's arm64
  kernel package), Rust targets via rustup, Linux 6.18.57 LTS source from kernel.org (sha256 checked).
- Kept in `$UOS` after cleanup (57 MB): `bin/` (every k10s build), `img/` (the k10s kernels, initramfs images, UKI),
  `runs/final/` (raw JSON of §1.2), `scripts/kconfig/generated-*.config` (full kernel configs). Kernel trees, tool
  extracts and disk images were deleted; the scripts rebuild them.

---

## 0. Summary and recommendations

1. **The idea works today, with ~1.2k lines in `k10s` and no other userspace program.** [measured]
   - The unchanged `k10s` binary of main boots as `/init` on a minimal 6.18 kernel with `ip=dhcp -- node --http
     0.0.0.0:80`: zero code changes, **720 ms** from QEMU start to the first HTTP 200.
   - With the PID 1 prototype (mounts, modalias module loading, own DHCPv4, boot-partition config, data partition,
     power button, watchdog, clean power-off), the same kernel serves in **295 ms** (median of 7, load 11–17). k10-metal
     takes 129 ms on the same host in the same rounds.
   - With **Debian's stock 6.1 kernel** and a 1.5 MB initramfs (k10s + 35 modules) it serves in **1.97 s**, of which
     ~1.5 s passes before the kernel's first message, i.e. in the decompressor of its 8 MB xz image (under load).
   - **One disk layout and one rootless builder** produce images that boot on legacy BIOS (GRUB), UEFI x86-64 (OVMF +
     systemd-boot, or a single UKI file) and UEFI arm64 (AAVMF + systemd-boot); the arm64 guest picks the interpreter
     automatically.
   - **Raspberry Pi 3, emulated** (QEMU `raspi3b`, Debian's arm64 kernel and Pi 3 devicetree, TCG): modalias loads the
     SoC drivers (dwc2 USB, MMC, RNG, watchdog), the USB NIC that enumerates later is loaded by uevent hotplug, DHCP
     on `usb0`, 62.7 MB of unused modules pruned, the 8 examples applied and served on 4 shards.
   - State survives reboots on an ext4 `K10S-DATA` partition (the `counter` service counted 1 → 2 → 3 over three
     power-button shutdowns); the power button drains and powers off in 0.17–0.37 s.

2. **Footprint** [measured]:

   | | image | min `-m` | idle used (MemTotal − MemFree), 256 MiB guest | first HTTP 200 |
   |---|---|---|---|---|
   | k10-metal (Tier 2) | 0.69 MB ELF | 3 MiB | 1.3 MiB (RESULTS.md) | 129 ms |
   | **k10s kernel 6.18 + k10s PID 1** | **3.1 MB bzImage + 0.79 MB initramfs** | **32 MiB** (28 fails) | **20.8 MiB** (+16 MiB kernel image not in MemTotal) | **295 ms** |
   | same, i386 (interpreter) | 2.9 MB + 0.80 MB | 24 MiB (20 fails) | 15.0 MiB | 532 ms (with an RNG) |
   | same, arm64 (interpreter), cross-built kernel | 6.8 MB Image (2.5 MB zstd) + 0.70 MB | 32 MiB (24 fails) | 16.0 MiB | 0.94 s **under TCG emulation** |
   | Debian 6.1 kernel + k10s PID 1 + 35 modules | 8.2 MB + 1.5 MB | 96 MiB | 31.7 MiB (+50 MiB outside MemTotal) | 1.97 s |
   | Debian 6.1 + "universal x86" initramfs (559 modules) | 8.2 MB + 12.5 MB | — | 93 MiB, **30 MiB with `prune_modules`** | 2.36 s |
   | kind (Kubernetes 1.34) worker node, k8s components only | — | — | 146 MiB working set; 579 MiB with control plane | — |

   The k10s process itself is 1.0–1.5 MiB RSS in all of them. Kubernetes figures: bench/results/k8s/SUMMARY.md [measured].

3. **Kernel strategy: build our own kernels from upstream LTS, in two configurations per architecture; keep the Debian
   kernel as the bootstrap and Secure-Boot path.** [measured + estimate]
   - *`vm` config* (everything built in, ~3 MB, zstd): clouds, VMs, microVMs, known boards. Against the distro kernel:
     a 2.7× smaller kernel file and no module tree, 6.7× faster to serve, 3× less minimum RAM.
   - *`metal` config* (Debian-like modular config, modules loaded by modalias from the initramfs, unused ones pruned):
     the long tail of PCs and servers.
   - Linux **6.18 LTS** (EOL Dec 2028) is the baseline: it is the first LTS with Pi 5 Ethernet. [doc]
   - Debian kernels are useful now (signed, so they should boot under Secure Boot with Debian's shim — that chain
     does not verify the initramfs [estimate, untested]) but they cost ~1.5 s of xz decompression, 50 MiB of RAM, no
     i386 since Debian 13, and lockdown patches that must be planned for.

4. **`k10s` as PID 1** needs (all implemented and tested, §4): mount `/proc` `/sys` `/dev`; block the shutdown signals
   before spawning threads (PID 1 drops default-disposition signals); auto-reap (k10s spawns nothing, confirmed);
   modalias coldplug + uevent hotplug + softdeps + acting as the kernel's `modprobe` helper; DHCPv4 with renewal; config from the
   command line and the boot partition; data partition by GPT name; power button (evdev, ACPI tiny-power-button
   signal 38, SIGPWR); watchdog; `reboot(2)`. Still to do: SNTP, IPv6 config beyond SLAAC, mDNS, update agent.

5. **Entropy is a boot-time trap.** k10s calls `getrandom()` at node start; on a machine without RDRAND or a hardware
   RNG driver the 6.18 kernel needs ~1 s of jitter entropy first (i386 guest: 1.48 s without an RNG, 0.55 s with
   virtio-rng, 0.38 s with RDRAND). [measured] Ship the hwrng drivers (virtio-rng, bcm2835-rng, TPM) built in.

6. **Multi-arch: done for the hosted runtime, under emulation.** [measured]
   - `coro.rs` gained aarch64, armv7-hf, i686 and riscv64 context switches; the JIT dependency became x86_64-only and
     the interpreter is chosen automatically.
   - All five targets build as static musl binaries **with no cross C toolchain** (rust-lld + rustup's musl CRT):
     1.56–1.93 MB stripped.
   - The k10-ir, k10-lang, k10-mesh and k10-node suites pass on i686 (native), armv7 (qemu-user) and, except one
     RSS-threshold test that measures the emulator, on aarch64 and riscv64. x86_64 suites (JIT) still pass.
   - 32-bit costs: the interpreter is 1.3–1.8× slower on i686 than on x86_64 on the same CPU; slots are capped at
     2^28 bytes; a 32-bit kernel must keep `COMPAT_32BIT_TIME` (musl uses time32 syscalls when values fit, and
     Rust's `thread::sleep` panics on ENOSYS — found by booting it).
   - **Next: the arm64 JIT backend** (~3k lines, 6–10 weeks [estimate], native-hardware.md §7.2), built once and tested
     hosted under Linux first. **No 32-bit JIT.**

7. **Prior art: gokrazy is the closest model** (Go program as PID 1, Pi + PC, A/B squashfs, update API in PID 1, kernel
   and Pi firmware as bot-bumped packages); **Nerves** for update rollback with Pi `tryboot`; **Talos** for the
   API-only management and Secure Boot/UKI/TPM model. §2.

8. **Image layout: one GPT layout for every platform** (`K10S-BOOT` FAT/ESP + `K10S-DATA`), A/B UKIs on the ESP with
   boot counting on UEFI, `autoboot.txt` + `tryboot` on the Pi, updates as signed content-addressed blobs spread by
   gossip, a slot marked good only after the node rejoins the mesh. The Tier 2 UEFI loader can pick the tier by
   chain-loading the Tier 1 UKI (§8.4).

9. **Out of scope for v1:** Wi-Fi (needs a supplicant and regulatory database), Secure Boot with our own shim, i386
   UEFI-only tablets, Pi 1/Zero (armv6), a 32-bit JIT.

**Roadmap** (§9): U0 PID 1 mode in main → U1 x86-64 images (vm + metal kernels, BIOS/UEFI, CI under QEMU) → U2 updates
(A/B, signing, gossip distribution) → U3 arm64 + Raspberry Pi 4/5 (interpreter) → U4 arm64 JIT → U5 Pi 3/armv7/i386
best-effort tier → U6 Secure Boot.

---

## 1. Measurements

### 1.1 What was built [measured]

| Artifact | Size | Notes |
|---|---|---|
| `k10s` x86_64, static musl, stripped (research branch, JIT + PID 1) | 1,926,992 B (zstd −19: 798 KB, xz: 744 KB) | main's binary: 1,787,864 B; PID 1 mode adds ~139 KB |
| `k10s` i686 / aarch64 / armv7 / riscv64 (interpreter + PID 1) | 1,738,744 / 1,564,208 / 1,590,956 / 1,558,536 B | xz 749 / 610 / 638 / 717 KB |
| k10s kernel x86_64 (6.18.57, `vm` config, zstd bzImage) | 3,085,312 B | vmlinux text 6.9 MB, data 2.5 MB, bss 2.5 MB |
| k10s kernel i386 (same config, M686, HIGHMEM4G) | 2,937,344 B | |
| k10s kernel arm64 (6.18.57, `vm` config for QEMU `virt`/UEFI, cross-built with Debian's gcc) | 6,785,536 B Image (gzip 2.9 MB, zstd 2.5 MB) | arm64 kernels are not self-decompressing; firmware/loaders take `Image` or `Image.gz` |
| k10s kernel arm64 + Raspberry Pi 3/4/5 drivers built in (GENET, RP1 + macb, LAN78xx, SMSC95xx, dwc2, xHCI, SDHCI-iProc, PCIe brcmstb, bcm2835 watchdog/RNG) | 7,938,560 B Image (gzip 3.4 MB) | +1.15 MB over `vm`; built, not booted (QEMU has no Pi 4/5 Ethernet) |
| Debian 6.1.0-53-amd64 vmlinuz (xz) | 8,230,848 B | + 515 MB of modules (4,033 files) |
| Debian 6.1.0-54-arm64 Image (uncompressed; gzip −9: 11.2 MB) | 32,956,352 B | + 351 MB of modules; DTBs for Pi 3/3+/Zero 2 W/4/400/CM4, no Pi 5 |
| initramfs: k10s only (zstd) | 794,572 B (arm64: 699,603 B) | for the k10s kernel |
| initramfs: k10s + 35 Debian modules ("qemu": virtio, ext4, vfat, evdev, button, watchdog, rng) | 1,513,368 B | 5.7 MB unpacked |
| initramfs: "x86 universal" (all Ethernet, USB-net, PHY, ATA, NVMe, USB host/storage, MMC, HID, RTC, watchdog) | 12.5 MB zstd / 11.0 MB xz | 559 modules, 63 MB unpacked |
| initramfs: "arm64 universal" (same classes + clocks, pinctrl, regulators, PHYs, GPIO, SoC) | 14.3 MB zstd / 12.3 MB xz | 845 modules, 66 MB unpacked |
| UKI x86_64 (systemd-stub + k10s kernel + initramfs + cmdline) | 3,964,257 B | one PE file |
| Disk image (GPT: 1 MiB BIOS boot, 46 MiB ESP, 15 MiB ext4) | 64 MiB (≈4 MB used) | built rootless, §6.4 |

### 1.2 Boot time to first HTTP 200 [measured]

`final-bench.sh`: 7 interleaved rounds (UEFI: 3), `-m 256M -smp 1`, KVM, virtio-net on slirp, QEMU start → first 200
on `/_k10s/health` (k10-metal: `/hello/`), polled after the console's ready line. Load 11.2–17.5 (median 15.7).
PID 1 binary of commit 21bf45f (hotplug and pruning came later; spot checks after them showed no change).

| Variant | median | min–max |
|---|---|---|
| k10-metal (PVH, SeaBIOS, `-kernel`) | 129 ms | 110–150 |
| **k10s kernel + k10s PID 1 (own DHCP)**, `-kernel` | **295 ms** | 246–309 |
| same, i386 kernel + i686 k10s (interpreter), virtio-rng | 532 ms | 471–561 |
| same x86_64 image from disk: SeaBIOS → GRUB (MBR + core.img) → kernel | 580 ms | 477–1,109 |
| k10s kernel + **unmodified** main `k10s` as `/init`, `ip=dhcp -- node …` | 720 ms | 668–752 |
| Debian 6.1 kernel + k10s PID 1 + 35 modules, `-kernel` | 1,970 ms | 1,661–2,031 |
| Debian 6.1 + "x86 universal" initramfs (559 modules, 16 loaded) | 2,355 ms | 2,145–2,390 |
| Debian 6.1 from disk via GRUB (+ ESP config, ext4 data mount) | 2,545 ms | 2,184–2,681 |
| k10s kernel from disk, **OVMF** → systemd-boot | 5,974 ms | 5,574–6,008 |
| k10s kernel as a **UKI** started directly by OVMF | 5,676 ms | 5,499–6,270 |

Where the time goes [measured]:
- **Debian kernel:** QEMU + SeaBIOS + real-mode setup ≈ 140 ms; then **~1.5 s between the setup code's last line and
  the first kernel message** (host-side serial timestamps, `linetimes.py`), which is where the decompressor unpacks
  the xz image (`CONFIG_KERNEL_XZ`) on a contended vCPU [attribution: estimate]; kernel init to `/init` 0.45–0.6 s; PID 1 0.2 s (modalias coldplug 150–185 ms for 10–12 modules,
  360 ms when 559 are shipped; DHCP 1–2 ms; node start ~5 ms).
- **k10s kernel:** zstd-compressed, no module probing: `/init` runs 95–110 ms after kernel start; DHCP 1 ms.
- **Kernel `ip=dhcp`** costs ~0.4 s more than the PID 1 client (956 vs 551 ms in a separate batch): `ipconfig` waits for
  carrier and backs off. It also cannot renew a lease and needs the NIC driver built in. [measured + doc]
- **OVMF** spends 4–5.5 s before the loader runs, whatever the loader (native-hardware.md §2.2 found 3.3–4.2 s at a
  lower load). On real PCs POST dominates anyway.
- **Entropy:** without RDRAND or an RNG device, node start waits ~1 s for the CRNG (i386 guest, `-cpu qemu32`):
  1,478 ms → 548 ms with `virtio-rng` → 379 ms with RDRAND (`-cpu host`).
- **Shutdown:** power button → graceful drain → `sync` → `reboot(RB_POWER_OFF)` → QEMU exit in 166–370 ms on every
  kernel and arch.
- **aarch64 under TCG** (functional only, every instruction emulated): Debian arm64 kernel `-kernel` 6.7–10.2 s (4 s of
  it modalias loading 9 modules); AAVMF + systemd-boot from disk 18.7 s; **the k10s arm64 kernel 0.94 s**.

### 1.3 Memory [measured]

Idle node, no services, 256 MiB guest, from `/proc/meminfo` when the node first answers (`k10s-init … ready`):

| Kernel | MemTotal | MemTotal − MemFree | MemTotal − MemAvailable | slab | k10s RSS |
|---|---|---|---|---|---|
| k10s kernel x86_64 | 239.9 MiB | 20.8 MiB | 23.6 MiB | 3.7 MiB | 1.29 MiB |
| k10s kernel i386 | 247.0 MiB | 15.0 MiB | 18.0 MiB | 2.6 MiB | 1.44 MiB |
| Debian 6.1 amd64 (35 modules) | 205.8 MiB | 31.7 MiB | 28.1 MiB | 15.7 MiB | 1.07 MiB |
| Debian 6.1 amd64, universal initramfs | 205.8 MiB | 93.3 MiB | 88.8 MiB | 17.5 MiB | 1.10 MiB |
| same with `k10s.prune_modules=1` (59.6 MB of `.ko` deleted in 24 ms) | 205.8 MiB | — | 30.2 MiB | 17.1 MiB | 1.10 MiB |
| Debian 6.1 arm64 (TCG) | 229.5 MiB | 20.4 MiB | 20.8 MiB | 14.6 MiB | 1.04 MiB |
| k10s kernel arm64 (TCG) | 243.4 MiB | 16.0 MiB | 18.9 MiB | 3.4 MiB | 1.27 MiB |

- The gap between `-m` and MemTotal is the kernel image, `memmap` and reservations: 16 MiB for the k10s kernel, 50 MiB
  for Debian's.
- **The initramfs lives in RAM.** A "universal" module set costs its unpacked size until the unused modules are
  deleted (or until modules move to a compressed read-only filesystem on disk, §3.3).
- With the 8 Ten examples applied and each called once (k10s kernel), k10s RSS goes from 1.34 to 1.88 MiB and system
  memory stays at 21–24 MiB used (within noise). Per-instance costs are those of NODE.md (≈4.4 KB RSS per idle instance).

**Minimum `-m`** (boots, applies the 8 Ten examples, serves `/hello/` and `/compute/?n=1000`; `minmem.sh`):

| | fails | works |
|---|---|---|
| k10-metal (RESULTS.md) | 2M | 3M |
| k10s kernel x86_64 + PID 1 | 28M | 32M |
| k10s kernel i386 + PID 1 (interpreter) | 20M | 24M |
| k10s kernel arm64 + PID 1 (interpreter, TCG) | 24M | 32M |
| Debian 6.1 + PID 1 | 88M | 96M |

### 1.4 Additional results [measured]

- **Persistence:** disk image without `snapshot=on`, three boots, `GET /counter/` → 1, 2, 3: the applied bundle and the
  KV log on the ext4 `K10S-DATA` partition survive clean shutdowns.
- **Boot-partition config:** `/k10s.conf` on the ESP (`K10S-BOOT`) is merged under the kernel command line
  (`config: /dev/vda2 (K10S-BOOT) /boot/k10s.conf merged`), on BIOS, UEFI x86-64 and UEFI arm64.
- **ext4 needs a soft dependency.** The first Debian attempt failed with `ENOENT`: ext4 asks the crypto API for
  `crc32c`, which triggers `request_module()` → `/sbin/modprobe`, which does not exist. Fixed twice over: PID 1 loads
  `modules.softdep` "pre:" modules, and points `/proc/sys/kernel/modprobe` at `/init`, which handles `-q -- NAME`
  like modprobe.
- **Power button, three mechanisms:** evdev `KEY_POWER` (Debian, ACPI button driver as a module, loaded by modalias),
  `gpio-keys` on arm64 `virt` (also modalias-loaded), and the kernel's `ACPI_TINY_POWER_BUTTON`, which signals PID 1
  directly with signal 38 (the k10s kernel; no input layer needed). With 6.18's ACPI button driver built in, no input
  device appeared in the k10s kernel (cause not investigated); the tiny driver is the better fit anyway.
- **Raspberry Pi 3 under QEMU `raspi3b`** (TCG, 4 emulated cores, QEMU's `usb-net` standing in for the LAN9514):
  `init` at 6.6 s, 16 modules by modalias in 8.7 s, the USB NIC (CDC) loaded by **hotplug** after the coldplug loop had
  ended, DHCP on `usb0`, `prune_modules` freed 62.7 MB, first HTTP 200 at 23.7 s wall; `apply` of the 8 examples, the
  SHA-256 `compute` and the KV `counter` answered through the host forward. Without hotplug the first attempt found no
  NIC: USB devices appear seconds after their host controller's driver loads.
- **Minimal configs find upstream Kconfig bugs:** 6.18.57's `PINCTRL_RP1` (Pi 5) uses regmap without selecting it; the
  build fails unless something else selects `REGMAP_MMIO` (worked around with `MFD_SYSCON=y`).
- **i386 kernel pitfall:** with `tinyconfig`, `COMPAT_32BIT_TIME` is off; musl calls the time32 `nanosleep` when the
  value fits, gets ENOSYS, and Rust's `thread::sleep` panics in every thread. Turning it on fixed the boot.
- **Docker Desktop** runs `linux/arm64`, `linux/arm/v7`, `linux/386` and `linux/riscv64` static binaries (FROM scratch
  images, nothing pulled, removed afterwards): `k10s run compute.ten` answers 200 on all four (`docker-multiarch.sh`).
- **qemu-user without binfmt** runs foreign test binaries in place (`CARGO_TARGET_<T>_RUNNER=qemu-<arch>-static`).
  It cannot run the `crates/k10s/tests` suites, which `exec` the k10s binary (no binfmt on the host; Docker's VM has it).

---

## 2. Prior art (question 1)

Facts below were collected on 2026-10-19 from project docs and release assets; "n/f" = looked for, not found.

| System | PID 1 / userland | Image size | Boot / RAM | Update model | Hardware model | Security model | Config / first boot |
|---|---|---|---|---|---|---|---|
| **gokrazy** | Go `init` supervises Go programs + built-in DHCP, NTP, randomd; update HTTP API inside PID 1; no shell (opt-in `breakglass` SSH) [1][2] | partitions: 100 MB FAT boot + 2× 500 MB squashfs root (A/B) + ext4 `/perm` [3]; Tailscale appliance ~70 MB [16] | n/f | A/B: `gok update` PUTs boot/root/MBR into the inactive slot (sha256), `/update/switch`, kexec reboot; Pi tryboot; HTTP Basic auth, no image signing found [6][7] | per-board Go "kernel packages" bot-bumped to each upstream release (linux 7.3 on 2026-10-19), separate Pi firmware and rpi-eeprom packages; Pi 3/4/5/Zero 2 W, amd64 PCs (systemd-boot), BIOS via MBR stub [2][4][5][14][15] | read-only squashfs root, no shell, listens on private networks only; no verity/Secure Boot found [1][7] | `config.json` baked at build time; DHCP sends hostname; SLAAC; NTP from DHCP option 42 or pool; RTC / saved-time fallback [9][10][11] |
| **Nerves** | `erlinit` → BEAM release; IEx, SSH optional [54][55] | "20–30 MB" firmware [52]; rpi4 layout: A/B boot ~19 MiB, A/B squashfs ~133 MiB, f2fs `/data` [56] | ~7.5 s (BBB, old), 19 s Pi Zero report [59] | `fwup` archive into the inactive slot: signed, streaming, deltas; Pi `tryboot` reverts unless the app validates [56][57] | one Buildroot "system" per board (13 targets) [53] | read-only squashfs, optional signed firmware [55][57] | VintageNet compiled in; MdnsLite `.local`; NervesTime [58] |
| **Talos** | `machined` as `/sbin/init`; apid, trustd, containerd, kubelet, etcd; "no SSH, no shell" [17][18] | v1.14.4: metal-amd64.raw.zst 231 MB, metal-arm64 106 MB, initramfs 96.5 MB [32] | "~1 min" [17]; min 1–2 GiB RAM [31] | A/B with automatic rollback, `talosctl upgrade`, kexec [21] | one KSPP kernel; firmware/drivers as "system extensions", SBC boot assets as "overlays"; Pi 4 official, Pi 5 community [26][27][28] | squashfs root in RAM, mTLS gRPC API only, UKI + Secure Boot, TPM unlock (PCR 7 + 11), forced module signatures [20][22][23] | config search: STATE → embedded → `talos.config=<URL>` → inline → maintenance mode [24][25] |
| **Bottlerocket** | systemd + API server; admin container off by default [33][34] | 2 GiB OS image default [35] | n/f | two partition sets A/B, TUF repository, automatic revert [33] | per-variant kernels; AWS/VMware; bare-metal k8s variants dropped [33][36] | dm-verity root, SELinux enforcing, lockdown=integrity, shim → GRUB → kernel [34] | TOML user data [33][37] |
| **Flatcar** | systemd, full GNU userland (2,391 binaries), SSH [18][42] | 585 MB production image [45] | n/f | `update_engine` into the passive `/usr`, Omaha/Nebraska, reboot coordination (locksmith/kured) [39][40][41] | one generic kernel per arch; Pi 4 via unofficial UEFI [43] | dm-verity `/usr` [38] | Ignition once in the initramfs [42] |
| **Fedora CoreOS** | systemd, rpm-ostree [46] | metal raw.xz 913 MB [51] | n/f | OSTree deployments, Zincati + Cincinnati graph, FleetLock [46][47] | Fedora kernel; Pi 4 via U-Boot/EDK2 [49] | n/f | Ignition [48] |
| **u-root** | Go init + Go multi-call "busybox" incl. a shell [60] | n/f | LinuxBoot "typically 20× faster" boot [61] | flashed with firmware [61] | any kernel, SPI-flash payload [60] | n/f | build-time command list [60] |
| **LinuxKit** | init + containerd/runc + YAML-declared containers [62] | "35MB" (2017) [65] | n/f | none: reprovision [63] | kernel images per arch; many output formats [62][64] | runs from initramfs, read-only [63] | metadata package → `/run/config` [66] |
| **Alpine (diskless)** | busybox init + OpenRC, shell, apk [68] | virt ISO 69 MB, minirootfs 3.7 MB, still ships x86 (32-bit) images [67] | ≥128 MB RAM [70] | no A/B; `lbu commit` overlay [69] | distro kernels incl. `linux-rpi`; linux-firmware split per vendor [67][71] | runs from tmpfs | apkovl [69] |
| **balenaOS** | systemd (Yocto), balenaEngine, supervisor, SSH, avahi, NetworkManager [72] | n/f | n/f | host OS into the inactive root; boot-count + 15-min health rollback; deltas [73][74][75] | one Yocto BSP per device family [76] | read-only root, keys in `config.json` [72] | `config.json` on the boot partition; `<host>.local` mDNS [72] |
| **k3OS** † | OpenRC, Alpine userland, Ubuntu kernel, k3s [77] | n/f | n/f | squashfs images + system-upgrade-controller [77] | x86_64/arm64/armv7 [77] | `/usr` read-only [77] | `config.yaml`, `k3os.install.config_url` [77] |

† **k3OS is dead:** last release 2021-10-09; a 2023-12-08 README notice says it "is no longer maintained and has been
superceeded by Elemental", and the repository is archived [77][78]. No official reason beyond that; users point to
SUSE's acquisition of Rancher (a non-SUSE distro) and the departure of its author in 2022 [79]. **Lesson: an OS tied
to one product's distribution dies with the product's priorities.**

**Not found anywhere:** published idle-RAM figures for gokrazy, Talos, Bottlerocket, Flatcar, FCOS, Nerves, balenaOS.
§1.3's numbers have no apples-to-apples counterpart; measuring gokrazy on the same QEMU setup is a cheap follow-up.

### 2.1 Verdict

- **Closest model: gokrazy.** One language's static programs, its init as PID 1 with DHCP and NTP, no shell, A/B
  read-only roots, an update API inside PID 1, kernel + Pi firmware + EEPROM as separately versioned, bot-bumped
  bundles, one disk layout for Pi firmware, UEFI (systemd-boot) and BIOS (MBR stub) [1][3][4][5][14][15]. k10s goes one
  step further: one binary, no other processes at all (DHCP, NTP, update agent are threads).
- **Borrow** [estimate]:
  - gokrazy: GPT + hybrid-MBR layout with the FAT partition first (Pi firmware needs partition 1); update API in PID 1
    streaming into the inactive slot; kernel/firmware as pinned bundles bumped by a bot; breakglass-style opt-in debug
    access; time fallback RTC → saved shutdown time → NTP.
  - Nerves/balenaOS: Pi `tryboot` + "validate after boot" (k10s: after rejoining the mesh); a 15-minute health window
    that reverts; `config.json`-style config on the FAT partition, editable from any PC; mDNS `.local`.
  - Talos: API-only management; config acquisition order ending in a maintenance mode; UKI + Secure Boot + TPM on PCs;
    SBC boot assets as pluggable overlays.
  - Bottlerocket/Flatcar/FCOS: signed update metadata (TUF), dm-verity, phased rollouts and reboot locks — which k10s
    can run over its own gossip instead of Omaha/Cincinnati servers.
  - Alpine: per-vendor firmware packages; run-from-RAM.
- **Avoid:** unsigned updates protected only by a password (gokrazy); per-board Buildroot/Yocto systems (Nerves,
  balenaOS); 100 MB–2 GB images built around containerd or systemd (Talos, Bottlerocket, Flatcar, FCOS); "cannot update
  itself" (LinuxKit); depending on one vendor's distribution (k3OS).

---

## 3. The kernel (questions 2–4)

### 3.1 Distribution kernels vs our own (Q2)

| | Debian kernel (or Ubuntu, Raspberry Pi OS) | k10s kernel from upstream LTS |
|---|---|---|
| Size | 8.2 MB (x86, xz) / 33 MB (arm64 Image) + 350–515 MB modules [measured] | 3.1 MB `vm` config, everything built in [measured]; a modular `metal` config ≈ distro size |
| Boot | 2.0 s to first 200 under load, ~1.5 s of it before the first kernel message (xz decompression) [measured] | 0.3 s [measured] |
| RAM | 50 MiB outside MemTotal + 32 MiB used; min 96 MiB [measured] | 16 MiB + 21 MiB; min 32 MiB (i386: 24 MiB) [measured] |
| Hardware coverage | everything Debian supports | `vm`: virtio, e1000(e), ACPI; `metal`: what we enable |
| Security updates | Debian security team, signed builds; Debian 13 supported to 2028-08, LTS to 2030-06 [doc] | ours to rebuild: LTS 6.18 to Dec 2028, 6.12 to Dec 2028 (CIP SLTS 6.12 to 2035) [doc] |
| Secure Boot | Debian's Microsoft-signed shim + signed kernel; lockdown on automatically (Debian patch) [doc] | needs our own shim review or MOK enrollment (§8.3) |
| Architectures | amd64, arm64, armhf; **no i386 kernel since Debian 13** [doc] | any Linux arch |
| Cost | none to build; a dependency on Debian's choices (module compression, configs, release cadence) | a CI job per arch × config; one LTS bump per year (~1–2 days [estimate]) |

**Recommendation** [estimate]:
- **v1 (U1):** k10s `vm` kernel for VMs/clouds (largest gain), Debian kernels + a curated module set for bare metal and
  for anyone who needs Secure Boot now.
- **v2:** own `metal` config too (start from Debian's config, drop what a k10s node never uses: sound, DRM beyond
  simple framebuffers, filesystems other than ext4/vfat/squashfs/erofs, wireless at first), zstd-compressed, modules
  signed with a per-build key. Keep the Debian-kernel path as a fallback flavour.
- **Pi:** mainline 6.18 has Pi 5 Ethernet (RP1 + macb), Pi 4 GENET since 5.5; Raspberry Pi's own `rpi-6.18.y` is the
  fallback if mainline gaps appear (camera, some HATs — irrelevant for k10s). [doc]

### 3.2 Drivers and firmware without udev (Q3)

- **Coldplug** [measured]: walk `/sys/devices/**/modalias`, match `modules.alias` patterns with fnmatch semantics,
  load dependencies from `modules.dep` (and "pre:" soft dependencies from `modules.softdep`), `finit_module()`;
  repeat until a scan finds nothing new (a bus driver creates new devices: virtio-pci → virtio0 → virtio_net).
  10–16 modules load in 150–360 ms on Debian 6.1 under load. Writing `add` to `uevent` files would replay devices
  instead of rescanning [doc].
- **On-demand loading** [measured]: `request_module()` (crypto algorithms, filesystems, protocols) execs
  `/proc/sys/kernel/modprobe`; pointing it at `/init`, which handles `-q -- NAME`, replaces kmod. Usermode helpers are
  children of kthreadd, so PID 1 reaps nothing.
- **Hotplug** [measured]: a `NETLINK_KOBJECT_UEVENT` socket in PID 1 feeding the same matcher (~60 lines), started
  before the coldplug scan so nothing is missed. Required for the Pi 3 (Ethernet is a USB device) and USB adapters;
  module pruning therefore runs only after the network is up and the partitions are mounted.
- **Compressed modules:** `finit_module(fd, "", MODULE_INIT_COMPRESSED_FILE)` lets the kernel decompress `.ko.xz`/`.zst`
  (Debian 13: `MODULE_DECOMPRESS=y`); implemented, untested here (Debian 12 modules are uncompressed). [code + doc]
- **Firmware:** the kernel loads `/lib/firmware/…` itself (no udev since 3.7); firmware for built-in drivers must be
  in the initramfs; `.xz`/`.zst` firmware with `FW_LOADER_COMPRESS`. [doc]
  - Needed for common NICs: Realtek r8169-family (`rtl_nic/*.fw`, optional: the driver only warns), Broadcom
    bnx2/bnx2x/tg3 (some chips), Intel Wi-Fi (`iwlwifi-*.ucode`), Pi Wi-Fi (`brcmfmac43430/43455` + CLM + NVRAM). Intel
    e1000e/igb/igc/ixgbe, Aquantia, mlx5: none. [doc]
  - Licences: redistributable but not GPL (WHENCE); Pi Wi-Fi blobs under Cypress/Synaptics EULAs. Ship them as a
    separate, clearly licensed `firmware` bundle (Debian puts them in `non-free-firmware`), never inside Apache-2.0 code
    and never with `CONFIG_EXTRA_FIRMWARE`. [doc]
  - The full linux-firmware release is 632 MB compressed: ship a curated subset (Realtek ≈8 MB, Broadcom ≈13 MB in
    Arch's compressed packages). [survey/data]

### 3.3 How small can it get (Q4) [measured + estimate]

- Measured: 3.1 MB kernel + 0.8 MB initramfs, 32 MiB minimum RAM (24 MiB for i386), 21 MiB used at idle, for VMs.
- Further cuts [estimate]: no IPv6/EFI/e1000 in a microVM build (−0.3–0.5 MB), `CONFIG_KERNEL_LZ4` for faster
  decompression (needs the `lz4` tool at build time; zstd was used here), PVH direct boot (`CONFIG_PVH` is on) for
  Firecracker/Cloud Hypervisor.
- For the bare-metal long tail the floor is set by modules, not the kernel: 60–66 MB unpacked for "every Ethernet,
  storage and USB-host driver" (12–14 MB compressed). Keep them out of RAM: either prune once the NIC is up and the partitions
  are mounted (measured: −59.6 MB in 24 ms; device classes plugged in later then need a reboot) or move them to a compressed read-only filesystem
  (squashfs/erofs) next to the kernel, mounted by PID 1 — the initramfs then holds k10s + storage/vfat drivers only.

---

## 4. `k10s` as PID 1 (questions 5–10)

### 4.1 PID 1 duties (Q5) [measured unless noted]

| Duty | Status in the prototype | Notes |
|---|---|---|
| Detect PID 1 | `main()`: `getpid() == 1` → `pid1::main()` | the rest of `k10s` is unchanged; `cmd_node` is reused |
| `/proc`, `/sys`, `/dev` | mounted (devtmpfs) | `/dev/console` in the cpio gives stdio before devtmpfs; re-opened if missing |
| Arguments | `k10s.KEY=VALUE` from `/proc/cmdline` → `--KEY VALUE`; args after `--` passed as is | dotted kernel parameters never reach init's environment: read `/proc/cmdline` |
| Signals | INT/TERM/PWR/38 blocked before any thread starts; `k10s node` sigwaits INT/TERM; a thread turns PWR/38 into TERM | **for PID 1 the kernel drops signals whose disposition is default** — a helper thread with an unblocked SIGTERM would swallow it |
| Ctrl-Alt-Del | `reboot(LINUX_REBOOT_CMD_CAD_OFF)` → SIGINT | |
| Reaping | SIGCHLD ignored (kernel auto-reaps) | k10s spawns no child process: no `Command`/`fork`/`exec` in the runtime crates (only tests spawn the binary) [code]; usermode helpers belong to kthreadd |
| Shutdown | drain → sync → unmount → `reboot(RB_POWER_OFF \| RB_AUTOBOOT \| RB_HALT)` per `k10s.on_exit` | if PID 1 exits or aborts (fail-stop threads abort the process [code]), the kernel panics: boot with `panic=5` or `panic=-1` to reboot |
| Watchdog | `k10s.watchdog=1`: open `/dev/watchdog`, pet every 5 s | arm the hardware watchdog in firmware where possible (Pi `kernel_watchdog_timeout`) so a hang before PID 1 also resets [doc] |
| Hardware clock | kernel `RTC_HCTOSYS` sets the clock at boot | Pi has no RTC: SNTP needed (§4.2) |
| Entropy | not handled | §1.2: ship hwrng drivers; consider `GRND_INSECURE` for non-crypto seeds (wyrand, node id) [estimate] |
| Modules (coldplug, hotplug, modprobe helper, pruning), network, config, data, power button | §3.2, §4.2–4.4 | |
| Readiness, memory report | logs "ready" and `/proc/meminfo` on the console (`k10s.meminfo=SECS`) | |

### 4.2 Networking without a userland (Q6)

- **DHCPv4 inside k10s** [measured]: ~350 lines with interface and static-address code (DISCOVER/OFFER/REQUEST/ACK
  with the broadcast flag on a UDP socket bound to the interface, `ioctl` for address and default route, renewal thread at T1). 1–2 ms on slirp. The kernel's
  `ip=dhcp` works with built-in drivers only, is 0.4 s slower, never renews, and Debian kernels lack it
  (`CONFIG_IP_PNP` off). Keep `ip=` as a zero-code fallback, not the design. Production gaps [estimate]: raw
  `AF_PACKET` sockets for servers that unicast offers, rebinding on carrier changes (netlink), several NICs/bonds,
  DHCP option 42/hostname use.
- **IPv6:** the kernel does SLAAC by itself once the link is up (`accept_ra=1` unless forwarding) [doc]. DHCPv6 and RA
  DNS need userspace — k10s needs neither for peering by IP [estimate].
- **DNS:** not needed by the node: peers are IPs from gossip, `--join` takes IPs. Only operator conveniences
  (`--join host`, update URLs) would need a stub resolver [code + estimate].
- **Time:** needed for TLS (future), logs and leases; the Pi has no RTC. Add SNTP in k10s (~150 lines [estimate]):
  servers from DHCP option 42 or config, then RTC → saved-shutdown-time fallback like gokrazy [doc].
- **Wi-Fi: out of scope for v1.** It needs `wpa_supplicant`-equivalent WPA2/3 handshakes, regulatory data and firmware;
  writing it into k10s is large and security-sensitive [estimate]. Ethernet (incl. USB adapters) only.

### 4.3 Configuration, first boot, discovery (Q7)

- **Sources, by precedence** [measured]: kernel command line `k10s.KEY=VALUE` → `k10s.conf` on the `K10S-BOOT` FAT
  partition (editable from any PC, like balenaOS's `config.json`) → `/etc/k10s.conf` baked in the initramfs.
- **Keys:** `name`, `join` (seed IPs), `advertise`, `mem`, `shards`, `backend`, `data`, `ip` (static `A.B.C.D/P,GW`),
  `iface`, `net=none`, `watchdog`, `on_exit`, plus any `k10s node` flag.
- **First boot** [estimate]: a node with `join` joins the cluster and receives desired state by gossip (the existing
  path); a node without `join` starts as a one-node cluster. Next steps: a "maintenance" state that waits for an
  operator `apply`/`join` on the admin API (Talos), and **cluster authentication before any "install on any device"
  story** — gossip and peer links are unauthenticated today [code].
- **Finding a headless node** [estimate]: (1) the console banner prints the address; (2) the DHCP request carries the
  hostname (router DNS); (3) mDNS `_k10s._tcp.local` announcement (~200 lines, receive-and-answer only); (4) members
  list on any node: once one node is known, `k10s status` lists everyone.

### 4.4 Persistent storage (Q8)

- **Discovery** [measured]: GPT partition named `K10S-DATA` (from `PARTNAME=` in `/sys/class/block/*/uevent`) or
  `k10s.data=/dev/X[:fstype]`; mounted `noatime,nosuid,nodev,noexec`. No disk → state in RAM (as today without
  `--data`).
- **Filesystem:** ext4 for v1 (zero new code; the node already writes `bundle.k10b` and KV logs with fsync).
  The Tier 2 design is a raw segmented log on its own partition type (native-hardware.md §6.4). **Converge on that
  format for both tiers** so a machine can switch tiers without losing state: Tier 1 can write it to the raw block
  device with `O_DIRECT` + `fdatasync` [estimate].
- **SD cards** [doc + estimate]: read-only root (the initramfs), writes only to `K10S-DATA`, KV group commit already
  batches (≈2 ms windows); consider f2fs or a larger ext4 `commit=` on SD; prefer USB-SSD/NVMe on Pi 4/5.

### 4.5 Updates and recovery (Q9) [estimate]

- **Artifact:** per arch, one *image blob* = UKI (UEFI) or kernel + initramfs (Pi, BIOS), content-addressed (sha256)
  and **signed** (ed25519 over the hash; the trust root baked into the running image). Images are 4–16 MB, so they can
  travel exactly like code blobs: the desired state names an image hash per arch, peers fetch it with `GetBlob`.
- **Slots:** A/B **files** on the ESP — `/EFI/Linux/k10s-a.efi`, `k10s-b.efi` with systemd-boot boot counting
  (`k10s-b+3-0.efi`) [doc], or for the Pi two boot partitions selected by `autoboot.txt` with `tryboot` (a crash or reset
  reverts) [doc]. FAT writes: write the new file completely, `fsync`, then flip one small pointer file.
- **Commit:** the new image marks itself good only after it **rejoins the mesh and serves its placed services** for a
  grace period (Nerves/balena pattern). Otherwise the boot counter or `tryboot` falls back.
- **Rollout:** a "max N nodes rebooting" lease and per-zone waves, computed by the same pure placement function every
  node already runs — no Omaha/Cincinnati server.
- **Recovery:** a bad image falls back automatically; a broken disk is reflashed from any other node's blob store.

### 4.6 Security (Q10)

- **Attack surface** [measured + code]: no shell, no SSH, no other process; root filesystem = initramfs in RAM (writes
  vanish at reboot); the only listeners are the node's HTTP ingress/admin API, gossip UDP and peer TCP.
- **Needed before deployment on untrusted networks** [estimate]: authenticated + encrypted gossip/peer links and an
  authenticated admin API (mTLS like Talos), signed images (§4.5).
- **Kernel hardening:** `lockdown=integrity`, module signature enforcement (our modules signed at build time, or
  Debian's signed modules), no `/dev/mem`, kexec only of signed kernels. Lockdown blocks nothing k10s uses
  (epoll, memfd, madvise, sockets, `finit_module` of signed modules) [doc + code].
- **Secure Boot:** see §8.3.
- **Remote debugging:** the admin API (status, metrics, logs) plus a future opt-in **breakglass** endpoint that accepts
  a signed, static debug-tools bundle into RAM (gokrazy's model) [estimate]. The serial/HDMI console shows the boot log.

---

## 5. Multi-arch port of the hosted runtime (questions 11–14)

### 5.1 `coro.rs` (Q11) [measured]

Implemented in commit 0fb85da (`mod arch` per target, common `Coro::new` over an arch frame):

| arch | ABI | integer state saved | FP/SIMD saved | frame | stack alignment |
|---|---|---|---|---|---|
| x86_64 | SysV | rbx rbp r12–r15 (+ return address) | none (all xmm caller-saved) | 56 B | 16 |
| aarch64 | AAPCS64 | x19–x28, x29 (fp), x30 (lr) | **d8–d15** (low halves of v8–v15) | 160 B | 16 (architectural) |
| arm (armv7, hard-float) | AAPCS-VFP | r4–r11 (+ r12 pad), lr→pc | d8–d15 | 104 B | 8 |
| x86 (i686) | SysV i386 | ebx esi edi ebp (+ return address); args on the stack | none | 20 B | 16 at `call` (LLVM) |
| riscv64 | LP64D | ra, s0–s11 | fs0–fs11 | 208 B | 16 |

- Control/status registers (MXCSR, FPCR/FPSCR, fcsr) stay unswitched, as on x86_64 (nothing on fibers changes them).
- Gotcha: `naked_asm!` on riscv64 is assembled without the function's target features: `.option arch, +d` is needed
  for `fsd`/`fld`.
- The coroutine unit tests (ping-pong with floating point live across switches, 200 interleaved coroutines with stack
  use, 16-byte-aligned SIMD spills) pass on all five targets.
- **Backend choice:** `BackendKind::native()` = JIT on x86_64, interpreter elsewhere; `K10S_BACKEND`/`--backend`
  still override; `--backend jit` on another arch is a clear error. `k10-jit` is a target-specific dependency
  (its `VmCtx` offsets assume 64-bit pointers, so it does not even compile for 32-bit targets).
- **Note for Tier 2 arm64** [estimate]: k10-metal's kernel is soft-float, but JIT code uses FP/NEON; a metal arm64
  coroutine switch must either save d8–d15 too or the arm64 JIT must not keep values in d8–d15 across safepoints and
  host calls (the x86 JIT keeps none live across them).

### 5.2 32-bit targets (Q12)

- **Value width** [measured]: the interpreter runs TIR's i64 on register pairs. Same CPU, same Ten services
  (`interp32.sh`, min of 7, load ~18): i686 vs x86_64 interpreter — compute (SHA-256) **1.78×** slower, merge sort
  1.38×, n-body (f64) 1.30×, hello equal. For scale, the x86_64 JIT is 8–20× faster than the x86_64 interpreter on the
  same three.
- **A 32-bit JIT is not worth it** [estimate]: masking stays cheap (with slots ≤ 2^28 only the low word of `addr + off`
  matters before the `and`), but every i64 add/sub/compare/shift becomes 2–4 instructions, multiply 3, divide a
  call, and register pressure on i386 is severe. Effort ≈ a new backend (~3k lines) for hardware that is a shrinking
  niche (Debian 13 dropped i386 kernels; mainline trims 486-era support) [doc]. Keep 32-bit as interpreter-only.
- **Address space** [code + measured]: 3 GiB user space on 32-bit kernels (4 GiB for i686 binaries on 64-bit
  kernels). The port caps slots at 2^28 and slot regions at 64 MiB (instead of 1 GiB) and rejects larger `memory` at
  apply time (before, `1usize << mem_log2` silently wrapped for 2^32+). Density [estimate]: ~2 GiB usable / 68 KiB per
  64 KiB-slot instance ≈ 30k instances of address space, minus fiber stacks (1 MiB each while busy: reduce to
  256 KiB on 32-bit). RAM is not the first limit: 1 GB (Pi 3) at ~4.4 KB RSS per idle instance would hold ~200k.
- **`AtomicU64`**: available on i686 (`cmpxchg8b`) and armv7 (`ldrexd/strexd`); used widely for counters, all tests
  pass [measured]. armv6 (Pi 1/Zero) is out of scope.
- **memfd, `MAP_NORESERVE`, epoll, eventfd, `madvise`**: work (suites pass on i686 natively and armv7 emulated)
  [measured]. musl's 64-bit `off_t` avoids the glibc `_FILE_OFFSET_BITS` trap.
- **time64**: a 32-bit kernel must enable `COMPAT_32BIT_TIME` (§1.4) [measured].

### 5.3 Static builds and toolchains (Q13) [measured]

- `rustup target add` + `-C linker=rust-lld`: the musl targets ship `crt1.o`, `libc.a` and libunwind
  ("self-contained"), so **no cross C compiler is needed** for k10s (no C dependencies). riscv64gc-musl defaults to
  dynamic linking: add `-C target-feature=+crt-static`. Stripping needs LLVM's objcopy from the toolchain
  (`rust-objcopy`, with `LD_LIBRARY_PATH` to the toolchain's `lib`).
- Binary sizes: §1.1 (1.56–1.93 MB; the non-x86 builds have no JIT).
- Kernels need a C cross compiler: Debian's `gcc-12-aarch64-linux-gnu` + binutils, extracted without root, worked
  (needs `LD_LIBRARY_PATH` for binutils' private libbfd/libopcodes).
- Tests: `CARGO_TARGET_<T>_RUNNER=qemu-<arch>-static` (Debian `qemu-user-static`, extracted) runs every in-process
  suite; i686 runs natively on an x86_64 kernel.

| Suite (`K10S_BACKEND=interp`) | i686 (native) | armv7 (qemu-user) | aarch64 (qemu-user) | riscv64 (qemu-user) |
|---|---|---|---|---|
| k10-ir (81), k10-lang (85), k10-mesh (26 + 2 + 5 + 13) | pass | pass | pass | pass |
| k10-node lib incl. coroutines (36), basic (1) | pass | pass | pass | pass |
| k10-node node (20) | pass | pass | 19/20 ¹ | 19/20 ¹ |
| crates/k10s/tests (spawn the binary) | not run ² | not run ² | not run ² | not run ² |

¹ `many_idle_keepalive_connections_stay_cheap` asserts < 8 MiB RSS growth for 1,000 connections and reads
`/proc/self/smaps_rollup`, which under qemu-user is the *emulator's* RSS (8.7 MB); one flaky backpressure failure on
aarch64 passed on rerun. ² They `exec` the k10s binary, which needs binfmt (Docker Desktop's VM has it; §7).
x86_64 with the JIT: every k10-node and k10s suite still passes on the ported tree.

### 5.4 JIT roadmap (Q14)

- **arm64 first** (Pi 4/5, Graviton, Ampere, Apple): ≈3k new lines, 6–10 weeks; the masking sequence has the same
  instruction count as on x86; I-cache maintenance after sealing; details and risks in native-hardware.md §7.2. Build
  it once for both tiers and test it **hosted on Linux first**: the existing differential, red-team and isolation
  suites run under `qemu-aarch64-static` on this machine today (the k10-node suites took minutes), then on a Pi 5 and
  a Graviton instance for performance.
- **SFI masking cost on arm64:** LFI measured 7% (SPEC 2017) for a related scheme [paper, research.md §3.5]; k10's
  power-of-two masks avoid guard regions but cannot use LFI's one-instruction `uxtw` form for small slots.
- **riscv64:** interpreter only until there is demand.
- **No 32-bit JIT** (§5.2).

---

## 6. Images and boot per platform (questions 15–18)

### 6.1 x86-64 and i386: one image for BIOS and UEFI (Q15) [measured]

Built and booted (`mkdisk.sh`, no root, no loop devices):

```
GPT (protective MBR whose first 440 bytes are GRUB's boot.img)
 p1  1 MiB   BIOS boot      GRUB i386-pc core.img (biosdisk part_gpt fat normal linux configfile)
 p2  ≥46 MiB EFI System     "K10S-BOOT" FAT32:
                              /EFI/BOOT/BOOTX64.EFI   systemd-boot  (or the UKI itself, or the Tier 2 loader §8.4)
                              /loader/loader.conf, /loader/entries/k10s.conf   (UEFI)
                              /boot/grub/grub.cfg                               (BIOS)
                              /k10s/vmlinuz, /k10s/initrd, /k10s.conf
 p3  rest    Linux data     "K10S-DATA" ext4 (deterministic UUID and hash seed)
```

- **BIOS**: GRUB's MBR code and core.img were installed by patching two sector numbers (boot.img offset 0x5C and
  core.img's first blocklist entry) instead of running `grub-install`/`grub-bios-setup`, which need devices. SeaBIOS →
  GRUB → k10s kernel served in 580 ms.
- **UEFI**: systemd-boot as the removable-media loader with a Type #1 entry, or a **UKI** booted directly as
  `BOOTX64.EFI` (3.96 MB) — both OVMF-tested. A UKI is the right Tier 1 artifact: one signed file that any UEFI loader
  (systemd-boot, the Tier 2 loader, firmware boot entries) can `LoadImage`/`StartImage`.
- **Loader choice** [doc + estimate]: GRUB (BIOS + UEFI, GPL, three builds: i386-pc, x86_64-efi, i386-efi);
  systemd-boot (UEFI only, boot counting built in); **Limine** (BSD-2, BIOS + UEFI + IA-32 UEFI, one tool, but not in
  Debian 12 — evaluate it to replace GRUB on BIOS); kernel EFI stub alone (no initrd/cmdline from the removable path:
  use a UKI instead).
- **i386**: the same layout with `BOOTIA32.EFI` covers 32-bit-UEFI tablets only with a loader that can start a 32-bit
  kernel; 64-bit kernels on 32-bit UEFI need mixed-mode handover (GRUB), not the bare stub [doc]. Best-effort tier.

### 6.2 Raspberry Pi 3/4/5 (Q16) [doc + estimate]

```
GPT+hybrid MBR or MBR (Pi firmware reads partition 1 as FAT)
 p1  FAT  "K10S-BOOT"  bootcode.bin (Pi 3), start4.elf/fixup4.dat (Pi 4), config.txt (non-empty: Pi 5), autoboot.txt,
                       bcm2710-rpi-3-b*.dtb, bcm2711-rpi-4-b.dtb, bcm2712-rpi-5-b.dtb, bcm2712d0-rpi-5-b.dtb, overlays/
 p2  FAT  boot slot A: kernel8.img (one arm64 Image for Pi 3/3+/Zero 2 W/4/5), initramfs8, cmdline.txt, k10s.conf
 p3  FAT  boot slot B
 p4  ext4 "K10S-DATA"
config.txt:  arm_64bit=1  kernel=kernel8.img  initramfs initramfs8 followkernel   (Pi 5 also tries kernel_2712.img)
autoboot.txt: [all] tryboot_a_b=1 boot_partition=2  [tryboot] boot_partition=3
```

- One generic arm64 kernel boots every 64-bit Pi through the firmware (no U-Boot needed); Pi 5 has no maintained UEFI,
  so firmware direct boot is the only reliable path there.
- 32-bit kernels on Pi 3: not needed (`arm_64bit=1`); Raspberry Pi's kernel dropped 32-bit support for BCM2711 (Pi 4)
  in 6.18.
- **Pi 3 Ethernet is USB (LAN9514/LAN7800)**: needs dwc2 + usbnet + smsc95xx/lan78xx loaded *after* the USB bus
  enumerates — uevent hotplug does it (validated in QEMU `raspi3b` with a CDC NIC; untested on hardware).
- Watchdog: `kernel_watchdog_timeout` in config.txt arms bcm2835_wdt before Linux; `kernel_watchdog_partition` can
  pick the fallback slot after a watchdog reset — combine with PID 1 petting it.
- Pi firmware and EEPROM updates as separate pinned bundles (gokrazy model); EEPROM self-update by dropping
  `pieeprom.upd` on the boot partition.
- Emulation: QEMU 7.2's `raspi3b` booted the Debian arm64 kernel with the Pi 3 devicetree and served over a USB CDC
  NIC (§1.4); it validates the DT boot, SoC driver coldplug and USB hotplug, not the real LAN9514/GENET/RP1 paths.

### 6.3 Generic arm64 servers and boards (Q17) [measured + doc]

- **One image for UEFI arm64** works: the arm64 kernel Image is itself a PE/EFI application; the same GPT/ESP layout
  with systemd-bootaa64 booted the Debian arm64 kernel under AAVMF (TCG), mounted the ESP config and ext4 data, and
  served.
- SystemReady (UEFI + ACPI) servers and EBBR boards (U-Boot's UEFI + devicetree) both boot that ESP; the kernel takes
  ACPI or the firmware-provided DT. One generic arm64 kernel config covers both; the Pi adds its firmware files and
  DTBs to the same FAT partition.

### 6.4 Image build pipeline (Q18) [measured]

- Everything above is built **rootless** with stock tools: Python newc cpio writer (device nodes are just headers,
  fixed mtimes, sorted entries), `sfdisk` on a regular file (fixed disk and partition GUIDs), `mkfs.fat -C -i` + mtools
  `mmd`/`mcopy`, `mkfs.ext4 -d DIR -U UUID -E hash_seed=…`, `dd conv=notrunc`, `grub-mkimage`, `objcopy` for the UKI,
  `depmod -b` to index a foreign-arch module tree.
- Reproducibility: fixed `SOURCE_DATE_EPOCH`, UUIDs, volume IDs and hash seeds; remaining sources of drift are the
  kernel build (`KBUILD_BUILD_TIMESTAMP`/`USER`/`HOST` must be pinned) and compressor versions [estimate].
- Per-arch outputs: `{x86_64,i386}-{vm,metal}.img`, `arm64-uefi.img`, `rpi.img` (arm64), plus UKIs and raw
  kernel+initramfs for PXE/`-kernel` use. `systemd-repart --offline` or `genimage` can replace the shell glue [doc].

---

## 7. Testing without the hardware (question 19)

| Need | Tool (no root) | Status |
|---|---|---|
| x86-64 / i386 boot, BIOS + UEFI | QEMU 7.2 + KVM, SeaBIOS, qboot, OVMF (system) | **done**: every variant of §1.2 |
| arm64 full system | `qemu-system-aarch64` 7.2 from Debian's `qemu-system-arm`, AAVMF from `qemu-efi-aarch64` | **done** (TCG): `-kernel` and UEFI disk boot |
| aarch64 / armv7 / riscv64 / i686 test suites | `qemu-*-static` from `qemu-user-static` as cargo runners; i686 native | **done** (§5.3) |
| Suites that exec the binary on foreign arches | Docker Desktop (binfmt in its VM): `docker run --platform linux/arm64` with a FROM-scratch image | binaries run (§1.4); wiring the suites in is a CI task |
| Kernel cross builds | kernel.org source + Debian cross gcc (extracted) | **done** for x86_64, i386 and arm64 |
| Pi boards | QEMU `raspi3b` (7.2), `raspi4b` (≥ 9.0, see native-hardware.md §8 for QEMU 10 rootless) | **done** on `raspi3b` with a USB CDC NIC (§1.4); no model of the real Pi Ethernet |
| Real NICs, Pi Ethernet/Wi-Fi, firmware quirks, SD wear, watchdog reset, RTC-less time | — | **needs hardware** |

Hardware the user should provide for Tier 1 (in priority order) [estimate]: a **Raspberry Pi 5** and a **Pi 4** (with
USB-serial adapters), a **Pi 3B+** (USB Ethernet path, 1 GB RAM), an x86 mini-PC with **Realtek RTL8111/8125** (the most
common PC NIC, needs `rtl_nic` firmware), one **old 32-bit or BIOS-only PC** if i386/BIOS are kept, and an **arm64 cloud
VM** (Graviton) for the arm64 JIT later. native-hardware.md §8 lists the x86 server set; one shared hardware rack
serves both tiers.

---

## 8. Design

### 8.1 Components

```
 firmware (BIOS / UEFI / Pi firmware)
   └─ loader: GRUB (BIOS) | systemd-boot or Tier 2 loader (UEFI) | Pi firmware (config.txt, autoboot.txt)
        └─ Linux kernel (k10s "vm" or "metal" config, or a distro kernel), cmdline: k10s.KEY=VALUE … panic=5
             └─ initramfs (RAM, read-only by convention): /init = k10s, /lib/modules subset, /lib/firmware subset
                  └─ k10s PID 1: mounts · signals · modalias cold/hotplug + modprobe helper · DHCP/SNTP/mDNS · config · data ·
                     watchdog · power button · update agent · k10s node (shards, mesh, KV)  — one process
```

### 8.2 PID 1 state machine

`boot → mounts → coldplug → network (DHCP or static, retry forever in the background) → config (ESP) → data
partition → time (SNTP, non-blocking) → node start → [update agent: verify, write inactive slot, reboot into it] →
commit after rejoining the mesh → … → shutdown (signal) → drain → sync → reboot(2)`. Failures never stop the node
from starting: no network → it serves nothing but keeps retrying; no data partition → memory-only state; the watchdog
covers hangs; a crash of PID 1 panics the kernel, which reboots (`panic=5`).

### 8.3 Configuration, update, storage, security (summary)

- Config: command line > `K10S-BOOT/k10s.conf` > baked-in; same keys as `k10s node` flags.
- Storage: `K10S-DATA` by GPT name; ext4 now, the shared raw segmented-log format later (both tiers).
- Updates: signed, content-addressed image blobs over gossip; A/B UKI files with boot counting (UEFI), A/B FAT
  partitions with `tryboot` (Pi), GRUB `next_entry` + `fallback` on BIOS; commit after rejoining; phased by placement.
- Security: no shell/SSH/processes, RAM root, lockdown=integrity, signed modules, signed images; authenticated mesh and
  admin API before untrusted networks; Secure Boot later (below).

**Secure Boot** [doc + estimate]: shipping our own Microsoft-signed shim needs a legal entity, an EV certificate, a
shim-review pass and the 2023 UEFI CA (the 2011 CA stopped signing in June 2026), plus justifying a ring-0-free but
JIT-generating node. For v1: Secure Boot off, or **Debian's signed shim + GRUB + kernel with our initramfs** (should
work because that chain does not verify the initrd — untested here; lockdown then allows only Debian-signed modules,
which is what the Debian-kernel flavour uses), or MOK enrollment of our own key on machines the user controls. Same conclusion as
native-hardware.md §0.2.

### 8.4 One installed image for both tiers (answer to native-hardware.md open question 8)

Proposal [estimate], compatible with both prototypes:

```
K10S-BOOT (ESP, FAT32, 256 MiB):
  /EFI/BOOT/BOOTX64.EFI | BOOTAA64.EFI   k10boot (Tier 2 loader + tier selector)
  /EFI/Linux/k10s-a.efi, k10s-b.efi      Tier 1 UKIs (A/B, boot-counted), also bootable by systemd-boot/firmware
  /k10s/metal-a.elf, metal-b.elf         Tier 2 kernels (A/B)
  /k10s/tier                             auto | linux | metal
  /k10s.conf                             node config, read by both tiers
  /boot/grub/grub.cfg + BIOS boot part.  BIOS: Tier 1 by default; Tier 2 via the Multiboot2 shim when tier=metal
  (Pi: config.txt, firmware, DTBs, kernel8.img on the same FAT partition — Pi boots Tier 1 directly)
K10S-DATA: shared persistent store (raw segmented log, one partition type GUID for both tiers)
```

- `tier=auto`: the loader enumerates PCI through the firmware before `ExitBootServices`; if the NIC (and the disk, when
  `K10S-DATA` exists) are on the Tier 2 list it boots k10-metal, otherwise it `LoadImage`s the Tier 1 UKI — no Linux
  boot-protocol code in the loader. A node that fails Tier 2 health checks twice pins `tier=linux` (boot counting).
- Both tiers announce `tier` and `arch` in gossip capacity metadata, so placement and the update agent can target them.
- The Tier 1 image builder owns the ESP layout for both tiers (native-hardware.md open question 3).

---

## 9. Roadmap [estimate]

| Milestone | Content | Exit criteria | Effort |
|---|---|---|---|
| **U0** PID 1 mode in main | review and merge `pid1.rs` (split: init, modules + hotplug, dhcp, config); SNTP; entropy handling; tests that run PID 1 logic in a user+mount namespace or a QEMU smoke job | QEMU CI: boot → apply → serve → powerdown on Debian and k10s kernels; DHCP renew test | 2–3 weeks |
| **U1** x86-64 images | kernel CI (6.18 LTS, `vm` + `metal` configs, signed modules, pinned build metadata); image builder (BIOS+UEFI, UKI); firmware bundle; Realtek/Intel mini-PC bring-up | images < 5 MB (`vm`) / < 40 MB (`metal`); boot-to-serve < 1 s in QEMU; real mini-PC serves after a cold boot | 3–4 weeks |
| **U2** updates | signed image blobs over gossip, A/B UKIs + boot counting, commit-after-rejoin, rollout waves, GRUB fallback on BIOS | kill power at every step of an update: the node always comes back on a good image; 20-node QEMU rollout with one bad image stops after the first wave | 3–4 weeks |
| **U3** arm64 + Pi 4/5 (interpreter) | multi-arch commit merged; arm64 kernel config + Pi boot files; `tryboot` A/B; Pi watchdog; arm64 CI under qemu-user + one Pi runner | Pi 4 and Pi 5 join an x86 cluster, survive power cuts, update A/B | 3–4 weeks (+ hardware) |
| **U4** arm64 JIT | shared with Tier 2 (native-hardware.md M6) | differential suites pass under qemu-user and on Graviton/Pi; red-team suite green | 6–10 weeks |
| **U5** best-effort tier | Pi 3 (USB Ethernet), armv7, i386 (BIOS) images, interpreter only | boots and serves on a Pi 3B+ and an old PC | 1–2 weeks |
| **U6** Secure Boot | MOK flow documented; decide on shim review | — | depends on the decision |

---

## 10. Risks and open questions for the architect

**Risks** [estimate]:
1. **Kernel maintenance is now ours** for the `vm`/`metal` configs: LTS bumps, CVE tracking, config drift per board.
   Mitigation: few configs, a bot that rebuilds on every LTS point release (gokrazy model), the Debian flavour as a
   fallback.
2. **Firmware licensing and size** (Realtek, Broadcom, Intel Wi-Fi blobs): separate bundle, WHENCE/licence files
   shipped, curated set.
3. **PID 1 bugs are fatal** (a panic reboots the machine): keep PID 1 code small, `catch_unwind` around init steps, the
   node's fail-stop already exists, and the watchdog covers hangs.
4. **Unauthenticated cluster traffic** on home/office networks: blocks "install on any device".
5. **Hardware diversity** (USB Ethernet on Pi 3, Realtek revisions, carrier detection, DHCP servers that unicast):
   QEMU cannot model it; budget for a small device lab.
6. **32-bit decline**: i386 has no distro kernels anymore; keep it best-effort or drop it.

**Open questions:**
1. Is Wi-Fi a requirement for any target (home users with Pis)? It means a supplicant inside k10s or one exception
   process — both large.
2. Kernel flavours: is the 1.5–2 s and ~50 MiB cost of distro kernels acceptable on bare metal in exchange for Secure
   Boot and zero kernel maintenance, or do we commit to our own `metal` kernel from U1?
3. Should the image blob be distributed by the cluster (gossip, like code) — implying every node stores images for
   other architectures — or fetched from a release URL?
4. Cluster bootstrap: which trust root signs images and authenticates nodes joining (per-cluster key in `k10s.conf`)?
5. Do we keep i386 and armv7 at all (no JIT, shrinking hardware), or stop at "best effort, interpreter only"?
6. One installed image for both tiers (§8.4): agree with the Tier 2 owner on the ESP layout, the `tier` switch and the
   shared `K10S-DATA` format before either builder is written.
7. Entropy policy: is `GRND_INSECURE` acceptable for runtime seeds (not keys) to avoid boot stalls on RNG-less boards?

---

## 11. Reproduction and sources

**Scripts** ([universal-os/](universal-os/), copy to `$UOS/scripts`):
`xenv.sh` (cross env: rust-lld linkers, qemu-user runners) · `strip-all.sh` · `interp32.sh` (i686 vs x86_64
interpreter) · `docker-multiarch.sh` · `kbuild.sh` + `kconfig/*.config` (rootless kernel builds: `tinyconfig` +
fragments; x86_64/i386 `vm`, arm64 `vm`, arm64 Pi additions) · `mkinitramfs.py` (newc cpio, module subsets with deps,
aliases and softdeps) · `initramfs-deb.sh` (Debian module profiles) · `mkdisk.sh` (GPT BIOS+UEFI disk, GRUB patched
in, systemd-boot, ext4 data) · `mkuki.sh` (UKI via objcopy) · `build-images.sh` · `boot.py` (QEMU boot + first-200
timing + apply/checks + power-down + meminfo) · `final-bench.sh` (interleaved rounds, §1.2) · `minmem.sh` ·
`linetimes.py` / `first-byte.py` (host-side serial timestamps).
Prototype code: branch `research/universal-os` (commits listed at the top).

**Sources** (numbers as cited in §2, §3, §6; all checked 2026-10-19):
1. https://gokrazy.org/ · 2. https://gokrazy.org/platforms/ · 3. https://github.com/gokrazy/tools/blob/main/packer/packer.go ·
4. https://github.com/gokrazy/tools/blob/main/internal/packer/write.go · 5. https://github.com/gokrazy/internal/blob/main/mbr/mbr.go ·
6. https://github.com/gokrazy/gokrazy/blob/main/update.go · 7. https://github.com/gokrazy/gokrazy/blob/main/authenticated.go ·
9. https://gokrazy.org/userguide/instance-config/ · 10. https://gokrazy.org/userguide/dhcp/ · 11. https://gokrazy.org/userguide/ntp/ ·
13. https://github.com/gokrazy/breakglass · 14. https://github.com/gokrazy/kernel · 15. https://github.com/gokrazy/firmware ·
16. https://github.com/tailscale/tailscale/blob/main/gokrazy/README.md ·
17. https://www.siderolabs.com/talos-linux/ · 18. https://www.siderolabs.com/blog/there-are-only-12-binaries-in-talos-linux/ ·
20. https://docs.siderolabs.com/talos/v1.13/learn-more/architecture.md · 21. https://docs.siderolabs.com/talos/v1.13/configure-your-talos-cluster/lifecycle-management/upgrading-talos.md ·
22. https://docs.siderolabs.com/talos/v1.13/platform-specific-installations/bare-metal-platforms/secureboot.md ·
23. https://docs.siderolabs.com/talos/v1.13/security/talos-default-hardening-and-cis-compliance.md ·
24. https://docs.siderolabs.com/talos/v1.13/configure-your-talos-cluster/system-configuration/acquire.md ·
25. https://docs.siderolabs.com/talos/v1.13/reference/kernel.md · 26. https://docs.siderolabs.com/talos/v1.13/platform-specific-installations/single-board-computers/rpi_generic.md ·
27. https://docs.siderolabs.com/talos/v1.13/platform-specific-installations/single-board-computers/unofficial.md ·
28. https://docs.siderolabs.com/talos/v1.13/build-and-extend-talos/custom-images-and-development/overlays.md ·
31. https://docs.siderolabs.com/talos/v1.13/getting-started/system-requirements.md · 32. https://github.com/siderolabs/talos/releases/tag/v1.14.4 ·
33. https://github.com/bottlerocket-os/bottlerocket/blob/develop/README.md · 34. https://github.com/bottlerocket-os/bottlerocket/blob/develop/SECURITY_FEATURES.md ·
35. https://github.com/bottlerocket-os/twoliter/blob/develop/tools/buildsys/src/manifest.rs · 36. https://github.com/bottlerocket-os/bottlerocket/blob/develop/variants/aws-k8s-1.35/Cargo.toml ·
37. https://github.com/bottlerocket-os/bottlerocket/blob/develop/PROVISIONING-METAL.md ·
38. https://github.com/flatcar/flatcar-docs/blob/main/docs/reference/developer-guides/sdk-disk-partitions.md ·
39. https://github.com/flatcar/flatcar-docs/blob/main/docs/setup/releases/update-strategies.md · 40. https://github.com/flatcar/flatcar-docs/blob/main/docs/setup/releases/update-conf.md ·
41. https://github.com/flatcar/nebraska · 42. https://github.com/flatcar/flatcar-docs/blob/main/docs/provisioning/ignition/boot-process.md ·
43. https://github.com/flatcar/flatcar-docs/blob/main/docs/installing/bare-metal/raspberry-pi.md · 45. https://stable.release.flatcar-linux.net/amd64-usr/4757.2.1/ ·
46. https://github.com/coreos/fedora-coreos-docs/blob/main/modules/ROOT/pages/auto-updates.adoc · 47. https://github.com/coreos/zincati ·
48. https://github.com/coreos/fedora-coreos-docs/blob/main/modules/ROOT/pages/faq.adoc · 49. https://github.com/coreos/fedora-coreos-docs/blob/main/modules/ROOT/pages/provisioning-raspberry-pi4.adoc ·
51. https://builds.coreos.fedoraproject.org/streams/stable.json · 52. https://nerves-project.org/ · 53. https://nerves.hexdocs.pm/supported-targets.html ·
54. https://github.com/nerves-project/erlinit · 55. https://nerves.hexdocs.pm/faq.html · 56. https://github.com/nerves-project/nerves_system_rpi4 ·
57. https://github.com/fwup-home/fwup · 58. https://github.com/nerves-project/nerves_pack · 59. https://elixirforum.com/t/trimming-speeding-up-startup/20790 [blog/forum] ·
60. https://github.com/u-root/u-root · 61. https://www.linuxboot.org/ · 62. https://github.com/linuxkit/linuxkit ·
63. https://github.com/linuxkit/linuxkit/blob/master/docs/security.md · 64. https://github.com/linuxkit/linuxkit/blob/master/src/cmd/linuxkit/moby/build/output.go ·
65. https://www.docker.com/blog/introducing-linuxkit-container-os-toolkit/ · 66. https://github.com/linuxkit/linuxkit/blob/master/docs/metadata.md ·
67. https://alpinelinux.org/downloads/ · 68. https://github.com/alpinelinux/aports/blob/master/main/alpine-baselayout/inittab ·
69. https://wiki.alpinelinux.org/wiki/Diskless_Mode (via search extracts) · 70. https://wiki.alpinelinux.org/wiki/Installation (via search extracts) ·
71. https://github.com/alpinelinux/aports/blob/master/main/linux-firmware/APKBUILD · 72. https://docs.balena.io/reference/OS/overview/ ·
73. https://docs.balena.io/reference/OS/updates/self-service/ · 74. https://docs.balena.io/reference/OS/updates/rollbacks/ · 75. https://docs.balena.io/learn/deploy/delta/ ·
76. https://github.com/balena-os/balena-raspberrypi · 77. https://github.com/rancher/k3os · 78. https://github.com/rancher/k3os/releases ·
79. https://github.com/rancher/k3os/issues/846 and https://github.com/rancher/k3os/discussions/838 [blog/forum].

Platform facts (§3, §4, §6):
- Raspberry Pi boot files, `config.txt`, `initramfs`, `os_prefix`, `tryboot`/`autoboot.txt`, network boot, watchdog keys:
  https://www.raspberrypi.com/documentation/computers/config_txt.html ·
  https://www.raspberrypi.com/documentation/computers/configuration.html ·
  https://www.raspberrypi.com/documentation/computers/raspberry-pi.html ·
  https://www.raspberrypi.com/documentation/computers/linux_kernel.html · https://github.com/raspberrypi/firmware/tree/master/boot
- Mainline Pi support: https://kernelnewbies.org/Linux_5.5 · https://kernelnewbies.org/Linux_6.12 ·
  https://kernelnewbies.org/Linux_6.17 · https://kernelnewbies.org/Linux_6.18 · Pi 4 UEFI https://github.com/pftf/RPi4 ·
  Pi 5 UEFI (archived) https://github.com/worproject/rpi5-uefi
- x86 boot: https://www.gnu.org/software/grub/manual/grub/html_node/BIOS-installation.html ·
  https://github.com/Limine-Bootloader/Limine · https://www.freedesktop.org/software/systemd/man/latest/systemd-boot.html ·
  https://docs.kernel.org/admin-guide/efi-stub.html · https://docs.kernel.org/arch/x86/boot.html ·
  https://uapi-group.org/specifications/specs/unified_kernel_image/ · https://wiki.debian.org/UEFI
- i386: https://www.debian.org/releases/trixie/release-notes.en.txt · https://kernelnewbies.org/Linux_7.1 · https://lwn.net/Articles/1035727/
- Secure Boot: https://github.com/rhboot/shim-review · https://github.com/rhboot/shim/blob/main/SBAT.md ·
  https://wiki.debian.org/SecureBoot · https://docs.kernel.org/admin-guide/kernel-parameters.html ·
  https://man7.org/linux/man-pages/man7/kernel_lockdown.7.html
- arm64: https://arm-software.github.io/ebbr/ · https://www.arm.com/architecture/system-architectures/systemready-compliance-program ·
  https://docs.kernel.org/arch/arm64/booting.html
- Firmware: https://git.kernel.org/pub/scm/linux/kernel/git/firmware/linux-firmware.git/tree/WHENCE ·
  https://docs.kernel.org/driver-api/firmware/fw_search_path.html · https://docs.kernel.org/driver-api/firmware/direct-fs-lookup.html ·
  https://docs.kernel.org/driver-api/firmware/built-in-fw.html · https://cdn.kernel.org/pub/linux/kernel/firmware/ ·
  https://github.com/RPi-Distro/firmware-nonfree/blob/trixie/debian/copyright · https://wiki.debian.org/Firmware ·
  r8169 firmware: https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/drivers/net/ethernet/realtek/r8169_main.c
- Kernel LTS: https://www.kernel.org/category/releases.html ·
  https://cip-project.org/blog/2025/01/13/kernel-6-12-will-have-10-years-support-via-cip-are-all-your-maintenance-problems-solved ·
  https://wiki.debian.org/LTS · Debian kernel config https://salsa.debian.org/kernel-team/linux/-/blob/debian/6.12/trixie/debian/config/config
- Modules/uevent: https://man7.org/linux/man-pages/man2/finit_module.2.html ·
  https://github.com/kmod-project/kmod/blob/master/libkmod/libkmod-index.c ·
  https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/Documentation/ABI/testing/sysfs-uevent
- IP autoconfig and IPv6: https://docs.kernel.org/admin-guide/nfs/nfsroot.html ·
  https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/net/ipv4/ipconfig.c · https://docs.kernel.org/networking/ip-sysctl.html
- Image tools: https://man7.org/linux/man-pages/man8/mke2fs.8.html · https://man7.org/linux/man-pages/man8/mkfs.fat.8.html ·
  https://www.freedesktop.org/software/systemd/man/latest/systemd-repart.html · https://github.com/pengutronix/genimage
- ACPI tiny power button: `drivers/acpi/Kconfig` in Linux 6.18 (`ACPI_TINY_POWER_BUTTON`, default signal 38).
