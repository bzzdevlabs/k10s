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

Out of scope by design (documented in `docs/DESIGN.md`):
- CPU side channels (Spectre-class) between workloads sharing a node, beyond the mitigations listed under "CPU side
  channels" below. Workloads on one node are assumed to belong to one organization; use separate nodes (separate
  machines or VMs) per mutually distrustful tenant. A cheap bypass of the listed mitigations is in scope.

## Control plane: who may deploy (docs/NODE.md "Authentication")
- **Admin API** (`/_k10s/*`: apply, status, logs, cluster, bench): a bearer token (`--admin-token-file`; metal
  `k10.admin_token=`), compared in constant time on the request head, before any body is read. Without a token a
  hosted node serves it to loopback clients only (and refuses requests carrying `Forwarded`/`X-Forwarded-For`);
  metal refuses it. `--admin ADDR` moves it to its own listener. Open without credentials: `health`, and on hosted
  nodes `metrics` (read-only aggregate counters and service names, so scrapers need no deploy-capable secret; use
  `--admin` to keep them off the public interface).
- **Cluster** (gossip UDP, peer TCP): one shared 32-byte key (`k10s keygen`, `--cluster-key-file`, metal
  `k10.cluster_key=`); cluster mode refuses to start without it unless `--insecure-unauthenticated-cluster`.
  Gossip datagrams are sealed (XChaCha20-Poly1305) and rejected before decoding when forged, stale (clock skew bound,
  default 300 s) or replayed. Peer links run a mutual handshake on fresh nonces and are then encrypted and
  authenticated record by record (ChaCha20-Poly1305, per-direction keys, counter nonces: no injection, replay,
  reordering or reflection). A desired state can only be announced by a key holder, and its manifest and code blobs
  are still checked against its SHA-256 when fetched.

Not covered (open):
- **Every key holder is fully trusted**: a node with the cluster key can announce desired states, run code
  everywhere and read every request it carries. There is no per-node identity, no revocation (rotate the key on every
  node), and no forward secrecy (a stolen key decrypts recorded traffic).
- **Bundles are not signed**: whoever holds the admin token of any node, or the cluster key, deploys code. Signed
  bundles with an offline trust root (which root signs images and authenticates joining nodes) are an open decision
  (docs/REPORT.md §9).
- **No TLS on the admin API or service ingress**: tokens travel in clear unless the admin listener sits on a trusted
  network or behind a TLS proxy. A local reverse proxy in front of the ingress makes remote clients look like loopback
  ones: configure a token (or `--admin`) in that setup.
- **Traffic analysis**: message sizes and timing are visible; gossip datagrams leak their size, peer records their
  length.

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

## Denial of service: what bounds one workload (details in `docs/NODE.md`)
- **Memory**: every service placed on a node reserves one instance (`2^memory` + 4 KiB) of `--mem`; burst instances
  share the remainder, so a service under heavy concurrency can exhaust the shared pool but never another service's
  reservation. Applies whose reservations do not fit are rejected; cluster placement charges the same footprint.
  Per service: `max_instances`, KV quota (`--kv-max`), wait queue (10 000), host copies (16 MiB).
- **CPU**: 2 ms time slices (preemption at JIT safepoints), per-request `cpu_ms` budget, crash-loop backoff.
- **Connections**: node-wide buffer budget (`--conn-buffer-max`), per-connection head/body limits and timeouts,
  `max_conns`; peer links have read/write caps.
- **Not isolated yet**: the shared pool, the connection buffer budget and CPU are first come, first served between
  services (no per-service share or weight); snapshots and prepared code are outside `--mem` (bounded per service
  version by the manifest); KV is bounded per service, not node-wide.
