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
- CPU side channels (Spectre-class) between workloads sharing a node. Workloads on one node are assumed to belong to
  one organization; use separate nodes (separate machines or VMs) per mutually distrustful tenant.
- **Gossip and peer links are not authenticated or encrypted yet.** Run clusters only on trusted networks, and do not
  expose the admin API (`/_k10s/*`) to untrusted clients.

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
