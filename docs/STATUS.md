# k10s — project status and resume guide

PROJECT STATE: HARDENING

Last updated: 2026-10-24. This file is the single place to learn where the project stands and what comes next.

## State of `main`
`main` is pushed; every push publishes a dev pre-release (v0.1.0-dev.147 = 44da5b5, step 12 U4–U6: ci, release, pages
workflows green; the `unit`, `musl`, `i686`, `arm64` (JIT and interpreter), `armv7`, `dist`, `os`, `os-arm64` and
`os-legacy` suites gate the release; glibc, musl (x86-64, aarch64, i686, armv7), metal, bench, examples and source
archives; disk images x86-64 `vm` 4.4 MB and `metal` 6.8 MB, `i686-metal` 5.7 MB, `armv7-vm` 3.5 MB, `aarch64-vm`
4.0 MB, `aarch64-pi` 5.3 MB (xz) with their update files (`.efi`, the Pi's `.fat`) signed by the release key and
verified against the key built into k10s; `SHA256SUMS`).
`main` contains:
- Isolation: SFI-masked JIT and interpreter, validator, red-team and differential suites, objdump encoder fuzzing and
  SFI verifier tests.
- Control plane: signed admin requests (HMAC over a per-process audience, method, target, time, nonce and body
  hash; clock window, nonce cache), every node has an admin key (`DATA/admin.key`), `metrics` open only on
  `--admin`; sealed gossip (key id and sender authenticated, replay state kept past the skew window) and encrypted,
  authenticated peer links in one `k10_mesh::secure::Link` for hosted and metal (nothing served before the peer's
  `Hello` opens, capped unauthenticated input and handshaking links); cluster key rotation (current + next key);
  zeroized key material; metal secrets via fw_cfg, wall-clock resync, RDRAND failure fails the link.
- Authority: per-service `calls` allowlist (manifest format 2, index-based, canonical).
- Side channels: `br_table` compare trees and clamped jump tables, opt-in lfence/retpoline, speculation-safe host
  bounds (`k10-nospec`), per-node SipHash-keyed fuzzy guest clock; one host ABI crate (`k10-host`) for hosted and metal.
- Defense in depth: two-phase start (bind, then `--user` drop, capability check, Landlock, seccomp, then workloads),
  root refused without `--user`, `--sandbox on|seccomp-only|log|off`.
- DoS: per-service memory reservations, bounded overshoot, placement versioned in gossip (1000 nodes × 10k services
  in ≈ 80 ms, output pinned by a test), rejoin through tombstone addresses and `DATA/peers`, KV store with ordered
  flushes and rewrite-from-memory on I/O errors; each peer link gets 1 MiB per mesh-loop iteration (bytes read plus
  replies queued), so a peer flooding requests cannot stall gossip.
- Tier 1 PID 1 mode (U0, docs/OS.md): `k10s` as `/init` of a Linux image: modalias cold/hotplug and modprobe helper,
  `K10S-BOOT` config and `K10S-DATA` data partitions, RFC 2131 DHCP (renew, rebind, NAK), SNTP, hostname, health-gated
  watchdog, power button; the node runs as a sandboxed child (`--user 65532:65532`), restarted with backoff. k10s
  kernel 6.18.57 `vm` profile (`os/kbuild.sh`, seccomp + Landlock) and the pinned Debian 13 kernel; `os/test.sh`
  (CI suite `os`, release gate) boots both under QEMU: apply, serve, power off, state kept across reboots; DHCP
  renew test (and the cluster node's restart on a new address). Non-key seeds use `GRND_INSECURE`.
- Tier 1 x86-64 images (U1, docs/OS.md): `os/images.sh` builds `k10s-os-V-x86_64-{vm,metal}.img.xz` (GPT, BIOS via
  GRUB patched into the MBR, UEFI via systemd-boot and a UKI; the §8.4 ESP layout with slot A, `/k10s/tier`; ext4
  `K10S-DATA` grown to the disk on first boot by PID 1) and their UKIs, reproducibly; `vm` 4.4 MB, `metal` 6.8 MB.
  `metal` kernel profile: platform, storage, USB and consoles built in, 63 signed modules (per-build key deleted after
  `modules_install`, `MODULE_SIG_FORCE`, lockdown=integrity on both profiles), linux-firmware 20261019 subset with
  licences. PID 1 waits for carrier before DHCP. `os/test.sh` adds image tests on SeaBIOS, OVMF and q35/NVMe/e1000e,
  an unsigned-module refusal, boot time (QEMU start → first HTTP 200, median 0.51 s) and reproducibility; the release
  tests and publishes the images built from its musl `k10s`.
- Tier 1 updates (U2, docs/OS.md "Updates"): OS images are UKIs with an appended Ed25519 signature (hand-written
  SHA-512/Ed25519, RFC 8032 vectors; `k10s image sign|verify|pubkey`), signed in releases by the k10s release key
  (public half in `os/release-key.pub`, built into k10s; private half: repository secret `K10S_IMAGE_KEY`), plus
  operator keys (`os_key`). The manifest (format 3) names one image per platform (`ARCH-PROFILE`) and the first wave;
  images travel as blobs. Tier 1 nodes report their image in a gossip member extension record, sent only to members
  that decode extensions (older nodes keep working). Every node runs `k10s_mesh::rollout::step`: waves of 1, 2, 4…
  ranked by `sha256(image | id)`, left/dead members keep their rank for 5 min, a failed image stops the rollout. Init
  verifies and writes the other A/B slot of K10S-BOOT (UKI, and kernel/initramfs/GRUB lines for BIOS), arms a 3-boot
  trial in systemd-boot (boot-counted file names) and GRUB (`grubenv` counted down by `grub.cfg`), commits when the
  node is back in the cluster and serves (`--os-commit-grace`), abandons after `os_trial`, records failed images.
  `os/test.sh` adds update-bios/update-uefi (power cut at every step, a panicking image, a never-committing one) and
  rollout (20 BIOS VMs: a bad image stops after the first wave, a good one goes through waves 1, 2, 4, 8, 5).
- arm64 and the Raspberry Pi (U3, docs/OS.md): the hosted runtime builds for aarch64 (AAPCS64 `coro.rs` switch with
  d8–d15, aarch64 seccomp filter); `scripts/ci-test.sh arm64` runs every suite on a GitHub arm64 runner (gnu and
  musl, the node and CLI suites on the JIT and the interpreter).
  `os/kbuild.sh arm64`: one kernel for QEMU virt/arm64 guests (UEFI+ACPI or DT) and the Pi 4/5 (RP1, GENET/GEM,
  `bcm2835_wdt`), plus `os/kpatches/` (the Pi firmware's tryboot reboot flag, from the Raspberry Pi kernel).
  `os/images.sh --arch aarch64`: `k10s-os-V-aarch64-vm.img.xz` (GPT, systemd-boot, UKI; 4.0 MB) and
  `k10s-os-V-aarch64-pi.img.xz` (MBR: K10S-BOOT with `autoboot.txt`, slots A/B as raw signed FAT16 images with the
  Pi 4 firmware, mainline DTBs and Image.gz, K10S-DATA; 5.3 MB) with `.efi`/`.fat` update files; UKIs are now written
  by `os/mkimage.py` itself (objcopy corrupted arm64 PE headers). PID 1 finds partitions by FAT/ext4 label on MBR,
  grows MBR partitions, and updates a Pi through tryboot (one-shot trial, `k10s/trying` retried 3 times, commit =
  `autoboot.txt` rewrite). `os/test.sh --arch aarch64` (CI suite `os-arm64`, release gate, TCG on an x86-64 runner):
  boot-arm64, image-arm64, update-arm64, pi (a model of the Pi firmware on QEMU virt: power cut at every step, a
  panicking image, a never-committing one), mixed (a Pi joins an x86-64 host node's cluster). The dist build adds
  `k10s-V-aarch64-unknown-linux-musl.tar.gz` (cross-linked with rust-lld).
- arm64 JIT (U4 = the hosted half of Tier 2 M6, docs/JIT-ABI.md): k10-jit has an AArch64 backend (`a64.rs` encoder,
  `codegen/a64`) with the x86-64 guarantees: every access is `[x27, (addr+off) & x28]`, safepoints at function entries
  and loop headers, jump tables clamped with `csel` + `csdb`, the Lfence mode as `sb` or `dsb nsh; isb` and a
  Retpoline thunk, Cortex-A53 erratum 835769 avoided (Pi 3). Code is mapped with the kernel's page size, the I-cache
  synced once per module (CTR_EL0 IDC/DIC) then `membarrier` SYNC_CORE (allowed by seccomp on aarch64). The JIT is the
  native backend on arm64 nodes. An independent A64 decoder (cross-checked against objdump) and SFI verifier check
  every module the test suites compile. The x86-64 code generator moved unchanged (byte-identical output on 20,288
  blobs). JIT vs interpreter on an arm64 Linux VM (Apple Silicon), medians of 15 interleaved runs: sha 14.0×,
  nbody 9.8×, sort 9.6×.
- Best-effort tier (U5, docs/OS.md): the hosted runtime builds for i686 and armv7 (static musl, rust-lld; interpreter
  only): i386 cdecl and AAPCS-VFP `coro.rs` switches, k10-jit only under `cfg(any(target_arch = "x86_64",
  target_arch = "aarch64"))`, slots ≤ 2^28 in 64 MiB regions (larger `memory` refused at apply), 256 KiB fibers,
  seccomp for AUDIT_ARCH_I386/ARM (106 calls), 32-bit-id syscalls for `--user`. Kernels `os/kbuild.sh i386` (M686,
  PAE/NX, IDE/SATA built in, 72 signed modules) and `armv7` (virt, UEFI); the arm64 kernel adds the Pi 3. Images
  `k10s-os-V-i686-metal` (MBR, BIOS-only GRUB A/B), `k10s-os-V-armv7-vm` (systemd-boot + UKI) with `.efi` update files;
  the Pi image also boots the Pi 3 B/B+ (bootcode.bin, start_cd, bcm2710 DTBs, the same autoboot.txt/tryboot A/B).
  `os/test.sh --arch i686|armv7` (CI suite `os-legacy`: Atom N270 with IDE and RTL8139 on SeaBIOS; armv7 on AAVMF32),
  `pi3` on QEMU raspi3b with a hotplugged usb-net in `os-arm64`, CI suites `i686` (x86-64 runner) and `armv7`
  (natively on the arm64 runner), all release gates.
  Releases add `k10s-V-i686-unknown-linux-musl.tar.gz` and `k10s-V-armv7-unknown-linux-musleabihf.tar.gz`.
- Secure Boot (U6, docs/OS.md "Secure Boot"): the images stay unsigned for Secure Boot; three documented and tested
  ways to boot them: Secure Boot off, own keys (sbctl: PK/KEK/db enrolled next to Microsoft's, systemd-boot and the
  UKIs signed with the db key), a MOK under Debian's Microsoft-signed shim 16.1 (`os/bootfiles.sh shim-ARCH`). On an
  Authenticode-signed UKI, `k10s image sign` stores the Ed25519 trailer as a `WIN_CERT_TYPE_EFI_GUID` entry of its PE
  certificate table, so both signatures hold; `k10s image payload` strips it. systemd-boot/stub 262 (Debian, pinned):
  `reboot-on-error auto` lets boot counting roll back a trial the firmware refuses; trials are armed with `preferred
  k10s-X.efi` plus the counted `default` that the 252 of older installs matches. Init logs `efi: Secure Boot on|off`.
  `os/test.sh` secureboot (OVMF q35+SMM, Microsoft keys) and secureboot-arm64 (AAVMF) gate releases: the release image
  is refused, and it serves with Secure Boot off, with own keys and with a MOK; under own keys a re-signed update
  commits and one signed by k10s only is refused 3 times and rolled back.
- Tier 2 boot (13a = M1's boot half, docs/OS.md "Tiers", metal/README.md "Booting"): `k10boot` (`metal/boot`,
  x86_64-unknown-uefi, no dependencies beyond k10-mesh and `metal/bootproto`) is `BOOTX64.EFI` of the x86-64 images:
  `/k10s/tier` (a GRUB environment block: `tier=linux|metal|auto`, `metal_cmdline=`) picks Tier 1 (LoadImage of
  systemd-boot, moved to `EFI/systemd/`) or the native kernel `/k10s/metal.elf`, a k10s image (profile `native`)
  verified against the release key, keys built in (`K10BOOT_KEYS`) and, with Secure Boot off only, `os_key`; `auto`
  matches PCI (firmware PCI I/O) against `metal/bootproto/src/devices.rs` (virtio-net and virtio-blk, transitional and modern). The kernel
  gets a PVH `start_info` (merged E820, command line, RSDP) with named modules: k10s.conf, `/k10s/bundle.k10b`, the
  key files k10s.conf names (they replace fw_cfg on real machines; fw_cfg stays for `-kernel`), the GOP framebuffer
  (for 13c), the runtime services; page 0x8000 reserved, ExitBootServices retried on a stale key. Boot counting: a
  NV UEFI variable counted by k10boot, deleted by the kernel through runtime services once healthy (ready 15 s and a
  frame received; own page tables for the call); 2 unhealthy boots pin Tier 1 until `/k10s/tier` is rewritten or
  the kernel changes. k10s.conf keys shared with Tier 1 become kernel arguments (bootcfg.rs). Multiboot2 header +
  32-bit shim in boot.rs: BIOS boots the native kernel through GRUB when `tier=metal` (no signature check, no
  counting). The images ship `tier=linux` (KV state: step 13d); releases sign and publish
  `k10s-os-V-x86_64-native.elf`. `os/test.sh` tier (OVMF pc/q35: native joins a hosted node and serves, auto,
  fallback, refused signatures, PVH), mb2 (SeaBIOS → GRUB → Multiboot2) and secureboot (k10boot in front, MOK through
  shim's LoadImage, os_key refused under Secure Boot) gate releases; every other UEFI test boots Tier 1 through
  k10boot.
- Tier 2 M1 platform and virtio (13b, metal/README.md): the native kernel reads ACPI without AML (MADT CPUs and I/O
  APICs, FADT, MCFG, HPET, SPCR; `\_S5` and virtio-mmio `LNRO0005` slots by DSDT scan; MP-table fallback), starts each
  listed CPU with its own INIT/SIPI (`MAX_CPUS` 64; x2APIC under hypervisors, after an x2APIC handover or for APIC
  ids > 254, xAPIC otherwise on bare metal), maps all RAM (1 GiB pages
  above 4 GiB) with every device range uncached (PAT reset; 64-bit BARs mapped on use), keeps a sorted, disjoint
  memory map, reclaims 64–640 KiB, calibrates the TSC against the HPET/PM timer/PIT channel 0 when there is no
  kvmclock, powers off (S5: PM1 or the hardware-reduced sleep register) and reboots (reset register, 8042, 0xCF9,
  triple fault) through ACPI — `POST /_k10s/power off|reboot` (signed; leaves the cluster first), fatal errors reboot
  after `k10.panic` s — with no isa-debug-exit. PCI through ECAM (segments, bridges enabled, 64-bit BARs, MSI-X);
  `NetDevice`/`BlockDevice` traits with `DmaRegion`s; virtio PCI modern + legacy and MMIO v1 + v2; virtio-net and
  virtio-blk (async requests, flush); DHCPv4 (renew/rebind/NAK) or `k10.ip=` (k10s.conf `ip`), advertised as the cluster address (a changed lease
  makes the member leave and reboot onto it, as Tier 1 restarts its node);
  console from SPCR or `k10.console=`. `metal/test.sh` (KVM or TCG) runs every QEMU platform (pc legacy/transitional/
  modern-only, q35 behind a root port, microvm mmio v1/v2): selftest with a disk, serve, reboot, power off; SPCR on
  COM2, static IP, no-HPET clock, DHCP against Tier 1's scripted server, 16 GiB + 32 vCPUs; `cluster-test.sh` adds a
  leave + power off; `vmm-test.sh` runs Firecracker v1.17.0 and Cloud Hypervisor v53.0 (pinned, CI's metal-e2e job);
  the metal-selftest gate runs pc, pc-legacy, q35 and microvm. Same throughput as before, 3 MiB guests place 16
  services instead of 9, image +100 KiB (metal/RESULTS.md §9).
- Releases: SemVer; tag `vX.Y.Z` on main → stable release, every push to main → `vX.Y.Z-dev.N` pre-release.
- Performance vs 08f2921 (`bench/k10s/ab.py --same`, 4 interleaved rounds, host load ≈ 7 from the benchmark itself;
  medians): single node within ±2.5 %; local ingress −4 %; forwarded requests −5 % at c=1 (CPU per request +7 %) and
  −2 % at c=64, the cost of encrypted peer links (~3 %) and seccomp (~2 %), same syscalls per request; ChaCha20 runs
  with AVX2 when present (docs/NODE.md, cluster measurements). Trivial invoke: JIT 7.8 ns on both.

## Remaining steps, in order (one per session)
Rules: production-ready (no workarounds), comments only where needed as English one-liners, docs (incl. website/)
updated with every change and written as the current state, no perf regression beyond the measured cost of the
security features (`bench/k10s/ab.py --same` vs 08f2921), no stable tag without the user, never create namespaces or
seek privileges on this machine.

13. **Tier 2: k10s on its own kernel, on QEMU and on real x86 machines** — NEXT (docs/TASK-native-hardware.md, plan
    in docs/research/native-hardware.md §9, decisions in docs/REPORT.md §9; the prototype branches no longer exist).
    Goal: the `metal/` kernel boots from the shared image (universal-os.md §8.4) on VMs and on a real x86 machine and
    serves in a k10s cluster with hosted and Tier 1 nodes. One session each, in this order:
    - 13a **M1 boot** — done ("State of main"; under QEMU only). Needs real hardware or cloud VMs: k10boot on real UEFI
      firmware (large memory maps, firmware using page 0x8000 or 1 MiB, real boot entries' load options, runtime
      SetVariable on SMM flash, x2APIC handover), GRUB's Multiboot2 on a real BIOS, a Hetzner or Vultr VM (UEFI and
      BIOS) and a DigitalOcean droplet (BIOS, GRUB → Multiboot2) once 13b's modern virtio-net lands, real machines
      with 13c's NICs.
    - 13b **M1 platform and virtio** — done (State of main). Needs real hardware (user): firmware x2APIC handover and
      APIC ids > 255 (no interrupt remapping yet), real MADT/FADT/DSDT quirks (`_PTS`-dependent S5, a `\_S5` that is
      not a plain package, broken checksums or SPCR), MTRR/PAT setups and 64-bit BARs that firmware covers with
      write-back MTRRs, large real memory maps (UEFI, overlapping entries), multi-socket root buses found by the bare
      metal ECAM brute-force scan, TSC calibration against real HPET/PM timers (and TSC sync across sockets), the
      8042/0xCF9 reset fallbacks, BMC serial consoles through SPCR; cloud VMs (Hetzner/Vultr modern virtio,
      DigitalOcean BIOS) with 13a's images. Not done here: wall-clock discipline without kvmclock (an NTP/SNTP client),
      ACPI events (power button), Firecracker/Cloud Hypervisor secrets (no fw_cfg and no k10boot there: their admin
      API stays closed until keys come through the boot protocol), interrupt remapping (VT-d, M4).
    - 13c **x86 bare metal** (M4's first part; after 13a and 13b, in parallel with 13d): Intel e1000e (I219 with
      ME/ULP handling, 82574L) and igc (I225/I226), Realtek r8169-class (8111/8125), GOP text console, UDP log
      console, watchdog; a USB-bootable image; then bring-up on the user's machine.
    - 13d **M2 storage** (after 13b, in parallel with 13c): NVMe, virtio-blk, AHCI, GPT `K10S-DATA`, the segmented KV
      log shared with the hosted node (`no_std` record codec; Tier 1 moves from ext4 to it), raw A/B kernel slots
      read by the loader, crash-injection tests.
    - Then M3 big-cloud NICs (ENA, VMBus, gVNIC; needs cloud accounts), the rest of M4 (tg3, IOMMU, HWP, a hardware
      CI rack), M5 arm64 arch layer, M6's metal half (the arm64 JIT from U4 on metal: I-cache sync and cross-CPU
      synchronization of its own), M7 Pi 4/5.
10. **Optional perf** — shard-owned peer links: each shard dials and reads its own link per peer, the mesh thread keeps
    gossip, fetch and accept; expected 7 → 3 wake-ups per forwarded request (≈ −40 % CPU at 1 conn).
16. **Hosted page size** — the hosted runtime assumes 4 KiB pages (`sys::PAGE`, the slot tail page and stride,
    snapshots, coroutine guard pages; only the JIT's code mapping uses the kernel's page size): port it to the
    kernel's page size so the aarch64 binaries run on 16 KiB-page kernels (Raspberry Pi OS on the Pi 5, Asahi), with
    a test under a 16 KiB-page arm64 kernel in QEMU.
14. **Final audit** — adversarial in-depth audit of the whole code base (isolation, JIT, host ABI, crypto and peer
    protocol, admin API, sandbox, KV durability, metal, release pipeline), every finding verified by independent
    refutation before it is reported, then fixed.
12. **Tier 1 universal image** — done (U0–U6) and frozen: fixes and maintenance only, no new platforms, boards or
    boot flows (docs/TASK-universal-os.md, plan in docs/research/universal-os.md §9, decisions in docs/REPORT.md §9).
    Done under QEMU/qemu-user; what remains needs real hardware (user): U1's Realtek or Intel mini-PC serving after
    a cold boot from the `metal` image; for U2, power cuts that reach a real disk's
    write cache and GRUB `save_env`/systemd-boot renames on real firmware; for U3 (its exit criteria), a Pi 4 and a
    Pi 5 joining an x86 cluster from the Pi image, surviving power cuts and updating A/B: the real firmware honouring
    `autoboot.txt` and the tryboot flag set by the patched kernel, the mainline DTBs under the Pi firmware, GENET/RP1
    Ethernet, SD/NVMe, `bcm2835_wdt`; for U4, the JIT suites and speed-up on Graviton/Neoverse and on Pi 3/4/5 cores
    (CI's arm64 runner is a Cobalt 100: I-cache sync on cores without IDC/DIC, membarrier across real cores, the cost
    of `csdb`/`sb`, the A53 erratum on silicon); for U5 (its exit criteria), a Pi 3B+ (whether its `bootcode.bin`
    honours `autoboot.txt`/tryboot — if not, a trial boots the committed slot and the image is recorded as failed —,
    the LAN9514/LAN7515, SD, `bcm2835_wdt` resets) and an old PC (real BIOS booting GRUB from the MBR of an
    IDE/SATA/USB disk, real NIC/disk chips, PAE CPUs, 1–4 GiB HIGHMEM); for U6, real firmware setup screens (Secure
    Boot off, setup mode, enrolling keys), `sbctl`/`mokutil` on a running OS, MokManager's screens, machines with only
    the 2023 Microsoft CAs or the third-party CA disabled, firmware rejecting RSA-4096 keys.
15. **User** — delete the GHCR images `k10s-bench-*` tagged `0.1.0`, `0.1.0-dev.46`, `latest`, `dev`, or grant `gh`
    the packages scope.

## Components
| Component | Where | Tests |
|---|---|---|
| Design, specs | `docs/DESIGN.md`, `TIR.md`, `HOSTAPI.md`, `JIT-ABI.md`, `TEN.md`, `NODE.md`, `research.md` | — |
| TIR: codec, validator, reference interpreter, builder, generator | `crates/k10-ir` | unit + property |
| Cluster layer: SWIM gossip, crypto, manifests, bundles, placement, wire, simulator, signed OS images, rollout | `crates/k10-mesh` | unit + simulator |
| Ten compiler, prelude, examples | `crates/k10-lang`, `examples/` | unit + examples |
| x86-64 and AArch64 JIT (single pass, SFI masking, safepoints, speculation hardening), A64 decoder and SFI verifier | `crates/k10-jit` | differential soak, red-team, objdump, verifier |
| Node runtime, sandbox, CLI | `crates/k10-node`, `crates/k10s` | unit, cluster, chaos, soak, abuse, auth |
| Bare-metal node (PVH, Multiboot2, k10boot UEFI loader and tier selector, virtio-net, smoltcp, JIT, SMP, cluster member) | `metal/` | `test.sh`, `cluster-test.sh`, `os/test.sh tier mb2` |
| PID 1 mode (Tier 1), A/B updates, Secure Boot flows, k10s kernels (`vm`, `metal`, `arm64`, `i386`, `armv7`), initramfs, firmware and disk image builders (x86-64, i686, armv7, arm64, Raspberry Pi 3/4/5) | `crates/k10s/src/pid1`, `os/` | unit + `os/test.sh [--arch aarch64\|i686\|armv7]` (QEMU) |
| Baselines (Kubernetes, bare processes), loadgen, k10s harness, A/B driver | `bench/` | — |
| Editor extension, documentation site | `editors/vscode`, `website/` | — |
| CI and releases | `.github/`, `scripts/` | — |

## Conventions
- Commit as `Quentin Gerard <quentingerard8@gmail.com>` via `git -c user.name=… -c user.email=…`, NO trailer
  `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`; push `main` to `origin` after each verified commit (from
  step 8 on).
- Use separate cargo target dirs per concurrent agent (`target/node`, `target/chaos`, …) to avoid lock contention.
- Never touch Docker containers/images not created by k10s scripts (the machine has the user's own).
- This machine has no flex, bison or libelf headers: kernel builds run with `. target/os/hosttools/env.sh`, a sysroot
  of Debian 12 packages (`apt-get download flex bison m4 libelf-dev zlib1g-dev libzstd-dev`, `dpkg-deb -x`, a
  `pkg-config` shim for libelf; not in git, recreate it if `target/` was cleaned).

## Open items
- Untested here (needs root): `k10s node --user U --data NEW_DIR` without `--admin-token-file` creates the data
  directory and `admin.key` owned by U. Metal has no test of the admin nonce-cache 429.
- Multi-hour soak to rule out the ~40 KB/min RSS drift seen once on a cluster node.
- Metal has no per-service memory reservations (it shares placement, not the accounting), and its peer links have no
  unsent-byte cap or per-iteration budget (a key holder that stops reading grows the write buffer): port the hosted
  ones (`LINK_BUDGET`, `LINK_SOFT_CAP`, `LINK_HARD_CAP` in `crates/k10-node/src/cluster/mesh.rs`).
- Hosted aarch64 nodes on 16 KiB-page kernels are unsupported until step 16; the k10s arm64 kernel uses 4 KiB pages.
- 32-bit nodes (U5): `k10-nospec` clamps are plain code there (`GUARANTEED = false`, docs/OS.md Limits); placement
  does not know a node's word size, so a 32-bit node refuses the whole desired state if a service over 256 MiB is
  placed on it. riscv64 has no build (no `coro.rs` switch or seccomp arch). The i686 image is BIOS-only (no 32-bit
  UEFI); the armv7 image has no SoC drivers for real boards.
- arm64 JIT (U4): the SFI verifiers run in the tests only, not when code is loaded; `csdb` costs ~12 ns per jump
  table on Apple cores.
- systemd-boot/stub 262.1-1 is pinned from Debian unstable (snapshot.debian.org fallback once the pool drops it).
  Machines installed from pre-U6 images keep systemd-boot 252: updates work through the counted `default`, but a
  trial the firmware refuses (Secure Boot) stops in the firmware; the docs say to copy in a current systemd-boot
  before turning Secure Boot on. The shim pin may need updates as SBAT revocations evolve.
- PID 1 mode, the images and updates are tested under QEMU only (SeaBIOS, OVMF, AAVMF, NVMe, e1000e, Intel TCO; the
  Pi through a model of its firmware on QEMU virt; power cuts are QEMU kills, which lose only what the guest had not
  written). The Pi's tryboot flag needs the patched kernel's mailbox call, untested on a real Pi. Needs real hardware: the U1 mini-PC cold boot,
  power cuts during an update on real disks, GRUB `save_env` and systemd-boot boot counting on real firmware (Realtek RTL8111/8125 with `rtl_nic` firmware, or Intel igc/e1000e), NICs and
  USB Ethernet outside QEMU's models, AHCI/eMMC boot disks, the screen console, BIOS/UEFI firmware quirks, slow USB
  storage (`bootwait`), AMD and SMBus-attached Intel watchdogs, RTC chips, real DHCP servers, switches and NTP pools.
- `metal/test.sh` under TCG (no KVM, e.g. the dev container): the `jit smp4` suite's two `instcount (instance state
  kept)` checks fail, before and after 13b alike: emulated, the service's CPU time exceeds the 20 µs local-dispatch
  threshold, so the second request runs on another CPU's instance pool. KVM runs (CI) pass.
- The x86-64 `update` test timed out once waiting for the commit (release run of 2d12614, before U3); it passed on
  the next run. Watch for a recurrence. Same for `graceful_leave_and_join_under_load` (musl suite, "placement includes
  the newcomer" not within 10 s; release run of 08a3fb7, first failure in the release history).
- Linux 6.18's ACPI button driver no longer binds fixed-feature power buttons; both profiles use the tiny power
  button driver (signal to init). The evdev path of PID 1 stays for distribution kernels.
