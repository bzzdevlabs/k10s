# Security policy

## Reporting a vulnerability
Please **do not open a public issue**. Use GitHub's private vulnerability reporting on this repository
(Security tab → "Report a vulnerability"). Include a minimal reproduction (ideally a TIR module or Ten program and
the node configuration) and the commit you tested. We aim to acknowledge reports within a week.

## Threat model (what k10s protects today)
k10s isolates workloads with **software fault isolation**: modules are validated (`crates/k10-ir`) and compiled
(`crates/k10-jit`) so that every memory access stays inside the instance's slot, control flow stays inside generated
code, and the only exits are the host functions of `docs/HOSTAPI.md`.

In scope — please report:
- Any module the validator accepts that reads or writes outside its slot, jumps outside its code, calls a host
  function it did not import, or corrupts the runtime (registers, stacks).
- Any way for one workload to crash, hang or starve the node or other workloads (resource exhaustion through host
  functions, ingress, the admin API or the peer protocol).
- Any way for a service to `call` a service its manifest `calls` allowlist does not grant, or to tell such a
  service from one that does not exist.
- Memory-safety bugs in the node runtime, the mesh, or the bare-metal kernel.

## Defense in depth
SFI is the isolation boundary; its trusted base is the validator, the code generator, the x86-64 encoder, the
bulk-memory helpers, the host API and the runtime's `unsafe` code. Two further layers bound the damage of a bug there.

**1. Machine-code assurance (tests, `crates/k10-jit/tests/xdis*`; skipped without `/usr/bin/objdump`).**
- `xdis_encoder`: differential fuzzing of the encoder against GNU objdump (random forms, all registers, every
  addressing special case, immediate/displacement boundaries); seeded, `K10_FUZZ_LONG=1` for 2 M instructions.
- `xdis_sfi`: a VeriWasm-style verifier, independent of the code generator, re-checks the JIT-ABI invariants on the
  decoded code of every function: guest accesses only `[r15 + (x & r13)]` or `[r15 + c]` with `c < 2^min_mem_log2`;
  frame accesses inside the checked frame, never the return address; vmctx read-only; r13/r14/r15 never written;
  entry stack check before `sub rsp`; direct branches inside the function or to prelude trap stubs; calls only to
  functions, host functions (`[host_fns + 8k]`, `k < imports`) and helpers; indirect jumps only through a bounded
  `br_table` (bound proven by `cmp`+`jae`, a `cmp`+`cmov` clamp or an `sbb`/`and` mask; it must equal the table
  length); no `syscall`/`int`/`rdtsc`/`cpuid`/privileged instructions. It knows each `--jit-indirect` mode (below):
  indirect calls and table jumps must have that mode's exact form (`lfence` before `call/jmp r11`, or the retpoline
  thunk, reached only with `r11` loaded from a host/helper slot or a table), the thunk must be byte-exact in the
  prelude in retpoline mode only, and preludes must be identical per mode. It runs over random modules, `br_table`
  modules (trees and tables) and the Ten examples in every mode and lowering, and every red-team module, and rejects
  hand-made violations and mutated real code. The full rule list is at the top of `tests/xdis/sfi.rs`. It could
  become a load-time check (decode with an in-tree length decoder, verify, then seal the code) at the cost of a
  disassembler in the TCB-adjacent path.

**2. Process sandbox of `k10s node` (Linux, `--sandbox on|log|off`, default `on`, or `$K10S_SANDBOX`).**
- At start: inherited descriptors ≥ 3 are closed, `PR_SET_NO_NEW_PRIVS` is set.
- Landlock (best effort, logged when the kernel lacks it), applied before the first thread: files are limited to the
  data directory (read-write, no execution, no special files) and a few read-only `/proc`/`/sys` entries; nothing can
  be executed. With Landlock ABI ≥ 4 (Linux 6.7) TCP `bind` is limited to the node's own ports (when they are fixed),
  with ABI ≥ 6 (Linux 6.12) signals and abstract UNIX sockets are scoped to the node.
- seccomp-bpf on every thread (`SECCOMP_FILTER_FLAG_TSYNC`) once the node runs: an allowlist of 83 syscalls
  (`SYSCALLS` in `crates/k10-node/src/sandbox.rs`); anything else kills the process (`SIGSYS`). Notably denied:
  `execve`, `fork`/non-thread `clone`, namespaces, `ptrace`, `process_vm_*`, `mount`, `bpf`, `perf_event_open`,
  `io_uring`, `keyctl`, modules, `personality`, `bind`/`listen` (no new ports), non-IP sockets, executable `mmap`
  and writable+executable `mprotect`, x32 and i386 syscalls. Hot-path syscalls take no argument checks, so the
  kernel's seccomp action cache serves them (no measurable cost; see docs/NODE.md).
- `log` mode is a dry run: no Landlock, seccomp violations are logged by the kernel (`journalctl -k | grep
  type=1326`) instead of killing. `off` disables everything.
- Limits: W^X is checked per `mmap`/`mprotect` call (a JIT needs `mprotect(PROT_EXEC)`, so RW→RX of one mapping and
  memfd double mapping stay possible); outbound TCP `connect` is not restricted (peers' ports are not known in
  advance); on kernels booted with `spec_store_bypass_disable=seccomp` the filter also turns on SSBD for the node.

**Running a node.** Start it as an unprivileged user, or as root with `--user k10s` (listeners are bound first, then
real/effective/saved ids drop to the user; the data directory must belong to that user). A systemd unit adds its own
layer:
```ini
[Service]
ExecStart=/usr/local/bin/k10s node --data /var/lib/k10s --http 0.0.0.0:8080
User=k10s
StateDirectory=k10s
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
PrivateDevices=yes
ProtectKernelTunables=yes
ProtectKernelModules=yes
ProtectControlGroups=yes
RestrictAddressFamilies=AF_INET AF_INET6
RestrictNamespaces=yes
LockPersonality=yes
# The JIT maps code RW then RX: leave MemoryDenyWriteExecute off.
CapabilityBoundingSet=
# For a port < 1024 instead: CapabilityBoundingSet=CAP_NET_BIND_SERVICE and AmbientCapabilities=CAP_NET_BIND_SERVICE
LimitNOFILE=65536
```
On bare metal (`metal/`) there is no OS layer: hardware NX and W^X for the kernel and JIT code are the only extra
barrier (metal/README.md), so the machine-code verification matters most there.

Out of scope by design (documented in `docs/DESIGN.md`):
- CPU side channels (Spectre-class) between workloads sharing a node, beyond the mitigations listed under "CPU side
  channels" below. Workloads on one node are assumed to belong to one organization; use separate nodes (separate
  machines or VMs) per mutually distrustful tenant. A cheap bypass of the listed mitigations is in scope.
- **Gossip and peer links are not authenticated or encrypted yet.** Run clusters only on trusted networks, and do not
  expose the admin API (`/_k10s/*`) to untrusted clients.

## CPU side channels (Spectre-class)
All instances on a node share one address space, the caches and the branch predictors, and run code their authors
chose: the textbook setting for Spectre. k10s closes the cheap gaps; a shared address space cannot become a hardware
boundary, so **one node (machine or VM) per mutually distrustful tenant remains the isolation boundary** for
side channels.

Mitigated by default (cost: JIT code without `br_table` is byte-for-byte unchanged; a host function gains a few
register instructions, the clock functions ~2-3 ns per call):
- **Guest memory accesses** (Spectre v1): every address is masked by data flow (`base + ((addr + off) & (S - 1))`),
  not by a bounds-check branch, so a mispredicted branch cannot steer a guest load outside its slot.
- **`br_table`** (Spectre v1): small tables (≤ 8 target runs) compile to compare trees — no table load, no indirect
  jump; jump tables clamp the index with `cmp` + `cmovae` before the table load, so a mispredicted path can neither
  read past the table nor jump to a target derived from foreign memory (sequences in docs/JIT-ABI.md).
- **Host functions and bulk-memory helpers** (Spectre v1), hosted node and k10-metal: every guest `(ptr, len)` goes
  through `k10_ir::sem::check_span`, which also zeroes the length with a branch-free mask (`cmp`/`sbb`) behind a
  mispredicted check; offsets and lengths into host buffers (request body, path, config, call response, KV values)
  and into fixed host arrays (the KV key buffer) are clamped with `cmp`/`cmova` (`k10_ir::nospec`, inline assembly
  the optimizer cannot turn back into branches — checked in the release assembly); the interpreter's `br_table`
  likewise. Host-function, helper and import indices are compile-time constants: there is no guest-indexed host
  table and no guest-visible handle.
- **Timer**: `now_ns` and `time_ms` are coarsened to **100 µs** (`time_ms`: 1 ms) with keyed random edges ("fuzzy
  time", as browsers do for `performance.now()`), and stay monotonic. 100 µs is Chrome's resolution for pages that
  are not cross-origin isolated: about a thousand cache misses, so a single probe cannot be timed, and the randomized
  edges defeat clock-edge amplification (spinning until the value changes, then counting iterations), while
  timeouts, rate limits and latency logging keep useful precision. Trusted single-tenant nodes restore full
  resolution with `k10s node --timer-res-us 0` (k10-metal: `k10.timer_res_us=0`); any other value sets the step.

Opt-in (measured on an i7-7820HQ: a host call goes from ~3-6 ns to ~8-12 ns with `lfence`, ~13-15 ns with a
retpoline; a jump-table dispatch from ~5 ns to ~8 ns / ~16 ns when predictable):
- **Spectre v2 / branch target injection on generated code**: by default a workload can train the branch target
  buffer so that another workload's indirect branch on the same core speculatively runs a chosen target with the
  victim's registers. `k10s node --jit-indirect retpoline` (k10-metal: `k10.jit_indirect=retpoline`) routes every
  indirect branch the JIT emits (jump tables, host-function and helper calls, the interrupt hook, the entry
  trampoline) through a retpoline; `lfence` serializes before each of them instead (cheaper, but it narrows the
  window rather than closing it). See docs/JIT-ABI.md for the sequences.

Not mitigated (known gaps):
- **Spectre v2 and branch history injection in the runtime itself**: the node and the metal kernel are ordinary Rust
  compiled without retpolines (`match` jump tables, trait-object calls, the host-function dispatch). On k10-metal the
  kernel and the workloads share one privilege level, so IBRS/eIBRS do not separate them either.
- **Return-stack speculation** (ret2spec, Retbleed-class RSB underflow to the branch target buffer): coroutine
  switches between workloads neither stuff nor flush the return stack buffer.
- **Speculative store bypass** (Spectre v4): not disabled (SSBD costs throughput on store-heavy code; Linux can turn
  it on per process with `prctl(PR_SET_SPECULATION_CTRL)` or for the system with `spec_store_bypass_disable=on`).
- **Other microarchitectural channels**: shared caches (Prime+Probe across cores and SMT siblings), execution-port
  and TLB contention, DRAM row buffers, frequency/power; MDS-class buffer leaks on affected CPUs (there is no kernel
  transition between workloads at which buffers would be cleared).
- **Implicit timers**: a service with concurrent instances, or cooperating services, can build a counting timer
  through the KV store or service calls, and external clients can time responses; the coarse clock raises the cost
  of a measurement, it does not make one impossible.
