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

## Measured on one laptop (preliminary)

i7-7820HQ (4 cores / 8 threads), all three systems on the same machine. The machine was shared with other jobs
during these runs; final numbers will come from `bench/run-final.sh` on a quiet machine. Read
[bench/results/COMPARISON.md](bench/results/COMPARISON.md) (and its caveats) before quoting.

| | Kubernetes (kind, 1 node) | k10s (1 node) |
|---|---|---|
| Idle platform memory (working set) | 579 MiB | 3.3 MiB |
| Idle CPU | 13.6 % of a core | 0.04 % |
| Memory per echo replica (N = 100) | 10.4 MiB | ~10 KiB |
| Deploy one service → first HTTP 200 | 1.85 s | 15 ms |
| Start 100 replicas/services → all serving | 18.3 s | 23 ms |
| Node dies → its services answer again | ~350 s (pod eviction) | 5–13 ms (failover routing), re-placed ~1 s |
| Echo artifact | 2.4 MB container image | 157 B module |
| Node software | kubelet + containerd + runc 120 MB, plus control plane | one 1.6 MB static binary (node, JIT, compiler, CLI) |

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
