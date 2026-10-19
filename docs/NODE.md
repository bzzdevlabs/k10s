# k10s node runtime (hosted mode) — design

One `k10s node` process per machine. It replaces kubelet + containerd + shims + kube-proxy + CNI + CoreDNS +
sidecars, and (together with its peers) the whole control plane. Implemented in `crates/k10-node` (library) and
`crates/k10s` (binary). This document describes the runtime as built: the single-node core first, then cluster mode
(`crates/k10-node/src/cluster/`), which adds gossip membership, desired state spread by gossip, deterministic
placement and peer links on top of it.

## Threads
- **Shards** (`--shards`, default = available parallelism): one OS thread each, shared-nothing on the hot path. A
  shard owns an `epoll` instance, its own `SO_REUSEPORT` HTTP listener and the connections it accepted, the
  instances it created (slot pools are per shard), its fibers and run queue, a pending-call table with a timer heap,
  and an inbox (`Mutex<Vec<Msg>>` + `eventfd`; the eventfd is written only if the shard is blocked in `epoll_wait`).
- **Ticker**: every 1 ms, preempts invocations that have run longer than the time slice (2 ms) by storing
  `usize::MAX` into the running fiber's *preemption word* (published by the shard around each coroutine resume).
  Parks after 100 idle ticks; shards unpark it when they resume a fiber.
- **KV flusher**: group commit for the KV logs (sleeps when nothing is dirty).
- **Apply helper**: `POST /_k10s/apply` prepares modules on a short-lived thread so the shard keeps serving.
- **Mesh thread** (cluster mode): gossip socket, peer listener and links, remote routing, desired-state fetching.
- **Applier** (cluster mode): persists fetched desired states and installs placements through the same path as
  `apply` (`NodeShared::apply_cluster`).

## Memory
- **Slots**: instance memory = `S = 2^k` + a 4096-byte tail page (TIR needs 8 tail bytes), carved out of 1 GiB
  `MAP_NORESERVE` regions per size class (per shard). Allocation pops a LIFO free list or bumps the newest region:
  no syscall. Freed slots are `madvise(MADV_DONTNEED)`-ed (they read back as zeros) and reused.
  Regions are `MADV_NOHUGEPAGE`: with THP `always`, one touch would commit 2 MiB, i.e. ~30 neighbouring 64 KiB
  slots (measured: 70 KB RSS per instance instead of 4.4 KB). Physical memory = pages actually touched.
  `--mem SIZE` caps the address space handed to live instances (sum of `S + 4 KiB`; default: physical RAM); beyond
  it instance creation fails (503 / call −2).
- **Copy-on-write snapshots**: after `init()`, an instance's resident non-zero pages are written into a sparse
  `memfd` (`extent` = up to the last non-zero page). New instances either *map* it `MAP_PRIVATE|MAP_FIXED` over their
  slot prefix (pages only read stay shared by all instances) or *copy* its non-zero pages. Freeing a mapped slot maps
  fresh anonymous memory back (the VMA merges into the region again).
  - `fresh` services always use a snapshot; after each request the instance is reset with one
    `madvise(MADV_DONTNEED)` over the slot (private copies dropped: the prefix reverts to the snapshot, the rest to
    zeros), plus a re-copy of the non-zero pages if the slot was not mapped.
  - Other services run data segments + `init()` per instance by default. `--snapshots all` makes every service
    start from a snapshot (`init()` then runs once per service version — its side effects and `random()`/time
    values are shared by all instances, which is why it is opt-in).
  - Mapping is used only for snapshots of at least `snapshot_map_min` (64 KiB) and within a VMA budget: each
  mapped instance costs 2 VMAs and Linux caps a process at `vm.max_map_count` (65530), so the budget is
  `(max_map_count − 2·shards·max_fibers − 8192) / 2` (≈ 26k on a default 2-shard box); beyond it, or if `mmap`
  fails, the snapshot is copied. `mmap` also takes the process-wide `mmap_lock` for writing.
  - Measured (table below): mapping wins on density (≈ 0.46 KB vs 4.4 KB RSS per instance when `init` dirties
    4 KiB) and when `init()` is expensive (12 µs vs 38 µs for a 56 KiB init); copying the snapshot is the fastest
    way to create instances with small inits (3.1 µs vs 5.0 µs data+init vs 12 µs mapping). Hence the defaults:
    snapshots for `fresh` services only, map only large snapshots.

## Execution
- **Fibers**: an invocation runs on a pooled coroutine (256 KiB `MAP_NORESERVE` stack + guard page; idle instances
  own no stack). The context switch is a naked function: push rbx/rbp/r12–r15, swap `rsp`, pop (21 ns resume+suspend
  round trip). MXCSR/x87 control words are not switched (nothing running on fibers changes them). A fiber runs jobs
  in a loop; it suspends with a *syscall*: `Done`, `Yield` (preempted) or `Call`. Panics in host code are caught per
  job and reported as an internal trap. Free fibers beyond 64 per shard get their stacks released (250 ms timer).
  At most `max_fibers` (1024) invocations run per shard; callee jobs may use up to 2× that; more wait in a backlog.
- **Scheduler loop** (per shard): refresh the service table if its version changed → drain the inbox → run all
  fresh work in the run queue (new invocations at the back; callees and callers whose reply arrived at the *front*,
  so call chains complete in one pass) → give **one** time slice to the oldest preempted fiber → `epoll_wait`
  (timeout 0 if work remains, else the next timer, capped at the 250 ms maintenance tick) → timers → maintenance.
  A request arriving while busy loops run thus waits at most one slice (measured: 341 echo requests served with
  ≤ 10 ms latency while 3 infinite loops shared the same single shard).
- **Actor rule**: an instance is either in its shard's idle list or owned by exactly one job.
- **Dispatch** of a request for service S on shard X: take an idle instance of S on X (LIFO) → else create one if S's
  node-wide `live < max_instances`, no crash backoff and the node memory cap (`--mem`) allows → else push the request
  on S's node-wide wait queue and poke a shard that has an idle instance of S. Whichever shard frees an instance of
  S first serves the queue (FIFO; expired entries are dropped) and sends the result to the origin shard's inbox.
  This is also how cross-shard calls happen: calls run on the caller's shard unless the callee is at capacity.
- **Instance creation** runs inside the job's fiber (data segments or snapshot, then `init()` with the host API and
  its own CPU budget), so `init()` can call services, use KV, and be preempted.
- **Idle reclaim**: idle instances older than `--idle-timeout` (30 s) are destroyed while more than
  `warm_instances` (1) instances of the service are alive node-wide (scale-to-one; the slot returns to the pool).
- **CPU**: time slice 2 ms. CPU time is accounted per job at every suspension (monotonic clock around each resume).
  The interrupt hook (entered when the preemption word is set) checks the job's budget (`cpu_ms` from the manifest)
  → trap `Interrupted`, else yields. Budget precision ≈ one slice (2–3 ms).
- **Traps** (any `Trap`, CPU budget, host-function panic): the request fails (HTTP 500 / call −4), the trap is
  logged in the service log, the instance is destroyed (its memory may be corrupt) and replaced on demand.
  **Crash-loop backoff**: after 3 consecutive traps, instance creation for that service version is refused for
  100 ms, doubling up to 30 s; a success resets it. While no instance is alive the service answers 503 and queued
  requests fail fast.
- **Executors** (`exec.rs`): `Backend::prepare(module, info, imports) -> Arc<dyn Code>` (once per module, shared by
  every instance on every shard); `Code::invoke(scratch, mem, mem_log2, host_ctx, func)` runs on the current
  (fiber) stack. The interpreter backend wraps `k10_ir::Interp` (`safepoint_interval = 16`, `max_call_depth`
  10 000, 1 Mi value slots). See "JIT backend" below.

## Host API
docs/HOSTAPI.md, implemented once in `host.rs` (`HostCtx::dispatch(HostFn, args, mem)`); the interpreter adapter is
`k10_ir::Host`. Notes on the exact semantics chosen where the spec is silent:
- Every `(ptr, len)` is checked with `k10_ir::sem::check_range` (like `mem.copy`; negative lengths trap
  `MemOutOfBounds`). Copy-out functions (`input_read`, `call_resp_read`, `req_path`, `kv_get`, `config`) check the
  whole destination `(dst, n)` and copy `min(n, available)` bytes; reads at offsets past the end return 0.
- `req_path` returns the suffix after `/<service>`, query string included (`/echo/a?x=1` → `/a?x=1`,
  `/echo?x` → `?x`, `/echo` → empty). Calls to `name/rest` get `/rest`.
- `output_status` values outside 100..=599 are stored as 500. The optional result of `handle()` is ignored.
- Imports must match a host function's name and parameter count; `results` may be 1 (the spec) or 0 (result
  discarded). Unknown names are rejected at apply time.
- Trap codes: `Host(1)` response body over 16 MiB, `Host(2)` KV key > 1 KiB or value > 1 MiB, `Host(0x7FFF)`
  internal error (host panic).
- `random`: per-invocation wyrand stream seeded from a per-shard generator (seeded by `getrandom`). `now_ns` is
  `CLOCK_MONOTONIC`, `time_ms` `CLOCK_REALTIME`.

## Service calls
`call` suspends the caller's fiber; the shard submits the callee like any request (method POST). Results: callee
status ≥ 0, −1 unknown service, −2 unavailable (crash backoff with nothing alive, queue full (10 000), call depth
> 32, more than 10 000 calls in one invocation, out of memory), −3 deadline, −4 callee trapped.
- **Deadlines**: each call gets `min(now + call_timeout (5 s), caller's deadline)`; ingress requests 30 s. A timed-out
  waiter is resumed with −3 and removed from the wait queue; a late reply is dropped (token gone). This breaks call
  cycles that exhaust instances (A→B→A with `max_instances = 1`): the inner call times out and everything unwinds.
- Payloads are copied once out of the caller's memory and once into the callee's (`input_read`).
- Latency (release, idle machine-ish): same shard 0.45 µs; cross shard 14 µs (two eventfd wake-ups of sleeping
  shards) or 1.8 µs with `--idle-spin-us 50` (shards poll briefly after activity instead of sleeping).

## Ingress and admin (HTTP/1.1, every shard)
- Edge-triggered epoll; heads and small bodies are read through a per-shard 64 KiB scratch buffer (an idle
  keep-alive connection holds only a few hundred bytes); responses are written with one `writev` (head + body).
- Keep-alive (HTTP/1.1 default; HTTP/1.0 with `Connection: keep-alive`), pipelining (one request at a time per
  connection, in order; the next request is parsed only once the previous response is fully written —
  back-pressure), `Expect: 100-continue`, `HEAD`.
- Limits: head 16 KiB / 100 headers (431), body 16 MiB (413; 256 MiB for `/_k10s/apply`), chunked bodies 411,
  other transfer codings 501, conflicting `Content-Length` 400, obsolete folding 400, versions ≠ 1.0/1.1 505.
- Timeouts (checked every 250 ms): a request head must complete within 10 s of its
  first byte (slowloris), a body within 10 s + 1 s per 64 KiB, idle keep-alive 60 s, stalled writes 30 s.
  At most `max_conns / shards` connections per shard.
- Routing: `/<service>/<rest>` → service (404 if unknown). Error mapping: −1 → 404, −2 → 503, −3 → 504, −4 → 500.
- Admin (`/_k10s/`): `GET status` (JSON: node, manifest version/hash, per service: instances live/idle/peak, queue,
  snapshot, requests/ok/errors/traps/interrupted/created/destroyed/reclaimed, CPU, backoff, KV keys; memory: RSS,
  slots used, slot bytes, reserved, snapshot-mapped; HTTP, execution and KV counters; per shard), `GET metrics`
  (Prometheus text), `GET logs/<svc>`, `GET health`, `POST apply`.

## Apply and persistence
- `POST /_k10s/apply` body = `k10_mesh::Bundle::encode()`. The node decodes the bundle, validates the manifest,
  decodes + validates + resolves imports + checks exports (`handle`, optional `init`, no parameters) and prepares
  every new module (code is reused by hash for unchanged modules), checks `memory ≥ 2^min_mem_log2`, then swaps the
  service table atomically (one `RwLock<Arc<ServiceTable>>` write + version bump; shards pick it up on their next
  loop iteration). Services whose spec is identical keep their runtime state and warm instances; changed or removed
  services are *retired*: in-flight and already-queued requests finish on the old version, then its instances are
  destroyed (a request that races with the swap is re-routed to the new version). During a transition the old and
  new versions each have their own `max_instances` budget.
- Versions: manifest `version = 0` means "current + 1" (what `k10s apply` sends); a lower version is rejected (409),
  the same version with the same hash is a no-op. Errors: 400 (bad bundle), 422 (invalid module/manifest).
- The applied bundle is written to `DATA/bundle.k10b` (tmp + fsync + rename + dir fsync) and re-applied at start.

## Storage
- `kv_*`: per service name (survives redeployments), in-memory `HashMap` + append-only log `DATA/kv/<svc>.log`
  (`len | crc32 | op | klen | key | value`), replayed at start; a torn/corrupt tail is truncated at the last good
  record. **Group commit**: `kv_put`/`kv_del` return once the record is buffered; the flusher batches for 2 ms, writes
  every dirty log and `fdatasync`s them. **Durability window**: an acknowledged write can be lost if the process or
  machine dies within ≈ 2 ms + write + fsync time. Compaction rewrites a log from the map when it exceeds 2× the live
  size (and 1 MiB). Without `--data` the store is memory-only.
- `log`: per-service ring (1000 lines / 256 KiB, lines ≤ 4 KiB), optionally echoed to stderr (`--log-stderr`).

## Shutdown
SIGINT/SIGTERM (`k10s node`) or `Node::shutdown`: shards stop accepting, close idle connections and answer in-flight
requests with `connection: close` (up to `drain_timeout`, 5 s, the ticker still preempting), then the ticker is
joined (it holds pointers into shard-owned fibers), shards exit, and the KV store is flushed and synced.

## Measurements (release, i7-7820HQ 4C/8T, Linux 6.1, other agents compiling: load 2–9)
| what | result |
|---|---|
| coroutine resume + suspend | 21 ns |
| interpreter `invoke` of an empty function (no scheduler) | 16–25 ns |
| invocation through the scheduler (empty `handle`, warm instance) | 250–300 ns; echo 330–400 ns |
| 8 shards invoking one shared service / one service each | 11.9 / 10.3 M invocations/s (no shared-state contention) |
| instance creation: empty module / data + 4 KiB init / snapshot copied / snapshot mapped | 1.2 / 5.0 / 3.1 / 12 µs |
| instance creation, 56 KiB init: data + init / snapshot copied / mapped | 38 / 26 / 12.6 µs |
| slot alloc + 1 page fault + free (`madvise`) | 3.2 µs |
| local call same shard / cross shard / cross shard with 50 µs idle spin | 0.45 / 14 / 1.8 µs |
| density, 64 KiB slots, `init` dirties 4 KiB, 10k instances: data+init / snapshot mapped | 4.6 KB / 0.46 KB RSS per instance (46 / 7 MiB total) |
| density, 100k instances: data+init / snapshot copied / mapped (26.6k mapped, rest copied) | 4.4 / 4.4 / 3.3 KB per instance (421 / 422 / 317 MiB) |
| idle `k10s node`, 8 shards: 0 services / 100 deployed / 100 with a warm instance each | < 0.1 % of one core; RSS 3.1 / 3.7 / 4.1 MiB (PSS 1.4 / 2.0 / 2.4) |
| HTTP echo, loadgen c=1 / 16 / 64 (8 shards) | 27k rps p50 33 µs / 124k p50 113 µs / 159k p50 266 µs, 0 errors |
| HTTP → service → `call` → echo, c=1 / 16 / 64 | 28k / 109k / 144k rps |
Bare-process Go baseline on the same machine (bench/results/native): echo 17k / 74k / 84k rps, chain (two processes
over loopback HTTP) 5.6k / 21k / 26k rps, 6.9 MiB RSS per process. HTTP numbers are bounded by the loadgen
(~2.5 cores) and the shared machine. Reproduce: `cargo run --release -p k10-node --example measure -- prims|invoke|
scale|create|density MODE N|idle N`, and `examples/demo_modules` + `k10s node` + `bench/bin/loadgen`.

## JIT backend (plug-in points)
Implement `exec::Backend`/`exec::Code` (`NodeConfig::backend` selects it):
- `prepare` = `k10_jit::compile(module)` (+ a per-module `host_fns` table: one `extern "C" fn(*mut VmCtx, a0..a4)`
  adapter per import, each calling `HostCtx::dispatch(HostFn, args, mem)` with `user` = `*mut HostCtx` and
  `mem` = `[mem_base, mem_base + S + 8)`; a trap result goes to `VmCtx.trap_request`).
- `invoke` builds the fiber's `VmCtx` in `Scratch::jit` (stable address for the fiber's lifetime), sets
  `real_stack_limit = Scratch::stack_lo + HOST_RESERVE` (use `NodeConfig::stack_size` ≥ 1 MiB for JIT frames), calls
  `Scratch::set_preempt_word(&vmctx.stack_limit)` so the ticker's store lands where JIT safepoints look (the shard
  publishes the word at every resume; the first slice of a fiber's first JIT invocation may still be polled through
  the own word), and the interrupt adapter restores `stack_limit` then calls `HostCtx::interrupt()` (which may
  switch coroutines and returns `Err(Interrupted)` when the CPU budget is exhausted).
- The ticker writes `stack_limit` concurrently: Rust code must access the `VmCtx` through raw pointers.

## Cluster mode
No control plane: membership and failure detection are SWIM gossip (`k10_mesh::Gossip`, UDP), the desired state (a
manifest + content-addressed code blobs) spreads by gossip, and placement is the pure function
`k10_mesh::place(manifest, live members)` that every node evaluates on its own. Self-healing falls out of membership
changes. Code: `cluster/mod.rs` (config, applier, operator apply, status), `cluster/mesh.rs` (mesh thread),
`cluster/router.rs` (inbox, `RemoteRouter`), `cluster/store.rs` (blobs, manifest, node id).

**Start-up.** `k10s node` runs in cluster mode when any of `--gossip ADDR` (UDP, default `0.0.0.0:7946`), `--rpc ADDR`
(TCP, default `0.0.0.0:7947`), `--join HOST:PORT[,…]` (gossip addresses of members) or `--advertise HOST` is given.
`ClusterSetup::bind` (sockets, store, id) → `Node::start_with_router(cfg, setup.router())` → `setup.start(node)`.
- Identity: a random `NodeId` persisted in `DATA/node-id`; the incarnation starts at the wall clock in ms, so a
  restarted node (same id) refutes the Dead/Left records of its previous life.
- The member advertises its gossip, rpc and http addresses (IP: `--advertise`, else the bind IP, else — wildcard bind —
  the source address of the default route, else 127.0.0.1) and its capacity: `mem_bytes = --mem`, `cores = --shards`.
- Tuning: `--probe-ms N` (gossip protocol period, default 200; probe timeout = 0.4 N), `--suspicion-mult N` (default 4),
  `--leave-grace-ms N` (default 1000), `--idle-spin-us` also applies to the mesh thread.
- The stored desired state (`DATA/manifest.k10m` + `DATA/blobs/`) is applied at start (placed over this node alone)
  and announced with `set_desired`: the highest version in the cluster wins, so a whole-cluster restart converges
  on the newest one. `DATA/bundle.k10b` (single-node mode) is not used.

**Desired state.** `POST /_k10s/apply` (`k10s apply`) works on any node: decode, version = highest known + 1 (stored,
applied or announced by gossip) when the bundle says 0 (an explicit version must be higher; the same version and
hash is a no-op, anything else 409), blobs into the store, **apply locally first** (this validates every module:
an invalid bundle is refused with 422 and never announced), persist (tmp + fsync + rename), then
`Gossip::set_desired(version, hash)`. A node learning a newer `DesiredState{version, hash, origin}` fetches the manifest
(`GetManifest`) and the blobs it lacks (`GetBlob`, pipelined) over the peer protocol — from the origin first, then the
other live members in random order (next one on a missing blob, a hash mismatch, a lost connection or 10 s without
progress; all failed → retry after 1 s) — checks `sha256` of everything, keeps it in the store, then the applier
persists (no fsync: a torn file fails its hash at load and is fetched again) and applies it. Versions are ordered by
`(version, hash)` like gossip's desired-state pointer: two operators applying concurrently may pick the same version;
the larger hash wins on every node. Blobs no longer referenced by the current manifest are deleted (except blobs
younger than 60 s and those of the manifest being fetched, which are pinned).

**Placement.** On every desired-state change and on every change of the live member set (ids + capacity; Alive and
Suspect count, like in `place`), batched in a 50 ms window (`ClusterConfig::debounce`; bounds the delay, not
extended by later events), the applier computes `place(manifest, live members)` and calls
`NodeShared::apply_cluster(bundle, placement)`: services placed on this node get `Placement::Local`, the others
`Placement::Remote { nodes }` (sorted). A service whose spec and placement did not change keeps its `ServiceRt` (warm
instances, counters: the `rt` id in `/_k10s/status` stays the same); a placement change replaces it like an update
(in-flight requests finish on the old one). Re-applying the same manifest with a new placement only replaces the
services whose placement changed. With each placement the applier also computes, per service, the rendezvous
ranking of all live members (`place` with `replicas = members`): where the service goes if its hosts fail — the
router's fallback order. Remote-placed services keep their prepared code (any node can run any service) but keep no
warm instance (idle reclaim down to 0).

**Peer links** (`k10_mesh::wire` framing, TCP_NODELAY). One persistent connection per peer pair, multiplexed by
request id (each side picks ids for its own requests; requests and responses are told apart by kind). Both sides
send `Hello{member}` first. A node dials members with a larger id as soon as they come up, and any member lazily on
first use; when both ends dialed at once, the connection dialed by the smaller id is kept and the other one is closed
by its dialer once its requests are answered. Frames are appended to the link's write buffer and written once per
mesh-loop iteration (batching; edge-triggered epoll, partial writes resume on `EPOLLOUT`). A failed dial (or one
not connected within 1 s) backs the peer off for 200 ms, doubling up to 5 s. Links to a member declared Dead are
closed; links to a member that Left stay open until the leaver closes them (it still answers what it was sent).

**Routing.** Shards hand invocations of `Remote` services (ingress requests and `call`s) to `MeshRouter::forward` →
mesh inbox. Candidates: (1) placed replicas that are Alive and not backed off — fewest outstanding requests on the
link, round-robin on ties; (2) the service's rendezvous ranking (Alive members, at most `replicas + 3` attempts in
total); if this node comes up in the ranking it runs the service itself (`NodeShared::serve_local`); (3) Suspect
replicas. `Call` frames carry the remaining deadline in ms; the method travels as a one-byte prefix of the path
(`0x10 + req_method`; the wire `Call` has no method field). The peer runs the call locally whatever its own
placement says — a request takes at most one hop — and answers `CallResult` (or `NOT_HERE` = −100, a peer-protocol
status that never reaches services, if the service is unknown there or the node is leaving).
**Retry semantics (at most once):** a request goes to the next candidate only when it is known not to have run — its
bytes never reached the socket (connect failure or timeout, connection lost before they were written; tracked with
the link's byte offsets) or the peer answered `NOT_HERE`. A request already written to a connection that breaks, or
outstanding on a peer that gossip declares Dead, fails with −2 (HTTP 503). Deadlines: the origin shard enforces its
own (−3); the router drops its record 100 ms later.

**Failover.** A crashed process (SIGKILL) resets its connections: requests in flight on them fail, the peer is backed
off, and new requests go straight to the next node of the ranking (which is where `place` moves the service once
gossip declares the node dead). A hung node (SIGSTOP; same as a machine that stops answering) is avoided as soon as
gossip suspects it (≈ one probe period + timeout); requests stuck on it fail when it is declared Dead.

**Graceful leave** (SIGTERM): gossip `Left` (16 direct sends + re-gossip), local re-placement without this node (its
own ingress forwards everything), `leave_grace` while peers converge (they stop sending new requests), then peers'
new calls are answered `NOT_HERE` (they retry elsewhere), `Node::shutdown` drains in-flight requests, and the mesh
thread stops.

**Status.** `/_k10s/status` gains `"cluster"`: id, incarnation, addresses, leaving, desired (version, hash, origin),
applied (version, hash), fetch in progress, last apply error, members (id, state, incarnation, gossip/rpc/http
addresses, capacity), placement (service → node ids), links (peer, state, outstanding, bytes), counters (gossip
packets/bytes, forwarded, retries, failed, local fallback, served for peers, not-here, dial failures, peer bytes,
fetches, applies). Each service also reports its `rt` id and `placement` (`"local"` or node ids). `GET
/_k10s/cluster` renders the same as text; `k10s status` prints it in cluster mode (`--json` for the JSON).

**Tests** (`crates/k10s/tests/cluster.rs`, real `k10s node` processes on ephemeral localhost ports, `--probe-ms 100`):
join + convergence, apply on one node → every node applies and serves every service through its own ingress (local
or forwarded, methods preserved), `call` across nodes, 3 replicas on 3 distinct nodes, same placement everywhere,
`k10s status`; SIGKILL of the host of a single-replica service → served again, declared dead, re-placed; apply
while it is down, restart with the same data directory → same id, higher incarnation, catches up; graceful leave
and a join under load (4 → 3 → 4 nodes): 0 failed requests, services whose placement did not change keep their
runtime; re-applies under load on 5 nodes (0 failed requests) and two concurrent applies on two nodes (one winner
everywhere); the origin of a desired state crashes and a node joining later fetches it from the other members.
Unit tests: store (persist/reload/corruption/gc/pinning, node id), ranking vs `place`, method tags; node test:
`apply_cluster` ordering and placement-only updates. `cargo test -p k10s --test cluster` takes ≈ 2–4 s.

### Cluster measurements (release, localhost, i7-7820HQ 4C/8T; other agents compiling: load 3.5–12, so numbers are noisy)
| what | result |
|---|---|
| membership convergence (spawn N processes one by one, until all see N live) | 3: 50–70 ms, 5: 130–180 ms, 20: 290–500 ms |
| apply → every node serving the new version (median / max of 5, two runs; 1 shard per node, default gossip) | 3 nodes: 9–17 / 17–29 ms, 5: 25–34 / 64–88 ms, 20: 146–190 / 214–284 ms (≈ +10 ms until every status reports it) |
| re-apply under load (5 nodes, 10 conns over all ingresses, 4 versions + 2 concurrent applies) | 0 failed of ≈ 30–37k requests; every node serving the new version 12–35 ms after apply |
| graceful leave + join under load (4 → 3 → 4 nodes) | 0 failed of ≈ 28–41k requests |
| SIGKILL of the host (replicas = 1), probe every 10 ms through another node, default gossip (200 ms, ×4) | first success 7–8 ms after the kill, 0 failed probes; declared dead after 1.0–1.1 s (re-placed ≈ 50 ms later); closed-loop load (8 conns): 7 failed = the requests in flight |
| same, tight gossip (`--probe-ms 50 --suspicion-mult 2`) | first success 5–13 ms; declared dead after 180–220 ms; 8 failed under load (in flight) |
| SIGSTOP (hung node, no reset), default / tight gossip | first success 270–315 / 57–103 ms (once suspected); 100 % of new probes OK after 260–300 / 47–90 ms; requests stuck on the node fail when it is declared dead (1.1 s / 0.2 s) |
| `call` latency, c=1 (2 shards per node): ingress → caller → callee same node / other node | p50 50 / 118 µs (17k / 7.8k rps); with `--idle-spin-us 50`: 26 / 109 µs |
| ingress forwarding, echo via the hosting node / via another node | c=1: p50 34 / 124 µs (25k / 6.7k rps); c=16: 90k / 36k rps (p50 149 / 294 µs); c=64: 91k / 65k rps (p50 516 / 652 µs) |
| idle 5-node cluster (2 shards each), no services / 20 services × 2 replicas | RSS 2.6–4.7 / 2.8–4.9 MiB per node; CPU ≈ 0.1 % of a core; gossip ≈ 300–350 B/s out and in per node |

A forwarded request crosses six thread hand-offs (shard → mesh → peer mesh → peer shard → peer mesh → mesh → shard),
each an eventfd/epoll wake-up on a busy machine: that, not the network or the framing, is most of the ≈ 70–90 µs.
Reproduce: `cargo test --release -p k10s --test cluster -- --ignored --nocapture --test-threads 1 measure_`
(`K10S_SPIN_US=50` for the spin variant; needs `bench/bin/loadgen`).

## Known limitations
- No TLS; no chunked request bodies; responses always use `Content-Length`.
- Cluster: no authentication or encryption on gossip (UDP) or peer links (TCP): run on a trusted network. KV is
  node-local (replicas of a service, and a service that moves, see different data). No consensus: two concurrent
  applies resolve by `(version, hash)`, the loser's change is dropped silently; an operator apply on a node that has
  not yet learned the cluster's latest version can likewise lose. A service unplaced for lack of memory, or whose
  hosts are all down, is run by the next node of its ranking (or this node) regardless of `--mem` headroom planning.
  Forwarding costs six thread hand-offs (see measurements). Non-UTF-8 call paths are converted lossily on the wire.
  Peer links have no application-level keepalive (gossip detects dead peers).
- KV is node-local with an asynchronous durability window (no fsync-before-ack mode yet); compaction serializes the
  whole map under its lock (pauses `kv_*` calls on that service for the copy).
- A host function that blocks (none does today except brief KV mutexes) would block its shard.
- CPU accounting uses wall time while a fiber runs (an OS-level preemption of the shard thread is charged to the
  invocation); the budget is enforced with ≈ one-slice precision.
- Coroutine stacks are 256 KiB with a guard page: a runaway *native* recursion in host code would SIGSEGV the process
  (TIR recursion is bounded by the executor).
- `k10_ir::InterpStack` cannot be shrunk; a fiber's interpreter buffers are dropped only after a `StackOverflow`
  trap or when the fiber's stack is trimmed.
