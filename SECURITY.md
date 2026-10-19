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
