# Task: universal k10s OS image and multi-arch support (Tier 1 — Linux as a driver layer)

Status: **open — analysis & research**. Owner: unassigned.
Companion task: [TASK-native-hardware.md](TASK-native-hardware.md) (Tier 2: native k10-metal on physical hardware).

## 1. Context
k10s is a ground-up replacement for Kubernetes (read [DESIGN.md](DESIGN.md) first). Workloads are TIR modules
(compiled from the Ten language) — **architecture-neutral bytecode**: the same module runs on any node, and nodes of
different kinds already form one cluster (hosted Linux nodes + bare-metal `k10-metal` guests).

The user's goal is to install a very simple system on **any physical device** — Raspberry Pi, HP server, home
workstation or desktop; amd64, i386, arm64 (and armv7) — and run everything. Writing native drivers for all hardware
is not realistic (most of Linux's code is drivers), so the agreed strategy has two tiers that run the same workloads in
the same cluster:
- **Tier 1 (this task)**: a universal image = a Linux kernel used *only as a driver layer* + the static `k10s` binary as
  PID 1. No distribution, no systemd, no container runtime, no shell by default. Boots wherever Linux boots.
- **Tier 2**: the native `k10-metal` kernel on VMs/clouds and a curated hardware list (companion task).

## 2. Current state (verified facts)
- Hosted runtime ([NODE.md](NODE.md), `crates/k10-node`, `crates/k10s`): uses only Linux syscalls (epoll, mmap,
  madvise, memfd, eventfd, sockets). The **only processor-specific code outside the JIT is the fiber context switch
  in `crates/k10-node/src/coro.rs`** (x86-64 naked function).
- `crates/k10-jit` exists for **x86-64 only**. The reference interpreter (`crates/k10-ir`, no_std Rust) runs on any
  architecture; measured 7–33× slower than the JIT on compute-heavy services, equal on I/O-bound ones.
- Ten compiler, gossip/placement (`crates/k10-mesh`), manifests and bundles are plain portable Rust.
- Measured in this repo: idle `k10s node` RSS ≈ 3–5 MiB; a Debian 6.1 Linux kernel + static init needed ≥ 88 MiB of
  guest RAM to boot under QEMU (`metal/RESULTS.md`) — a trimmed kernel should need much less (to be measured).
- Dev machine: x86-64, no root, Rust 1.98 (targets installed: x86_64 gnu/musl, wasm32, x86_64-unknown-none), Docker
  Desktop, QEMU 7.2 with only `qemu-system-x86_64` / `qemu-system-i386` (no aarch64 emulator, no qemu-user/binfmt).

## 3. Goal of this task
Produce an **analysis and research report** with a concrete design for the universal image and the multi-arch port:
what to build, how, in which order, at what cost, and how to test it. Small throwaway experiments/prototypes are welcome
(separate branch/worktree or the scratch area) to validate key claims; do not change the main code base.

## 4. Research questions
### 4.1 Prior art (closest models)
1. Study and compare: **gokrazy** (Go program as PID 1, Pi support — closest to this idea), **Nerves** (embedded Linux +
   BEAM VM), **u-root**, LinuxKit, Talos Linux, Bottlerocket, Flatcar/Fedora CoreOS, balenaOS, Alpine, k3OS (dead —
   why?). For each: image size, idle RAM, boot time, update mechanism, hardware support model, security model,
   what to borrow, what to avoid.

### 4.2 The kernel
2. Kernel source: reuse distribution kernels (Debian, Raspberry Pi OS, Ubuntu) vs a k10s-specific minimal config
   built per architecture. Trade-offs: size, idle RAM, boot time, hardware coverage, security updates/LTS policy,
   build/maintenance cost.
3. Driver and firmware loading without udev: built-in drivers vs modules loaded by `modalias` from the PID 1;
   which firmware blobs (linux-firmware) are needed for common NICs (e.g. some Realtek, Intel Wi-Fi) and how to ship
   them; licensing of blobs.
4. How small can the kernel + initramfs get while still covering the target device classes? Measure under QEMU.

### 4.3 `k10s` as PID 1
5. PID 1 duties and what `k10s` must add: mounting `/proc`, `/sys`, `/dev` (devtmpfs), signal handling, reaping
   (k10s spawns no children — confirm), clean reboot/poweroff via `reboot(2)`, panic behaviour (kernel `panic=` +
   watchdog), hardware clock, entropy.
6. Networking without a userland: kernel `ip=dhcp` autoconfiguration vs a minimal DHCP client inside k10s; IPv6 SLAAC;
   DNS (needed at all? k10s addresses peers by IP); time sync (SNTP inside k10s); Wi-Fi (needs a supplicant —
   in scope or explicitly not?).
7. Configuration: kernel command line vs a config file on the boot partition (node name, join seeds, data partition,
   static IP); first-boot cluster join; how an operator finds and manages a headless node (k10s admin API, mDNS?).
8. Persistent storage: discovering and mounting a data partition, filesystem choice (ext4 vs raw log), SD-card wear
   on Pis, behaviour when no disk exists.
9. Updates and recovery: A/B partitions or image swap, signing/verification, rollback; can a node update itself from
   the cluster (image as a content-addressed blob spread like code blobs)?
10. Security: no shell, read-only root, kernel lockdown, Secure Boot (shim), minimal attack surface; remote debug story.

### 4.4 Multi-arch port of the hosted runtime
11. Port `coro.rs` to aarch64, i686, armv7 (and riscv64?): calling conventions, callee-saved registers, FP/SIMD state
    rules per ABI, stack alignment; choose the interpreter backend automatically where no JIT exists.
12. 32-bit targets (i686, armv7): TIR values are 64-bit (cost of register pairs for the interpreter and a future JIT),
    4 GiB address space (instance density limit — estimate), `AtomicU64` availability, `memfd`/`MAP_NORESERVE`
    behaviour, epoll — anything that breaks?
13. Static builds per target (musl), binary size per target, and cross-compilation toolchain needs (linkers).
14. JIT roadmap: arm64 backend first (Pi, Graviton, Ampere, Apple); effort relative to the x86-64 backend in
    `crates/k10-jit`; SFI masking cost on arm64 (see [research.md](research.md)); is a 32-bit JIT worth it at all?

### 4.5 Images and boot per platform
15. x86-64 and i386: a single USB/disk image bootable on both UEFI and legacy BIOS (bootloader choice: systemd-boot,
    GRUB, Limine, EFI stub kernel directly).
16. Raspberry Pi 3/4/5: SD-card layout (firmware, `config.txt`, devicetree, `kernel8.img`/initramfs), 32-bit vs
    64-bit kernels on Pi 3, Pi-specific kernel vs mainline.
17. Generic arm64 servers/boards: UEFI + ACPI vs devicetree; one image for all?
18. Image build pipeline: reproducible, rootless (no loop mounts — e.g. `mkfs` from directories, `mtools`,
    `genimage`, Docker), per-arch outputs.

### 4.6 Testing without the hardware
19. What can be tested on this machine without root: `qemu-system-x86_64`/`i386` (with OVMF if available), Docker
    Desktop's multi-arch emulation (`docker run --platform linux/arm64` — check whether it works here) to run
    aarch64/armv7 `k10s` binaries and test suites, obtaining `qemu-system-aarch64` without root. Which validation
    needs real devices (and which ones the user should provide)?

## 5. Deliverables
- `docs/research/universal-os.md`: findings for every question above, with sources (URLs), clear labels for
  estimates vs measured facts, and a prior-art comparison table.
- A concrete design: image layout per platform, PID 1 responsibilities, config/update/storage model, security model.
- Measurements from experiments (kernel + initramfs size, boot time, idle RAM under QEMU for x86-64/i386 and, if
  possible, aarch64), compared with the native kernel and with a Kubernetes node.
- A multi-arch porting plan (coro.rs, builds, interpreter fallback, arm64 JIT) with effort estimates.
- A recommended roadmap with milestones, test plans and exit criteria; risks and open questions for the architect.

## 6. Constraints
- Do not modify existing crates or `metal/`; experiments go in a separate branch/worktree or the scratch area.
- No root on the dev machine; don't install system packages without asking.
- Keep k10s principles: one static binary, no central control plane, minimal footprint, Apache-2.0 for our code
  (GPL kernel is fine as a separate component).
