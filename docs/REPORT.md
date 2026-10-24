# k10s — final report

Measured 2026-10-19/20 by `bench/run-final.sh` on one laptop (i7-7820HQ, 4 cores / 8 threads): Kubernetes v1.34
(kind), the same Go services as bare processes, and k10s, run one suite after the other, each started only once the
machine was idle (load < 1). Background CPU during latency runs (desktop session, Docker Desktop's VM) was
0.7–0.9 cores for all three systems alike.

## 0. Verdict

**k10s is viable and very promising.** Against a real Kubernetes and against bare Linux processes running
equivalent services, it removes two to three orders of magnitude of platform overhead while being faster per
request, and it already provides the core of what an orchestrator must do: deployment, placement,
service-to-service calls, ingress, self-healing, rolling updates, durable per-service state. The same runtime also
boots as its own kernel with no Linux underneath.

| | Kubernetes (kind, 1 node) | k10s (1 node) | k10s advantage |
|---|---|---|---|
| Idle platform memory (working set) | 583 MiB | 1.3 MiB | 450× |
| Idle platform CPU | 21.9 % of a core | 0.10 % | 214× |
| Memory per echo replica (N = 100) | 11.0 MiB | 10.2 KiB | 1 084× |
| Replicas per GiB of node memory | 95 (and max 110 pods/node) | 142 485 (measured at 100k) | 1 500× |
| Deploy one service → first HTTP 200 | 1.17 s | 23.7 ms | 49× |
| 100 replicas/services → all serving | 17.6 s | 28 ms | 620× |
| echo, 1 connection, p50 | 55 µs | 29 µs | 1.9× |
| echo, 64 connections | 87 k req/s | 180 k req/s | 2.1× |
| service → service hop (chain), 64 connections | 25.6 k req/s | 170.6 k req/s | 6.7× |
| chain, 1 connection, p50 | 221 µs | 31 µs | 7.1× |
| echo at a fixed 5 k req/s, p99 | 2 288 µs | 540 µs | 4.2× |
| CPU per echo request (16 connections) | 47 µs | 20 µs | 2.3× |
| Replica lost → requests OK again | 2.26 s (pod deleted) | 12 ms (whole node SIGKILLed) | 188× |
| Hung node: detected / workloads replaced elsewhere (3 nodes) | 51 s / 353 s | 1.04 s / 1.08 s | 49× / 326× |
| Rolling update | 3.66 s, 11 failed probes | 0.022 s, 0 failed probes | 166× |
| echo artifact | 2.39 MB container image | 157 B module | 15 000× |
| Node software | kubelet + containerd + runc 120 MB, plus control-plane images | one 1.68 MB static binary | 72× |

Kubernetes latency figures use a load generator *inside* the kind node (through Docker Desktop's port forwarder the
host sees 386 µs p50 and 19 k req/s). k10s is also faster than the same services run as **bare Linux processes with
no orchestration at all**: echo 180 k vs 109 k req/s, chain 171 k vs 33 k req/s at 64 connections, open-loop p99
540 vs 796 µs. Full tables, definitions and caveats: [bench/results/COMPARISON.md](../bench/results/COMPARISON.md).

**Where k10s is not better today:** CPU-bound code. `compute` (1000 chained SHA-256) answers in 836 µs vs 386 µs for
Kubernetes/Go and 338 µs for a bare Go process — Go's SHA-256 is hand-written assembly; against portable Go
(`-tags purego`) Ten is 1.1–1.6× slower (§7). That is a compiler-quality gap, not a platform cost.

## 1. The question, answered from first principles

Every row of Kubernetes' cost traces back to one decision: **the unit of isolation is a hardware-protected process**
(or a VM). Processes are expensive (page tables, kernel stacks, syscalls, kernel scheduling, copies across the
boundary), opaque (hence probes and sidecars) and can only talk through the kernel (hence overlay networks,
kube-proxy, DNS). Images, cgroups, namespaces and the control plane exist to manage them. k10s replaces that one
decision and everything that follows from it:

| Layer in Kubernetes | Replaced in k10s by | Why it is cheaper |
|---|---|---|
| Process / container / pod | **Instance** = a power-of-two memory slot + machine code shared by all instances | KBs and µs instead of MBs and seconds; idle instances cost only touched pages |
| MMU + privilege rings for isolation | **Software fault isolation** compiled into every access (`base + (addr & mask)`), structured control flow, closed host-call table | one `and` per access; thousands of workloads in one address space; switch = function call |
| cgroups (memory, CPU) | Slot size *is* the memory limit; CPU budgets enforced at compiler-inserted safepoints | nothing to account on the page-fault path |
| namespaces, PIDs, filesystem | Capabilities: a workload can only name what its host functions give it | no kernel objects per workload |
| Container images, registries | Content-addressed TIR modules (bytes to KBs) spread peer-to-peer | 2.4 MB → 157 B for echo |
| Kernel scheduler per thread | One thread per core, fibers on pooled stacks, preemption via the safepoint check | idle instances own no stack |
| veth, bridge, iptables, kube-proxy, CoreDNS, sidecars | Local calls are function calls + one copy; remote calls ride one multiplexed link per peer; names resolved locally | chain at 64 connections: 24.5k → 124k req/s |
| etcd + apiserver + scheduler + controllers | Gossip (SWIM) + a **pure placement function** every node computes itself | no leader, no consensus on placement, nothing to watch |
| Liveness/readiness probes | The runtime runs the code: traps, timeouts and CPU overruns are facts | no polling |
| Guest Linux in every VM | Optional: the same runtime boots as its own kernel (`metal/`) | 3 MiB RAM, ~3 ms to ready after kernel entry |
| Language runtimes, GC, libc per workload | **Ten** (small C-like language, no GC, no runtime) → **TIR** (single value type, structured stack IR) → single-pass x86-64 / AArch64 JIT | no per-workload runtime; any language can target TIR later |

Design rationale: [DESIGN.md](DESIGN.md); literature evidence: [research.md](research.md).

## 2. What was built

| Component | Path | Rust lines (src / tests) | Status |
|---|---|---|---|
| TIR: codec, validator, reference interpreter, random program generator | `crates/k10-ir` | 4.3k / 2.7k | done, `no_std` |
| Ten compiler + prelude + 8 example services | `crates/k10-lang`, `examples/` | 6.6k / 3.0k | done |
| Single-pass TIR → x86-64 and AArch64 JIT (the isolation boundary) | `crates/k10-jit` | 7.8k / 10.7k | done, `no_std` codegen |
| Gossip membership, manifests, bundles, placement, wire protocol, simulator | `crates/k10-mesh` | 7.8k / 0.7k | done, sans-IO, `no_std` |
| Host API shared by the node and the bare-metal kernel; speculation-safe bounds | `crates/k10-host`, `crates/k10-nospec` | 0.9k (+ unit tests) | done, `no_std` |
| Node runtime (shards, fibers, slots, CoW snapshots, host API, KV, ingress, admin, cluster, JIT backend) | `crates/k10-node` | 13.3k / 1.6k (+ unit tests) | done |
| `k10s` binary (node, apply, build, run, status, logs, keygen) + integration/chaos/soak/abuse suites | `crates/k10s` | 0.8k / 4.7k | done |
| Bare-metal kernel (PVH, ACPI-free, virtio-net, smoltcp, SMP, JIT, cluster member) | `metal/` | 9.6k | done for VMs |
| Kubernetes / bare-process / k10s benchmark harness, Go load generator | `bench/` | 3.7k (sh/py/go) | done |
| Research: universal OS image (Tier 1), native hardware (Tier 2) | `docs/research/` | — | done, prototypes on local branches |

About 47k lines of Rust plus 20k lines of tests, written from scratch with no dependencies beyond `libc` (and
`smoltcp` in the bare-metal kernel).

## 3. Measured results

See [bench/results/COMPARISON.md](../bench/results/COMPARISON.md) for every table, the measurement definitions and
the caveats. Highlights beyond §0:
- **Idle:** a single node idles at 1.3 MiB and 0.1 % of a core; a 3-node cluster at 6.4 MiB and 0.57 % in total
  (Kubernetes: 583 MiB / 21.9 % for one node, 881 MiB / 27.3 % for three). Gossip costs ~300–850 B/s per node.
- **Density:** 7.4 KiB per deployed service at 100k services; 0.46 KiB per instance when instances share a
  copy-on-write snapshot.
- **Latency/throughput:** echo p50 29 µs at 1 connection, 180 k req/s at 64; a service-to-service hop costs
  0.45 µs on the same shard; a request forwarded through another node costs ~70 µs more (p50 99 µs, 108 k req/s at
  64 connections) — mostly thread hand-offs, an optimisation target.
- **Cold start:** the first request to a never-used service costs 4 µs more than a warm one.
- **Cluster:** apply → every node serving: 9–17 ms (3 nodes), 25–34 ms (5), 146–190 ms (20). Failure detection
  ~1 s by default, 0.2 s with tight gossip; services keep answering through fallback routing within 5–13 ms.
- **Bare metal (QEMU/KVM):** 694 KB kernel; ready 2–3 ms after kernel entry; serves in 3 MiB of RAM; a Linux guest
  needed 1.36 s and 88 MiB in the same setup. On 4 vCPUs a CPU-bound service scales 2.9×.
- **Universal image (Tier 1 prototype):** the unchanged `k10s` binary as PID 1 on a minimal 6.18 kernel serves its
  first request 295 ms after boot, in 32 MiB of RAM (20.8 MiB used idle).

## 4. Evidence that it is correct and contained

- **Isolation:** the JIT is differential-tested against the reference interpreter on randomly generated programs:
  1 000 000 modules, 2 M invocations, 0 mismatches, with canary regions around every instance proving no write ever
  leaves its slot; 800 000 adversarial memory programs; a red-team audit (14 attack classes: offsets, wrap-around,
  straddling, bulk ops, br_table bounds, stack clash, register state across calls, host-arg shuffles…) found no
  escape. All attacks are regression tests.
- **Runtime:** unit, integration and multi-process cluster tests on both execution backends (≈ 390 tests on the
  JIT, ≈ 100 more re-run on the interpreter), including node kill, graceful leave, restart with the same data,
  concurrent applies, re-apply under load (0 failed requests).
- **Chaos:** 20 minutes on 5 nodes, 2 585 fault rounds (322 SIGKILL, 309 SIGSTOP/SIGCONT, 310 leave/join, 320 joins,
  1 324 re-applies): **0 invariant violations, 0 wrong bodies**.
- **Soak:** 30 minutes single node — flat fds, threads, VMAs, executable mappings, RSS; 3-node — flat except a slow
  RSS drift on one node (~40 KB/min) to be confirmed by a multi-hour run.
- **Abuse / noisy neighbour:** output bombs, log spam, 20 infinite loops on one shard, crash loops, KV bombs,
  slowloris, 400 concurrent CPU burners, hostile bundles, call cycles, fd exhaustion, peers that never read: a
  well-behaved service on the same node kept **0 errors** (p99 ≤ 23 ms in the worst case).
- **Code review:** an independent unsafe-code review found 11 issues (2 high: a tenant-triggerable mesh-thread panic,
  uncapped host-side copies); all 11 are fixed with regression tests.

## 5. Challenging Linux, hypervisors and hardware

- **Isolation does not need the MMU** for same-organization workloads; it needs a compiler and a validator. That is
  the single largest saving, and it works the same with or without Linux underneath.
- **Linux is optional, not mandatory.** `k10-metal` proves the runtime is self-sufficient: no syscalls, no
  user/kernel transitions, no page-table switches, 3 MiB of RAM. In clouds (where every node is already a VM) this
  deletes the guest kernel entirely.
- **But Linux's drivers are worth keeping where hardware is diverse.** The two research tasks converge on a two-tier
  strategy running the same workloads in one cluster:
  - **Tier 1** ([research/universal-os.md](research/universal-os.md)): a universal image = a minimal Linux kernel used
    only as a driver layer + `k10s` as PID 1. Works today; boots on BIOS, UEFI x86-64 and UEFI arm64; ~20–30
    engineer-weeks to production images, updates and arm64/Pi.
  - **Tier 2** ([research/native-hardware.md](research/native-hardware.md)): the native kernel on VMs/clouds first,
    then a curated hardware list. Ten drivers (virtio, NVMe, AHCI, Intel GbE, Realtek, ENA, VMBus, gVNIC, tg3, Pi set)
    cover ~80 % of PC NICs, ~89 % of PC storage controllers, the three hyperscalers' NIC+disk, and mainstream servers'
    1 GbE ports. Prototypes (UEFI loader, ACPI, virtio 1.x, NVMe, Intel GbE, arm64 bring-up) all worked under QEMU.
    ~50–70 engineer-weeks.
- **Hypervisors** remain the right tool for one thing only: separating mutually hostile tenants. k10s does not need
  them to isolate its own workloads.
- **A new instruction set or hand-written assembly everywhere** was considered and rejected: the expensive part of
  today's stack is the layers, not instruction selection. A simple single-pass compiler already runs within
  1.06–1.76× of LLVM-optimised native code on compute kernels.

## 6. What was kept, and why
- **TCP/HTTP at the edge** (clients speak it), **UDP gossip** and **one TCP link per peer** inside the cluster — the
  wire is not where the cost was.
- **Linux as host** in hosted mode and as a driver layer in Tier 1 — its drivers are its real value.
- **x86-64 first** for the JIT; every other architecture runs the portable interpreter today (the multi-arch port of
  the hosted runtime builds for aarch64, armv7, i686 and riscv64 in the Tier 1 prototype).

## 7. Limitations (honest list)
- **Security model:** same-organization workloads per node. Spectre-class side channels between co-located workloads
  are only partly mitigated (SECURITY.md); use separate nodes per hostile tenant. Gossip and peer links are
  authenticated and encrypted with one shared cluster key (rotatable) and admin requests are signed, but every key
  holder is trusted (no per-node identity, no signed bundles yet).
- **Storage:** KV is durable but node-local (atomic per node via `kv_add`/`kv_cas`); no replication or consensus yet.
- **Compute speed:** Ten + baseline JIT is ~1.1–1.6× slower than portable Go and ~2.5× slower than Go's SHA-256
  assembly; the JIT keeps at most six locals in registers and every memory access pays the mask.
- **Ecosystem:** workloads must be written in Ten (or compiled to TIR); no Wasm/LLVM front end yet.
- **Measurements:** Kubernetes ran under Docker Desktop (kind inside a VM whose kernel and port forwarder are not
  counted — favourable to Kubernetes on fixed overhead); "replica" means a pod vs. a process vs. an instance.
- **Bare metal:** VM guests only today (QEMU, Firecracker, Cloud Hypervisor; virtio net and blk; the disk is not used for storage yet).

## 8. Roadmap (recommended order)
1. **Tier 2 (next), M1–M2 and x86 bare metal:** UEFI + BIOS + ACPI + modern virtio + Intel/Realtek NICs + NVMe +
   durable KV log on the native kernel; then clouds (ENA, VMBus, gVNIC), the rest of the curated x86 bare metal, arm64
   and the Pi.
2. **Cluster security:** node identity and signed bundles (done: shared-key authenticated/encrypted gossip and peer
   links, signed admin requests).
3. **Replicated state:** per-service replicated KV (Raft per shard group) and leases for singletons.
4. **Tier 1 images** (U0–U6, done, in maintenance): PID-1 mode, x86-64/arm64/Pi/i686/armv7 images, signed A/B updates
   spread over gossip, Secure Boot flows.
5. **arm64:** the hosted node runs the AArch64 JIT (U4); the native kernel's arm64 port (Tier 2 M5, M6) will reuse it.
6. **Performance:** register allocation in the JIT, 32-bit TIR ops, cheaper cross-node forwarding (fewer hand-offs).
7. **Ecosystem:** a Wasm → TIR translator so existing languages (Rust, C, Go, Zig) can target k10s.

## 9. Decisions
Platform decisions for both tiers (rationale in research/universal-os.md §10 and research/native-hardware.md §11):
- **Secure Boot:** not required in v1. Images ship unsigned; the docs explain how to disable Secure Boot or sign the
  image with an own key and enroll it (sbctl, MOK; docs/OS.md "Secure Boot", tested under OVMF and AAVMF with
  Microsoft's keys). No own Microsoft-signed shim.
- **Legacy BIOS:** in scope for both tiers (Tier 1 through GRUB, Tier 2 through a Multiboot2 shim).
- **Wi-Fi:** not supported; wired Ethernet only (USB Ethernet adapters on Tier 1).
- **Tier 1 kernel:** our own Linux 6.18 LTS kernels (`vm` and `metal` configs) from U1; the Debian kernel is kept as
  the bootstrap and fallback flavour.
- **Trust root:** OS images are signed (ed25519 over the image hash) by the k10s release key, whose public key is
  built into the image; operators may pin an extra key for their own builds. Joining nodes are authenticated by the
  cluster key in `k10s.conf` (the shared, rotatable key of today); per-node identity stays roadmap item 1.
- **Image distribution:** over gossip, as signed content-addressed blobs fetched with `GetBlob`, like code.
- **Order:** x86 first in both tiers (Tier 1 U1 before the Pi in U3; Tier 2 M1 → M7 in order: clouds and curated x86
  before arm64 and the Pi).
- **i386 and armv7:** best effort, Tier 1 only, interpreter only (U5); no 32-bit JIT.
- **One installed image for both tiers:** the Tier 1 builder owns the ESP, the UEFI loader picks the tier from the PCI
  device list (`tier=auto|linux|metal`), and both tiers share one raw segmented-log `K10S-DATA` format.
- **Disks:** k10s uses only its own GPT partitions (ESP and `K10S-DATA`) and leaves other partitions alone.
- **Addressing:** DHCPv4 by default, static `ip=` in `k10s.conf` or on the command line; IPv6 by kernel SLAAC on
  Tier 1, IPv4 only on Tier 2.
- **Entropy:** `GRND_INSECURE` only for non-key seeds; keys, nonces and node secrets use blocking `getrandom`;
  hardware RNG drivers are built into the kernels.
- **Drivers:** `NetDevice`/`BlockDevice` traits and slot-like DMA regions from M1 so TIR drivers can implement them
  later; first-tier drivers in Rust in the kernel; IOMMU (VT-d) in M4.
- **Multi-queue NICs:** not in Tier 2 v1 (one queue, network I/O on CPU 0); RSS comes with 10G+ NICs on demand.

## 10. Reproduce
```bash
cargo test --release -p k10-nospec -p k10-ir -p k10-host -p k10-mesh -p k10-lang -p k10-jit -p k10-node -p k10s
```
```bash
K10S_BACKEND=interp cargo test --release -p k10-node -p k10s
```
```bash
metal/build.sh && metal/test.sh
```
```bash
bench/run-final.sh
```
