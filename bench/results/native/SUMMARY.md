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

| target | conns | rps | p50 µs | p90 µs | p99 µs | p99.9 µs | max µs | errors | loadgen cores | SUT CPU µs/req |
|---|---|---|---|---|---|---|---|---|---|---|
| echo | 1 | 16,465 | 55 | 74 | 116 | 378 | 24,947 | 0 | 0.55 | 63.8 |
| echo | 16 | 69,326 | 223 | 350 | 748 | 1,896 | 16,732 | 0 | 1.72 | 40.8 |
| echo | 64 | 78,510 | 716 | 1,432 | 3,344 | 7,200 | 26,241 | 0 | 1.63 | 37.6 |
| chain | 1 | 6,315 | 151 | 179 | 282 | 652 | 19,535 | 0 | 0.26 | 225.4 |
| chain | 16 | 26,889 | 540 | 876 | 1,736 | 3,632 | 18,898 | 0 | 0.83 | 163.8 |
| chain | 64 | 29,218 | 1,960 | 3,600 | 6,176 | 10,048 | 24,307 | 0 | 0.73 | 154.9 |
| compute | 1 | 2,733 | 346 | 478 | 612 | 1,128 | 21,841 | 0 | 0.13 | 377.7 |
| compute | 16 | 11,666 | 1,192 | 2,128 | 3,760 | 7,968 | 26,938 | 0 | 0.32 | 470.9 |
| compute | 64 | 8,728 | 6,048 | 13,120 | 27,520 | 44,288 | 74,479 | 0 | 0.22 | 480.8 |
| compute-purego | 1 | 883 | 964 | 1,320 | 4,768 | 13,760 | 29,027 | 0 | 0.05 | 932.9 |
| compute-purego | 16 | 2,963 | 4,192 | 9,536 | 22,144 | 37,632 | 74,527 | 0 | 0.10 | 909.1 |
| compute-purego | 64 | 4,177 | 11,968 | 28,288 | 68,096 | 116,224 | 190,510 | 0 | 0.12 | 908.7 |

## Open-loop latency vs one instance (fixed rate, 20 s)

| target | rate | achieved rps | p50 µs | p99 µs | p99.9 µs | p50 µs (jc) | p99 µs (jc) | p99.9 µs (jc) | errors | SUT CPU µs/req | noisy runs |
|---|---|---|---|---|---|---|---|---|---|---|---|
| chain | 5,000 | 5,000 | 362 | 1,560 | 7,520 | 249 | 1,004 | 6,048 | 0 | 338.8 | 3/3 |
| chain | 20,000 | 20,000 | 286 | 7,904 | 17,792 | 217 | 7,840 | 17,792 | 0 | 197.2 | 2/3 |
| compute | 500 | 500 | 1,512 | 2,832 | 5,792 | 1,208 | 2,064 | 3,888 | 0 | 1,150.5 | 3/3 |
| compute | 2,000 | 2,000 | 756 | 2,320 | 4,704 | 478 | 1,736 | 3,856 | 0 | 550.3 | 3/3 |
| compute-purego | 500 | 500 | 1,960 | 4,048 | 5,664 | 1,560 | 3,248 | 4,512 | 0 | 1,625.7 | 3/3 |
| compute-purego | 2,000 | 2,000 | 956 | 1,912 | 4,512 | 636 | 1,336 | 3,504 | 0 | 674.6 | 3/3 |
| echo | 5,000 | 5,000 | 394 | 1,544 | 10,688 | 221 | 860 | 8,640 | 0 | 202.3 | 3/3 |
| echo | 20,000 | 20,000 | 80 | 1,144 | 3,984 | 56 | 350 | 2,992 | 0 | 60.7 | 3/3 |

`jc` = jitter-corrected: latency from the loadgen's actual wake-up when it slept until the intended send time (Go timers wake up to ~1 ms late), from the intended time when it was behind schedule (queueing in the system under test stays counted).
