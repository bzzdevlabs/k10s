# k10s node runtime (hosted mode) — design

One `k10s node` process per machine. It replaces kubelet + containerd + shims + kube-proxy + CNI + CoreDNS +
sidecars, and (together with its peers) the whole control plane.

## Threads
- **Shards**: one OS thread per core (`--shards`, default = available parallelism). Shared-nothing as far as possible.
  Each shard owns: an `epoll` instance, its own `SO_REUSEPORT` HTTP listener, the connections it accepted, the
  instances it created, its run queue, its pool of coroutine stacks, an inbox (MPSC queue + `eventfd`) for messages
  from other threads, and a timer list.
- **Mesh thread** (cluster mode): gossip UDP socket (`k10-mesh` driver), peer TCP links (`k10-mesh::wire`),
  manifest/blob store, placement. Publishes an immutable `Arc<ServiceTable>` snapshot to shards when the local
  service set changes; forwards remote calls.
- **Ticker**: wakes every ~1 ms; requests preemption of invocations that exceeded their time slice by storing
  `usize::MAX` into their `VmCtx.stack_limit` (see docs/JIT-ABI.md).

## Execution
- **Slots**: instance memory = power-of-two `S` + 4096-byte tail, from per-size-class pools carved out of large
  `MAP_NORESERVE` reservations. Freed slots are `madvise(MADV_DONTNEED)`-ed and reused: no syscall on the hot path
  except page faults. Physical memory = pages actually touched.
- **Snapshots**: after `init()`, an instance's memory can be captured into a `memfd`; new instances (and `fresh`
  resets) map it `MAP_PRIVATE|MAP_FIXED` over their slot → copy-on-write sharing across all instances of a service.
- **Backends**: an `Executor` abstraction with two implementations: the reference interpreter (`k10-ir`) and the
  JIT (`k10-jit`). Both run inside coroutines and call the same host-function implementations.
- **Coroutines**: an invocation (one `handle()` call) runs on a pooled stack (e.g. 128 KiB usable + guard page)
  only while it is in flight; idle instances own no stack. Context switch = a few instructions of hand-written asm
  (save callee-saved registers + rsp, swap). Blocking host calls (`call`, remote anything) and preemption suspend the
  coroutine; the shard runs something else.
- **Actor rule**: an instance runs one invocation at a time. Requests for a service go to an idle local instance on
  the receiving shard; else a new instance is created if the node-wide `max_instances` allows; else the request
  queues. Idle instances above a small warm pool are reclaimed after an idle timeout (scale-to-zero beyond one warm
  instance per node by default).
- **CPU**: time slice (default 2 ms) enforced at safepoints; per-request CPU budget (`cpu_ms`) → trap `Interrupted`.
- **Traps**: the invocation fails (HTTP 500 / call error -4); the instance is discarded and replaced (its memory may
  be corrupt). Crash-looping services are backed off.

## Host API
docs/HOSTAPI.md. Each host function has one implementation working on `(invocation context, memory slice)`, with
thin adapters for the JIT (`extern "C" fn(*mut VmCtx, …)`) and the interpreter (`k10_ir::Host`).
Service calls: same shard → enqueue callee invocation, suspend caller; other shard → inbox; other node → mesh thread
→ peer link. Payloads are copied once from caller memory into the callee's input buffer (and back for responses).

## Ingress and admin
HTTP/1.1 with keep-alive on every shard. `/<service>/<rest>` → invoke `<service>` (local if hosted here, else
forwarded to a node that hosts it). `/_k10s/…` is the admin API: `status`, `metrics`, `logs/<svc>`, `apply`
(manifest bundle = manifest + code blobs).

## Storage
`kv_*`: per-service namespace, in-memory map + append-only log under `--data` with group commit (fsync batching);
replayed at start, compacted when the log exceeds 2× the live size.

## Cluster mode (k10-mesh)
Gossip membership → `place(manifest, members)` on every change → the node hosts exactly the services placed on it
(compiling blobs fetched from peers by hash), and knows where every other service lives for forwarding. No leader.
