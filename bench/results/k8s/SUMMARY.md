# k8s (kind) baseline results

```
date: 2026-10-19T21:39:57+0200
host_kernel: 6.1.0-53-amd64
host_cpu: Intel(R) Core(TM) i7-7820HQ CPU @ 2.90GHz
host_threads: 8
host_mem_gib: 62.7
docker_server: 28.3.3
docker_os: Docker Desktop
docker_vm_kernel: 6.10.14-linuxkit
docker_vm_cpus: 8
docker_vm_mem_gib: 15.62
kind: kind v0.30.0 go1.24.6 linux/amd64
kubectl_client: v1.34.12
node_image: kindest/node:v1.34.0@sha256:7416a61b42b1662ca6ca89f02028ac133a309a2a30ba309614e8ec94d976dc5a
cluster: k10s-bench
k8s_version: v1.34.0
node_kernel: 6.10.14-linuxkit
container_runtime: containerd://2.1.3
nodes: 1
max_pods: 110
```

## Idle footprint, single-node cluster (60 s windows, median of repeats)

| metric | value |
|---|---|
| cluster working set (cgroup memory.current - inactive_file), MiB | 583 (575-584) |
| cluster memory.current (incl. page cache), MiB | 1,323 (1,315-1,324) |
| sum of process RSS inside node, MiB | 1,051 (1,048-1,056) |
| sum of process PSS inside node, MiB | 929 (926-934) |
| idle CPU, cores (cgroup cpu.stat) | 0.219 (0.217-0.222) |

Per-component (inside node): procs, RSS MiB, PSS MiB, CPU % of one core

| component | procs | RSS MiB | PSS MiB | CPU % core |
|---|---|---|---|---|
| kube-apiserver | 1 | 269.4 | 269.4 | 6.86 |
| coredns | 2 | 126.2 | 79.3 | 0.65 |
| containerd-shim | 9 | 113.2 | 54.5 | 0.33 |
| kube-controller-manager | 1 | 110.3 | 110.3 | 2.95 |
| kubelet | 1 | 90.3 | 89.1 | 4.61 |
| containerd | 1 | 65.7 | 64.6 | 1.16 |
| kube-scheduler | 1 | 60.0 | 60.0 | 1.39 |
| etcd | 1 | 55.7 | 55.7 | 3.66 |
| kube-proxy | 1 | 50.2 | 50.2 | 0.07 |
| kindnetd | 1 | 46.9 | 46.9 | 0.03 |
| local-path-provisioner | 1 | 36.1 | 36.1 | 0.02 |
| systemd | 1 | 12.8 | 9.2 | 0.00 |
| systemd-journal | 1 | 10.9 | 7.3 | 0.00 |
| pause | 9 | 6.6 | 1.0 | 0.00 |
| sh | 1 | 1.6 | 0.5 | 0.00 |

## Idle footprint, 3-node cluster (1 control-plane + 2 workers)

| metric | value |
|---|---|
| cluster working set, MiB | 881 (879-883) |
| cluster idle CPU, cores | 0.273 (0.270-0.282) |
|   k10s-bench3-control-plane working set MiB / CPU cores | 598 / 0.216 |
|   k10s-bench3-worker working set MiB / CPU cores | 141 / 0.030 |
|   k10s-bench3-worker2 working set MiB / CPU cores | 140 / 0.027 |

## Per-pod overhead and all-Ready time (echo scaled 0 -> N, no readiness probe)

| N | all Ready s (med, min-max) | node working-set delta / pod MiB | RSS sum / pod MiB | PSS sum / pod MiB | app RSS | shim RSS | pause RSS | app PSS | shim PSS | pause PSS |
|---|---|---|---|---|---|---|---|---|---|---|
| 1 | 1.28 (1.15-1.52) | 10.6 | 21.0 | 12.9 | 6.5 | 13.0 | 0.7 | 6.5 | 5.7 | 0.0 |
| 10 | 1.94 (1.60-2.23) | 11.6 | 22.0 | 10.2 | 6.5 | 13.1 | 0.7 | 2.7 | 5.7 | 0.0 |
| 50 | 6.98 (6.74-7.77) | 10.8 | 21.3 | 9.1 | 6.5 | 12.9 | 0.7 | 2.4 | 5.6 | 0.0 |
| 100 | 17.58 (15.50-17.95) | 10.8 | 21.0 | 8.8 | 6.5 | 12.7 | 0.7 | 2.3 | 5.3 | 0.0 |

RSS columns are MiB per pod. Working set includes kernel/cgroup/page-cache memory charged to the node; RSS double-counts shared pages (PSS does not).

## One pod: scale 0->1 to first response (ms)

| phase | median (min-max) |
|---|---|
| scale_cmd_ms | 98 (91-105) |
| to_scheduled_ms | 183 (183-197) |
| to_ready_ms | 1,172 (1,042-1,790) |
| to_first_http_ms | 1,165 (1,147-1,796) |
| scheduled_to_first_http_ms | 982 (950-1,613) |

## Request latency/throughput through NodePort (20 s, closed loop, median of repeats)

`host` = loadgen on host via kind extraPortMappings (on Docker Desktop this crosses the VM port forwarder); `node` = loadgen inside the kind node hitting node-IP:NodePort. `chain` adds one ClusterIP (kube-proxy) hop.

| loadgen at | target | conns | rps | p50 µs | p90 µs | p99 µs | p99.9 µs | max µs | errors | loadgen cores | SUT CPU µs/req |
|---|---|---|---|---|---|---|---|---|---|---|---|
| host | echo | 1 | 2,309 | 390 | 494 | 1,144 | 3,856 | 19,347 | 0 | 0.12 | 184.7 |
| host | echo | 16 | 11,973 | 1,224 | 1,816 | 3,632 | 7,840 | 73,114 | 0 | 0.48 | 121.3 |
| host | echo | 64 | 18,767 | 3,088 | 4,896 | 8,384 | 14,784 | 301,913 | 0 | 0.63 | 97.3 |
| host | chain | 1 | 1,608 | 580 | 708 | 1,400 | 4,512 | 53,793 | 0 | 0.09 | 446.9 |
| host | chain | 16 | 7,936 | 1,800 | 2,864 | 5,408 | 12,480 | 60,759 | 0 | 0.34 | 335.0 |
| host | chain | 64 | 11,689 | 4,896 | 8,256 | 13,888 | 28,032 | 325,352 | 0 | 0.40 | 291.6 |
| host | compute | 1 | 1,345 | 700 | 852 | 1,320 | 4,768 | 24,672 | 0 | 0.07 | 474.9 |
| host | compute | 16 | 6,559 | 2,224 | 3,440 | 6,048 | 11,712 | 249,278 | 0 | 0.29 | 567.9 |
| host | compute | 64 | 8,401 | 6,880 | 12,096 | 20,864 | 43,776 | 210,140 | 0 | 0.30 | 588.3 |
| node | echo | 1 | 14,997 | 55 | 93 | 163 | 540 | 56,908 | 0 | 0.48 | 64.0 |
| node | echo | 16 | 70,021 | 163 | 394 | 1,272 | 3,088 | 317,131 | 0 | 2.37 | 46.9 |
| node | echo | 64 | 87,277 | 470 | 1,512 | 4,320 | 7,776 | 181,230 | 0 | 2.41 | 43.0 |
| node | chain | 1 | 4,284 | 217 | 298 | 548 | 1,832 | 80,375 | 0 | 0.20 | 291.8 |
| node | chain | 16 | 20,702 | 612 | 1,320 | 3,184 | 5,792 | 119,809 | 0 | 0.93 | 206.4 |
| node | chain | 64 | 25,650 | 1,960 | 4,832 | 9,152 | 15,936 | 318,601 | 0 | 1.01 | 189.1 |
| node | compute | 1 | 2,452 | 386 | 470 | 676 | 1,448 | 319,033 | 0 | 0.15 | 396.3 |
| node | compute | 16 | 12,168 | 1,080 | 2,320 | 5,280 | 10,688 | 178,741 | 0 | 0.52 | 500.9 |
| node | compute | 64 | 12,824 | 3,056 | 11,456 | 25,984 | 62,208 | 335,797 | 0 | 0.58 | 527.4 |

## Open-loop latency through NodePort (fixed rate, 20 s, median of repeats)

| loadgen at | target | rate | achieved rps | p50 µs | p99 µs | p99.9 µs | p50 µs (jc) | p99 µs (jc) | p99.9 µs (jc) | errors | SUT CPU µs/req | noisy runs |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| host | chain | 5,000 | 4,999 | 1,304 | 214,016 | 313,344 | 988 | 214,016 | 313,344 | 0 | 366.5 | 3/3 |
| host | chain | 20,000 | 11,168 | 8,847,360 | 15,663,104 | 15,794,176 | 8,847,360 | 15,663,104 | 15,794,176 | 0 | 171.9 | 3/3 |
| host | compute | 500 | 500 | 2,288 | 9,920 | 19,840 | 1,928 | 8,896 | 18,048 | 0 | 1,539.1 | 3/3 |
| host | compute | 2,000 | 2,000 | 1,096 | 6,112 | 14,528 | 772 | 5,408 | 14,016 | 0 | 483.4 | 3/3 |
| host | echo | 5,000 | 5,000 | 724 | 5,472 | 14,272 | 516 | 4,960 | 13,120 | 0 | 129.0 | 3/3 |
| host | echo | 20,000 | 18,990 | 651,264 | 1,155,072 | 1,155,072 | 651,264 | 1,155,072 | 1,155,072 | 0 | 90.8 | 3/3 |
| node | chain | 5,000 | 5,000 | 398 | 49,408 | 132,096 | 274 | 49,408 | 132,096 | 0 | 287.5 | 2/3 |
| node | chain | 20,000 | 19,997 | 1,416 | 296,960 | 346,112 | 1,208 | 296,960 | 346,112 | 0 | 211.6 | 3/3 |
| node | compute | 500 | 500 | 1,496 | 5,216 | 24,448 | 916 | 3,408 | 11,840 | 0 | 1,357.7 | 3/3 |
| node | compute | 2,000 | 2,000 | 852 | 5,408 | 72,192 | 524 | 3,728 | 58,112 | 0 | 558.2 | 2/3 |
| node | echo | 5,000 | 5,000 | 564 | 4,640 | 48,896 | 282 | 2,288 | 48,896 | 0 | 214.8 | 2/3 |
| node | echo | 20,000 | 20,000 | 118 | 9,664 | 86,528 | 90 | 8,256 | 84,480 | 0 | 63.7 | 2/3 |

`jc` = jitter-corrected: latency from the loadgen's actual wake-up when it slept until the intended send time (Go timers wake up to ~1 ms late), from the intended time when it was behind schedule (queueing in the system under test stays counted).

## Pod failover

| scenario | delete -> replacement Ready ms | delete -> requests OK again ms | outage window ms (first to last failed probe) | failed probes (each fails fast or after 0.5 s timeout) / loadgen errors |
|---|---|---|---|---|
| 1-replica-forced | 1,704 (1,138-1,780) | 2,257 (1,308-2,357) | 1,645 (694-1,664) | 11 (11-12) |
| 1-replica-graceful | 1,300 (1,207-1,689) | 1,568 (1,412-2,319) | 802 (702-1,664) | 12 (12-17) |
| 3-replicas-delete-1-under-300rps | - | - | - | 4 (3-6) errors of ~3597 reqs |

## Rolling update, 10 replicas, echo v1<->v2

| metric | median (min-max) |
|---|---|
| `rollout status` complete, s | 3.66 (3.34-4.06) |
| until old pods fully gone, s | 3.73 (3.41-4.13) |
| failed probes during update (no readiness probe) | 11 (8-14) |

## 3-node: worker failure (what Kubernetes actually does, seconds after failure)

| mode | node_notready_s | node_tainted_unreachable_s | pods_marked_notready_on_victim_s | endpoints_reduced_s | first_pod_terminating_s | replacement_pod_created_s | all_replicas_ready_on_healthy_nodes_s | probe fail ratio | waited s |
|---|---|---|---|---|---|---|---|---|---|
| pause | 51.1 | 51.1 | 51.1 | 51.1 | 350.7 | 350.7 | 353.3 | 0.023 | 374 |
| stop | 51.1 | 51.1 | 51.1 | 51.1 | 351.0 | 351.0 | 352.2 | 0.024 | 369 |

## Artifact sizes

| artifact | size |
|---|---|
| image echo:v1 | 2.39 MB |
| image compute:v1 | 2.40 MB |
| image chain:v1 | 2.61 MB |
| image counter:v1 | 2.40 MB |
| binary echo | 5.56 MB |
| binary compute | 5.56 MB |
| binary chain | 6.05 MB |
| binary counter | 5.56 MB |
| binary loadgen | 2.56 MB |
| kindest/node image (docker images) | 1.45GB |
| kindest/node image (inspect .Size) | 445 MB |
| containerd store inside node | 1176 MiB |
| etcd data dir (idle) | 155 MiB |
| k8s binary kubelet | 59.2 MB |
| k8s binary kubeadm | 74.0 MB |
| k8s binary kubectl | 60.6 MB |
| k8s binary containerd | 44.3 MB |
| k8s binary runc | 16.8 MB |
| k8s binary crictl | 42.7 MB |
| image in node: docker.io/kindest/kindnetd:v20250512-df8de77b | 44.4 MB |
| image in node: docker.io/kindest/local-path-helper:v20241212-8ac705d0 | 3.1 MB |
| image in node: docker.io/kindest/local-path-provisioner:v20250214-acbabc1a | 22.5 MB |
| image in node: registry.k8s.io/coredns/coredns:v1.12.1 | 22.4 MB |
| image in node: registry.k8s.io/etcd:3.6.4-0 | 74.3 MB |
| image in node: registry.k8s.io/kube-apiserver-amd64:v1.34.0 | 89.0 MB |
| image in node: registry.k8s.io/kube-controller-manager-amd64:v1.34.0 | 76.0 MB |
| image in node: registry.k8s.io/kube-proxy-amd64:v1.34.0 | 73.1 MB |
| image in node: registry.k8s.io/kube-scheduler-amd64:v1.34.0 | 53.8 MB |
| image in node: registry.k8s.io/pause:3.10 | 0.3 MB |
