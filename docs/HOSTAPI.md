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

Keys ≤ 1 KiB, values ≤ 1 MiB (else trap `Host(2)`). Each service has a live-bytes quota (default 256 MiB,
`--kv-max`); a `kv_put` that would exceed it traps `Host(3)` and stores nothing (delete entries to make room).

## Misc
| name | params | returns |
|---|---|---|
| `log` | (ptr, len) | 0; appends a line to the service log |
| `now_ns` | () | monotonic nanoseconds |
| `time_ms` | () | wall-clock Unix milliseconds |
| `random` | () | 64 random bits |
| `config` | (dst, max) | copies the service's config bytes (from the manifest); returns the full length |
