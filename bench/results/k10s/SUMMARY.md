# k10s results

```
date: 2026-10-19T19:43:13+0200
host_kernel: 6.1.0-53-amd64
host_cpu: Intel(R) Core(TM) i7-7820HQ CPU @ 2.90GHz
host_threads: 8
host_mem_gib: 62.7
loadavg: [8.59, 10.49, 9.69]
k10s_binary: /home/qgerard/dev/k10s/bench/.cache/k10s/bin/k10s
k10s_binary_bytes: 11455728
k10s_build: {'source': '/home/qgerard/dev/k10s/target/node/release/k10s', 'source_mtime': '2026-10-19T17:56:23+0200', 'copied_at': '2026-10-19T19:23:30+0200', 'git_rev': 'd4e67f3', 'crates_dirty_at_copy': False, 'sha256': '2f87b7d18e2bab90', 'crates_rs_files_newer_than_binary': 0}
note: k10s node processes on the host kernel (not inside the Docker Desktop VM used by the kind baseline)
shards_single: 8
shards_cluster_node: 2
cgroup_scopes: True
chain_module: bench chain.ten
```

Every k10s node runs in its own cgroup (`systemd-run --user --scope`): working set = memory.current - inactive_file, as for the kind nodes. `noisy` = runs whose 1-min load average exceeded the threshold (shared machine).

## Idle footprint (60 s windows, median of repeats)

| config | services | working set MiB | RSS MiB | PSS MiB | CPU % of one core | threads/node | noisy |
|---|---|---|---|---|---|---|---|
| single-1 | 0 | 2.44 (2.43-2.44) | 4.83 (4.68-4.89) | 3.29 (3.14-3.35) | 0.005 (0.005-0.005) | 4 | 3/3 |
| single-1 | 4 | 3.91 (3.90-3.91) | 5.35 (5.34-5.35) | 3.68 (3.68-3.68) | 0.007 (0.006-0.007) | 4 | 3/3 |
| single-8 | 0 | 1.30 (1.29-1.31) | 3.54 (3.46-3.61) | 1.86 (1.78-1.93) | 0.035 (0.035-0.037) | 11 | 3/3 |
| single-8 | 4 | 2.85 (2.84-2.86) | 4.17 (4.16-4.18) | 2.43 (2.42-2.44) | 0.038 (0.037-0.041) | 11 | 3/3 |
| cluster-3 | 0 | 2.45 (2.43-2.47) | 9.77 (9.74-9.79) | 2.82 (2.79-2.83) | 0.240 (0.208-0.253) | 7 | 3/3 |
| cluster-3 | 4 | 4.15 (4.11-4.19) | 10.57 (10.53-10.61) | 3.36 (3.32-3.40) | 0.220 (0.213-0.221) | 7 | 3/3 |

`single-N`: one node with N shards; `cluster-3`: 3 nodes on localhost (sums over the nodes). services = the 4 reference services with one warm instance each.

## Per-workload overhead (memory delta / unit, measured 20 s after the instances exist)

| variant | N | units measured | working set KiB/unit | RSS KiB/unit | PSS KiB/unit | of which service state (ws) | of which instance (ws) | apply ms | apply CLI-side ms | noisy |
|---|---|---|---|---|---|---|---|---|---|---|
| instances | 1 | 1 | 28.00 | 32.00 | 32.00 | - | - | - | - | 3/3 |
| instances | 10 | 10 | 17.60 | 12.40 | 12.40 | - | - | - | - | 3/3 |
| instances | 50 | 50 | 14.14 | 10.51 | 10.51 | - | - | - | - | 3/3 |
| instances | 100 | 100 | 12.76 | 9.72 | 9.72 | - | - | - | - | 3/3 |
| instances | 1000 | 996 | 8.89 | 7.34 | 7.34 | - | - | - | - | 3/3 |
| services | 1 | 1 | 92.00 | 504.00 | 397.00 | 64.00 | 28.00 | 44 | 6 | 3/3 |
| services | 10 | 10 | 24.80 | 72.40 | 59.70 | 9.20 | 16.00 | 12 | 4 | 3/3 |
| services | 50 | 50 | 27.52 | 19.76 | 16.96 | 20.24 | 8.00 | 20 | 6 | 3/3 |
| services | 100 | 100 | 10.16 | 14.60 | 13.08 | 3.24 | 6.88 | 37 | 11 | 3/3 |
| services | 1000 | 1,000 | 7.80 | 8.15 | 7.96 | 2.72 | 5.08 | 50 | 71 | 3/3 |
| services | 10000 | 10,000 | 7.38 | 7.28 | 7.26 | 2.37 | 5.01 | 2,336 | 319 | 1/1 |
| services | 100000 | 100,000 | 7.38 | 7.24 | 7.24 | 2.40 | 4.98 | 62,586 | 60,712 | 1/1 |

`instances`: one service (`hold`, echo + spin) with N live instances forced by N concurrent requests (units = instances actually live). `services`: N echo services, one request each (= one instance each); per-unit = (service state + instance). `apply CLI-side ms` = the same `k10s apply` against an unreachable node (parsing + loading the code files).

## Startup: `k10s apply` of N services -> every service answers 200 (ms)

| where | N services | `k10s apply` returns | first service answers | all answer (every node) | not serving | noisy |
|---|---|---|---|---|---|---|
| single | 1 | 29.8 (14.4-45.3) | 50.3 (21.0-59.4) | 50.4 (21.0-59.4) | 0 | 3/3 |
| single | 10 | 23.3 (20.8-34.3) | 35.7 (30.5-64.7) | 35.7 (30.5-64.7) | 0 | 3/3 |
| single | 50 | 22.2 (15.3-25.1) | 35.5 (26.0-40.4) | 35.5 (26.0-40.4) | 0 | 3/3 |
| single | 100 | 20.0 (16.3-59.2) | 33.3 (24.7-78.8) | 33.3 (24.7-78.8) | 0 | 3/3 |
| single | 1000 | 57.5 (45.9-61.2) | 79.6 (74.4-94.9) | 79.8 (74.5-95.1) | 0 | 3/3 |
| cluster-3 | 1 | 35.0 (24.5-81.3) | 41.8 (28.8-111.5) | 49.4 (30.6-112.6) | 0 | 3/3 |
| cluster-3 | 10 | 36.2 (35.4-39.5) | 56.2 (54.0-61.3) | 102.5 (71.0-150.7) | 0 | 3/3 |
| cluster-3 | 100 | 38.3 (35.2-68.4) | 59.6 (42.2-156.5) | 110.6 (50.9-284.6) | 0 | 3/3 |

### Cold start: first request to a never-instantiated service vs second request (ms, client-side, Python)

| module | cold, new conn | warm, new conn | cold, keep-alive | warm, keep-alive | cold - warm (keep-alive) | server: init ms/instance |
|---|---|---|---|---|---|---|
| compute | 0.221 | 0.221 | 0.074 | 0.067 | 0.004 | 0.0041 |
| echo | 0.216 | 0.221 | 0.085 | 0.078 | 0.005 | 0.0043 |

## Request latency/throughput through the k10s ingress (20 s, closed loop, median of repeats)

`single-8` = one node, 8 shards; `cluster-3-local` = echo through the node that hosts it; `cluster-3-forwarded` = through another node (one peer hop). SUT CPU = cgroup CPU of the node(s) over the measured window / requests.

| loadgen at | target | conns | rps | p50 µs | p90 µs | p99 µs | p99.9 µs | max µs | errors | loadgen cores | SUT CPU µs/req |
|---|---|---|---|---|---|---|---|---|---|---|---|
| cluster-3-forwarded | echo | 1 | 9,290 | 100 | 129 | 213 | 708 | 16,584 | 0 | 0.31 | 93.4 |
| cluster-3-forwarded | echo | 16 | 63,294 | 225 | 338 | 812 | 1,896 | 22,504 | 0 | 1.89 | 37.5 |
| cluster-3-forwarded | echo | 64 | 108,407 | 540 | 852 | 1,368 | 2,736 | 25,703 | 0 | 2.54 | 25.4 |
| cluster-3-local | echo | 1 | 29,481 | 29 | 42 | 73 | 199 | 14,655 | 0 | 0.72 | 18.5 |
| cluster-3-local | echo | 16 | 117,314 | 125 | 195 | 318 | 716 | 14,495 | 0 | 3.17 | 14.9 |
| cluster-3-local | echo | 64 | 128,985 | 458 | 772 | 1,080 | 1,928 | 18,904 | 0 | 3.03 | 13.5 |
| single-8 | echo | 1 | 14,432 | 41 | 62 | 796 | 4,128 | 40,636 | 0 | 0.42 | 22.4 |
| single-8 | echo | 16 | 51,725 | 173 | 466 | 3,824 | 8,768 | 55,790 | 0 | 1.23 | 20.0 |
| single-8 | echo | 64 | 109,964 | 410 | 1,208 | 3,248 | 7,200 | 43,667 | 0 | 2.23 | 17.3 |
| single-8 | chain | 1 | 15,373 | 57 | 94 | 227 | 716 | 14,098 | 0 | 0.49 | 26.4 |
| single-8 | chain | 16 | 34,609 | 262 | 948 | 3,280 | 8,768 | 43,262 | 0 | 0.89 | 22.8 |
| single-8 | chain | 64 | 109,148 | 414 | 1,224 | 3,056 | 6,944 | 31,904 | 0 | 2.23 | 19.3 |
| single-8 | compute | 1 | 779 | 1,240 | 1,624 | 2,288 | 4,768 | 26,615 | 0 | 0.04 | 1,241.7 |
| single-8 | compute | 16 | 1,810 | 5,984 | 18,304 | 41,216 | 76,288 | 145,203 | 0 | 0.07 | 1,514.5 |
| single-8 | compute | 64 | 3,821 | 13,888 | 24,192 | 76,288 | 148,480 | 449,483 | 0 | 0.10 | 1,513.8 |

noisy runs: 45/45; CPU used by other processes during the windows: median 1.29, max 7.55 cores

## Open-loop latency (fixed rate, 64 conns, 20 s)

| loadgen at | target | rate | achieved rps | p50 µs | p99 µs | p99.9 µs | p50 µs (jc) | p99 µs (jc) | p99.9 µs (jc) | errors | SUT CPU µs/req | noisy runs |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| single-8 | chain | 5,000 | 5,000 | 366 | 1,560 | 4,192 | 179 | 556 | 2,992 | 0 | 84.9 | 3/3 |
| single-8 | chain | 20,000 | 20,000 | 117 | 1,320 | 4,512 | 81 | 620 | 3,504 | 0 | 40.1 | 3/3 |
| single-8 | compute | 500 | 500 | 1,960 | 5,600 | 9,024 | 1,496 | 4,960 | 7,840 | 0 | 1,566.6 | 3/3 |
| single-8 | compute | 2,000 | 2,000 | 1,448 | 4,192 | 11,840 | 956 | 3,472 | 9,920 | 0 | 1,044.9 | 3/3 |
| single-8 | echo | 5,000 | 5,000 | 596 | 1,288 | 7,200 | 88 | 290 | 3,216 | 0 | 35.0 | 3/3 |
| single-8 | echo | 20,000 | 20,000 | 125 | 1,336 | 5,216 | 81 | 660 | 4,320 | 0 | 37.8 | 3/3 |

`jc` = jitter-corrected: latency from the loadgen's actual wake-up when it slept until the intended send time (Go timers wake up to ~1 ms late), from the intended time when it was behind schedule (queueing in the system under test stays counted).

## Failover

| scenario | event -> first success ms | event -> 100% success ms | outage window ms | failed probes | suspected ms | declared dead / re-placed ms |
|---|---|---|---|---|---|---|
| instance-trap | 1.67 (1.58-1.74) | 1.67 (1.58-1.74) | 0 (0-0) | 0 (0-0) (+ the trapping request) | - | - |
| kill-1-of-2-under-300rps | - | - | - | 0 (0-0) errors of ~3601 reqs | - | - |
| node-kill | 11 (6-17) | 11 (6-17) | 0 (0-0) | 0 (0-0) | 290 (224-551) | 1,071 (1,024-1,344) / 1,133 (1,065-1,386) |
| node-stop | 515 (508-522) | 515 (508-522) | 0 (0-0) | 1 (1-1) | 372 (353-512) | 1,184 (1,162-1,324) / 1,226 (1,207-1,365) |

## Rolling update under probe + 1000 req/s load

| mode | replicas | `k10s apply` ms | new version served everywhere ms | old version gone ms | failed probes | load errors (of ~11k) | noisy |
|---|---|---|---|---|---|---|---|
| cluster-3 | 10 | 14.6 (9.4-17.6) | 22.2 (17.0-24.7) | 23.7 (18.4-26.3) | 0 (0-0) | 0 (0-0) | 6/6 |
| single-8 | 1 | 16.0 (8.4-18.7) | 23.2 (15.3-25.4) | 23.8 (16.0-26.0) | 0 (0-0) | 0 (0-0) | 6/6 |

## Artifact sizes

| artifact | size |
|---|---|
| module hello.k10 (TIR) | 52 B |
| module echo.k10 (TIR) | 157 B |
| module chain.k10 (TIR) | 641 B |
| module compute.k10 (TIR) | 2,282 B |
| module counter.k10 (TIR) | 723 B |
| source hello.ten | 343 B |
| source echo.ten | 731 B |
| source chain.ten | 1,303 B |
| source compute.ten | 4,217 B |
| source counter.ten | 1,167 B |
| JIT machine code hello (code / executable mapping) | 288 B / 4,096 B |
| JIT machine code echo (code / executable mapping) | 520 B / 4,096 B |
| JIT machine code chain (code / executable mapping) | 1,976 B / 4,096 B |
| JIT machine code compute (code / executable mapping) | 5,376 B / 8,192 B |
| JIT machine code counter (code / executable mapping) | 2,272 B / 4,096 B |
| k10s binary (as built: release + debuginfo) | 11.46 MB |
| k10s binary stripped | 1.66 MB |
| k10s dynamic deps | linux-vdso.so.1, libgcc_s.so.1, libc.so.6, /lib64/ld-linux-x86-64.so.2 |
| node data dir after the reference services + 400 requests | 6,026 B |
