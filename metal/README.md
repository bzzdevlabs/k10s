# k10-metal — the k10s node runtime as its own kernel

k10-metal boots the k10s node runtime directly on a (virtual) x86-64 machine, with no Linux underneath
(docs/DESIGN.md §2.9). Workloads are TIR modules, isolated by construction: masked memory accesses, structured
control flow, and a closed host API. So the kernel needs no processes, no system calls, no user/kernel boundary,
and no per-workload page tables. It runs one identity-mapped address space on one core, with a timer, a NIC and a
serial console.

It boots in QEMU/KVM with `-kernel`, deploys a `k10_mesh::Bundle` (manifest + TIR blobs) and serves HTTP/1.1 on
port 80 with the same routing and host API as the hosted node (docs/NODE.md, docs/HOSTAPI.md).
Measurements: [RESULTS.md](RESULTS.md).

## Quick start

```sh
./build.sh                 # out/k10-metal.elf (kernel), out/bundle.k10b (13 test services); EMBED=1 also bakes the bundle in
./run.sh                   # QEMU/KVM, serial on stdout, host 127.0.0.1:8090 -> guest :80 (PORT=, MEM=, BIOS=, APPEND=)
curl localhost:8090/hello/ # -> hello from k10-metal
curl localhost:8090/_k10s/status
curl --data-binary @out/bundle.k10b localhost:8090/_k10s/apply   # redeploy (manifest version must not go down)
./test.sh                  # in-kernel self tests + 68 end-to-end checks (boots QEMU on TEST_PORT=18090)
tools/linux-compare.sh     # builds the Linux comparison guest (host kernel + tiny initramfs)
tools/measure.py           # all measurements -> out/results/*.json (see RESULTS.md)
```

Kernel command line (`APPEND=` for run.sh): `k10.verbose` (boot banner, phases, apply log), `k10.selftest` (run the
in-kernel tests, then exit QEMU with status 33 on success), `k10.exit_on_ready`, `k10.spin_us=N` (busy-poll window
before halting, default 50).

The deployment bundle comes from QEMU's `-initrd` (PVH module 0). If there is none, the kernel uses the bundle
baked in at build time (`K10_EMBED_BUNDLE=path`). If neither exists, the node starts empty and waits for
`POST /_k10s/apply`.

## Layout

```
metal/                    its own cargo workspace (own target dir); path-depends on ../crates/k10-ir and k10-mesh
  kernel/                 the kernel (x86_64-unknown-none, no_std + alloc), ~5k lines incl. comments and self tests
    linker.ld, build.rs   linked at 1 MiB, non-PIE (.cargo/config.toml), optional embedded bundle
    src/boot.rs           PVH ELF note + 32-bit entry → page tables → long mode → kmain
    src/arch/             port I/O, MSRs, GDT+TSS (IST for #DF), IDT + asm entry stubs, local APIC timer
    src/pvh.rs            hvm_start_info: E820 map, command line, modules
    src/mem/              frame allocator (bitmap), kernel heap, instance slots
    src/time.rs           TSC clock (kvmclock / CPUID 0x15 / PIT), wall clock, preemption flag
    src/pci.rs            config space via 0xCF8/0xCFC, BAR assignment when firmware did none
    src/virtio_net.rs     legacy virtio-net driver + smoltcp Device glue
    src/net.rs            smoltcp interface, listener pool, HTTP connections
    src/http.rs           HTTP/1.1 framing
    src/coro.rs           coroutines (context switch in asm), stack pool
    src/runtime/          services, instances, scheduler, host API, engine seam, admin API
    src/selftest.rs       in-kernel tests (k10.selftest)
  mkbundle/               host tool: test services built with k10_ir::builder → Bundle
  tools/                  boottime.py, measure.py, linux-guest (comparison /init), mkinitramfs.py, linux-compare.sh
  build.sh run.sh test.sh
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
3. Enables PAE, long mode and paging. It also enables SSE, for the future JIT; the kernel itself is soft-float.
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
* All 17 host functions of docs/HOSTAPI.md are implemented. `log` writes to the serial console and a ring buffer.
  The KV store is per service and in memory, and survives redeploys. `call` runs a local service.

Apply validates every module, resolves imports by name and parameter count, and checks the `handle`/`init`
exports. It then runs `k10_mesh::place` over a one-member cluster (this node, with its free memory as capacity) and
swaps the service table atomically. In-flight invocations finish on the old code.

**Scheduling.** Each in-flight invocation runs on a 32 KiB coroutine stack. The switch is six callee-saved
registers plus `rsp`. A canary is checked at every switch; the measured high-water mark is under 3 KiB. Queued
requests start one at a time just before they run, so on one core a request that finishes within its slice hands
its warm instance and stack straight to the next one. Under steady load that means one instance per service and
one stack, whatever the number of connections.

The timer interrupt sets a preemption flag when the running invocation's 2 ms slice or its `cpu_ms` budget ends.
The interpreter polls the flag through `Host::interrupt` every 64 safepoints. On a slice end the invocation yields
to the "long" queue, which gets one slice per main-loop iteration. While a long task runs, RX interrupts are
enabled and an arriving packet also requests preemption, so network I/O and short requests wait microseconds, not
slices, behind any number of CPU hogs. On budget exhaustion the invocation traps `Interrupted`, the request gets a
500 and the instance is discarded.

A service-to-service `call` runs the callee on one of its instances inside the caller's coroutine. Nesting is
limited to depth 8. The callee gets its own budget, capped by the caller's remaining one. When the callee's
service is at `max_instances`, the caller yields until an instance is released (or -3 after 5 s). Callee traps
return -4. `fresh` services restore the post-`init` snapshot before each request.

**JIT seam.** `runtime/engine.rs` holds a `Prepared` module whose `Code` enum has one variant today,
`Interp(k10_ir::Interp)`. Every engine reaches the host API through one trait, `HostCalls` (`call(HostFn, args,
mem)` and `safepoint()`). The interpreter adapter maps `k10_ir::Host` onto it. A `Code::Jit(CompiledModule)`
variant would need three things (docs/JIT-ABI.md):
* a `VmCtx` per invocation whose `host_fns` table holds `extern "C"` thunks into the same `HostCalls` and whose
  `interrupt` hook calls `safepoint()`;
* the timer interrupt writing `usize::MAX` to `stack_limit`, instead of setting the `time::PREEMPT` flag;
* executable code memory (frames mapped RX).

Coroutines already give the JIT the stack switching it expects inside host calls and the interrupt hook.

## Dependencies and why

| crate | used by | why |
|---|---|---|
| `k10-ir` (path, no default features) | kernel, mkbundle | TIR types, decoder, validator, reference interpreter (execution engine), builder |
| `k10-mesh` (path, no default features) | kernel, mkbundle | `Bundle`, `Manifest`, `sha256`, `place()` |
| `smoltcp` 0.14 (`alloc`, `medium-ethernet`, `proto-ipv4`, `socket-tcp`) | kernel | A correct TCP (retransmission, windows, RST/TIME-WAIT, congestion control) is the one part where writing our own would be large and risky. smoltcp is the standard `no_std` stack, 0BSD-licensed and dependency-light (`heapless`, `managed`, `byteorder`, `bitflags`, `cfg-if`). |
| `libc` | tools/linux-guest only | ioctl/finit_module for the Linux comparison `/init` (not part of the kernel) |

Deliberately *not* used:
* The `x86_64` crate. The GDT, IDT, APIC and port-I/O code is about 450 lines, and the interrupt entry stubs need
  assembly anyway (`extern "x86-interrupt"` is unstable).
* An allocator crate. The heap is about 140 lines with exact per-tag accounting and grows from the same frames as
  the instance slots, so there is no fixed heap size. That is what makes 3–4 MiB guests possible.

## Limitations (v0)

* **Single core**, no SMP; the scheduler is per core by design, so SMP means one main loop per core plus
  inter-core queues.
* **Interpreter only.** No JIT yet; see the seam above.
* **No demand paging.** A slot is fully backed (64 KiB + 4 KiB for the test services). There are no CoW snapshots;
  `fresh` restores with `memcpy`.
* **KV is in memory only.** There is no virtio-blk, so it is not durable across reboots.
* **No cluster mode.** No gossip or peer links yet: placement runs over a one-node cluster, and `call` is local only.
* **Static IP, IPv4 only.** No DHCP or TLS.
* **Legacy virtio-pci only.** It needs `disable-modern=on`. There is no virtio-mmio, so no QEMU `microvm`.
* **Slow serial console.** Logging costs about 15 µs per byte under QEMU (one host `write()` per byte), so boot is
  quiet unless `k10.verbose` is given.
* **No guard pages on coroutine stacks.** A canary is checked at every switch instead, and the call depth is
  bounded.
* **No ACPI.** The kernel parses no ACPI tables and assumes an xAPIC at the MSR-reported base. Power-off works
  only through `isa-debug-exit`.

## Changes needed in other crates

None were required: `k10-ir` and `k10-mesh` work as is with `default-features = false`. Nice to have:
* `k10_ir::init_memory` taking the data segments rather than a whole `Module` (we keep a data-only `Module`).
* A way to shrink or bound `InterpStack` capacity (we use one per invocation level instead of one per instance).
* `Bundle::decode` borrowing blobs instead of copying them (the boot module is copied once, then freed).
