# k10s benchmarks: baselines

Baselines to compare k10s against (see `docs/DESIGN.md` section 5): real Kubernetes (kind) and bare Linux processes
running the same workloads. Everything is scripted, idempotent and re-runnable; results land in `bench/results/`.

## Layout
- `loadgen/` HTTP/1.1 load generator, stdlib only, raw `net.Conn` client (same cheap client for every system under test).
  Closed loop (`-conns N`) or open loop (`-rate R`, latency measured from the *intended* send time, so no coordinated omission;
  `-conns` then bounds in-flight requests). Log-linear histogram (~1% error), JSON on stdout: requests, errors by kind,
  rps, latency p50/p90/p99/p99.9/max in µs, loadgen CPU seconds. Open loop also reports `latency_us_jc` (jitter-corrected: a sender
  that slept until the intended time is timed from its actual wake-up, one that was behind schedule from the intended time, so
  queueing in the system under test stays counted) and `wakeup_late_us` (Go timers wake up to ~1 ms late). Example:
  `bench/bin/loadgen -addr 127.0.0.1:8080 -path / -conns 64 -duration 20s -warmup 3s`
  (`-method POST -body xyz`, `-rate 2000`, `-timeout`, `-label`). Open-loop timing uses `time.Sleep`, so expect ~100-500 µs of
  scheduler wake-up jitter at low rates.
- `workloads/` static `CGO_ENABLED=0` Go servers (`echo`, `compute`, `chain`, `counter`), `FROM scratch` images
  `k10s-bench/<name>:v1` (+ `echo:v2`, same binary, used for the rolling update). `workloads/build.sh` builds `bench/bin/*` and the images.
  - `echo`: GET -> `hello`, POST -> echoes body. `compute`: `/?n=N` N chained sha256 rounds. `chain`: GET -> `BACKEND_URL`
    (default `http://echo:8080/`; k8s manifest uses `echo.default.svc.cluster.local`). `counter`: increments a counter stored in `COUNTER_FILE`
    (default `/data/counter`, pwrite without fsync). All use `PORT` (default 8080).
  `compute-purego` (native only) = `compute` built with `-tags purego` (Go's portable SHA-256 instead of its hand-written assembly), to separate the
  language/codegen gap from the platform gap.
- `k8s/` kind scripts (see below). `native/` bare-process scripts. `k10s/` the k10s suite (see below). `summarize.py` builds the `SUMMARY.md` files,
  `compare.py` builds `results/COMPARISON.md` (all three side by side, ratios, caveats; closed-loop cells use the repeat with the least foreign CPU,
  `COMPARE_STAT=median` for medians). `run-final.sh` runs everything on a quiet machine.

## Running
```
bench/k8s/run-all.sh        # ~1 h: build, single-node suite, 3-node suite, SUMMARY.md, deletes the kind clusters
bench/native/run-all.sh     # ~7 min: bare processes
SKIP_THREE=1 bench/k8s/run-all.sh   # skip the 3-node part (idle + worker failure, ~15 min)
QUICK=1 bench/k8s/run-all.sh        # smoke test, a few minutes, numbers meaningless
```
Requirements: docker, kind, kubectl, Go, python3, no root. Free disk >= 3 GB on /home is enforced (`check_disk` aborts).
Single scripts (each needs the cluster: `k8s/create-cluster.sh && k8s/load-images.sh`; `VARIANT=three` for the 3-node cluster;
`REPEATS=3`; `RESULTS=dir`): `idle.sh pods.sh first-response.sh latency.sh failover.sh rollout.sh sizes.sh nodefail.sh delete-cluster.sh`.
The single (`k10s-bench`) and three-node (`k10s-bench3`) clusters publish the same host ports 30080-30082 so they cannot coexist.
The scripts use private kubeconfigs under `results/.kubeconfig-*` (your `~/.kube/config` is never touched) and only ever delete clusters named `k10s-bench*`.

Kind node image is pinned: `kindest/node:v1.34.0@sha256:7416a61b...` (Kubernetes v1.34.0, containerd 2.1.3, kind v0.30.0).

### k10s suite (`k10s/`)
`k10s/run-all.sh` (~1 h; `QUICK=1` smoke test; `SKIP_BUILD=1`, `K10S_SCRIPTS="idle latency"` to run a subset) builds `target/node/release/k10s`,
copies it to `.cache/k10s/bin` (provenance in `k10s.info`: so a concurrent rebuild cannot change it mid-run), compiles `examples/*.ten` and
`k10s/ten/*.ten` to `.k10`, and runs, each writing `results/k10s/<name>.json` (every record carries the 1-min load average and a `noisy` flag):
- `idle.sh`: 60 s windows × REPEATS after a settle period, single node (8 shards, 1 shard) and 3 nodes (2 shards each), with 0 services and with
  the 4 reference services (one warm instance each). Each node runs in its own cgroup (`systemd-run --user --scope`, unit `k10sbench-*`):
  working set = `memory.current - inactive_file` exactly as for the kind nodes; RSS/PSS from smaps_rollup; CPU from the cgroup's `cpu.stat`.
- `overhead.sh`: per-workload memory (k8s `pods.sh` analogue) two ways: one service with N live instances (`ten/hold.ten`: echo that spins 50 ms,
  forced by N concurrent requests, node run with `--idle-timeout 3600`) and N echo services with one instance each, N = 1..1000; density to 100k services.
- `startup.sh`: fresh node, `k10s apply` of N services (precompiled `.k10`) -> every service answers (1 node and 3 nodes); instance cold start
  (first request to a never-instantiated service vs the second one).
- `latency.sh`: closed loop 1/16/64 conns × 20 s (+3 s warm-up) on echo, chain (`call`), compute?n=1000 (single node), echo local vs forwarded
  (3 nodes), open loop at fixed rates; CPU of the node cgroup(s) over the measured window -> µs/request.
- `failover.sh`: 3 nodes: SIGKILL / SIGSTOP of the node hosting a 1-replica service (probe = `k8s/probe.py` through another node), kill one of 2
  replicas' nodes under 300 req/s; single node: a request that traps (`ten/crash.ten`, the pod-delete analogue).
- `rollout.sh`: re-apply a new version (`ten/version.ten`, config v1 <-> v2) under the 20 ms probe + 1000 req/s, 1 node and 3 nodes × 10 replicas.
- `sizes.sh`: `.ten`/`.k10` sizes, JIT code per module, k10s binary (as built and stripped), data dir.
Ports: 127.0.0.1:28080 (single node), 28181-28183 / 28281-28283 (UDP gossip) / 28381-28383 (peer TCP) for the cluster (`K10S_PORT_BASE`).
`k10s/ten/chain.ten` is `examples/chain.ten` + `heap_reset()` (the example leaks a heap allocation per request, see the file header).
`k10s/ab.py [--same] BASE_BIN NEW_BIN LOADGEN OUT.jsonl [rounds] [dur_s] [port_base]` is an interleaved A/B of two `k10s` binaries (the order
alternates every round): single node (8 shards) echo, chain and compute at 1 and 64 conns, then 3 nodes (2 shards each) echo via the hosting
node and forwarded; every record carries rps, p50/p99, node CPU per request and the load average. The new binary runs with a `calls` grant
and a cluster key; `--same` gives the base the same manifest and key file when it supports them (read from its `help` and an offline `apply`).

Extensions of the k8s/native harness (defaults unchanged): `LAT_TARGETS="echo chain compute"` (k8s, adds `manifests/compute.yaml` on NodePort 30082)
and `LAT_TARGETS="echo chain compute compute-purego"` (native) add the compute targets; `k8s/latency-open.sh` and `native.py latency-open`
(or `OPEN_LOOP=1` in either `run-all.sh`) run the open-loop rates (`OPEN_RATES_<target>`, default 5000 and 20000 req/s for echo/chain, 500 and 2000
for compute, 64 conns); latency runs now also record the system-under-test CPU over the measured window (`sut_cpu_us_per_req` for kind: node
cgroup, loadgen's own CPU subtracted for the in-node loadgen; `server_cpu_us_per_req` for native: server processes' utime+stime).

### Final run
`bench/run-final.sh` (~3 h): builds, then for each suite (k8s, native, k10s) waits until the 1-min load average is below `LOAD_MAX` (1.0; aborts
after `LOAD_WAIT_MAX`, 2 h), runs it with the extensions above, and regenerates `results/COMPARISON.md`. Log: `results/.final.log`.

## Methodology and caveats (read before comparing)
- **Docker Desktop**: on this machine Docker is Docker Desktop, so the kind nodes run in a LinuxKit VM (kernel 6.10.14-linuxkit, 8 vCPU,
  15.6 GiB), not on the host kernel (6.1). Host cgroups of the nodes are therefore invisible; all node numbers are read from *inside* the node
  (`/sys/fs/cgroup/*` of the node container, `/proc/*/smaps_rollup` of every process). The VM's own kernel/dockerd overhead is *not* counted.
  Bare-process numbers run directly on the host. Host-side loadgen -> NodePort goes through Docker Desktop's port forwarder (that is
  the "ingress" a laptop user gets); `latency.sh` also runs loadgen *inside* the node (`place=node`) to separate that cost from kube-proxy/NodePort.
- Memory: "working set" = cgroup `memory.current - inactive_file` of the node container(s) (includes kernel/slab/page cache in use); RSS is
  summed per component and double counts pages shared between processes (containerd-shim, Go runtime), PSS does not. Per-pod overhead is the
  node-level difference before/after scaling divided by N, measured 20 s after Ready.
- Idle CPU comes from the node cgroup `cpu.stat` over 60 s windows (3 consecutive windows, medians), per component from `/proc/pid/stat` ticks.
- Startup: Deployments have **no readiness probe**, so "Ready" = container started (a probe would add up to its period). Times start just before
  `kubectl scale` (includes ~100-150 ms kubectl process time) and are polled at ~100 ms (20 ms for first-response).
- Failover probe: a fresh-connection GET every 20 ms from the host through the NodePort. Default `terminationGracePeriodSeconds`, default
  eviction timers (node-monitor-grace 40 s, toleration 300 s). Worker failure waits at most `NODEFAIL_CAP` (390 s) per mode.
- Preliminary numbers were taken while other agents compiled Rust concurrently: latency/throughput are noisy; redo on a quiet machine.

## Speculation-hardening costs (SECURITY.md, "CPU side channels")
`K10_BENCH_ONLY=brtable cargo bench -p k10-jit` (i7-7820HQ, loaded machine, minimum of round-robin samples):
- `br_table` dispatch, predictable index: ~4–5 ns per iteration, compare tree or clamped jump table alike; random
  index: trees win up to 7 target runs, tables from 9 (hence `br_tree_max` 8).
- Host call: ~3–6 ns with `--jit-indirect jump`, ~8–12 ns with `lfence`, ~13–15 ns with `retpoline`; jump-table
  dispatch ~5, ~8 and ~16 ns.
- Host-function clamps: within noise (±3 ns). `now_ns`/`time_ms` compute a division and a SipHash once per clock
  interval per shard (cached): nothing measurable per call.
