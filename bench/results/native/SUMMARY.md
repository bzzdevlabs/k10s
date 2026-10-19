# Native (bare process) baseline results

```
date: 2026-10-19T23:16:21+0200
host_kernel: 6.1.0-53-amd64
host_cpu: Intel(R) Core(TM) i7-7820HQ CPU @ 2.90GHz
host_threads: 8
host_mem_gib: 62.7
note: bare processes on the host kernel (not inside the Docker Desktop VM used by the kind baseline)
```

## echo x N on consecutive ports (no orchestration)

| N | spawn all ms | all answering HTTP ms | RSS/proc MiB | PSS/proc MiB | total RSS MiB | total PSS MiB | idle CPU cores |
|---|---|---|---|---|---|---|---|
| 1 | 0.6 (0.6-0.9) | 2.6 (2.6-3.7) | 6.85 | 6.84 | 6.8 | 6.8 | 0.000 |
| 10 | 21.2 (8.7-26.4) | 27.4 (13.5-34.6) | 6.86 | 2.75 | 68.6 | 27.5 | 0.000 |
| 50 | 38.4 (34.9-40.8) | 55.1 (50.3-56.7) | 6.85 | 2.38 | 348.5 | 125.2 | 0.000 |
| 100 | 102.7 (100.4-102.8) | 134.6 (130.3-136.8) | 6.85 | 2.34 | 695.1 | 243.9 | 0.000 |

## Latency/throughput vs one instance (loadgen on same host, 127.0.0.1, 20 s closed loop)

`chain` = frontend process -> echo process over loopback HTTP (the bare-process analogue of the Service hop).

| target | conns | rps | p50 µs | p90 µs | p99 µs | p99.9 µs | max µs | errors | loadgen cores | SUT CPU µs/req |
|---|---|---|---|---|---|---|---|---|---|---|
| echo | 1 | 19,305 | 48 | 60 | 94 | 199 | 4,429 | 0 | 0.56 | 55.4 |
| echo | 16 | 90,665 | 153 | 282 | 668 | 1,560 | 15,419 | 0 | 2.39 | 40.9 |
| echo | 64 | 107,821 | 478 | 1,128 | 2,672 | 5,152 | 22,192 | 0 | 2.35 | 37.8 |
| chain | 1 | 6,656 | 145 | 167 | 255 | 498 | 10,420 | 0 | 0.26 | 212.9 |
| chain | 16 | 29,444 | 454 | 924 | 1,928 | 3,440 | 22,715 | 0 | 1.02 | 168.1 |
| chain | 64 | 33,255 | 1,592 | 3,728 | 6,880 | 10,688 | 28,890 | 0 | 1.07 | 159.8 |
| compute | 1 | 2,852 | 338 | 406 | 588 | 748 | 4,948 | 0 | 0.13 | 360.0 |
| compute | 16 | 13,812 | 556 | 2,480 | 8,256 | 15,296 | 38,395 | 0 | 0.50 | 474.7 |
| compute | 64 | 13,723 | 572 | 13,760 | 49,408 | 86,528 | 162,301 | 0 | 0.50 | 475.6 |
| compute-purego | 1 | 1,530 | 628 | 724 | 1,048 | 1,240 | 3,606 | 0 | 0.08 | 649.2 |
| compute-purego | 16 | 7,711 | 964 | 3,952 | 18,048 | 31,616 | 56,372 | 0 | 0.30 | 891.7 |
| compute-purego | 64 | 7,649 | 980 | 24,960 | 98,816 | 185,344 | 313,628 | 0 | 0.30 | 892.8 |

## Open-loop latency vs one instance (fixed rate, 20 s)

| target | rate | achieved rps | p50 µs | p99 µs | p99.9 µs | p50 µs (jc) | p99 µs (jc) | p99.9 µs (jc) | errors | SUT CPU µs/req | noisy runs |
|---|---|---|---|---|---|---|---|---|---|---|---|
| chain | 5,000 | 5,000 | 354 | 1,592 | 5,152 | 243 | 1,080 | 4,256 | 0 | 346.0 | 3/3 |
| chain | 20,000 | 20,000 | 270 | 7,840 | 18,304 | 209 | 7,776 | 17,792 | 0 | 199.0 | 3/3 |
| compute | 500 | 500 | 1,528 | 2,832 | 5,408 | 1,224 | 2,040 | 3,056 | 0 | 1,176.0 | 3/3 |
| compute | 2,000 | 2,000 | 764 | 2,128 | 5,024 | 482 | 1,656 | 3,536 | 0 | 544.0 | 3/3 |
| compute-purego | 500 | 500 | 2,008 | 4,048 | 5,728 | 1,624 | 3,248 | 4,448 | 0 | 1,657.4 | 3/3 |
| compute-purego | 2,000 | 2,000 | 956 | 1,880 | 3,664 | 636 | 1,240 | 2,960 | 0 | 677.2 | 3/3 |
| echo | 5,000 | 5,000 | 374 | 1,512 | 6,432 | 229 | 796 | 3,376 | 0 | 209.9 | 3/3 |
| echo | 20,000 | 20,000 | 80 | 1,112 | 3,088 | 56 | 314 | 2,192 | 0 | 60.9 | 3/3 |

`jc` = jitter-corrected: latency from the loadgen's actual wake-up when it slept until the intended send time (Go timers wake up to ~1 ms late), from the intended time when it was behind schedule (queueing in the system under test stays counted).
