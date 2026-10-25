# k10-metal — measurements

**Host:** Intel i7-7820HQ (4 cores / 8 threads), Linux 6.1.0-53-amd64, QEMU 7.2.22 with KVM (`-cpu host`), one
vCPU. The guest's NIC is `virtio-net-pci,disable-modern=on` on QEMU user networking (slirp), reached through a
`hostfwd`. The client is `bench/bin/loadgen` on the same host.

**Noisy host.** Other agents were compiling and running tests throughout: load average 4–8, with peaks of 11.
Every figure below is therefore one of:
* the median of interleaved repetitions (boot: 15 runs per firmware; HTTP: 3 runs × 5 s per case);
* a per-request CPU cost, which is less sensitive to contention than throughput.

Raw data is in `out/results/*.json`. Reproduce with `./build.sh && tools/linux-compare.sh && tools/measure.py`
(`tools/measure.py engines` for §6, `tools/cluster_measure.py` for §7, `tools/measure.py smp` for §8).

**Two kernel versions.** §2, §4 and §5 (boot, HTTP, isolation) were measured on v0 (interpreter only, single
node, 14 services). The current kernel adds the JIT (default engine), W^X/NX paging, cluster mode and SMP; §1, §3,
§6, §7 and §8 are measured on it (§3's RAM minimum and §6–§7 just before the SMP changes, which leave the one-CPU
paths alike). Its HTTP fast path is unchanged (test.sh's and §8's loadgen runs give the same 40–55k rps for
`hello`, load permitting), and §6 compares the engines directly.

## Summary

The other figures of this table predate Tier 2's milestone M1 (13a, 13b); §9 measures what M1 changed against the
same kernel, interleaved on one machine, §10 what M2 (13d) added.

| | k10-metal | Linux guest (same QEMU/slirp setup) |
|---|---|---|
| Image | **1,150 KB** ELF incl. JIT, cluster layer, SMP, ACPI, virtio 1.x/MMIO/blk, NVMe, AHCI, the K10S-DATA store, DHCP and the Multiboot2 entry (1,116 KiB in memory incl. .bss; after M1: 997 KB; before Tier 2's M1: 694 KB / 670 KB; v0: 440 KB) + 13 KB bundle (20 services) | 8.2 MB bzImage + 0.9 MB initramfs |
| QEMU start → "ready" (median / p10) | **95 ms / 76 ms** (SeaBIOS); qboot 98 / 66 ms | 1,360 ms (boot only) / 1,435 ms (HTTP) |
| …of which inside the kernel (entry → ready) | **1.7–2.5 ms** (v0); 2.9 ms with 18 services JIT-compiled at boot (2.1 interpreted) | ≈ 1.25 s |
| QEMU start → first HTTP 200 | **≈ 2 ms after ready** (median 96 ms) | 1,437 ms |
| Minimum `-m` that boots *and* serves under load | **3M** (2M boots but cannot hold the test set) | 88M (80M: "System is deadlocked on memory") |
| Memory used after boot (no services) | **1.32 MiB** (0.65 MiB image + 0.68 MiB, of which 0.52 MiB NIC rings/buffers) | — |
| Per deployed service / per instance | **0.9 KB / 69.9 KB** (JIT code shared by code hash; 64 KiB slot + 4 KiB tail + ~200 B; per-CPU pools add ~100 B per service and extra CPU) | — |
| `hello`, 1 connection: p50 latency | **63 µs** | 102 µs |
| `hello`, 64 connections: throughput | **54.3k rps** (guest 31% busy; QEMU main thread is the bottleneck) | 31.4k rps |
| Guest (vCPU thread) CPU per request, 64 conns | **8.3 µs** | 30.9 µs |
| QEMU main thread (slirp + virtio emulation) per request | 15.6 µs | 30.8 µs |
| JIT vs interpreter, CPU per invocation (SHA-256 chain ×1000 / n-body ×1000 / merge sort 50k) | **0.84 / 0.31 / 6.5 ms** vs 13.6 / 10.4 / 46 ms: **16× / 33× / 7×** | — |
| Cluster (2 hosted `k10s node` + 1 k10-metal guest): QEMU start → all members see 3 live | **112 ms** (median of 5) | — |
| Apply through a hosted node → applied on the guest / a guest-placed service answers | **23 ms / 28 ms** (hosted members: 20–21 ms) | — |
| Guest killed (SIGKILL) → its services answered by hosted nodes | **0 failed probes** (every 5 ms); declared dead after 315 ms | — |
| Forwarded request, 1 connection, p50: hosted ingress → guest service / guest ingress → hosted service | 129 µs / 137 µs (direct: 71 µs guest, 31 µs hosted) | — |
| CPU-bound throughput with `-smp 1 / 2 / 4` (SHA-256 chain, 16 connections) | **1,030 / 1,920 / 2,942 rps (1.86× / 2.86×)**; n-body 1.82× / 2.91× | — |

The "Linux guest" is the host's own Debian kernel, booted by QEMU's Linux loader (no Linux in k10-metal's path, of
course). Its initramfs holds a static Rust `/init` (tools/linux-guest). In boot mode `/init` prints a line and
powers off. In HTTP mode it loads virtio-net, configures 10.0.2.15 and serves "hello" with a thread per
connection. A distro kernel probes much more hardware than a custom minimal one would, so read the Linux boot time
and RAM as "stock Linux", not "best-case Linux".

## 1. Kernel image

| | bytes |
|---|---|
| | current (JIT + cluster + SMP) | JIT + cluster | v0 (interpreter, single node) |
|---|---|---|---|
| `out/k10-metal.elf` (ELF file, with symbols) | 693,664 | 679,392 | 440,072 |
| loaded from file (PT_LOAD filesz) | 557,038 | 545,508 | 377,994 |
| in memory incl. .bss | 670,418 | 657,819 | 491,946 |
| .text / .rodata / .data / .bss | 497,443 / 52,903 / 6,492 / 111,680 | 489,219 / 52,960 / 3,129 / 111,344 | 337,699 / 38,615 / 1,480 / 111,336 |

.bss holds the boot stack (64 KiB), the boot page tables (24 KiB) and the double-fault IST stack (16 KiB). The text
covers the TIR validator and interpreter, the JIT compiler, smoltcp (TCP + UDP), the k10-mesh
manifest/bundle/placement/gossip/wire code, `core::fmt` and the kernel itself; with fat LTO, crate boundaries
largely disappear. The JIT added ≈ 135 KB of text at `opt-level=3` and the cluster layer ≈ 290 KB more, most of
it inlining; building the kernel crate, `k10-jit` and `k10-mesh` with `opt-level="s"` (the interpreter, smoltcp
and `core` stay at 3) brought the image from 883 KB back to 679 KB with no measurable change in request cost or
JIT/interpreter speed (interleaved bench: `hello` 4.48 vs 4.58 µs of guest CPU per request at 51–55k rps,
interpreted `compute` 29.0 vs 27.6 µs, JIT SHA-256 chain 858 vs 882 µs). All-`"s"` would save only 14 KB more.

TIR modules are tiny: from 24 bytes (`trap`) and 27 bytes (`spin`) to 311 bytes (`chain`) for the builder-made
services; the four services compiled from Ten (`examples/*.ten`, std prelude included) are 2.0–2.7 KB. The whole
18-service bundle (manifest + blobs) is 12,343 bytes.

## 2. Boot time

`tools/measure.py boot` runs 15 interleaved boots per firmware. It times QEMU process start → the serial
"k10-metal ready" line → the first `GET /hello/` answered 200 through slirp; polling starts at the ready line. Each
breakdown column has its own median.

| (ms) | QEMU start → VM created (median) | VM created → kernel entry (median) | kernel entry → ready (median) | QEMU start → ready (min / p10 / p50 / p90) | → first HTTP 200 (median) |
|---|---|---|---|---|---|
| SeaBIOS (QEMU default) | 40 | 59 | 1.7 | 72 / 76 / **95** / 144 | 96 |
| qboot (`-bios qboot.rom`) | 55 | 41 | 2.5 | 59 / 66 / **98** / 137 | 100 |

* **The breakdown is measured from the guest.** kvmclock counts from 0 when KVM creates the VM. The kernel reads
  it at entry ("vm→kmain" in the ready line) and the TSC to the ready line.
* **The firmware matters more than the kernel.** qboot saves ~18 ms of firmware time over SeaBIOS; the kernel adds
  ~2 ms. Across runs, QEMU's own startup (process, libraries, KVM) varied between 40 and 75 ms, which is why the
  totals barely differ between the two firmwares.
* **Inside the kernel (`k10.verbose` phases):**
  * memory map, frame allocator and heap: ~0.1 ms;
  * clock (kvmclock: exact TSC frequency, no calibration wait): 0.01 ms;
  * LAPIC: 0.1 ms;
  * PCI + virtio-net: ~1.0–1.3 ms;
  * smoltcp: 0.01 ms;
  * bundle decode + validate + prepare 14 services: ~0.2 ms.
* **PCI + virtio is dominated by VM exits.** A port-I/O exit to QEMU costs ~7.5 µs here and a PCI config read
  ~14 µs (two exits). That is why the PCI scan stops at the NIC, and why the console is quiet by default: QEMU's
  16550 does one host `write()` per byte, ~15 µs per character, so the full verbose banner would add ~5 ms to boot.
* **First request ~2 ms after ready.** That covers slirp's ARP for 10.0.2.15, the TCP handshake and the request.
* **Linux, same QEMU and firmware:** 1,339–1,360 ms to `/init`'s ready line (`quiet`), and 1,435 ms with the
  virtio-net modules loaded and the listener up. Its first HTTP 200 comes at 1,437 ms.

## 3. Memory

**Minimum RAM** (`tools/measure.py ram`; "serves" means `/hello`, a 20 KB echo POST, `/chain`, `/counter`, and
loadgen with 16 keep-alive connections for 2 s with zero errors):

| `-m` | 2M | **3M** | 4M | 6M | 8M | 16M | 32M |
|---|---|---|---|---|---|---|---|
| RAM seen by the guest | 1.5 MiB | 2.5 MiB | 3.5 MiB | 5.5 MiB | 7.5 MiB | 15.5 MiB | 31.5 MiB |
| boots / serves (v0) | yes / no | **yes / yes (55.9k rps)** | yes / yes | yes / yes | yes / yes | yes / yes | yes / yes |
| boots / serves (JIT, 18 services) | yes / no | **yes / yes (18.0k rps)** | yes / yes (31.0k) | yes / yes (53.6k) | yes / yes | yes / yes | yes / yes |

At 2M, the RAM above 1 MiB minus the image leaves ~0.4 MiB, too little for four 68 KiB instances plus socket
buffers. With the JIT a 3 MiB guest still serves, slower (18k rps): its JIT coroutine stack is 96 KiB and 88 KiB of
code pages are mapped, so less is left for socket buffers under 16 connections. Below 12 MiB the kernel switches to small socket and NIC buffers (16 × 2 KiB per ring). A 3 MiB
guest still serves 56k rps, because steady load needs only one warm instance per service and a single 32 KiB
coroutine stack: on one core, a request that completes within its time slice hands both straight to the next one.

Linux needs **88M** to reach `/init` (boot-only and HTTP alike).

**Usage** (`/_k10s/status`, frames by tag; 64M guest):

| state | used (dynamic) | + kernel image | notes |
|---|---|---|---|
| booted, no services (v0) | 688 KiB | 1.15 MiB | NIC rings + 2×128 buffers 536 KiB, heap 148 KiB (4 listening sockets × 24 KiB buffers…) |
| booted, no services (current kernel) | 692 KiB | 1.32 MiB | same; the image is larger |
| 18 test services deployed (JIT) | 856 KiB | 1.48 MiB | + 88 KiB of code pages (18 modules, one or more 4 KiB pages each: 34.6 KB of machine code), + ~4 KiB per service |
| after one request to each of 11 services (JIT) | 1.73 MiB | 2.38 MiB | 11 instances (68 KiB slots) + 1 coroutine stack (96 KiB with the JIT) |
| 1,000 `hello` services, 1 warm instance each (JIT) | 68.1 MiB | | **69,857 B per instance**, 872 B per deployed service (the 1,000 services share one compiled module) |

* **Per-instance cost** is the slot: `2^mem_log2` bytes of TIR memory (64 KiB here), a 4 KiB tail page, and
  ~170 B of bookkeeping. Slots are fully backed (no demand paging yet), so a 4 KiB-memory service would cost about
  8.2 KB per instance.
* **Instance creation** (allocate the slot, zero it, copy the data segments, run `init`) plus the first request,
  including a fresh TCP connection through slirp, averaged 0.41 ms over the 1,000 services.

## 4. Request latency and throughput

`tools/measure.py http`: loadgen in closed loop, 5 s plus 1 s warmup, median of 3 interleaved runs at load average
4–5.8. "vCPU" and "QEMU main" are per-thread CPU of the QEMU process (`/proc/<pid>/task`, `-name
debug-threads=on`) divided by requests. "guest busy" is the kernel's own accounting: time not halted and not
busy-polling.

| target | conns | rps | p50 µs | p99 µs | vCPU µs/req | QEMU main µs/req | guest busy | guest pkts rx/tx per req |
|---|---|---|---|---|---|---|---|---|
| k10-metal `hello` | 1 | 14,786 | **63** | 169 | 64.8* | 42.6 | 16% | 1.02 / 1.0 |
| k10-metal `hello` | 16 | **54,727** | 286 | 596 | 15.9 | 16.4 | 33% | 1.09 / 1.0 |
| k10-metal `hello` | 64 | **54,290** | 1,032 | 2,544 | 8.3 | 15.6 | 31% | 1.25 / 1.0 |
| k10-metal `echo` POST 1 KiB | 64 | 53,401 | 1,020 | 2,128 | 9.2 | 16.1 | 36% | 1.23 / 1.0 |
| k10-metal `chain` (→ `echo` call) | 1 / 64 | 14,473 / 51,943 | 65 / 1,012 | 169 / 2,384 | 9.7 (c=64) | 15.9 | 36% | |
| k10-metal `counter` (KV get+put) | 1 / 64 | 14,126 / 48,038 | 66 / 1,020 | 169 / 2,704 | 10.5 (c=64) | 16.7 | 38% | |
| k10-metal `compute?n=1000` (interpreted) | 1 / 64 | 6,414 / 16,520 | 122 / 2,864 | 326 / 10,304 | 58.9 (c=64) | 18.3 | **98%** | |
| Linux guest `hello` | 1 | 9,386 | 102 | 211 | 103.9* | 79.0 | | |
| Linux guest `hello` | 16 | 30,216 | 516 | 1,144 | 32.4 | 31.8 | | |
| Linux guest `hello` | 64 | 31,411 | 1,928 | 3,920 | 30.9 | 30.8 | | |
| host-native Go echo, no VM | 1 / 16 / 64 | 17,971 / 86,749 / 103,751 | 51 / 177 / 564 | 111 / 564 / 2,448 | | | | |

\* At one connection the vCPU figure is mostly KVM halt-polling and the guest's 50 µs busy-poll window, not work.
With `k10.spin_us=0` (no busy-poll): 13.8k rps, p50 65 µs, vCPU 67 µs/req — the same.

**Separating slirp from the guest.** k10-metal's own work per request (guest busy time / requests) is ~6 µs at
c=16–64. That covers virtio rings, smoltcp, HTTP parsing, scheduling, instance hand-off, the interpreter and
building the response. QEMU's main thread runs slirp's TCP/IP, virtio-net device emulation and the event loop, and
spends **15.6 µs per request**. Being one thread, it caps throughput near 1 / 15.6 µs ≈ 64k rps: at 54k rps it is
85% busy while the guest is 31% busy. **The ceiling is slirp/QEMU, not k10-metal.** The same holds for the Linux
guest, which makes QEMU's main thread work twice as hard per request (30.8 µs; more packets, kicks and
notifications), hence its ~31k rps ceiling. A tap/vhost-net backend needs root and was out of scope.

With the interpreter, `compute?n=1000` (~25k TIR instructions) is guest-bound: 98% busy, ~59 µs per request,
~2 ns per instruction. Isolated runs measured ~1.1 ns per instruction; under host load the vCPU's SMT sibling and
clock frequency are shared. This is the case the JIT is for.

## 5. Isolation: an infinite loop next to other services

The `spin` service is `loop { br 0 }` with `cpu_ms = 100`. Each request runs until its 100 ms CPU budget, then
traps with `Interrupted`, answers 500, and its instance is discarded. Meanwhile loadgen drives `hello` with 16
keep-alive connections, and a second loadgen keeps 4 connections hammering `/spin`, so four infinite loops are
always runnable (`tools/measure.py spin`, median of 3):

| | hello rps | p50 µs | p99 µs | max µs | errors | spin requests trapped |
|---|---|---|---|---|---|---|
| `hello` alone | 52,258 | 294 | 796 | 5,421 | 0 | — |
| `hello` + 4 infinite loops | **52,169** | **262** | **860** | 8,711 | 0 | 52 / 52 (each at exactly 100 ms CPU), ~50k preemptions |
| same, before RX-triggered preemption | 3,650 (vs 29k alone then) | 4,192 | 7,584 | 10,447 | 0 | 71 / 71 |

The infinite loops only use CPU the guest would otherwise spend halted. Short requests are not slowed, for three
reasons:
* Every invocation is a coroutine, preempted at TIR safepoints by the one-shot LAPIC timer (2 ms slices).
* Preempted tasks go to a "long" queue that gets one slice per main-loop iteration, behind new requests.
* While a long task runs, RX interrupts are on, and an arriving packet preempts it within microseconds.

The last row is the first version, which only had time slices: other services kept answering, but with
milliseconds of added latency. With new connections (curl, one TCP handshake per request, `test.sh`), the worst
`hello` latency during a 4-loop storm is 6–8 ms. A loop that has just started runs its first 2 ms slice before it
counts as a CPU hog.

## 6. JIT vs interpreter

`tools/measure.py engines`: 3 interleaved boots per engine (`k10.engine=jit` / `interp`), load average 4.0–5.4. For
each service, `/_k10s/bench/<n>/…` runs n invocations back to back in one task and reports the CPU time of each
(TSC time while the invocation runs: host calls included, network and queueing excluded); the table gives the
median over boots of the per-boot minimum and median. Then loadgen with one keep-alive connection for 3 s gives the
end-to-end view through slirp. Raw data: `out/results/engines.json`.

| service (request) | JIT CPU min / median | interpreter CPU min / median | speedup (min) | p50 latency JIT / interp (1 conn) |
|---|---|---|---|---|
| `hello` (1 host call) | 0.17 / 0.17 µs | 0.27 / 0.28 µs | 1.6× | 62 / 62 µs |
| `compute?n=1000` (xorshift loop, builder TIR) | 2.28 / 2.37 µs | 26.5 / 133 µs | 11.6× | 64 / 131 µs |
| `sha/?n=1000` (Ten: SHA-256 chain, examples/compute.ten) | **841 / 929 µs** | 13.6 / 13.9 ms | **16×** | 1.13 / 14.4 ms |
| `nbody/?n=1000` (Ten: f64 n-body) | **314 / 340 µs** | 10.4 / 10.7 ms | **33×** | 0.48 / 10.8 ms |
| `sort/?n=50000` (Ten: merge sort, 2 MiB memory) | **6.5 / 7.1 ms** | 46 / 55 ms | **7×** | 6.6 / 49 ms |
| `todo/` GET (Ten: KV read + JSON) | 0.38 / 0.39 µs | 1.23 / 1.27 µs | 3.2× | 62 / 65 µs |

* **Compute-bound code gains 7–33×.** SHA-256 is integer/bit work over 32-bit words kept in i64 (16×); n-body is
  f64, which the kernel's interpreter executes with *software* floating point (the kernel is compiled soft-float),
  while JIT code uses SSE2 — hence 33×. Merge sort is load/store-bound (7×). The JIT SHA-256 compression costs
  ~0.84 µs per 64-byte block, a few times what an optimizing native compiler does: the JIT is single-pass (no
  register allocation across blocks beyond 6 pinned locals).
* **Request-sized services barely change end to end**: `hello` and `todo` spend < 1.5 µs in the engine out of
  ~62 µs of round trip through slirp. The interpreter's `compute` median (133 µs) is far above its minimum
  (26.5 µs): its per-invocation value stacks are heap-allocated and sized by use, and under host load the
  allocation/zeroing path varies; the JIT allocates nothing per invocation (its frames are on the coroutine stack).
* **Compile cost**: the 18 modules (11.6 KB of TIR) compile to 34.6 KB of machine code in 0.84 ms in total (median;
  `hello` 4 µs, `sha` 132 µs, `nbody` 205 µs), i.e. ~14 MB/s of TIR, done once per module at apply and shared by
  code hash. Kernel entry → ready with the 18-service bundle: 2.9 ms JIT vs 2.1 ms interpreted (10 interleaved
  boots each).
* **Recursion depth**: with the 64 KiB of JIT frame space per coroutine (96 KiB stack − 32 KiB host reserve), a
  one-local recursive function reaches depth ~2,030 before `StackOverflow` (32 bytes per frame); the interpreter
  allows 4,000 frames (its own limit).
* **Isolation is unchanged**: test.sh's spin storm (4 infinite loops, 100 ms budgets) runs under both engines; JIT
  loops are preempted through `VmCtx::stack_limit` at loop heads and trapped at exactly their budget.

## 7. Cluster mode: a k10-metal guest among hosted nodes

`tools/cluster_measure.py`, 5 repetitions, load average 2.8–6.1. Each repetition starts 2 hosted `k10s node`
processes (2 shards, `--mem 64MiB`) and a k10-metal guest (JIT, 128 MiB, advertised capacity 64 MiB), all with
`probe_ms = 100` and `suspicion_mult = 2`, then applies 12 copies of `hello` through a hosted node (3–5 land on the
guest). Polling is every 2–5 ms. Raw data: `out/results/cluster.json`; functional checks: `cluster-test.sh`
(≈ 100 checks, also with `ENGINE=interp`).

| what | median (min–max) |
|---|---|
| QEMU start → every member sees 3 live (boot + join) | 112 ms (94–162) |
| apply through hosted n1: request answered | 18 ms (12–21) — n1 validates/compiles before announcing |
| → applied on n1 / n2 / the guest | 20 / 21 / **23 ms** (guest 21–30); the guest's fetch of manifest + 17 blobs takes 0.6–2 ms |
| → a guest-placed service answers through the guest's ingress | 28 ms (26–41) |
| guest SIGKILLed, probing one of its services through n1 every 5 ms | **0 failed probes** (all 5 runs): the reset peer link sends requests straight down the ranking |
| → n1 declares the guest dead | 315 ms (310–364) |
| guest restarted with the same node id → 3 live everywhere | 116 ms (97–180), higher incarnation (wall-clock start) |
| → guest re-fetched and applied the desired state / serves its services again | 118 ms / 161 ms |

Request forwarding, one keep-alive connection (loadgen, 3 s; p50 / p99, median of 5):

| path | p50 | p99 | rps |
|---|---|---|---|
| guest service via the guest's ingress (no forwarding) | 71 µs | 215 µs | 12.4k |
| guest service via hosted n1 (n1 → peer link → guest) | 129 µs | 446 µs | 6.5k |
| n1 service via n1 | 31 µs | 85 µs | 27.7k |
| n1 service via hosted n2 | 107 µs | 258 µs | 8.4k |
| n1 service via the guest (guest → peer link → n1) | 137 µs | 572 µs | 6.2k |
| guest service via n1, 16 connections | 294 µs | 908 µs | **49.7k** |

* A forwarded request through the guest costs about what a hosted-to-hosted hop costs (+22–30 µs): the extra is
  slirp on the guest side of the link, not the guest (whose per-request work is a few µs).
* Through n1 with 16 connections the guest serves 50k rps — about what it serves on its own ingress — because the
  peer link is one persistent TCP connection carrying many requests per segment, which costs slirp less than one
  HTTP connection per client.

## 8. SMP: throughput with `-smp 1 / 2 / 4`

`tools/measure.py smp`: JIT, 128 MiB guest, 3 interleaved repetitions of each CPU count, loadgen 4 s per case (load
average 2.7–3.0 for this run; a first run at load 5.4–6.0 gave 1.53× / 2.62× for SHA-256 and 1.32× / 2.47× for
n-body, `out/results/smp-run1.json`). "busy" is the guest's own accounting per CPU (time not halted). Raw data:
`out/results/smp.json`.

| case | 1 CPU | 2 CPUs | 4 CPUs | scaling 2 / 4 | busy per CPU at 4 CPUs |
|---|---|---|---|---|---|
| `sha/?n=1000` (0.9 ms of CPU each), 16 connections | 1,030 rps, p50 14.9 ms | 1,920 rps, p50 7.7 ms | **2,942 rps**, p50 5.0 ms | **1.86× / 2.86×** | 98 / 86 / 82 / 83 % |
| `nbody/?n=1000` (0.31 ms each), 16 connections | 2,691 rps, p50 5.6 ms | 4,893 rps, p50 3.0 ms | **7,836 rps**, p50 1.9 ms | **1.82× / 2.91×** | 99 / 91 / 89 / 85 % |
| `hello`, 64 connections | 45.0k rps | 47.8k rps | 45.0k rps | 1.06× / 1.00× | 21 / 0 / 0 / 0 % |

* **CPU-bound services scale** — 1.8–1.9× on two CPUs, 2.9× on four — with the requests spread over all CPUs
  (the busiest, CPU 0, runs at most 28% more invocations than the least busy). The host has 4 physical cores
  shared with QEMU's main thread (slirp), loadgen and other agents' builds, so four vCPUs never get four idle
  cores; the workers' 82–91 % busy time is the rest of the gap: their queues briefly drain while CPU 0 turns
  responses into new requests (dispatch is decided once per request; there is no work stealing).
* **`hello` does not need more CPUs and does not get them**: its average CPU time (~0.3 µs) is far below the 20 µs
  dispatch threshold, so it stays on CPU 0, which is 21 % busy; throughput is slirp's (§4) whatever `-smp` is.
  Before the cost-aware dispatch, spreading `hello` over 4 CPUs (an IPI per request to a halted CPU) dropped it to
  28k rps.
* Bring-up cost: four CPUs online ≈ 1.5 ms after the INIT/STARTUP broadcast (of which 1 ms is the INIT delay);
  with `-smp 1` the fw_cfg probe adds ~0.06 ms to boot.
* test.sh runs its whole JIT suite (functional, spin storm, redeploys, `max_instances = 1` contention, load, 8 MiB
  guest) on 4 CPUs too, and cluster-test.sh passes with a 4-CPU guest (`SMP=4`).

## 9. M1 platform: ACPI, all RAM, x2APIC, virtio 1.x / MMIO / blk, DHCP (before / after)

Step 13b replaced the VM-only assumptions (fw_cfg CPU count, 4 GiB mapped, legacy virtio-net, static IP,
isa-debug-exit) by ACPI, a full identity map, PCI ECAM, the virtio transports behind device traits, DHCP and ACPI
power control. Measured on the development machine of that step — Apple Silicon, Docker Linux VM, **QEMU 10.0.13 with
TCG** (no KVM: x86 emulated; other VMs ran on the same host), so absolute figures are not comparable with §1–§8 — as
interleaved runs of three kernels on the same QEMU command line (`pc`, SeaBIOS, legacy virtio-net, slirp, 64 MiB):
before Tier 2 (44da5b5), with 13a (the boot half: k10boot, Multiboot2, named modules; 716899e) and with 13b;
medians of 10 boots (`k10.exit_on_ready`) and 10 × 5 s loadgen runs (`/hello/`, 64 connections) per case:

| | 44da5b5 | 13a | 13b, static IP | 13b, DHCP | 13b, DHCP, modern virtio-net |
|---|---|---|---|---|---|
| QEMU start → ready | 140 ms | 140 ms | 156 ms | 161 ms | 165 ms |
| kernel entry → ready | 57 ms | 57 ms | 74 ms | 79 ms | 82 ms |
| memory used at ready (19 services) | 880 KiB | 880 KiB | 892 KiB | 896 KiB | 848 KiB |
| image in memory | 804 KiB | 892 KiB | 992 KiB | 992 KiB | 992 KiB |
| `hello`, 64 connections | 28.8k rps | 29.3k rps | 27.9k rps | 28.3k rps | 27.9k rps |

* **Throughput** is 3–5 % below 13a's kernel under TCG, in every interleaved series (three more series with fixes
  in between gave the same gap). It is not the trait-object dispatch (a build with the NIC statically dispatched
  measured the same), nor x2APIC (`k10.apic=xapic`: the same), nor DHCP, nor the reclaimed low memory (now
  allocated last). Bisecting found it in the per-CPU array sized for 64 CPUs instead of 8 (a 13b build with
  `MAX_CPUS = 8`: 29.6k rps, 13a 30.1k, 13b 28.6k in that series): the layout of the per-CPU state and the statics after it, which TCG's
  translation-block cache and the host's caches see, not more work per packet. Under KVM (CI) 13b is not slower
  (below).
* **Boot**: under TCG the TSC is calibrated against the HPET (20 ms window, for tens of ppm instead of the old
  10 ms PIT); with KVM (CI, and every KVM cloud) kvmclock gives the frequency and no calibration runs. The other
  additions are the ACPI parse (microseconds), the page-table rebuild, a full bus-0 PCI scan instead of stopping at
  the NIC, and DHCP (two round trips to slirp; the DISCOVER goes out before the bundle is applied, so most of it
  overlaps the apply; `k10.ip=` skips it).
* **Memory**: 13b grew the image by 100 KiB (ACPI, PCI ECAM, virtio modern/MMIO/blk, DHCP, the power code, per-CPU
  state for 64 CPUs; 13a added 88 KiB, mostly a 128 KiB boot stack), and the kernel now also uses the RAM between
  64 KiB and 640 KiB, which it left idle before (allocated last: it is mapped with 4 KiB pages). Net effect at the
  small end (`-m 3M`, 2.5 MiB visible): **1,500 KiB usable instead of 1,116, 15 of the 19 test services placed
  instead of 9**, `hello` with 16 connections at 23.0k rps instead of 19.2k (TCG).
* **KVM** (the release workflow's `metal/test.sh` on GitHub's ubuntu-24.04 runners, nested KVM; one run each, boot
  with DHCP for 13b):

  | ready (kernel entry → ready line) | aca1a86 (= 44da5b5's kernel) | 13a (716899e) | 13b (9e5df1f) | 13b (26aca76) |
  |---|---|---|---|---|
  | jit, 64 MiB / 8 MiB | 10.0 / 8.7 ms | 12.5 / 10.6 ms | 18.7 / 18.5 ms | 15.8 / 16.1 ms |
  | interpreter, 64 MiB / 8 MiB | 9.3 / 9.6 ms | 9.9 / 12.1 ms | 15.5 / 18.1 ms | 12.9 / 15.6 ms |
  | jit, 4 CPUs, 64 MiB / 8 MiB | 11.8 / 11.7 ms | 14.2 / 11.8 ms | 20.7 / 17.8 ms | 17.4 / 17.8 ms |
  | memory used at ready, jit 64 MiB | 840 KiB | 840 KiB | 848 KiB | 848 KiB |

  26aca76 cut 3 ms of 9e5df1f's increase (fewer configuration-space and I/O APIC accesses, each an exit to the L1
  hypervisor; DHCP during the apply); 3–4 ms remain over 13a, mostly DHCP's round trips and the device probe's
  exits (nested KVM makes each exit tens of microseconds; on a non-nested host they are 1–2 µs). Throughput on
  these runners is bimodal whatever the kernel (`hello`, 64 connections: 21k rps or 28–42k from one run to the
  next; aca1a86 28.6k / 28.8k / 42.5k, 13a 36.9k / 21.3k / 21.1k, 13b 41.6k / 21.1k / 21.6k and 21.6k / 21.5k /
  20.9k for jit / interpreter / jit with 4 CPUs), so CI shows no throughput regression and cannot resolve one of a
  few percent; the controlled comparison is the TCG series above.

Platforms covered by `test.sh` (each: self test with a virtio-blk disk, serve, reboot and power off on a signed
request), and the others:

| machine (QEMU 10, TCG) | firmware | NIC / disk | ACPI | TSC source | result |
|---|---|---|---|---|---|
| `pc` | SeaBIOS (PVH) | virtio-net-pci transitional → virtio 1.x, virtio-blk-pci | full, PM1a 0x604 | HPET | pass |
| `pc`, `disable-modern=on` | SeaBIOS | legacy virtio-pci | full | HPET | pass |
| `pc`, `disable-legacy=on` | SeaBIOS | modern-only `1af4:1041`/`1af4:1042` | full | HPET | pass |
| `q35` | SeaBIOS | modern-only behind PCIe root ports, ECAM (MCFG) | full, reset register 0xCF9 | HPET | pass |
| `microvm` | qboot | virtio-mmio v1 (DSDT `LNRO0005` slots) | hardware-reduced, sleep control register | PIT channel 0 | pass |
| `microvm`, `force-legacy=false` | qboot | virtio-mmio v2 | hardware-reduced | PIT channel 0 | pass |
| `microvm,acpi=off` | qboot | virtio-mmio v1 (command line) | none: MP table | PIT | serves; no power off (no ACPI) |
| `pc`, `hpet=off` | SeaBIOS | | full | ACPI PM timer | pass |
| `q35`, 16 GiB, 32 vCPUs | SeaBIOS | | full | HPET | 32 CPUs online in 5 ms, 32 ran invocations, power off |
| `pc` + SPCR table (COM2) | SeaBIOS | | full | | console on COM2 |
| Firecracker v1.17.0, Cloud Hypervisor v53.0 | PVH | virtio-mmio v2 / virtio-pci modern | | kvmclock | `vmm-test.sh`, CI (KVM) |

## 10. M2 storage: K10S-DATA, NVMe, AHCI, kernel slots (13d)

Step 13d added the `K10S-DATA` store (crates/k10-kvlog), the NVMe and AHCI drivers, storage.rs (group commit,
checkpoints, the node's files) and the kernel updates (osupdate.rs; k10boot's slots.rs).

| | 13b (dev.153) | 13d |
|---|---|---|
| kernel ELF | 996,977 bytes | 1,150,264 bytes (+150 KiB: the store, NVMe, AHCI, GPT, storage, updates with Ed25519 verification) |
| k10boot | 72,192 bytes | 106,496 bytes (+33 KiB: GPT and superblock parsing, Block I/O, the kernel choice) |
| QEMU start → ready with a 128 MiB disk (CI, KVM, `pc`/`q35`) | — | 0.10–0.15 s, the first boot formatting and growing `K10S-DATA` included; the same after a power cut (replay and the recovery seal) |

* **Power cuts** (`tools/storage_test.py`, CI with KVM, the metal-selftest gate): a client PUTs 4 KiB values to 64
  keys in sequence (about 1,900 to 2,000 a second through slirp, the same on virtio-blk, NVMe and AHCI: the HTTP
  round trip bounds it, not the disk) while QEMU is killed at random moments, 4 times per controller; each reboot
  finds the store at the last 0–6 acknowledged writes (the 2 ms group commit window and the batch in flight), every
  key at its value of that prefix, then a clean power off keeps all of them (about 15,000 to 23,000 writes per
  controller, the log checkpointed every few hundred).
* **Hosted node** (`bench/k10s/ab.py --same`, base 56132f2 and 13d, 4 interleaved rounds on an arm64 Linux VM: the
  base of the STATUS figures, 08f2921, has no arm64 JIT): every case within ±2 % (single node, local and forwarded
  cluster requests, at 1 and 64 connections). The KV path (`examples/counter.ten`, one `kv_add` per request, 3
  interleaved rounds): 25.1k / 391k req/s (base) against 24.8k / 388k (13d, data directory) and 24.8k / 394k (13d,
  raw store) at 1 / 64 connections.

## 11. What was not measured / caveats

* **slirp ceiling.** Throughput is capped by slirp (single-threaded, in QEMU's main loop). k10-metal's own
  capacity, extrapolated from its busy time, is well above the 54k rps observed; measuring it needs a tap +
  vhost-net backend (root).
* **Host noise.** Boot-time totals carry ±30 ms of host noise (QEMU process start-up); the in-guest phases are
  stable to ~0.5 ms.
* **Linux comparison.** Linux uses a distro kernel with modular virtio. A tailored kernel would boot faster and
  smaller, but would still carry a TCP/IP stack, a scheduler, processes and a page-table switch per request
  boundary.
* **k10-metal "used" memory** counts frames owned by the kernel heap, slots, stacks and the NIC, plus the image. The
  heap never returns small-object pages to the frame allocator (bounded by the peak).
