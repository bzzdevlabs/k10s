# Native (bare process) baseline results

```
date: 2026-10-19T11:40:32+0200
host_kernel: 6.1.0-53-amd64
host_cpu: Intel(R) Core(TM) i7-7820HQ CPU @ 2.90GHz
host_threads: 8
host_mem_gib: 62.7
note: bare processes on the host kernel (not inside the Docker Desktop VM used by the kind baseline)
```

## echo x N on consecutive ports (no orchestration)

| N | spawn all ms | all answering HTTP ms | RSS/proc MiB | PSS/proc MiB | total RSS MiB | total PSS MiB | idle CPU cores |
|---|---|---|---|---|---|---|---|
| 1 | 0.7 (0.6-3.0) | 3.6 (2.7-9.1) | 6.85 | 6.84 | 6.8 | 6.8 | 0.000 |
| 10 | 26.8 (25.9-29.4) | 44.8 (44.3-46.0) | 6.85 | 2.75 | 68.6 | 27.5 | 0.000 |
| 50 | 130.2 (37.2-142.6) | 148.4 (55.3-167.3) | 6.85 | 2.38 | 354.5 | 131.2 | 0.000 |
| 100 | 135.7 (102.0-159.6) | 191.3 (134.9-191.8) | 6.85 | 2.34 | 703.2 | 251.8 | 0.000 |

## Latency/throughput vs one instance (loadgen on same host, 127.0.0.1, 20 s closed loop)

`chain` = frontend process -> echo process over loopback HTTP (the bare-process analogue of the Service hop).

| target | conns | rps | p50 µs | p90 µs | p99 µs | p99.9 µs | max µs | errors | loadgen cores |
|---|---|---|---|---|---|---|---|---|---|
| echo | 1 | 17,152 | 52 | 73 | 137 | 516 | 13,933 | 0 | 0.54 |
| echo | 16 | 73,546 | 195 | 342 | 868 | 2,064 | 24,840 | 0 | 1.88 |
| echo | 64 | 84,086 | 644 | 1,416 | 3,408 | 6,816 | 37,852 | 0 | 1.78 |
| chain | 1 | 5,560 | 159 | 229 | 474 | 1,592 | 18,676 | 0 | 0.24 |
| chain | 16 | 20,524 | 636 | 1,256 | 2,960 | 6,496 | 23,687 | 0 | 0.65 |
| chain | 64 | 25,918 | 2,128 | 4,256 | 7,904 | 13,888 | 43,564 | 0 | 0.66 |
