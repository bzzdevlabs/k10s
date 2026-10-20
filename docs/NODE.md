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
  `--mem SIZE` caps the address space handed to live instances (sum of `S + 4 KiB`; default: physical RAM), with
  per-service reservations (below).
- **Memory reservations** (`slot::MemBudget`):
  - every service placed on the node reserves one instance (`S + 4 KiB`) of `--mem`; burst instances share the
    rest, so a busy service can exhaust the shared pool but never another service's reservation;
  - an operator apply whose reservations exceed `--mem` is refused (422) before any code is prepared; the reloaded
    bundle and cluster states are installed in full, over-committed (one instance each, no burst), logged
    `DEGRADED` and reported (`memory.mem_overcommit`, `k10s_memory_overcommit_bytes`); a failed reload is reported
    (`reload_error`, `k10s_reload_failed`);
  - placement charges the same footprint per replica (placement version 2; see "Placement");
  - burst instances are created only while committed bytes fit; a request that finds no memory waits for one of
    the service's instances, and queued requests fail at once (503) when the last instance goes and no memory is
    left for a replacement;
  - after an apply that adds reservations while bursts hold the pool, idle burst instances are destroyed at once and
    busy ones when their request ends: `--mem` is exceeded by at most the instances mid-request at apply time, until
    those requests finish; a replaced version's instances still running count as retiring (bounding bursts, not
    triggering relief), so updates do not count twice;
  - status `memory.mem_{limit,reserved,shared_used,shared_limit,committed,retiring,overcommit,refused}` and per
    service `mem_{reserved,used,shared}`; matching `k10s_memory_*` metrics;
  - outside `--mem` (bounded separately): snapshots, prepared code, KV (`--kv-max`), connection buffers
    (`--conn-buffer-max`), fiber stacks.
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
- **Fibers**: an invocation runs on a pooled coroutine (1 MiB `MAP_NORESERVE` stack + guard page; idle instances
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
  node-wide `live < max_instances`, no crash backoff and the memory budget allows (S's reservation, else the shared
  pool; with no memory and no live instance of S: 503) → else push the request on S's node-wide wait queue and poke
  a shard that has an idle instance of S. Whichever shard frees an instance of
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
  (fiber) stack. Two backends: the JIT (`k10-jit` machine code, the default; see "JIT backend" below) and the
  reference interpreter (`k10_ir::Interp`, `safepoint_interval = 16`, `max_call_depth` 10 000, 1 Mi value slots).
  `NodeConfig::backend` / `k10s node --backend jit|interp` select one; the default is `K10S_BACKEND` if set, else jit
  (the test suites run against both: `K10S_BACKEND=interp cargo test`). `/_k10s/status` reports `"backend"` and
  `"code"` (prepared modules alive, code bytes, executable bytes mapped, modules prepared and time spent).

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
- Trap codes: `Host(1)` response body over 16 MiB, `Host(2)` KV key > 1 KiB or value > 1 MiB, `Host(3)` KV
  per-service quota exhausted, `Host(4)` `kv_add` on a value that is not 8 bytes, `Host(0x7FFF)` internal error
  (host panic).
- `random`: per-invocation wyrand stream seeded from a per-shard generator (seeded by `getrandom`). `now_ns` is
  `CLOCK_MONOTONIC`, `time_ms` `CLOCK_REALTIME`, both coarsened with fuzzy edges (`clock.rs`, `k10_ir::clock`) to
  `--timer-res-us` (default 100; 0 = full resolution). Every guest `(ptr, len)`, offset and index is clamped
  branch-free after its check (`check_span`, `k10_ir::nospec`; SECURITY.md, "CPU side channels").

## Service calls
`call` suspends the caller's fiber; the shard submits the callee like any request (method POST). Results: callee
status ≥ 0, −1 unknown service, −2 unavailable (crash backoff with nothing alive, queue full (10 000), call depth
> 32, more than 10 000 calls in one invocation, out of memory), −3 deadline, −4 callee trapped.
- **Allowlist**: a service may call only the services of its manifest `calls` list (`calls = ["echo", "kv-store"]`;
  none by default, itself included; `["*"]` = any, discouraged). Apply refuses names that are not services of the
  manifest (typos). `on_call` checks the caller's list (the fiber records its job's service) after the depth and
  call-count limits and before any lookup, routing or forwarding: one binary search over the sorted list, no
  allocation. A denied call gets −1 and an empty response, exactly like an unknown service, without looking the
  name up, so whether such a service exists changes neither the result nor the path taken. HTTP ingress is not
  restricted (any client of the node reaches any service).
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
- **Node-wide buffer budget** (`--conn-buffer-max SIZE`, default max(`--mem`/8, 64 MiB); `NodeConfig::
  conn_buffer_max`): one atomic counter shared by the shards charges every request body from the moment its head
  is parsed until its response is produced (the body lives in the connection, then in the invocation), plus every
  response byte the client has not read yet (what `writev` could not hand to the socket). A body that does not fit
  is answered 503 with `connection: close` before it is read (and before `100 Continue`); a body always fits when
  nothing at all is buffered, so one request of any allowed size works on an idle node. While the total is over the
  budget (slow readers holding large responses), every new request without a body is answered 503 too, without
  running, until they drain or time out (stalled writes: 30 s); admin requests without a body are never shed. Status:
  `http_stats.buffered_bytes`, `buffer_max`, `buffer_refused`; metrics `k10s_http_buffered_bytes`,
  `k10s_http_buffer_refused_total`.
- **Out of file descriptors**: an `accept` that fails (`EMFILE`/`ENFILE`, or any other error) leaves the connection
  in the backlog, and the level-triggered listener would wake the shard at once forever. The shard instead takes the
  listener out of its epoll set for 10 ms, doubling up to 1 s while accepts keep failing (`sys::AcceptPause`); the
  log line is rate-limited (one per 10 s, with a count of the suppressed ones); `http_stats.accept_pauses` counts
  the pauses. The mesh thread does the same with the peer listener (`cluster.stats.accept_pauses`).
- Timeouts (checked every 250 ms): a request head must complete within 10 s of its
  first byte (slowloris), a body within 10 s + 1 s per 64 KiB, idle keep-alive 60 s, stalled writes 30 s.
  At most `max_conns / shards` connections per shard.
- Routing: `/<service>/<rest>` → service (404 if unknown). Error mapping: −1 → 404, −2 → 503, −3 → 504, −4 → 500.
- Admin (`/_k10s/`): `GET status` (JSON: node, build version, manifest version/hash, per service: instances
  live/idle/peak, queue, snapshot, requests/ok/errors/traps/interrupted/created/destroyed/reclaimed, CPU, backoff,
  KV keys; memory: RSS,
  slots used, slot bytes, reserved, snapshot-mapped; HTTP, execution and KV counters; per shard), `GET metrics`
  (Prometheus text), `GET logs/<svc>`, `GET health`, `POST apply`.
- **Admin access** (`admin::gate`, decided on the request head before any body is read or reserved; a refusal
  closes the connection): `health` and `metrics` are open (liveness, read-only counters: a scraper should not need a
  secret that can deploy code). Everything else needs `Authorization: Bearer <token>` when the node has
  `--admin-token-file FILE` (only its SHA-256 is kept; constant-time comparison; 401 with `WWW-Authenticate`).
  Without a token only loopback peers (`getpeername`) are served, and not when a `Forwarded`/`X-Forwarded-For`
  header shows a local proxy relaying someone else (403). `--admin ADDR` serves the admin API on its own listener
  (accepted by shard 0); the ingress then answers only `/_k10s/health` of it (404 otherwise), and the admin listener
  serves no services. Credentials are only parsed for `/_k10s` targets: service traffic pays one prefix check.
  CLI: `k10s apply|status|logs` send the token from `--admin-token-file`, `$K10S_ADMIN_TOKEN_FILE` or
  `$K10S_ADMIN_TOKEN`; `k10s keygen [-o FILE]` writes 32 random bytes as hex (mode 0600, never overwrites).

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
- CLI: `k10s apply MANIFEST` compiles `code = "x.ten"` sources with `k10-lang` (rendered diagnostics and exit code 1
  on errors, nothing is sent) and sends precompiled `.k10` blobs as they are. `k10s build FILE.ten [-o FILE.k10]`
  compiles only. `k10s run FILE.ten [--path /p?q=1] [--method M] [--body STR|@FILE] [--backend jit|interp]` runs one
  request in-process (a one-shard node on a loopback port, memory-only KV, service logs on stderr): the dev loop.

## Storage
- `kv_*`: per service name (survives redeployments), in-memory `HashMap` + append-only log `DATA/kv/<svc>.log`
  (`len | crc32 | op | klen | key | value`), replayed at start. One namespace object (map + mutex) is shared by every
  shard, so the calls are linearizable node-wide; `kv_add` and `kv_cas` read, compare and write under that lock and
  are logged as ordinary put/del records in lock order (same durability as `kv_put`; an 8-byte same-size overwrite
  is done in place); a torn/corrupt tail is truncated at the last good
  record. **Group commit**: `kv_put`/`kv_del` return once the record is buffered; the flusher batches for 2 ms, writes
  every dirty log and `fdatasync`s them. **Durability window**: an acknowledged write can be lost if the process or
  machine dies within ≈ 2 ms + write + fsync time. Compaction rewrites a log from the map when it exceeds 2× the live
  size (and 1 MiB). Without `--data` the store is memory-only. **Per-service quota**: a `kv_put` that would push a
  namespace past its live-bytes quota (default 256 MiB, `--kv-max`, 0 = unlimited) traps `Host(3)` and stores nothing,
  so one service cannot fill the node's memory/disk (it bounds live bytes; the log is then bounded at ~2× that by
  compaction). Deleting entries frees room.
- `log`: per-service ring (1000 lines / 256 KiB, lines ≤ 4 KiB), optionally echoed to stderr (`--log-stderr`).

## Process sandbox (`k10s node` only; `crates/k10-node/src/sandbox.rs`, SECURITY.md "Defense in depth")
- `--sandbox on|log|off` (default `on`, or `$K10S_SANDBOX`); `--user USER[:GROUP]` (started as root).
- Order: close inherited fds ≥ 3 and `PR_SET_NO_NEW_PRIVS` → with `--user`: groups and fs uid/gid of the user (files
  are created as the user; the data directory must belong to it) → Landlock on the main thread before any thread
  exists (inherited by all threads): data dir rw, `/proc/self`, `/proc/sys/vm/{max_map_count,overcommit_memory}`,
  `/sys/devices/system/cpu` and the binary read-only, `K10_JIT_DUMP` rw if set; TCP `bind` limited to the HTTP, admin and
  peer ports with ABI ≥ 4 → listeners bound, threads started → `setresgid`/`setresuid` to the user (all threads) →
  seccomp allowlist installed with `SECCOMP_FILTER_FLAG_TSYNC`; threads created later inherit it. One log line
  states what was applied (Landlock ABI or why not, syscall count, user).
- The allowlist is argument-independent for every hot-path syscall (`epoll_wait`, `read`/`write`/`writev`,
  `accept4`, `recvfrom`/`sendto`, `futex`, `madvise`, `clock_gettime`, …), so Linux ≥ 5.11 serves them from the
  seccomp action cache without running the filter. Argument checks only on rare calls: `mmap` (no `PROT_EXEC`),
  `mprotect` (not W+X), `clone` (threads only, no namespaces; `clone3` → `ENOSYS` so glibc falls back to `clone`),
  `socket` (IPv4/IPv6), `prctl`, `ioctl`, `tgkill` (own pid), `prlimit64` (read only), `sched_setaffinity` (self).
- Library users (`Node::start` in tests, `k10s run`, benchmarks) get no sandbox.
- Cost (i7-7820HQ, Linux 6.1 with IBRS: a bare syscall ≈ 600 ns; shared machine, load 10–35, so medians of 8
  interleaved runs): `getpid` best-of-20 +0–30 ns with the filter, +80 ns when the action cache is defeated
  (`sandbox::tests::syscall_overhead`); node CPU per request unchanged, on/baseline 1.01 echo, 0.99 chain, 1.00
  compute at 64 conns (17.8 / 19.1 / 1404 µs); rps and p50 of echo, chain, compute (1 and 64 conns), a forwarded
  3-node request and apply → first answer (18.3 vs 19.2 ms) all within run-to-run noise of the pre-sandbox binary. After the filter a
  request costs ≈ 1 `read`, 1 `writev`, 0.24 `epoll_wait` (+1 `madvise` per `fresh` request); `mmap`/`mprotect`
  happen only on instance, fiber or thread creation (≈ 1 per 7000 requests).

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

## JIT backend
`jit.rs` implements `exec::Backend`/`exec::Code` with `k10_jit` (docs/JIT-ABI.md).
- **Prepare** (apply thread): `k10_jit::compile(module)` → `CompiledModule` (W^X mapping, `Send + Sync`) shared by
  every instance and shard through `Arc<dyn Code>`, plus the module's host-function table: one `extern "C"` adapter
  per resolved import (19 monomorphic adapters, one per host function). Unchanged modules are reused by code hash
  across applies; a replaced module is unmapped when the last `Arc` drops, i.e. once the retired version's in-flight
  and queued requests are done and its instances destroyed (tests check the anonymous executable mappings in
  `/proc/self/maps` against the `"code"` counters).
- **Invoke**: the `VmCtx` lives in the fiber's `Scratch::jit` (128 bytes, stable for the fiber's lifetime) and is
  re-initialized field by field through raw pointers at every invocation (`stack_limit` with an atomic store, since
  the ticker may write it concurrently): `mem_base`/`mem_mask` = the slot (`S + 8` bytes addressable; slots have a
  4 KiB tail), `trap_request = 0`, `user` = a per-invocation record (the `HostCtx` and the memory range) on the
  invoking frame.
- **Stacks**: JIT frames use the fiber's native stack. `real_stack_limit = Scratch::stack_lo + HOST_RESERVE`
  (128 KiB): the safepoint at every function entry guarantees that a host function or the interrupt hook entered from
  the deepest JIT frame still has ≥ 128 KiB, so runaway recursion traps `StackOverflow` and never reaches the guard
  page (tested with host calls and preemption at every depth down to the limit). With the default 1 MiB stacks a small
  function recurses 20–30k deep (TIR requires ≥ 1000). After a `StackOverflow` trap the fiber's stack is trimmed back
  to 64 KiB (`madvise`).
- **Host calls**: each adapter rebuilds the memory slice from the invocation record and calls the same
  `HostCtx::dispatch` as the interpreter; `Err(trap)` becomes `VmCtx.trap_request` (the JIT raises it when the
  adapter returns). Panics are caught in the adapter (they must not unwind through JIT frames), stashed, and re-raised
  after `invoke` returns, so they surface as internal errors exactly like interpreter panics. `call` and the
  interrupt hook may switch coroutines with JIT frames on the suspended stack (allowed by the ABI).
- **Preemption**: the shard publishes a per-fiber preemption word to the ticker at every resume; a JIT invocation
  points it at `VmCtx.stack_limit` (`Scratch::set_preempt_word`, which also re-publishes it immediately for the running
  slice). The ticker's `usize::MAX` store makes the next safepoint (function entry or loop back-edge) call the
  interrupt adapter, which restores `stack_limit` and runs `HostCtx::interrupt`: the same CPU-budget check
  (`Interrupted`) and yield as the interpreter path. A store that lands just after a restore costs one spurious
  yield; one lost to a re-initialization is re-sent at the next tick.
- JIT assumptions the runtime honours: `S ≥ 2^max(12, min_mem_log2)` (checked at apply, re-checked by `invoke`),
  `trap_request = 0` at entry, MXCSR/x87 control and the direction flag at their SysV defaults (nothing on fibers
  changes them; coroutine switches do not touch them). Code is compiled for the node's own CPU (`CpuFeatures::detect`),
  so every cluster member compiles the blobs it fetches itself.
- **Spectre v2 hardening** (opt-in): `--jit-indirect lfence|retpoline` (`NodeConfig::jit_indirect`) compiles every
  indirect branch of generated code through `lfence` or a retpoline (docs/JIT-ABI.md, "Speculation hardening").

### JIT measurements (release, i7-7820HQ 4C/8T, 8 shards; other agents running: load 3–8, numbers are noisy)
loadgen (bench/bin/loadgen, closed loop, 10 s after 2 s warm-up) against `k10s node --backend jit|interp` with the
Ten examples, and the Go reference workloads (bench/bin/{echo,compute}) as plain processes on the same machine.
rps at c = 1 / 16 / 64 (p50 at c = 1):

| workload | JIT | interp | Go process |
|---|---|---|---|
| echo GET | 28.6k (30 µs) / 112k / 158k | 21.1k (42 µs) / 108k / 166k | 17.8k (51 µs) / 80k / 104k |
| compute `?n=1000` | 1.03k (0.92 ms) / 3.6k / 3.9k | 51 (16.8 ms) / 210 / 193 | 2.9k (0.34 ms) / 12.7k / 13.5k |
| compute `?n=100000` | 10.3 (94 ms) / 39 / 38 | 0.54 (1.74 s) / 1.85 / 1.37 ¹ | 41.9 (23 ms) / 161 / 159 |
| nbody `?n=1000` | 2.07k (0.44 ms) / 9.6k / 9.4k | 340 (2.5 ms) / 1.58k / 1.36k | — |

¹ 12 requests hit the 30 s request deadline at c = 64. compute is byte-identical to the Go service; Go's SHA-256 is
hand-written assembly, Ten's is portable code on 64-bit words (the JIT is 18× the interpreter, ≈ 4× slower than Go).

| what | result |
|---|---|
| apply → all 8 examples serving (fresh node; `k10s apply` process included; median of 7) | precompiled `.k10`: 5.1 ms (interp 4.9); `.ten` compiled by the CLI: 8.2 ms (interp 7.9); JIT compilation of the 8 modules: 0.7–0.8 ms in total |
| JIT compile time / machine code / executable mapping per module | hello 19 µs / 288 B / 4 KiB, echo 35 µs / 520 B / 4 KiB, chain 48 µs / 1.9 KiB / 4 KiB, counter 56 µs / 2.2 KiB / 4 KiB, compute 151 µs / 5.3 KiB / 8 KiB, sort 113 µs / 5.6 KiB / 8 KiB, todo 176 µs / 7.4 KiB / 8 KiB, nbody 134 µs / 8.4 KiB / 12 KiB (TIR: 52 B … 2.7 KiB) |
| instance creation (JIT / interp): empty module / data + 4 KiB `init()` / snapshot copied | 1.5 / 4.1 / 3.6 µs vs 1.2 / 4.4 / 3.5 µs |
| invocation through the scheduler (JIT / interp): empty `handle` / echo / same-shard `call` | 265 / 308 / 394 ns vs 251 / 356 / 469 ns |
| recursion depth of a one-line recursive function, 1 MiB fiber stacks | 20–30k frames (interpreter: 10 000) |
Reproduce: `K10S_BACKEND=jit|interp cargo run --release -p k10-node --example measure -- create|invoke`, and loadgen
against `k10s node --backend …` with the examples applied.

## Cluster mode
No control plane: membership and failure detection are SWIM gossip (`k10_mesh::Gossip`, UDP), the desired state (a
manifest + content-addressed code blobs) spreads by gossip, and placement is the pure function
`k10_mesh::place(manifest, live members)` that every node evaluates on its own. Self-healing falls out of membership
changes. Code: `cluster/mod.rs` (config, applier, operator apply, status), `cluster/mesh.rs` (mesh thread),
`cluster/router.rs` (inbox, `RemoteRouter`), `cluster/store.rs` (blobs, manifest, node id).

**Start-up.** `k10s node` runs in cluster mode when any of `--gossip ADDR` (UDP, default `0.0.0.0:7946`), `--rpc ADDR`
(TCP, default `0.0.0.0:7947`), `--join HOST:PORT[,…]` (gossip addresses of members) or `--advertise HOST` is given.
It refuses to start without `--cluster-key-file FILE` (the same `k10s keygen` key on every node) unless
`--insecure-unauthenticated-cluster` is given (see "Cluster authentication").
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
- **Joining and rejoining.** Give every node, the first one included, the same `--join a,b,c` list (a node skips
  its own address). Each node also keeps the gossip addresses of up to 16 live members in `DATA/peers` (previous
  ones kept while live, the rest random; stamped with the cluster key id, ignored on mismatch) and contacts them
  at start and whenever it is isolated, so a restarted node rejoins even with dead, moved or no seeds. Reconnects
  (one sync per second, round-robin, per-address backoff 1 s → 60 s) try seeds that are not live, dead members and
  tombstones (which keep the last address for an hour). The incarnation is persisted (`DATA/incarnation`) and
  starts above both the wall clock and the previous life's, so a clock stepped back cannot hide a restart.

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

**Placement.** Members advertise their placement version; `place` uses the lowest live one (version 2 charges
`2^memory + 4 KiB` per replica, version 1 `2^memory`), so mixed-version clusters agree during a rolling upgrade.
On every desired-state change and on every change of the live member set (ids + capacity + placement version; Alive and
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
request id (each side picks ids for its own requests; requests and responses are told apart by kind). With a
cluster key both sides start with the `Auth` handshake (see "Cluster authentication"); then both send
`Hello{member}`. A node dials members with a larger id as soon as they come up, and any member lazily on
first use; when both ends dialed at once, the connection dialed by the smaller id is kept and the other one is closed
by its dialer once its requests are answered. Frames are appended to the link's write buffer and written once per
mesh-loop iteration (batching; edge-triggered epoll, partial writes resume on `EPOLLOUT`). A failed dial (or one
not connected within 1 s) backs the peer off for 200 ms, doubling up to 5 s.
**Bounded buffering**: a link is read 64 KiB at a time and the frames are decoded after every read (the decoder
holds at most one partial frame), with at most 1 MiB read per link per mesh-loop iteration — a link with input
left is read again on the next iteration, so a peer streaming frames cannot monopolize the thread. Toward a slow
peer, the written prefix of a write buffer is dropped once it reaches 1 MiB, and a link holding more than 16 MiB of
unsent bytes gets no new calls (the router skips that peer like a backed-off one: next candidate, else −2;
`congested_skips`); a frame that would push it past 16 MiB + the maximum frame size (80 MiB: the peer stopped
reading) is dropped and the link closed, which fails or re-routes its outstanding calls like any lost connection,
and backs the peer off (`links_overflowed`). `/_k10s/status` reports each link's `unsent` bytes. Links to a member declared Dead are
closed; links to a member that Left stay open until the leaver closes them (it still answers what it was sent).

**Routing.** Shards hand invocations of `Remote` services (ingress requests and `call`s) to `MeshRouter::forward` →
mesh inbox. Candidates: (1) placed replicas that are Alive and not backed off — fewest outstanding requests on the
link, round-robin on ties; (2) the service's rendezvous ranking (Alive members, at most `replicas + 3` attempts in
total); if this node comes up in the ranking it runs the service itself (`NodeShared::serve_local`); (3) Suspect
replicas. `Call` frames carry the remaining deadline in ms and the request method (`req_method` code). The peer runs the call locally whatever its own
placement says — a request takes at most one hop — without checking any `calls` allowlist (the calling node did:
the peer is trusted to have checked, one more reason peer links must be authenticated), and answers `CallResult` (or `NOT_HERE` = −100, a peer-protocol
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
addresses, capacity), placement (service → node ids), links (peer, state, outstanding, bytes, unsent), counters
(gossip packets/bytes, forwarded, retries, failed, local fallback, served for peers, not-here, dial failures,
congested skips, links overflowed, accept pauses, peer bytes, fetches, applies, link auth failures, gossip datagrams
rejected: bad tag / stale / replayed), and `"auth"`: `{"mode":"key","key_id":…}` or `{"mode":"insecure"}`. Each service also reports its `rt` id and `placement` (`"local"` or node ids). `GET
/_k10s/cluster` renders the same as text; `k10s status` prints it in cluster mode (`--json` for the JSON).

**Tests** (`crates/k10s/tests/cluster.rs`, real `k10s node` processes on ephemeral localhost ports, `--probe-ms 100`):
join + convergence, apply on one node → every node applies and serves every service through its own ingress (local
or forwarded, methods preserved), `call` across nodes, 3 replicas on 3 distinct nodes, same placement everywhere,
`k10s status`; SIGKILL of the host of a single-replica service → served again, declared dead, re-placed; apply
while it is down, restart with the same data directory → same id, higher incarnation, catches up; graceful leave
and a join under load (4 → 3 → 4 nodes): 0 failed requests, services whose placement did not change keep their
runtime; re-applies under load on 5 nodes (0 failed requests) and two concurrent applies on two nodes (one winner
everywhere); the origin of a desired state crashes and a node joining later fetches it from the other members; a
call the caller's `calls` does not grant is answered −1 locally and never forwarded; memory reservations (3 nodes
with `--mem 1MiB`, 45 services of 64 KiB: exactly 15 placed and reserved per node).
`crates/k10s/tests/chaos.rs` faults every node, the seed included (SIGKILL, SIGSTOP, leave + rejoin, apply then kill
of its origin, restarts with live-member, static or no `--join`, on the same or new ports) and checks membership,
version and placement convergence and 0 errors through every ingress after each round.
Unit tests: store (persist/reload/corruption/gc/pinning, node id, peers), ranking vs `place`; node test:
`apply_cluster` ordering and placement-only updates. `cargo test -p k10s --test cluster` takes ≈ 2–4 s.
`crates/k10s/tests/mesh_abuse.rs`: a hostile non-UTF-8 call path; a fake peer that requests ≈ 1 GB of replies and
never reads (the link is closed at the cap, peak RSS stays < 100 MB, forwarding unaffected); a node started with
`RLIMIT_NOFILE = 64` whose HTTP and peer listeners both hit `EMFILE` (≈ 0 CPU while out of descriptors, two log
lines, both serve again afterwards).

### Cluster authentication
Every member holds the same 32-byte cluster key (`k10_mesh::secure`; hand-written `no_std` crypto in
`k10_mesh::crypto`, checked against the RFC 8439 / 5869 / 4231 and XChaCha draft vectors). HKDF-SHA256 derives a
gossip key, a peer key and a public key id (`"auth"."key_id"` in status: equal on nodes sharing a key).
- **Gossip**: each datagram is sealed with XChaCha20-Poly1305,
  `'K' | 1 | nonce[24] = boot[8] (random per process) ctr[8] ts_ms[8] | ciphertext | tag[16]` (42 bytes; gossip's
  `max_packet` shrinks by as much, so datagrams stay ≤ 1400 bytes). A receiver drops a datagram before decoding it
  when the tag fails (another key, plaintext, forgery), when `ts` is more than `--gossip-max-skew` (300 s; 0 = off)
  from its own clock, or when it is a replay: per sender it keeps the newest boot (a later `ts` replaces it) and a
  64-counter sliding window. SWIM incarnations and desired-state versions are monotonic as well, so nothing can be
  rolled back; a desired state can only come from a key holder, and the manifest and blobs it names are still checked
  against their SHA-256 when fetched.
- **Peer links**: each side sends `Auth` (kind 11, in clear: version, node id, 32 fresh random bytes from
  `getrandom`), then derives one key per direction,
  `k(A→B) = HKDF-Expand(HKDF-Extract("k10s link v1", peer key), id_A | id_B | nonce_A | nonce_B)`. Frames then
  travel in sealed records, `len:u32 | ChaCha20-Poly1305(frames) | tag[16]` (`len` counts the tag and is
  authenticated), with the record counter as nonce: one record per write (the frames a loop iteration queued for that
  peer; records close at 64 KiB, a larger frame travels alone), so crypto costs one seal per batch, not per frame.
  The first record each side receives starts with the peer's `Hello`, whose id must match its `Auth`: opening it
  proves the peer holds the key and agrees on both nonces (mutual challenge–response).
  Fresh nonces give fresh keys per connection; per-direction keys stop reflection; the implicit counter stops
  injection, replay, reordering and dropping — any failure closes the link (`link_auth_failures`; a dialed peer is
  backed off). Before `Auth` the decoder accepts nothing larger than an `Auth` frame, and a link that has not
  completed the handshake within 5 s is closed. A peer claiming this node's id, or a dialed link reaching another
  node, is refused. Records queued during the handshake wait unsealed at the end of the write buffer (not written)
  and are sealed in place once the peer's `Auth` arrives; afterwards records are sealed in place when written and
  opened in place in the decoder's buffer (no extra copy). One handshake round trip is added per connection.
- Cost: ChaCha20 runs four blocks per batch with SSE2 intrinsics on hosted x86-64 (the only `unsafe` in `k10-mesh`;
  LLVM does not vectorize the rotations itself; portable interleaved lanes on metal), fused with Poly1305; a short
  record costs one batch. Sealing per record rather than per frame keeps it to one seal/open per write: the first
  per-frame version cost ≈ 2 µs of mesh CPU per forwarded request (−8 % throughput at c = 64 on the loaded host).
  Numbers: `cargo bench -p k10-mesh --bench crypto` and the table below.
- `--insecure-unauthenticated-cluster` keeps the old plaintext protocol (no `Auth`, no sealing) and prints a warning;
  it does not interoperate with keyed nodes (each side drops the other's datagrams and links).

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
| cluster authentication: crypto (`cargo bench`, host load ≈ 10–20) | seal or open of a ≤ 128 B record 0.3–0.5 µs; 1 MiB 400–480 MB/s (ChaCha20 SSE2 0.8–0.9 GB/s, Poly1305 0.9–1.2 GB/s, portable lanes 0.2–0.3 GB/s); gossip datagram (1400 B, XChaCha20) 3.4–4.5 µs |
| cluster authentication vs 08f2921 (3 nodes × 2 shards, echo, 10 interleaved rounds, host load 9–22; medians) | forwarded c=1 p50 156 / 156 µs; forwarded c=64 51k / 58k rps (spread 9–101k: noise); open loop 4000 rps forwarded p50 700 / 752 µs; mesh-thread CPU per forwarded request +5.8 µs at c=1 (four cold-cache seal/open), +0.4 µs at c=64 (records batch); node CPU per request +6 % / +1 %; local ingress c=1 p50 45 / 46 µs, CPU per request +2 % (noise) |
| idle 5-node cluster (2 shards each), no services / 20 services × 2 replicas | RSS 2.6–4.7 / 2.8–4.9 MiB per node; CPU ≈ 0.1 % of a core; gossip ≈ 300–350 B/s out and in per node |

A forwarded request crosses six thread hand-offs (shard → mesh → peer mesh → peer shard → peer mesh → mesh → shard),
each an eventfd/epoll wake-up on a busy machine: that, not the network or the framing, is most of the ≈ 70–90 µs.
Reproduce: `cargo test --release -p k10s --test cluster -- --ignored --nocapture --test-threads 1 measure_`
(`K10S_SPIN_US=50` for the spin variant; needs `bench/bin/loadgen`).

## Known limitations
- No TLS; no chunked request bodies; responses always use `Content-Length`.
- Cluster: one shared key authenticates every member; any key holder is fully trusted (no per-node identity,
  revocation = re-keying every node, no forward secrecy), and bundles are not signed. KV is node-local (replicas of a service, and a service that moves, see different data). No consensus: two concurrent
  applies resolve by `(version, hash)`, the loser's change is dropped silently; an operator apply on a node that has
  not yet learned the cluster's latest version can likewise lose. An apply whose node dies before any peer fetched
  it stays pending (peers retry every second) until that node restarts with its data, or a newer apply supersedes
  it (chaos-tested: kill right after apply, then restart). A service unplaced for lack of memory, or whose
  hosts are all down, is run by the next node of its ranking (or this node) from that node's shared pool (no
  reservation: it may get 503 when the pool is full).
  Forwarding costs six thread hand-offs (see measurements). Non-UTF-8 call paths are converted lossily on the wire.
  Peer links have no application-level keepalive (gossip detects dead peers).
- KV is node-local with an asynchronous durability window (no fsync-before-ack mode yet); compaction serializes the
  whole map under its lock (pauses `kv_*` calls on that service for the copy). `kv_add`/`kv_cas` are atomic on one
  node only: in cluster mode each replica's node has its own namespace.
- A host function that blocks (none does today except brief KV mutexes) would block its shard.
- CPU accounting uses wall time while a fiber runs (an OS-level preemption of the shard thread is charged to the
  invocation); the budget is enforced with ≈ one-slice precision.
- Coroutine stacks are 1 MiB (`MAP_NORESERVE`) with a guard page: a runaway *native* recursion in host code would
  SIGSEGV the process (TIR recursion is bounded by the executors: JIT frames by the stack limit, the interpreter by its
  own stacks). Stack pages a deep (but not overflowing) JIT recursion touched stay committed until the fiber's stack
  is trimmed (after a `StackOverflow` trap) or released (idle fibers beyond 64 per shard): up to 1 MiB per busy fiber.
- `k10_ir::InterpStack` cannot be shrunk; a fiber's interpreter buffers are dropped only after a `StackOverflow`
  trap or when the fiber's stack is trimmed.
