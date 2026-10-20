# k10s host API (v1)

The only way a workload interacts with anything outside its memory slot. A module imports these by name; the node
rejects modules that import unknown names or with the wrong parameter count. Every host function returns exactly one
i64 (`results = 1`). Pointer/length arguments are addresses in the instance's memory; ranges are masked and checked
exactly like `mem.copy` (out-of-range → trap `MemOutOfBounds`).

## Exports a service module provides
- `handle()` (0 params, 0 or 1 result) — called once per request. Required.
- `init()` (0 params, 0 or 1 result) — optional; called once when an instance is created, before it serves.
  For `fresh` services the memory state after `init` is snapshotted and every request starts from that snapshot.

An instance handles one request at a time (actor semantics); the runtime runs many instances in parallel.

## Request / response
| name | params | returns |
|---|---|---|
| `input_len` | () | length of the request body |
| `input_read` | (dst, off, n) | copies body bytes `[off, off+n)` (clamped to the body) to `dst`; returns bytes copied |
| `req_path` | (dst, max) | copies the request path suffix (ingress: the part after `/<service>`; calls: the path given by the caller, may be empty) to `dst`, at most `max` bytes; returns the full length |
| `req_method` | () | 0 GET, 1 POST, 2 PUT, 3 DELETE, 4 other. Service-to-service calls are POST |
| `output_write` | (src, n) | appends `n` bytes to the response body; returns `n`. Body limit 16 MiB → trap `Host(1)` |
| `output_status` | (code) | sets the response status (default 200); returns 0 |

## Service-to-service calls
| name | params | returns |
|---|---|---|
| `call` | (name_ptr, name_len, req_ptr, req_len) | Invokes service `name` (optionally `name/path`) with the given body. Suspends the caller until the response arrives. Returns the callee's status (≥ 0) or an error: -1 unknown service, -2 unavailable, -3 timeout, -4 callee trapped |
| `call_resp_len` | () | length of the last call's response body |
| `call_resp_read` | (dst, off, n) | copies bytes of the last call's response; returns bytes copied |

## Durable key-value store (namespace = the service; node-local in v1)
| name | params | returns |
|---|---|---|
| `kv_get` | (kptr, klen, dst, max) | value length, or -1 if absent; copies `min(len, max)` bytes to `dst` |
| `kv_put` | (kptr, klen, vptr, vlen) | 0 |
| `kv_del` | (kptr, klen) | 1 if the key existed, else 0 |
| `kv_add` | (kptr, klen, delta) | the new value (see below) |
| `kv_cas` | (kptr, klen, buf, exp_len, new_len) | 1 if the value was replaced, 0 if it did not match (see below) |

Keys ≤ 1 KiB, values ≤ 1 MiB (else trap `Host(2)`). Each service has a live-bytes quota (default 256 MiB,
`--kv-max`); a write (`kv_put`, `kv_add`, `kv_cas`) that would exceed it traps `Host(3)` and stores nothing (delete
entries to make room).

**Atomicity.** All `kv_*` calls on one namespace are linearizable on a node: each one takes effect at a single
point, whatever shard or instance makes it, and they are logged (made durable) in that order. `kv_add` and
`kv_cas` are atomic read-modify-writes, so concurrent instances of a service (up to `max_instances`) do not lose
updates. A `kv_get` followed by a `kv_put` is *not* atomic. In v1 a namespace is node-local: replicas of a service
on different cluster nodes do not share it.

**`kv_add(kptr, klen, delta)`** reads the value as an 8-byte little-endian i64 (an absent key counts as 0), adds
`delta` with wrapping two's-complement arithmetic, stores the 8-byte result and returns it. `delta = 0` only
reads: nothing is written and an absent key stays absent. If the key holds a value that is not exactly 8 bytes long,
it traps `Host(4)` and changes nothing.

**`kv_cas(kptr, klen, buf, exp_len, new_len)`** (compare-and-swap; TIR host functions take at most 5 parameters,
so the expected and new values share one buffer): `buf` holds the expected value followed immediately by the new
value, `expected = buf[0 .. e)` and `new = buf[e .. e + n)` with `e = max(exp_len, 0)`, `n = max(new_len, 0)`.
- `exp_len ≥ 0`: the key must currently hold exactly `expected`; `exp_len = -1`: the key must be absent.
- `new_len ≥ 0`: on a match the value becomes `new`; `new_len = -1`: on a match the key is deleted.
- Returns 1 if the expectation held (and the write or delete was done), else 0 (nothing changed).
- Checks, in order: the key range `(kptr, klen)` and `(buf, e + n)` like `mem.copy` (and `exp_len` or `new_len`
  below -1 → trap `MemOutOfBounds`); key > 1 KiB, `e` or `n` > 1 MiB → trap `Host(2)`; a matching write over the
  quota → trap `Host(3)` (nothing changed).

A read-modify-write of any value is a loop: read the value (`kv_get`), compute the new one after it in the same
buffer, `kv_cas`; on 0 another instance changed it meanwhile, so read again.

## Misc
| name | params | returns |
|---|---|---|
| `log` | (ptr, len) | 0; appends a line to the service log |
| `now_ns` | () | monotonic nanoseconds, in steps of the node's guest-clock resolution (default 100 µs) |
| `time_ms` | () | wall-clock Unix milliseconds |

The guest clock is coarse on purpose (SECURITY.md, "CPU side channels"): `now_ns` advances in steps of the
resolution (`k10s node --timer-res-us`, k10-metal `k10.timer_res_us`; 0 = full nanosecond resolution), each step
landing at a random point of its interval, so it is monotonic and at most one step ahead of real time; `time_ms`
gets the same treatment at 1 ms (or the resolution, if coarser).
| `random` | () | 64 random bits |
| `config` | (dst, max) | copies the service's config bytes (from the manifest); returns the full length |
