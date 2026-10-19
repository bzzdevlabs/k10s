# k10-metal — measurements

**Host:** Intel i7-7820HQ (4 cores / 8 threads), Linux 6.1.0-53-amd64, QEMU 7.2.22 with KVM (`-cpu host`), one
vCPU. The guest's NIC is `virtio-net-pci,disable-modern=on` on QEMU user networking (slirp), reached through a
`hostfwd`. The client is `bench/bin/loadgen` on the same host.

**Noisy host.** Other agents were compiling and running tests throughout: load average 4–8, with peaks of 11.
Every figure below is therefore one of:
* the median of interleaved repetitions (boot: 15 runs per firmware; HTTP: 3 runs × 5 s per case);
* a per-request CPU cost, which is less sensitive to contention than throughput.

Raw data is in `out/results/*.json`. Reproduce with `./build.sh && tools/linux-compare.sh && tools/measure.py`.

## Summary

| | k10-metal | Linux guest (same QEMU/slirp setup) |
|---|---|---|
| Image | **440 KB** ELF (492 KB in memory incl. .bss) + 2.8 KB bundle (14 services) | 8.2 MB bzImage + 0.9 MB initramfs |
| QEMU start → "ready" (median / p10) | **95 ms / 76 ms** (SeaBIOS); qboot 98 / 66 ms | 1,360 ms (boot only) / 1,435 ms (HTTP) |
| …of which inside the kernel (entry → ready) | **1.7–2.5 ms** | ≈ 1.25 s |
| QEMU start → first HTTP 200 | **≈ 2 ms after ready** (median 96 ms) | 1,437 ms |
| Minimum `-m` that boots *and* serves under load | **3M** (2M boots but cannot hold the test set) | 88M (80M: "System is deadlocked on memory") |
| Memory used after boot (no services) | **1.15 MiB** (0.48 MiB image + 0.67 MiB, of which 0.52 MiB NIC rings/buffers) | — |
| Per deployed service / per instance | **1.7 KB / 69.8 KB** (64 KiB slot + 4 KiB tail + ~170 B) | — |
| `hello`, 1 connection: p50 latency | **63 µs** | 102 µs |
| `hello`, 64 connections: throughput | **54.3k rps** (guest 31% busy; QEMU main thread is the bottleneck) | 31.4k rps |
| Guest (vCPU thread) CPU per request, 64 conns | **8.3 µs** | 30.9 µs |
| QEMU main thread (slirp + virtio emulation) per request | 15.6 µs | 30.8 µs |

The "Linux guest" is the host's own Debian kernel, booted by QEMU's Linux loader (no Linux in k10-metal's path, of
course). Its initramfs holds a static Rust `/init` (tools/linux-guest). In boot mode `/init` prints a line and
powers off. In HTTP mode it loads virtio-net, configures 10.0.2.15 and serves "hello" with a thread per
connection. A distro kernel probes much more hardware than a custom minimal one would, so read the Linux boot time
and RAM as "stock Linux", not "best-case Linux".

## 1. Kernel image

| | bytes |
|---|---|
| `out/k10-metal.elf` (ELF file, with symbols) | 440,072 |
| loaded from file (PT_LOAD filesz) | 377,994 |
| in memory incl. .bss | 491,946 |
| .text / .rodata / .data / .bss | 337,699 / 38,615 / 1,480 / 111,336 |

.bss holds the boot stack (64 KiB), the boot page tables (24 KiB) and the double-fault IST stack (16 KiB). The text
covers the TIR validator and interpreter, smoltcp, the k10-mesh manifest/bundle/placement code, `core::fmt` and the
kernel itself; with fat LTO, crate boundaries largely disappear.

TIR modules are tiny: from 24 bytes (`trap`) and 27 bytes (`spin`) to 311 bytes (`chain`). The whole 14-service
bundle (manifest + blobs) is 2,801 bytes.

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
| boots / serves | yes / no | **yes / yes (55.9k rps)** | yes / yes | yes / yes | yes / yes | yes / yes | yes / yes |

At 2M, the RAM above 1 MiB minus the 0.48 MiB image leaves ~0.4 MiB, too little for four 68 KiB instances plus
socket buffers. Below 12 MiB the kernel switches to small socket and NIC buffers (16 × 2 KiB per ring). A 3 MiB
guest still serves 56k rps, because steady load needs only one warm instance per service and a single 32 KiB
coroutine stack: on one core, a request that completes within its time slice hands both straight to the next one.

Linux needs **88M** to reach `/init` (boot-only and HTTP alike).

**Usage** (`/_k10s/status`, frames by tag; 64M guest):

| state | used (dynamic) | + kernel image | notes |
|---|---|---|---|
| booted, no services | 688 KiB | 1.15 MiB | NIC rings + 2×128 buffers 536 KiB, heap 148 KiB (4 listening sockets × 24 KiB buffers…) |
| 13 test services deployed | 728 KiB | 1.19 MiB | +3.1 KiB per service (prepared code + metadata) |
| after one request to each of 11 services | 1.54 MiB | 2.02 MiB | 11 instances (68 KiB slots) + 1 coroutine stack (32 KiB) |
| 1,000 `hello` services, 1 warm instance each | 68.8 MiB | | **69,796 B per instance**, 1,692 B per deployed service |

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

## 6. What was not measured / caveats

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
