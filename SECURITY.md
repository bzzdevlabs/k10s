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
  length); no `syscall`/`int`/`rdtsc`/`cpuid`/privileged instructions. It runs over random modules, `br_table`
  modules, the Ten examples and every red-team module, and rejects hand-made violations. The full rule list is at the
  top of `tests/xdis/sfi.rs`. It could become a load-time check (decode with an in-tree length decoder, verify,
  then seal the code) at the cost of a disassembler in the TCB-adjacent path.

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
- CPU side channels (Spectre-class) between workloads sharing a node. Workloads on one node are assumed to belong to
  one organization; use separate nodes (separate machines or VMs) per mutually distrustful tenant.
- **Gossip and peer links are not authenticated or encrypted yet.** Run clusters only on trusted networks, and do not
  expose the admin API (`/_k10s/*`) to untrusted clients.
