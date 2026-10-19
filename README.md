# k10s

**Run services on a fleet of machines for a fraction of what Kubernetes costs — by removing the layers, not
tuning them.**

Kubernetes isolates workloads with *processes* (and VMs), so it needs images, cgroups, namespaces, overlay networks,
kube-proxy, sidecars, probes, and a control plane to manage it all. k10s isolates workloads with the **compiler**:

- Services are written in **Ten** (a small C-like language) or any language targeting **TIR**, a compact IR.
- Each node validates TIR and compiles it to x86-64 with **software fault isolation**: every memory access is masked
  into the instance's own slot. Thousands of instances share **one address space**; switching between them is a
  function call. No processes, containers, cgroups, namespaces or PIDs.
- An instance is a memory slot plus shared machine code: it costs **kilobytes** and starts in **microseconds**, so
  scale-to-zero is the default.
- Calls between services on a node are function calls; across nodes, one multiplexed connection per peer.
- **No control plane**: desired state spreads by gossip, membership is SWIM, and placement is a pure function every
  node computes on its own. Self-healing falls out of membership changes.
- The same runtime also boots **as its own kernel** (`metal/`), with no Linux underneath.

Design rationale: [docs/DESIGN.md](docs/DESIGN.md) · evidence from the literature: [docs/research.md](docs/research.md)

## Measured on one laptop

i7-7820HQ (4 cores / 8 threads); Kubernetes v1.34 (kind), the same Go services as bare processes, and k10s, each
suite started on an idle machine by `bench/run-final.sh`. Details and caveats:
[bench/results/COMPARISON.md](bench/results/COMPARISON.md) · summary: [docs/REPORT.md](docs/REPORT.md).

| | Kubernetes (kind, 1 node) | k10s (1 node) |
|---|---|---|
| Idle platform memory (working set) | 583 MiB | 1.3 MiB |
| Idle CPU | 21.9 % of a core | 0.10 % |
| Memory per echo replica (N = 100) | 11.0 MiB | 10.2 KiB |
| Deploy one service → first HTTP 200 | 1.17 s | 24 ms |
| 100 replicas/services → all serving | 17.6 s | 28 ms |
| echo at 64 connections | 87 k req/s | 180 k req/s |
| Service-to-service hop at 64 connections | 25.6 k req/s | 171 k req/s |
| CPU per echo request | 47 µs | 20 µs |
| Hung node → workloads replaced elsewhere (3 nodes) | 353 s | 1.1 s |
| Rolling update | 3.7 s, 11 failed probes | 0.022 s, 0 failed |
| Echo artifact | 2.4 MB container image | 157 B module |
| Node software | kubelet + containerd + runc 120 MB, plus control plane | one 1.7 MB static binary (node, JIT, compiler, CLI) |

k10s also outruns the same services run as bare Linux processes (echo 180 k vs 109 k req/s). It is slower on
CPU-bound code: Ten's SHA-256 takes 2.5× as long as Go's hand-written assembly (1.1–1.6× vs portable Go).

Bare metal (`metal/`, QEMU/KVM): a 694 KB kernel, ready ~3 ms after kernel entry, serving HTTP in 3 MiB of RAM;
the same kernel joins a cluster of Linux-hosted nodes.

## Quick start

```bash
cargo build --release -p k10s
```
```bash
./target/release/k10s node --http 127.0.0.1:8080 --data /tmp/k10s-data
```
In another terminal:
```bash
./target/release/k10s apply examples/app.k10s --node 127.0.0.1:8080
```
```bash
curl 127.0.0.1:8080/hello/
```
```bash
curl '127.0.0.1:8080/compute/?n=1000'
```
Try a single service without a node: `k10s run examples/echo.ten --method POST --body hi`.
A cluster: start more nodes with `--gossip`, `--rpc` and `--join HOST:PORT` (see [docs/NODE.md](docs/NODE.md)),
then `apply` to any of them.

## Repository

| Path | What |
|---|---|
| [docs/](docs/) | design, specs ([TIR](docs/TIR.md), [host API](docs/HOSTAPI.md), [JIT ABI](docs/JIT-ABI.md), [Ten](docs/TEN.md)), [node runtime](docs/NODE.md), [status](docs/STATUS.md) |
| [crates/k10-ir](crates/k10-ir) | TIR: codec, validator, reference interpreter, random program generator |
| [crates/k10-lang](crates/k10-lang) | Ten compiler and prelude |
| [crates/k10-jit](crates/k10-jit) | single-pass TIR → x86-64 compiler (the isolation boundary) |
| [crates/k10-mesh](crates/k10-mesh) | gossip membership, manifests, bundles, placement, wire protocol |
| [crates/k10-node](crates/k10-node), [crates/k10s](crates/k10s) | node runtime and the `k10s` binary |
| [metal/](metal/) | the node as a bare-metal kernel (PVH, virtio-net, SMP, JIT, cluster member) |
| [examples/](examples/) | Ten services: hello, echo, compute (SHA-256), chain, counter, todo, nbody, sort |
| [bench/](bench/) | Kubernetes / bare-process / k10s benchmark harness and results |

## How it is tested
- Differential testing of the JIT against the reference interpreter on randomly generated programs (1 M modules,
  zero mismatches), with canary regions proving no write ever leaves an instance's slot; a red-team suite.
- Unit, integration and multi-process cluster tests (node kill, graceful leave, re-apply under load, restart).
- Chaos, soak and abuse campaigns (see [docs/STATUS.md](docs/STATUS.md)).

## Status and limits
Research prototype. Not yet: authentication/encryption of gossip and peer links, replicated storage (KV is
node-local), consensus for singletons, hostile multi-tenancy on one node (Spectre-class side channels are out of
scope by design), architectures other than x86-64 for the JIT (others run on the interpreter). See
[docs/STATUS.md](docs/STATUS.md) and [SECURITY.md](SECURITY.md).

## License
Apache-2.0 — see [LICENSE](LICENSE) and [NOTICE](NOTICE). Contributions welcome: [CONTRIBUTING.md](CONTRIBUTING.md),
[CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md).
