# k10s results

```
date: 2026-10-20T00:08:36+0200
host_kernel: 6.1.0-53-amd64
host_cpu: Intel(R) Core(TM) i7-7820HQ CPU @ 2.90GHz
host_threads: 8
host_mem_gib: 62.7
loadavg: [1.21, 1.08, 1.53]
k10s_binary: /home/qgerard/dev/k10s/bench/.cache/k10s/bin/k10s
k10s_binary_bytes: 11595032
k10s_build: {'source': '/home/qgerard/dev/k10s/target/node/release/k10s', 'source_mtime': '2026-10-19T21:19:31+0200', 'copied_at': '2026-10-19T23:48:59+0200', 'git_rev': '412ad4f', 'crates_dirty_at_copy': False, 'sha256': '3431263df371baf9', 'crates_rs_files_newer_than_binary': 0}
note: k10s node processes on the host kernel (not inside the Docker Desktop VM used by the kind baseline)
shards_single: 8
shards_cluster_node: 2
cgroup_scopes: True
chain_module: examples chain.ten
```

Every k10s node runs in its own cgroup (`systemd-run --user --scope`): working set = memory.current - inactive_file, as for the kind nodes. `noisy` = runs whose 1-min load average exceeded the threshold (shared machine).

## Idle footprint (60 s windows, median of repeats)

| config | services | working set MiB | RSS MiB | PSS MiB | CPU % of one core | threads/node | noisy |
|---|---|---|---|---|---|---|---|
| single-1 | 0 | 0.45 (0.45-0.45) | 2.79 (2.78-2.79) | 1.33 (1.32-1.33) | 0.014 (0.014-0.014) | 4 | 0/3 |
| single-1 | 4 | 1.95 (1.95-1.95) | 3.36 (3.36-3.36) | 1.72 (1.72-1.72) | 0.015 (0.015-0.016) | 4 | 0/3 |
| single-8 | 0 | 1.30 (1.30-1.30) | 3.48 (3.47-3.48) | 2.02 (2.02-2.02) | 0.102 (0.094-0.104) | 11 | 0/3 |
| single-8 | 4 | 1.53 (1.52-1.54) | 4.03 (4.02-4.04) | 2.39 (2.38-2.39) | 0.105 (0.101-0.105) | 11 | 2/3 |
| cluster-3 | 0 | 6.44 (6.41-6.45) | 13.82 (13.79-13.83) | 6.84 (6.80-6.85) | 0.569 (0.567-0.570) | 7 | 0/3 |
| cluster-3 | 4 | 8.19 (8.16-8.20) | 14.80 (14.77-14.82) | 7.40 (7.37-7.42) | 0.565 (0.564-0.566) | 7 | 3/3 |

`single-N`: one node with N shards; `cluster-3`: 3 nodes on localhost (sums over the nodes). services = the 4 reference services with one warm instance each.

## Per-workload overhead (memory delta / unit, measured 20 s after the instances exist)

| variant | N | units measured | working set KiB/unit | RSS KiB/unit | PSS KiB/unit | of which service state (ws) | of which instance (ws) | apply ms | apply CLI-side ms | noisy |
|---|---|---|---|---|---|---|---|---|---|---|
| instances | 1 | 1 | 28.00 | 24.00 | 24.00 | - | - | - | - | 3/3 |
| instances | 10 | 10 | 18.40 | 12.00 | 12.00 | - | - | - | - | 3/3 |
| instances | 50 | 50 | 13.68 | 9.92 | 9.92 | - | - | - | - | 3/3 |
| instances | 100 | 100 | 12.72 | 9.60 | 9.60 | - | - | - | - | 3/3 |
| instances | 1000 | 998 | 8.97 | 7.38 | 7.38 | - | - | - | - | 3/3 |
| services | 1 | 1 | 92.00 | 452.00 | 261.00 | 64.00 | 32.00 | 13 | 3 | 3/3 |
| services | 10 | 10 | 22.40 | 56.40 | 40.50 | 9.20 | 13.60 | 13 | 4 | 3/3 |
| services | 50 | 50 | 12.00 | 19.20 | 15.94 | 4.00 | 7.92 | 17 | 5 | 3/3 |
| services | 100 | 100 | 10.16 | 13.80 | 12.53 | 3.28 | 6.92 | 17 | 5 | 3/3 |
| services | 1000 | 1,000 | 7.87 | 8.06 | 7.93 | 2.73 | 5.14 | 29 | 17 | 3/3 |
| services | 10000 | 10,000 | 7.51 | 7.20 | 7.19 | 2.58 | 4.92 | 205 | 131 | 1/1 |
| services | 100000 | 100,000 | 7.36 | 7.22 | 7.22 | 2.40 | 4.96 | 1,712 | 1,447 | 1/1 |

`instances`: one service (`hold`, echo + spin) with N live instances forced by N concurrent requests (units = instances actually live). `services`: N echo services, one request each (= one instance each); per-unit = (service state + instance). `apply CLI-side ms` = the same `k10s apply` against an unreachable node (parsing + loading the code files).

## Startup: `k10s apply` of N services -> every service answers 200 (ms)

| where | N services | `k10s apply` returns | first service answers | all answer (every node) | not serving | noisy |
|---|---|---|---|---|---|---|
| single | 1 | 18.5 (16.0-38.1) | 23.7 (18.0-41.9) | 23.7 (18.0-41.9) | 0 | 3/3 |
| single | 10 | 15.3 (13.2-18.0) | 20.5 (18.9-27.6) | 20.5 (18.9-27.6) | 0 | 3/3 |
| single | 50 | 16.5 (16.2-16.7) | 21.4 (21.4-22.8) | 21.4 (21.4-22.8) | 0 | 3/3 |
| single | 100 | 18.4 (11.9-21.9) | 28.3 (16.7-39.4) | 28.3 (16.7-39.5) | 0 | 3/3 |
| single | 1000 | 38.1 (34.2-38.1) | 50.8 (45.8-50.9) | 50.9 (45.8-51.1) | 0 | 3/3 |
| cluster-3 | 1 | 19.2 (13.8-24.2) | 21.5 (16.1-28.3) | 22.7 (17.2-30.4) | 0 | 3/3 |
| cluster-3 | 10 | 19.8 (19.6-21.8) | 25.1 (23.6-27.3) | 30.1 (27.4-31.8) | 0 | 3/3 |
| cluster-3 | 100 | 22.1 (17.8-22.5) | 29.2 (25.4-47.6) | 37.5 (34.3-63.6) | 0 | 3/3 |

### Cold start: first request to a never-instantiated service vs second request (ms, client-side, Python)

| module | cold, new conn | warm, new conn | cold, keep-alive | warm, keep-alive | cold - warm (keep-alive) | server: init ms/instance |
|---|---|---|---|---|---|---|
| compute | 0.175 | 0.175 | 0.052 | 0.050 | 0.003 | 0.0038 |
| echo | 0.200 | 0.190 | 0.050 | 0.046 | 0.004 | 0.0035 |

## Request latency/throughput through the k10s ingress (20 s, closed loop, median of repeats)

`single-8` = one node, 8 shards; `cluster-3-local` = echo through the node that hosts it; `cluster-3-forwarded` = through another node (one peer hop). SUT CPU = cgroup CPU of the node(s) over the measured window / requests.

| loadgen at | target | conns | rps | p50 µs | p90 µs | p99 µs | p99.9 µs | max µs | errors | loadgen cores | SUT CPU µs/req |
|---|---|---|---|---|---|---|---|---|---|---|---|
| cluster-3-forwarded | echo | 1 | 9,343 | 99 | 129 | 201 | 532 | 11,455 | 0 | 0.31 | 93.1 |
| cluster-3-forwarded | echo | 16 | 67,288 | 219 | 314 | 596 | 1,432 | 16,569 | 0 | 2.06 | 38.6 |
| cluster-3-forwarded | echo | 64 | 106,959 | 548 | 876 | 1,480 | 2,864 | 17,279 | 0 | 2.58 | 26.8 |
| cluster-3-local | echo | 1 | 29,364 | 29 | 42 | 73 | 195 | 11,599 | 0 | 0.72 | 18.6 |
| cluster-3-local | echo | 16 | 117,568 | 127 | 197 | 334 | 716 | 13,243 | 0 | 3.21 | 14.9 |
| cluster-3-local | echo | 64 | 127,396 | 466 | 772 | 1,128 | 2,024 | 21,183 | 0 | 3.06 | 13.6 |
| single-8 | echo | 1 | 29,157 | 29 | 42 | 72 | 187 | 8,699 | 0 | 0.72 | 18.7 |
| single-8 | echo | 16 | 135,685 | 99 | 197 | 422 | 980 | 18,395 | 0 | 3.40 | 20.3 |
| single-8 | echo | 64 | 179,708 | 225 | 748 | 1,848 | 3,824 | 20,384 | 0 | 3.60 | 17.3 |
| single-8 | chain | 1 | 27,888 | 31 | 44 | 77 | 199 | 12,650 | 0 | 0.69 | 20.3 |
| single-8 | chain | 16 | 127,128 | 109 | 209 | 458 | 1,112 | 16,161 | 0 | 3.23 | 23.0 |
| single-8 | chain | 64 | 168,187 | 239 | 796 | 2,064 | 4,448 | 23,454 | 0 | 3.35 | 19.1 |
| single-8 | compute | 1 | 1,132 | 844 | 1,020 | 1,480 | 1,848 | 7,804 | 0 | 0.07 | 831.6 |
| single-8 | compute | 16 | 4,856 | 2,896 | 5,728 | 8,512 | 20,352 | 52,883 | 0 | 0.19 | 1,336.2 |
| single-8 | compute | 64 | 4,961 | 11,968 | 18,048 | 39,168 | 80,384 | 194,464 | 0 | 0.19 | 1,413.1 |

noisy runs: 45/45; CPU used by other processes during the windows: median 0.76, max 1.24 cores

## Open-loop latency (fixed rate, 64 conns, 20 s)

| loadgen at | target | rate | achieved rps | p50 µs | p99 µs | p99.9 µs | p50 µs (jc) | p99 µs (jc) | p99.9 µs (jc) | errors | SUT CPU µs/req | noisy runs |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| single-8 | chain | 5,000 | 5,000 | 358 | 1,560 | 5,856 | 189 | 572 | 4,448 | 0 | 89.6 | 3/3 |
| single-8 | chain | 20,000 | 20,000 | 126 | 1,320 | 5,088 | 81 | 604 | 3,600 | 0 | 40.5 | 3/3 |
| single-8 | compute | 500 | 500 | 1,976 | 5,216 | 9,536 | 1,592 | 4,576 | 7,520 | 0 | 1,555.4 | 3/3 |
| single-8 | compute | 2,000 | 2,000 | 1,368 | 3,696 | 10,176 | 900 | 3,152 | 9,536 | 0 | 964.6 | 3/3 |
| single-8 | echo | 5,000 | 5,000 | 358 | 1,544 | 4,512 | 185 | 540 | 2,448 | 0 | 85.0 | 3/3 |
| single-8 | echo | 20,000 | 20,000 | 125 | 1,320 | 3,792 | 86 | 732 | 3,088 | 0 | 41.5 | 3/3 |

`jc` = jitter-corrected: latency from the loadgen's actual wake-up when it slept until the intended send time (Go timers wake up to ~1 ms late), from the intended time when it was behind schedule (queueing in the system under test stays counted).

## Failover

| scenario | event -> first success ms | event -> 100% success ms | outage window ms | failed probes | suspected ms | declared dead / re-placed ms |
|---|---|---|---|---|---|---|
| instance-trap | 1.59 (1.59-1.60) | 1.59 (1.59-1.60) | 0 (0-0) | 0 (0-0) (+ the trapping request) | - | - |
| kill-1-of-2-under-300rps | - | - | - | 0 (0-1) errors of ~3601 reqs | - | - |
| node-kill | 12 (9-16) | 12 (9-16) | 0 (0-0) | 0 (0-0) | 288 (230-424) | 1,088 (1,045-1,218) / 1,154 (1,089-1,280) |
| node-stop | 513 (510-514) | 513 (510-514) | 0 (0-0) | 1 (1-1) | 234 (224-391) | 1,040 (1,024-1,187) / 1,082 (1,066-1,253) |

## Rolling update under probe + 1000 req/s load

| mode | replicas | `k10s apply` ms | new version served everywhere ms | old version gone ms | failed probes | load errors (of ~11k) | noisy |
|---|---|---|---|---|---|---|---|
| cluster-3 | 10 | 15.0 (10.6-20.2) | 22.3 (20.1-33.0) | 23.8 (22.9-36.1) | 0 (0-0) | 0 (0-0) | 6/6 |
| single-8 | 1 | 15.2 (9.4-17.4) | 22.0 (16.6-24.4) | 22.6 (17.2-24.9) | 0 (0-0) | 0 (0-0) | 6/6 |

## Artifact sizes

| artifact | size |
|---|---|
| module hello.k10 (TIR) | 52 B |
| module echo.k10 (TIR) | 157 B |
| module chain.k10 (TIR) | 641 B |
| module compute.k10 (TIR) | 2,305 B |
| module counter.k10 (TIR) | 815 B |
| source hello.ten | 343 B |
| source echo.ten | 731 B |
| source chain.ten | 1,303 B |
| source compute.ten | 4,217 B |
| source counter.ten | 1,167 B |
| JIT machine code hello (code / executable mapping) | 288 B / 4,096 B |
| JIT machine code echo (code / executable mapping) | 520 B / 4,096 B |
| JIT machine code chain (code / executable mapping) | 1,976 B / 4,096 B |
| JIT machine code compute (code / executable mapping) | 5,464 B / 8,192 B |
| JIT machine code counter (code / executable mapping) | 2,672 B / 4,096 B |
| k10s binary (as built: release + debuginfo) | 11.60 MB |
| k10s binary stripped | 1.68 MB |
| k10s dynamic deps | linux-vdso.so.1, libgcc_s.so.1, libc.so.6, /lib64/ld-linux-x86-64.so.2 |
| node data dir after the reference services + 400 requests | 6,549 B |
