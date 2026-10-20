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
