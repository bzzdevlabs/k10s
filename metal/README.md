# k10-metal — the k10s node runtime as its own kernel

k10-metal boots the k10s node runtime directly on a (virtual) x86-64 machine, with no Linux underneath
(docs/DESIGN.md §2.9). Workloads are TIR modules, isolated by construction: masked memory accesses, structured
control flow, and a closed host API. So the kernel needs no processes, no system calls, no user/kernel boundary,
and no per-workload page tables. It runs one identity-mapped address space on one or more cores (`-smp N`, up to 8),
with a timer, a NIC and a serial console.

It boots in QEMU/KVM with `-kernel`, deploys a `k10_mesh::Bundle` (manifest + TIR blobs) and serves HTTP/1.1 on
port 80 with the same routing and host API as the hosted node (docs/NODE.md, docs/HOSTAPI.md). Services run as
machine code from `k10-jit` (the default) or on the `k10-ir` interpreter. With `k10.join=…` the guest joins a k10s
cluster as a full member next to Linux-hosted `k10s node` processes: gossip membership, desired state fetched from
peers, the same placement, forwarding both ways.
Measurements: [RESULTS.md](RESULTS.md).

## Quick start

```sh
./build.sh                 # out/k10-metal.elf (kernel), out/bundle.k10b (18 test services, 4 of them compiled from
                           # ../examples/*.ten); EMBED=1 also bakes the bundle in
./run.sh                   # QEMU/KVM, serial on stdout, host 127.0.0.1:8090 -> guest :80 (PORT=, MEM=, BIOS=, APPEND=)
curl localhost:8090/hello/ # -> hello from k10-metal
curl 'localhost:8090/sha/?n=1000'                # Ten SHA-256 chain (examples/compute.ten), JIT-compiled
curl 'localhost:8090/_k10s/bench/100/sha/?n=1000' # 100 invocations back to back: per-invocation CPU time (JSON)
curl localhost:8090/_k10s/status
curl --data-binary @out/bundle.k10b localhost:8090/_k10s/apply   # redeploy (manifest version must not go down)
SMP=4 ./run.sh             # four CPUs: CPU 0 runs the network, CPU-heavy requests are spread over all four
./test.sh                  # in-kernel self tests + 248 end-to-end checks: JIT, interpreter, JIT on 4 CPUs (TEST_PORT=18090)
./cluster-test.sh          # 2 hosted `k10s node` + 1 k10-metal guest: join, apply, routing, failover, rejoin
tools/linux-compare.sh     # builds the Linux comparison guest (host kernel + tiny initramfs)
tools/measure.py           # measurements -> out/results/*.json (see RESULTS.md); `engines` = JIT vs interpreter,
                           # `smp` = throughput with -smp 1/2/4
tools/cluster_measure.py   # cluster measurements with a metal member -> out/results/cluster.json
```

Kernel command line (`APPEND=` for run.sh): `k10.verbose` (boot banner, phases, apply log), `k10.selftest` (run the
in-kernel tests, then exit QEMU with status 33 on success), `k10.exit_on_ready`, `k10.spin_us=N` (busy-poll window
before halting, default 50), `k10.engine=jit|interp` (default jit), `k10.smp=N` (use at most N CPUs). Cluster
mode: `k10.join=HOST:PORT[,…]`, `k10.gossip=`, `k10.rpc=`, `k10.http=` (advertised addresses), `k10.node_id=HEX`,
`k10.probe_ms=N`, `k10.suspicion_mult=N`, `k10.mem=BYTES`, `k10.host_alias=IP` (see "Cluster mode" below).

The deployment bundle comes from QEMU's `-initrd` (PVH module 0). If there is none, the kernel uses the bundle
baked in at build time (`K10_EMBED_BUNDLE=path`). If neither exists, the node starts empty and waits for
`POST /_k10s/apply`.

## Layout

```
metal/                    its own cargo workspace (own target dir); path-depends on ../crates/k10-ir, k10-mesh, k10-jit
                          (kernel) and k10-lang (mkbundle)
  kernel/                 the kernel (x86_64-unknown-none, no_std + alloc), ~8.3k lines incl. comments and self tests
    linker.ld, build.rs   linked at 1 MiB, non-PIE (.cargo/config.toml), optional embedded bundle
    src/boot.rs           PVH ELF note + 32-bit entry → page tables → long mode → kmain
    src/arch/             port I/O, MSRs, per-CPU GDT+TSS (IST for #DF), IDT + asm entry stubs, local APIC timer
                          and IPIs, paging.rs (NX, W^X for the kernel image and JIT code, TLB generations)
    src/percpu.rs         per-CPU state through the GS base (preemption request, idle flag, load, counters)
    src/smp.rs            application processors: real-mode trampoline, INIT-SIPI-SIPI, AP entry
    src/pvh.rs            hvm_start_info: E820 map, command line, modules
    src/mem/              frame allocator (bitmap), kernel heap, instance slots
    src/time.rs           TSC clock (kvmclock / CPUID 0x15 / PIT), wall clock, preemption flag + JIT preemption word
    src/pci.rs            config space via 0xCF8/0xCFC, BAR assignment when firmware did none
    src/virtio_net.rs     legacy virtio-net driver + smoltcp Device glue
    src/net.rs            smoltcp interface, listener pool, HTTP connections, UDP + extra listeners + TCP dial
    src/cluster.rs        cluster mode: gossip over UDP, peer links over TCP, desired-state fetch, placement, routing
    src/http.rs           HTTP/1.1 framing
    src/coro.rs           coroutines (context switch in asm), stack pool
    src/runtime/          services, instances, per-CPU schedulers + dispatch, host API, admin API; engine.rs (engine seam),
                          jit.rs (k10-jit: code memory, VmCtx, host thunks, interrupt hook)
    src/selftest.rs       in-kernel tests (k10.selftest)
  mkbundle/               host tool: test services built with k10_ir::builder and compiled from Ten → Bundle
  tools/                  boottime.py, measure.py, cluster_measure.py, linux-guest (comparison /init),
                          mkinitramfs.py, linux-compare.sh
  build.sh run.sh test.sh cluster-test.sh
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

The image works with SeaBIOS (QEMU's default) and with qboot (`BIOS=/usr/share/qemu/qboot.rom`, faster). qboot
leaves PCI BARs unprogrammed, so the PCI code assigns them itself.

**Memory.** A bitmap frame allocator covers all E820 RAM above 1 MiB. It excludes the kernel image and the boot
module until the bundle has been decoded. Each allocation is tagged, so `/_k10s/status` reports where memory goes:
heap small/large, slots, stacks, net, kernel. There is no fixed heap region. The global allocator takes
power-of-two size classes up to 2 KiB from 4 KiB frames, and larger blocks directly from the frame allocator. So
the heap and the instance slots share all of RAM. An instance slot is `2^k + 4096` bytes of contiguous frames.
Slots are cached per size class for reuse. Instance creation, coroutine stacks and socket buffers are refused when
they would eat into a heap reserve (max(256 KiB, RAM/16)), and requests then queue instead.

**Page permissions (arch/paging.rs).** Right after the frame allocator starts, EFER.NXE is enabled and every 2 MiB
page of the identity map becomes non-executable. The first 2 MiB (low memory and the kernel image) are split into
4 KiB pages: `.text` read+execute, `.rodata` read-only, the rest read+write, page 0 not present (null guard). JIT
code gets its own frames (tag `code`): written through the writable, non-executable identity mapping, then
*sealed* read+execute (the 2 MiB page around them is split once, its page table kept) and never written again;
when the last service using the code is gone, the pages are unsealed and freed. So nothing is ever writable and
executable at the same time: not the kernel, not instance memory, not JIT code.

**Interrupts and time.** The IDT has handlers for every exception, which print a register dump (CR2 for page
faults) and exit QEMU. The double fault, NMI and machine-check handlers use an IST stack. The 8259 PICs are
remapped and masked; only the local APIC and MSI-X are used. The TSC frequency comes from kvmclock (exact, with no
calibration wait), else CPUID 0x15, else 10 ms of PIT. Wall time comes from the kvmclock wall clock, else the CMOS
RTC. The LAPIC timer is one-shot and tickless: TSC-deadline mode, or count mode calibrated against the TSC. It is
armed only for the next deadline: the end of the running invocation's slice or budget, a TCP timer, or a queue
timeout. When idle the CPU `hlt`s, so an idle node takes no timer interrupts.

**NIC.** PCI enumeration uses port I/O. The scan stops at the first 1AF4:1000 function, because each config read
costs two VM exits (~14 µs here). The legacy virtio-pci driver (`disable-modern=on`) negotiates MAC and
ANY_LAYOUT only, so there is no checksum or GSO offload and no mergeable buffers. It sets up 2 KiB RX and TX
buffers bound to fixed descriptors, so nothing is allocated per packet. MSI-X vector 0 is used for RX:

* Interrupts are suppressed while the kernel is busy. They are re-enabled, and the ring re-checked, just before
  `hlt`.
* TX completions are reaped lazily, without interrupts.
* Kicks are batched per main-loop iteration and skipped while the device sets `VRING_USED_F_NO_NOTIFY`.
* After activity the loop busy-polls for 50 µs before halting.

**TCP/IP and HTTP.** smoltcp uses a static 10.0.2.15/24 address with gateway 10.0.2.2 (QEMU slirp). Ingress is
processed one packet at a time (`poll_ingress_single`), and the listening sockets are topped up after every
packet. A SYN burst therefore never meets an empty backlog, without keeping a large pool of idle sockets.
HTTP/1.1 supports keep-alive, pipelining (one request in flight per connection), Content-Length bodies and
`Expect: 100-continue`. Chunked request bodies get a 411.

**Runtime.** Routing and the host API follow the hosted node:
* `/<service>/<rest>` invokes the service. `req_path` sees `/<rest>` plus the `?query`.
* `/_k10s/status` returns JSON with memory per tag, counters, network and per-service stats.
* `/_k10s/apply` takes a bundle. `/_k10s/logs/<service>` returns the last 64 `log` lines.
* `/_k10s/bench/<n>/<service>/<rest>` runs `/<rest>` on `<service>` n times back to back in one task and answers
  the per-invocation CPU time (min, median, p90, mean; TSC time while the invocation runs, host calls included,
  network excluded) — how RESULTS.md compares the engines.
* All 19 host functions of docs/HOSTAPI.md are implemented. `log` writes to the serial console and a ring buffer.
  The KV store is per service and in memory, and survives redeploys. Every `kv_*` call runs under the node lock,
  so `kv_add` and `kv_cas` are atomic across CPUs; a write that would eat into the heap reserve traps `Host(3)`. `call` runs a local service in the caller's
  coroutine, or (cluster mode) forwards to the node it is placed on.

Apply validates every module, resolves imports by name and parameter count, and checks the `handle`/`init`
exports. Single-node: it runs `k10_mesh::place` over a one-member cluster (this node, with its free memory as
capacity); cluster mode: see below. Then it swaps the service table atomically: services whose spec and placement
did not change keep their runtime (warm instances, counters), prepared code is reused by code hash, in-flight
invocations finish on the old code and queued ones move to the new version.

**SMP (smp.rs, percpu.rs).** The boot CPU reads the CPU count from QEMU's fw_cfg (`-smp N`; no ACPI parsing),
copies a 16→32→64-bit trampoline to page 0x8000 (made read+execute just for the bring-up; the APs take their index
with a `lock xadd` while paging is still off, so the page never needs to be writable for them), and broadcasts INIT +
2×STARTUP. Each AP loads the shared page tables (with NXE), gets its own stack, GDT + TSS + IST stack, the shared
IDT, its local APIC (TSC-deadline timer) and its per-CPU block (GS base), and enters `runtime::worker_main`. Four
CPUs are online ≈ 1.5 ms after the broadcast. Locks (`IrqLock`) are spinlocks that also mask interrupts on the
holding CPU; per-CPU state (preemption request, JIT preemption word, coroutine bookkeeping, scheduler queues) needs
none.
* *Dispatch*: CPU 0 owns the NIC (MSI-X targets it), TCP/IP, HTTP parsing, the cluster layer and the admin API, and
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
`HostCalls` (`call(HostFn, args, mem)` and `safepoint()`), implemented once (runtime/host.rs).
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
* *Peer links*: `k10_mesh::wire` frames over smoltcp TCP (port 7947, plus outgoing connections): `Hello` both ways,
  one link per peer pair (dial members with a larger id when they come up, others lazily; when both ends dialed,
  the connection dialed by the smaller id wins), requests multiplexed by id, write buffers flushed every main-loop
  iteration, 200 ms…5 s dial backoff, 1 s connect timeout.
* *Desired state*: a newer `DesiredState` from gossip is fetched (`GetManifest`, then the missing blobs with
  `GetBlob`, pipelined; the origin first, then the other live members; 10 s without progress → next one), checked
  with SHA-256 and kept in memory (no disk). `POST /_k10s/apply` on the guest works like on a hosted node: version
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
* *NAT*: the guest is 10.0.2.15 behind QEMU's slirp. `hostfwd` rules expose its gossip (UDP), rpc and http ports
  on the host; it advertises those host addresses (`127.0.0.1:<port>`), and every loopback address it sends to —
  hosted members, or other guests, advertising `127.0.0.1:<port>` — is reached through the slirp host alias
  10.0.2.2 (`k10.host_alias`). Replies to datagram sources work because slirp maps them back.
* `/_k10s/status` gains the hosted node's `"cluster"` object (members, desired/applied, placement, links,
  counters) and a `placement` per service.

```sh
# two hosted nodes on the host, one guest; the guest's forwarded ports are 19003 (http), 19103/udp, 19203
k10s node --http 127.0.0.1:19001 --gossip 127.0.0.1:19101 --rpc 127.0.0.1:19201 --mem 64MiB &
k10s node --http 127.0.0.1:19002 --gossip 127.0.0.1:19102 --rpc 127.0.0.1:19202 --mem 64MiB --join 127.0.0.1:19101 &
qemu-system-x86_64 … -netdev user,id=n0,hostfwd=tcp:127.0.0.1:19003-:80,hostfwd=udp:127.0.0.1:19103-:7946,hostfwd=tcp:127.0.0.1:19203-:7947 \
    -append "k10.join=127.0.0.1:19101 k10.gossip=127.0.0.1:19103 k10.rpc=127.0.0.1:19203 k10.http=127.0.0.1:19003 k10.mem=67108864"
curl --data-binary @out/bundle.k10b localhost:19001/_k10s/apply     # any member; every service answers on every ingress
```

## Dependencies and why

| crate | used by | why |
|---|---|---|
| `k10-ir` (path, no default features) | kernel, mkbundle | TIR types, decoder, validator, reference interpreter (execution engine), builder |
| `k10-jit` (path, no default features: `no_std + alloc`) | kernel | TIR → x86-64 compiler (`compile_blob`, `CodeBlob::write_to`, `BlobMeta::invoke`, `VmCtx`); the kernel provides executable memory |
| `k10-mesh` (path, no default features) | kernel, mkbundle | `Bundle`, `Manifest`, `sha256`, `place()`, gossip state machine, peer-protocol framing |
| `k10-lang` (path) | mkbundle only | compiles the Ten example services (`examples/*.ten`) into the test bundle |
| `smoltcp` 0.14 (`alloc`, `medium-ethernet`, `proto-ipv4`, `socket-tcp`, `socket-udp`) | kernel | A correct TCP (retransmission, windows, RST/TIME-WAIT, congestion control) is the one part where writing our own would be large and risky. smoltcp is the standard `no_std` stack, 0BSD-licensed and dependency-light (`heapless`, `managed`, `byteorder`, `bitflags`, `cfg-if`). |
| `libc` | tools/linux-guest only | ioctl/finit_module for the Linux comparison `/init` (not part of the kernel) |

Deliberately *not* used:
* The `x86_64` crate. The GDT, IDT, APIC and port-I/O code is about 450 lines, and the interrupt entry stubs need
  assembly anyway (`extern "x86-interrupt"` is unstable).
* An allocator crate. The heap is about 140 lines with exact per-tag accounting and grows from the same frames as
  the instance slots, so there is no fixed heap size. That is what makes 3–4 MiB guests possible.

## Limitations (v0)

* **SMP is simple**: CPU 0 does all network I/O (one NIC queue, slirp-bound anyway), dispatch is decided once per
  request (no work stealing or migration of queued work), at most 8 CPUs, CPU count from QEMU's fw_cfg (no ACPI
  MADT), xAPIC with MMIO ICR writes (a VM exit per IPI; x2APIC would be cheaper). TLB coherence is lazy
  (generation check before running JIT code) rather than shootdown IPIs.
* **No demand paging.** A slot is fully backed (64 KiB + 4 KiB for the test services). There are no CoW snapshots;
  `fresh` restores with `memcpy`.
* **KV is in memory only.** There is no virtio-blk, so it is not durable across reboots.
* **Cluster mode is volatile.** The desired state lives in memory: a rebooted guest fetches it again from its
  peers (a guest alone keeps nothing). No graceful leave (a stopped guest is detected as failed). KV is node-local,
  as on hosted nodes. Gossip and peer links are unauthenticated (trusted network only).
* **JIT code is page-granular per module** (W^X forbids appending to an executable page): a 300-byte module takes a
  4 KiB page; modules deployed several times share one copy by code hash.
* **Static IP, IPv4 only.** No DHCP or TLS.
* **Legacy virtio-pci only.** It needs `disable-modern=on`. There is no virtio-mmio, so no QEMU `microvm`.
* **Slow serial console.** Logging costs about 15 µs per byte under QEMU (one host `write()` per byte), so boot is
  quiet unless `k10.verbose` is given.
* **No guard pages on coroutine stacks.** A canary is checked at every switch instead, and the call depth is
  bounded.
* **No ACPI.** The kernel parses no ACPI tables and assumes an xAPIC at the MSR-reported base and QEMU's fw_cfg for
  the CPU count. Power-off works only through `isa-debug-exit`.

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
