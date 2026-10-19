# k10s benchmarks: baselines

Baselines to compare k10s against (see `docs/DESIGN.md` section 5): real Kubernetes (kind) and bare Linux processes
running the same workloads. Everything is scripted, idempotent and re-runnable; results land in `bench/results/`.

## Layout
- `loadgen/` HTTP/1.1 load generator, stdlib only, raw `net.Conn` client (same cheap client for every system under test).
  Closed loop (`-conns N`) or open loop (`-rate R`, latency measured from the *intended* send time, so no coordinated omission;
  `-conns` then bounds in-flight requests). Log-linear histogram (~1% error), JSON on stdout: requests, errors by kind,
  rps, latency p50/p90/p99/p99.9/max in µs, loadgen CPU seconds. Example:
  `bench/bin/loadgen -addr 127.0.0.1:8080 -path / -conns 64 -duration 20s -warmup 3s`
  (`-method POST -body xyz`, `-rate 2000`, `-timeout`, `-label`). Open-loop timing uses `time.Sleep`, so expect ~100-500 µs of
  scheduler wake-up jitter at low rates.
- `workloads/` static `CGO_ENABLED=0` Go servers (`echo`, `compute`, `chain`, `counter`), `FROM scratch` images
  `k10s-bench/<name>:v1` (+ `echo:v2`, same binary, used for the rolling update). `workloads/build.sh` builds `bench/bin/*` and the images.
  - `echo`: GET -> `hello`, POST -> echoes body. `compute`: `/?n=N` N chained sha256 rounds. `chain`: GET -> `BACKEND_URL`
    (default `http://echo:8080/`; k8s manifest uses `echo.default.svc.cluster.local`). `counter`: increments a counter stored in `COUNTER_FILE`
    (default `/data/counter`, pwrite without fsync). All use `PORT` (default 8080).
- `k8s/` kind scripts (see below). `native/` bare-process scripts. `summarize.py` builds the `SUMMARY.md` files.

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
