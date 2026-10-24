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

**1. Machine-code assurance (tests, `crates/k10-jit/tests/xdis*`; need GNU objdump, `$K10_OBJDUMP` else
`/usr/bin/objdump`: skipped without it, failed under `CI` or `K10_REQUIRE_OBJDUMP`).**
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

**2. Process sandbox of `k10s node` (Linux x86-64, AArch64, i686 and armv7, `--sandbox on|seccomp-only|log|off`, default `on`, or
`$K10S_SANDBOX`).**
- At start: inherited descriptors ≥ 3 are closed, `PR_SET_NO_NEW_PRIVS` is set. The listeners are bound and the
  secrets (admin key, cluster keys) read; then, still single-threaded: `--user` (groups, gid, uid; root without
  `--user` is refused), every capability dropped and checked, Landlock, seccomp. Only then are the stores opened, the
  last bundle reloaded and the threads started, all inside the sandbox.
- Landlock (required with `on`: the node refuses to start without it; `seccomp-only` runs without it): files are
  limited to the data directory (read-write, no execution, no special files or symlinks) and a few read-only `/sys`,
  `/proc/sys` entries and the binary; `/proc/self` is not reachable (its statistics files are opened before);
  nothing can be executed. With ABI ≥ 6 (Linux 6.12) signals and abstract UNIX sockets are scoped to the node.
- seccomp-bpf on the single thread, inherited by every thread created afterwards: an allowlist of 98 syscalls on
  x86-64, 88 on AArch64, which has only their `*at`/`p*` forms, 106 on i686 and armv7, whose musl also uses the
  64-bit-offset, 32-bit-id and time64 forms next to the time32 ones (`SYSCALLS` in
  `crates/k10-node/src/sandbox/seccomp.rs`); anything else kills the process (`SIGSYS`). Notably
  denied: `execve`, `fork`/non-thread `clone`, namespaces, `ptrace`, `process_vm_*`, `mount`, `bpf`,
  `perf_event_open`, `io_uring`, `keyctl`, modules, `personality`, `bind`/`listen` (no new ports), sockets other
  than IPv4/IPv6 stream or datagram, signals to other processes, executable `mmap` and writable+executable
  `mprotect` (any executable `mprotect` on i686 and armv7, which have no JIT), x32 and i386 syscalls on x86-64, 32-bit
  ARM syscalls on AArch64, 64-bit ones from a 32-bit node on a 64-bit kernel. On i686, where musl makes every socket
  call through `socketcall` (its arguments in memory, out of the filter's reach), the node creates its sockets with
  the direct `socket` call, which the filter checks as elsewhere, and `socketcall` is limited to the operations of
  the direct calls it allows (no `socket`, `bind`, `listen` or `socketpair`). Hot-path syscalls take no argument checks, so the kernel's seccomp action cache
  serves them (no measurable cost; see docs/NODE.md).
- `log` mode helps find a missing syscall: no Landlock, calls the filter would kill are logged by the kernel
  (`journalctl -k | grep type=1326`) and run, while its errno rules (`clone3`, other socket types, `prctl`, `ioctl`,
  …) apply as in `on`. `off` disables Landlock and seccomp (`--user` still applies).
- Limits: W^X is checked per `mmap`/`mprotect` call (a JIT needs `mprotect(PROT_EXEC)`, so RW→RX of one mapping and
  memfd double mapping stay possible); TCP `connect` is not restricted (peers' ports are not known in advance);
  `tkill(SIGABRT)` (musl `abort()`) cannot be limited to the node's own threads; on kernels booted with
  `spec_store_bypass_disable=seccomp` the filter also turns on SSBD for the node.

**Running a node.** Start it as an unprivileged user, or as root with `--user k10s` (listeners are bound first, then
real/effective/saved ids drop to the user; the data directory must belong to that user, a missing one is created for
it). A systemd unit adds its own layer:
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

## Control plane: who may deploy (docs/NODE.md "Ingress and admin", "Cluster authentication")
- **Admin API** (`/_k10s/*`: apply, status, logs, cluster, bench): every request is signed with the node's admin
  key (HMAC-SHA256 over the node process's random audience, method, target, time, nonce and the body's SHA-256;
  hosted: `--admin-token-file`, else `DATA/admin.key` created with mode 0600; metal: fw_cfg `opt/k10s/admin_key`).
  Audience, signature, clock window (60 s) and nonce cache are checked on the request head before any body is read,
  the body against its signed hash after: the key never travels, and a captured request can be neither altered nor
  replayed, on that node, another node sharing the key, or the node after a restart. Open without credentials:
  `health`, `audience` (what requests are signed for), and `metrics` on the dedicated admin listener (`--admin ADDR`,
  which also takes the admin API off the ingress). A metal node without an admin key serves only `health`.
- **Cluster** (gossip UDP, peer TCP): a shared 32-byte key (`k10s keygen`, `--cluster-key-file`, metal fw_cfg
  `opt/k10s/cluster_key`), plus a next key while rotating (`--cluster-next-key-file`, `opt/k10s/cluster_next_key`);
  cluster mode refuses to start without one unless `--insecure-unauthenticated-cluster`. Gossip datagrams are sealed
  (XChaCha20-Poly1305, key id and sender id authenticated) and rejected before decoding when forged, stale (clock skew
  bound, default 300 s, at least 1 s) or replayed. Peer links run a mutual handshake on fresh nonces and are then
  encrypted and authenticated record by record (ChaCha20-Poly1305, per-direction keys, counter nonces: no injection,
  replay, reordering or reflection); nothing is served on a link before the peer has proved it holds the key, and
  unauthenticated input is capped (one `Auth` frame, then one 256-byte record; 128 handshaking links at most, 16 on
  metal). A desired state can only be announced by a key holder, and its manifest and code blobs are still checked
  against their SHA-256 when fetched. Key material is zeroized when dropped; metal reads secrets from fw_cfg, never
  from the kernel command line, and fails a link rather than use a weak nonce when RDRAND fails.

Not covered (open):
- **Every key holder is fully trusted**: a node with the cluster key can announce desired states, run code
  everywhere and read every request it carries. There is no per-node identity, no revocation (rotate the key on every
  node, docs/NODE.md "Key rotation"), and no forward secrecy (a stolen key decrypts recorded traffic).
- **Bundles are not signed**: whoever holds the admin key of any node, or the cluster key, deploys code. Signed
  bundles with an offline trust root (which root signs images and authenticates joining nodes) are an open decision
  (docs/REPORT.md §9).
- **No TLS on the admin API or service ingress**: signed admin requests cannot be forged or replayed, but their
  contents (bundles, status, logs) travel in clear unless the admin listener sits on a trusted network or behind a
  TLS proxy.
- **Traffic analysis**: message sizes and timing are visible; gossip datagrams leak their size, peer records their
  length.

## CPU side channels (Spectre-class)
All instances on a node share one address space, the caches and the branch predictors, and run code their authors
chose: the textbook setting for Spectre. k10s closes the cheap gaps; a shared address space cannot become a hardware
boundary, so **one node (machine or VM) per mutually distrustful tenant remains the isolation boundary** for
side channels.

Mitigated by default (cost: a few register instructions per host function and per `br_table`; measurements in
bench/README.md):
- **Guest memory accesses** (Spectre v1): every address is masked by data flow (`base + ((addr + off) & (S - 1))`),
  not by a bounds-check branch, so a mispredicted branch cannot steer a guest load outside its slot.
- **`br_table`** (Spectre v1): small tables (≤ 8 target runs) compile to compare trees — no table load, no indirect
  jump; jump tables clamp the index with `cmp` + `cmovae` before the table load, so a mispredicted path can neither
  read past the table nor jump to a target derived from foreign memory (sequences in docs/JIT-ABI.md).
- **Host functions, bulk-memory helpers, the interpreter** (Spectre v1), hosted node and k10-metal alike (one
  implementation, `k10_host`): every guest `(ptr, len)` goes through `k10_nospec::span`, whose length is zeroed by
  a branch-free mask behind a mispredicted check; offsets and lengths into host buffers (request body, path, config,
  call response, KV values) and fixed host arrays (the KV key buffer) are clamped with `k10_nospec::min`. The clamps
  are inline assembly the optimizer can neither fold nor turn into branches: `cmp`/`sbb`, `cmp`/`cmova` on x86-64,
  `cmp`/`csel` + `csdb` on aarch64; on other architectures (`k10_nospec::GUARANTEED = false`) they are plain code
  without that guarantee. Host-function, helper and import indices are compile-time constants: there is no
  guest-indexed host table and no guest-visible handle.
- **Timer**: `now_ns` and `time_ms` move in steps of **100 µs** (`time_ms`: 1 ms) whose edges fall at a
  SipHash-keyed point of each interval (key from `getrandom`, k10-metal: RDRAND, required), monotonic and at most one
  step ahead. 100 µs is Chrome's resolution for pages without cross-origin isolation, about a thousand cache misses:
  one probe cannot be timed. The fuzzy edges raise the cost of clock-edge amplification (spinning to an edge, then
  counting iterations) — each interval adds noise of about `res/√6` — but averaging many measurements still works.
  Trusted single-tenant nodes restore full resolution with `k10s node --timer-res-us 0` (k10-metal:
  `k10.timer_res_us=0`); any other value sets the step.

Opt-in:
- **Spectre v2 / branch target injection on generated code**: by default a workload can train the branch target
  buffer so that another workload's indirect branch on the same core speculatively runs a chosen target with the
  victim's registers. `k10s node --jit-indirect retpoline` (k10-metal: `k10.jit_indirect=retpoline`) routes every
  indirect branch the JIT emits (jump tables, host-function and helper calls, the interrupt hook, the entry
  trampoline) through a retpoline; `lfence` serializes before each of them instead (cheaper, narrows the window
  rather than closing it). Caveats: retpolines do not stop return-stack attacks on AMD Zen 1/2 (Branch Type
  Confusion, Retbleed) or Zen 3/4 (SRSO, "Inception"); AMD-SB-1036 found `lfence; jmp` insufficient on some AMD
  cores; a retpoline's `ret` to a non-call site is incompatible with user-space CET shadow stacks. Sequences in
  docs/JIT-ABI.md.

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
  reservation. Operator applies whose reservations do not fit are rejected (reloaded and cluster states run
  over-committed, reported); placement charges the same footprint.
  Per service: `max_instances`, KV quota (`--kv-max`), wait queue (10 000), host copies (16 MiB).
- **CPU**: 2 ms time slices (preemption at JIT safepoints), per-request `cpu_ms` budget, crash-loop backoff.
- **Connections**: node-wide buffer budget (`--conn-buffer-max`), per-connection head/body limits and timeouts,
  `max_conns`; each peer link is served at most 1 MiB (bytes read plus replies queued) per mesh-loop iteration, so
  one busy peer cannot starve the others, and its unsent bytes are capped.
- **Not isolated yet**: the shared pool, the connection buffer budget and CPU are first come, first served between
  services (no per-service share or weight); snapshots and prepared code are outside `--mem` (bounded per service
  version by the manifest); KV is bounded per service, not node-wide.
