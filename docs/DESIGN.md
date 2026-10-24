# k10s — design from first principles

## 0. The question

We own CPUs, RAM, disks and NICs spread over machines. We want to run many services on them so that:
each service gets its share of resources, a fault in one cannot hurt another, services can talk to each other,
the outside world can reach them, and the whole thing keeps running and can be updated.

Kubernetes answers this by stacking machinery around the **hardware-protected process**. This document asks,
layer by layer, what the *minimum* machinery is between "a request arrives" and "the service's code runs" —
without assuming that processes, containers, VMs, Linux, TCP or any existing language must be part of the answer.

Constraints chosen with the user:
- Workloads are compiled for the new platform (no obligation to run existing container images).
- Trust model: same organization. A workload must never crash, corrupt or starve another. CPU side channels
  (Spectre-class) between co-located workloads are only partly mitigated — masking by data flow, branch-free clamps
  in host code, a coarse guest clock, optional retpolines (SECURITY.md); shared caches and predictors remain, so a
  hardware boundary per *tenant* (one node per tenant) is the real answer.

## 1. Where Kubernetes spends resources, and why

| Layer | Typical cost | Why it exists | Root cause |
|---|---|---|---|
| Control plane (etcd, apiserver, scheduler, controller-manager, CoreDNS) | 0.5–2 GB RAM, constant CPU for watches, JSON/protobuf, TLS | A central database + reconcilers that drive many dumb agents | Placement and desired state treated as mutable shared state that needs consensus |
| Node agents (kubelet, containerd, shims, kube-proxy, CNI daemon) | 150–500 MB RAM per node, periodic CPU | Turn API objects into processes, cgroups, namespaces, iptables rules | Workloads are processes that must be dressed up as isolated units |
| Per pod (pause + shim + process + libc + language runtime + image) | 5–100+ MB RAM, 1–5 s start, 10–1000 MB image | Process isolation + filesystem isolation + portability | The unit of isolation is an OS process with its own userspace |
| Network (veth, bridge, iptables/IPVS, VXLAN, conntrack, sidecars, JSON over HTTP) | 10s of µs to ms per hop, CPU for encap/serialization, 30–60 MB per sidecar | Processes in different namespaces can only talk via a network stack | Isolation boundary forces communication through the kernel |
| Kernel scheduling + cgroups | context switches (µs), TLB flushes, page-fault accounting | Fair CPU/memory sharing between processes | Hardware isolation means the kernel must mediate everything |
| Probes | periodic HTTP polls per pod | The orchestrator cannot see inside processes | Opaque units |
| Guest Linux per VM node | 100–300 MB RAM, duplicate scheduling under the hypervisor | General-purpose OS needed to host processes | Same |

Almost every row traces back to one decision: **isolation is enforced by the MMU and privilege rings, so the unit
of isolation is a process.** Processes are expensive (page tables, kernel stacks, syscalls, kernel scheduling,
copies across the boundary), opaque (hence probes, sidecars) and only communicate through the kernel (hence the
network machinery). Everything else is scaffolding around that choice.

## 2. Challenging each layer

### 2.1 Isolation: MMU → compiler (software fault isolation)
For fallible-but-not-hostile code, isolation only needs three guarantees: a workload cannot touch memory that is not
its own, cannot jump to code that is not its own, and cannot call anything it was not granted. A compiler can
guarantee all three:
- **Memory**: every access compiles to `base + ((addr + off) & mask)` — one `and` per access, no bounds checks,
  no guard pages, no page tables. The workload's memory is a power-of-two slot inside one shared address space.
  The mask is data flow, not a branch, so it holds even on mispredicted (speculative) paths.
- **Control flow**: the IR has only structured control flow and direct calls, so no jump can leave the code the
  platform itself generated.
- **Authority**: the only way out is a closed table of host calls. No syscalls, no filesystem, no sockets, no PIDs:
  a workload cannot even *name* anything it was not given (capability model; replaces namespaces). Other services
  are named, so the manifest grants them explicitly: a service may `call` only the services in its `calls`
  allowlist (default: none). The calling node enforces it, and a denied call looks exactly like a call to a service
  that does not exist.

Result: thousands of workloads share **one** address space and one OS thread per core. Switching between them is a
function call. This is the decisive change; every other gain follows from it.

### 2.2 Unit of execution: process → instance (a memory slot + shared machine code)
An instance is a slot of memory (its whole state) plus a pointer to machine code shared by all instances of the
same module. Creating one = take a free slot, copy the data segment (or map a copy-on-write snapshot taken after
`init`). Cost: kilobytes and microseconds instead of megabytes and seconds. Idle instances cost only the pages they
touched. Because instantiation is ~free, *scale-to-zero and per-request instances become the default*, not a
feature: idle services cost ≈ 0 RAM.

### 2.3 CPU scheduling: kernel threads + CFS + cgroups → one thread per core + compiler safepoints
One OS thread per core (or none at all on bare metal). Each runs a run queue of invocations. Invocations run on small
pooled stacks only while in flight (idle instances have no stack). The compiler inserts a compare-and-branch safepoint
check at function entries and loop headers (`cmp rsp, [ctx.limit]; jb slow`), which doubles as stack-overflow
check and preemption point: a ticker sets the limit to ∞ to preempt. Blocking host calls (calling another service)
suspend the invocation by switching stacks — developers write straight-line code; the runtime multiplexes.

### 2.4 Resource limits: cgroups → structure
- Memory: the slot size *is* the limit. Exceeding it is impossible by construction; nothing to account on the
  page-fault path.
- CPU: per-invocation CPU budget and time slices enforced at safepoints; preempted invocations run round-robin
  (no per-service share or weight).
- Stack: per-invocation stack budget checked at function entry.

### 2.5 Packaging: container images → content-addressed modules
A module is the compiled IR of the service: typically kilobytes, no OS userspace, no libc, no layers. Identified by
its hash, pulled from any peer, compiled to machine code once per node and shared by every instance.

### 2.6 Networking: overlay + kube-proxy + sidecars → calls
- Same node: a service call is a function invocation in the callee's instance plus a `memcpy` of the payload.
  No serialization format is imposed (the bytes are the message), no TCP, no loopback, no proxy.
- Cross node: one persistent multiplexed connection per peer pair, binary framing, batched writes. Addressing is by
  service name resolved locally from replicated state — no DNS, no virtual IPs, no iptables, no conntrack.
- Ingress: every node accepts HTTP and routes to a local or remote replica.

### 2.7 Control plane: central database + reconcilers → replicated desired state + a pure placement function
- Desired state is one small versioned document (the manifest) plus code blobs by hash, spread by gossip.
- Membership and failure detection by gossip (SWIM-style) over UDP: O(1) messages per node per period.
- **Placement is a pure function** `place(manifest, live members) → assignments` (capacity-aware weighted
  rendezvous hashing). Every node computes the same answer independently. There is no scheduler process, no
  leader, no consensus on placement, nothing to watch. Self-healing is automatic: when membership changes,
  every node recomputes and the survivors start the missing replicas (µs).
- What genuinely needs consensus (leases/singletons) is opt-in and out of scope for v0.

### 2.8 Health: probes → direct observation
The runtime runs the code, so it sees every trap, timeout and CPU overrun directly. Liveness is a fact, not a poll.

### 2.9 The operating system: Linux → optional
With isolation done by the compiler, the node runtime needs from the OS only: memory, a timer, a NIC, a disk. In
"hosted" mode it is one Linux process (good for development and for bare-metal fleets that keep Linux). In "metal"
mode the same runtime core boots directly on a (virtual) machine as its own kernel: no syscalls, no user/kernel
transitions, no page-table switches, polling virtio queues directly. In clouds, where every node is already a VM,
this deletes the guest Linux entirely.

### 2.10 Language: a thin, safe-by-construction systems language
We do not need a big language runtime or GC per workload. **Ten** is a small C-like language with typed pointers,
structs, i64/f64, no GC, no runtime. It compiles to **TIR**, a compact single-value-type (i64, with f64 bit
patterns) stack IR with structured control flow. TIR is designed for *single-pass* translation to machine code on
the node. Ten can be "unsafe" internally — a bug corrupts only its own slot — which keeps it close to the machine
while the platform stays safe. Any other language can target TIR later (including via a Wasm→TIR translator).

Why not hand-written assembly or raw binary for everything? Because the expensive part of today's stack is not
instruction selection — it is the layers. A simple compiler already emits code within a small factor of hand
assembly; the hand-written assembly is reserved for the few places where it matters (context switches, trap unwinds).

## 3. Architecture

```
            developer                         any node accepts `apply` and HTTP
   app.ten ──k10s build──► app.k10 (TIR, KB) ──k10s apply──► gossip spreads manifest + blobs

 ┌──────────────────────────── one k10s node (one process, or one bare-metal kernel) ────────────────────────────┐
 │ mesh: gossip membership · manifest · blob store · place(manifest, members) → local replicas                     │
 │ net:  HTTP ingress · peer links (framed, multiplexed) · (metal: virtio queues, polled)                          │
 │ exec: per-core shard ─ run queue ─ pooled stacks ─ safepoint preemption ─ trap unwinding                        │
 │ code: TIR → x86-64 or AArch64 (single pass, cached by hash, shared by all instances)                           │
 │ mem:  one address space: [slot][slot][slot]… power-of-two slots, lazily committed, CoW snapshots after init    │
 │ host API: input/output, call(service), kv, log, time, random, config — the only way out of a slot              │
 └─────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
```

Crates:
- `k10-ir` — TIR types, codec, validator, reference interpreter (no_std + alloc).
- `k10-nospec` — branch-free clamps for host code indexed by guest values (Spectre v1).
- `k10-host` — the host API (function table, limits, semantics), shared by the hosted node and `k10-metal` (no_std).
- `k10-lang` — Ten compiler (source → TIR) and its standard prelude.
- `k10-jit` — TIR → x86-64 / AArch64 single-pass compiler and invocation trampolines (codegen no_std; exec needs a
  platform).
- `k10-mesh` — sans-IO membership gossip, manifests, placement, wire codecs (no_std + alloc), plus a std driver.
- `k10-node` — the hosted node runtime: shards, scheduler, stacks, instance pool, host API, ingress, KV, peers.
- `k10s` — the single CLI/daemon binary.
- `k10-metal` (`metal/`, its own workspace) — the node runtime booting without Linux.

## 4. Out of scope for v0 (deliberately)
- Hostile multi-tenancy (side channels): the cheap Spectre gaps are closed (SECURITY.md, "CPU side channels": no
  bounds-check branch on guest memory, clamped `br_table` and host-function indices, 100 µs fuzzy clock, opt-in
  retpolines), but the runtime's own indirect branches, the return stack, speculative store bypass, shared caches
  and SMT are not; the answer stays one node per tenant on a hardware boundary.
- Running unmodified Linux binaries/containers.
- Strongly consistent singletons / leader election; replicated storage (KV is node-local + durable in v0).

## 5. How success is measured
Against a real Kubernetes (kind) and against bare Linux processes running equivalent services:
fixed per-node/cluster overhead (RAM, idle CPU), per-workload overhead (RAM), density (instances per GB),
cold start, request latency/throughput through ingress, service-to-service call latency, failover time,
rollout time, artifact size. See `bench/` and `docs/REPORT.md`.
