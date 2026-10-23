# k10s — project status and resume guide

PROJECT STATE: HARDENING

Last updated: 2026-10-23. This file is the single place to learn where the project stands and what comes next.

## State of `main`
`main` is pushed; every push publishes a dev pre-release (v0.1.0-dev.132 = 5cd99cc, step 12 U0: ci, release, pages
workflows green, the `os` QEMU suite gating the release; glibc (≥ 2.34), musl, metal, bench, examples and source
archives and `SHA256SUMS`).
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
  `K10S-DATA` grown to the disk on first boot by PID 1) and their UKIs, reproducibly; `vm` 4.3 MB, `metal` 6.7 MB.
  `metal` kernel profile: platform, storage, USB and consoles built in, 63 signed modules (per-build key deleted after
  `modules_install`, `MODULE_SIG_FORCE`, lockdown=integrity on both profiles), linux-firmware 20261019 subset with
  licences. PID 1 waits for carrier before DHCP. `os/test.sh` adds image tests on SeaBIOS, OVMF and q35/NVMe/e1000e,
  an unsigned-module refusal, boot time (QEMU start → first HTTP 200, median 0.51 s) and reproducibility; the release
  tests and publishes the images built from its musl `k10s`.
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

10. **Optional perf** — shard-owned peer links: each shard dials and reads its own link per peer, the mesh thread keeps
    gossip, fetch and accept; expected 7 → 3 wake-ups per forwarded request (≈ −40 % CPU at 1 conn).
12. **Tier 1 universal image** (docs/TASK-universal-os.md, plan in docs/research/universal-os.md §9, decisions in
    docs/REPORT.md §9; prototypes on branch `research/universal-os`): milestones U0 → U1 → U2 → U3 → U4 (= Tier 2 M6)
    → U5 → U6, one session each. U0 (PID 1 mode) and U1 (x86-64 images) done; U1's last exit criterion, a real
    Realtek or Intel mini-PC serving after a cold boot from the `metal` image, needs the hardware (user). Next U2
    (signed image blobs over gossip, A/B slot B, boot counting, GRUB fallback on BIOS). The multi-arch
    `coro.rs`/JIT-gating commit of the prototype branch belongs to U3.
13. **Tier 2 native hardware** (docs/TASK-native-hardware.md, plan in docs/research/native-hardware.md §9, decisions
    in docs/REPORT.md §9; prototypes on branch `research/native-hardware`): milestones M1 → M2 → M3 → M4 → M5 → M6 →
    M7 (x86 first), one session each.
14. **Final audit** — adversarial in-depth audit of the whole code base (isolation, JIT, host ABI, crypto and peer
    protocol, admin API, sandbox, KV durability, metal, release pipeline), every finding verified by independent
    refutation before it is reported, then fixed.
15. **User** — delete the GHCR images `k10s-bench-*` tagged `0.1.0`, `0.1.0-dev.46`, `latest`, `dev`, or grant `gh`
    the packages scope.

## Components
| Component | Where | Tests |
|---|---|---|
| Design, specs | `docs/DESIGN.md`, `TIR.md`, `HOSTAPI.md`, `JIT-ABI.md`, `TEN.md`, `NODE.md`, `research.md` | — |
| TIR: codec, validator, reference interpreter, builder, generator | `crates/k10-ir` | unit + property |
| Cluster layer: SWIM gossip, crypto, manifests, bundles, placement, wire, simulator | `crates/k10-mesh` | unit + simulator |
| Ten compiler, prelude, examples | `crates/k10-lang`, `examples/` | unit + examples |
| x86-64 JIT (single pass, SFI masking, safepoints, speculation hardening) | `crates/k10-jit` | differential soak, red-team, objdump |
| Node runtime, sandbox, CLI | `crates/k10-node`, `crates/k10s` | unit, cluster, chaos, soak, abuse, auth |
| Bare-metal node (PVH, virtio-net, smoltcp, JIT, SMP, cluster member) | `metal/` | `test.sh`, `cluster-test.sh` |
| PID 1 mode (Tier 1), k10s kernels (`vm`, `metal`), initramfs, firmware and disk image builders | `crates/k10s/src/pid1`, `os/` | unit + `os/test.sh` (QEMU) |
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
- The hosted node builds for Linux x86-64 only (seccomp filter, `coro.rs`); Tier 1 needs portable versions
  (docs/TASK-universal-os.md, U3).
- PID 1 mode and the images are tested under QEMU only (SeaBIOS, OVMF, NVMe, e1000e, Intel TCO). Needs real
  hardware: the U1 mini-PC cold boot (Realtek RTL8111/8125 with `rtl_nic` firmware, or Intel igc/e1000e), NICs and
  USB Ethernet outside QEMU's models, AHCI/eMMC boot disks, the screen console, BIOS/UEFI firmware quirks, slow USB
  storage (`bootwait`), AMD and SMBus-attached Intel watchdogs, RTC chips, real DHCP servers, switches and NTP pools.
- Linux 6.18's ACPI button driver no longer binds fixed-feature power buttons; both profiles use the tiny power
  button driver (signal to init). The evdev path of PID 1 stays for distribution kernels.
