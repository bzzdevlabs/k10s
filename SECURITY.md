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
- Memory-safety bugs in the node runtime, the mesh, or the bare-metal kernel.

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

Mitigated by default (no measurable cost; numbers in the commit that introduced them):
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

Opt-in (costs time on every indirect branch):
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
