# k8s (kind) baseline results

```
date: 2026-10-19T11:56:56+0200
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
| cluster working set (cgroup memory.current - inactive_file), MiB | 579 (574-584) |
| cluster memory.current (incl. page cache), MiB | 1,319 (1,314-1,324) |
| sum of process RSS inside node, MiB | 1,044 (1,041-1,049) |
| sum of process PSS inside node, MiB | 922 (920-927) |
| idle CPU, cores (cgroup cpu.stat) | 0.136 (0.133-0.176) |

Per-component (inside node): procs, RSS MiB, PSS MiB, CPU % of one core

| component | procs | RSS MiB | PSS MiB | CPU % core |
|---|---|---|---|---|
| kube-apiserver | 1 | 265.6 | 265.6 | 3.87 |
| coredns | 2 | 125.6 | 79.0 | 0.42 |
| containerd-shim | 9 | 110.0 | 51.3 | 0.28 |
| kube-controller-manager | 1 | 107.1 | 107.1 | 1.98 |
| kubelet | 1 | 88.7 | 87.5 | 2.98 |
| containerd | 1 | 67.6 | 66.5 | 0.80 |
| kube-scheduler | 1 | 58.0 | 58.0 | 0.80 |
| etcd | 1 | 56.7 | 56.6 | 2.21 |
| kube-proxy | 1 | 50.6 | 50.6 | 0.02 |
| kindnetd | 1 | 46.3 | 46.3 | 0.02 |
| local-path-provisioner | 1 | 35.9 | 35.9 | 0.02 |
| systemd | 1 | 12.8 | 9.2 | 0.00 |
| systemd-journal | 1 | 10.9 | 7.2 | 0.00 |
| pause | 9 | 6.6 | 1.0 | 0.00 |
| sh | 1 | 1.7 | 0.5 | 0.00 |

## Idle footprint, 3-node cluster (1 control-plane + 2 workers)

| metric | value |
|---|---|
| cluster working set, MiB | 888 (884-890) |
| cluster idle CPU, cores | 0.347 (0.333-0.353) |
|   k10s-bench3-control-plane working set MiB / CPU cores | 600 / 0.273 |
|   k10s-bench3-worker working set MiB / CPU cores | 146 / 0.037 |
|   k10s-bench3-worker2 working set MiB / CPU cores | 147 / 0.037 |

## Per-pod overhead and all-Ready time (echo scaled 0 -> N, no readiness probe)

| N | all Ready s (med, min-max) | node working-set delta / pod MiB | RSS sum / pod MiB | PSS sum / pod MiB | app RSS | shim RSS | pause RSS | app PSS | shim PSS | pause PSS |
|---|---|---|---|---|---|---|---|---|---|---|
| 1 | 1.63 (1.18-1.64) | 13.1 | 27.0 | 19.0 | 6.5 | 12.0 | 0.7 | 6.5 | 4.6 | 0.0 |
| 10 | 2.52 (2.50-2.56) | 11.0 | 21.4 | 9.6 | 6.5 | 13.0 | 0.7 | 2.7 | 5.6 | 0.0 |
| 50 | 8.29 (7.74-11.40) | 10.5 | 21.2 | 9.0 | 6.5 | 12.7 | 0.7 | 2.4 | 5.4 | 0.0 |
| 100 | 18.26 (15.26-18.47) | 10.4 | 20.7 | 8.5 | 6.5 | 12.7 | 0.7 | 2.3 | 5.3 | 0.0 |

RSS columns are MiB per pod. Working set includes kernel/cgroup/page-cache memory charged to the node; RSS double-counts shared pages (PSS does not).

## One pod: scale 0->1 to first response (ms)

| phase | median (min-max) |
|---|---|
| scale_cmd_ms | 119 (81-178) |
| to_scheduled_ms | 204 (164-272) |
| to_ready_ms | 1,732 (1,617-1,839) |
| to_first_http_ms | 1,852 (1,835-1,883) |
| scheduled_to_first_http_ms | 1,631 (1,611-1,688) |

## Request latency/throughput through NodePort (20 s, closed loop, median of repeats)

`host` = loadgen on host via kind extraPortMappings (on Docker Desktop this crosses the VM port forwarder); `node` = loadgen inside the kind node hitting node-IP:NodePort. `chain` adds one ClusterIP (kube-proxy) hop.

| loadgen at | target | conns | rps | p50 µs | p90 µs | p99 µs | p99.9 µs | max µs | errors | loadgen cores |
|---|---|---|---|---|---|---|---|---|---|---|
| host | echo | 1 | 2,308 | 398 | 510 | 1,112 | 3,600 | 17,461 | 0 | 0.12 |
| host | echo | 16 | 9,618 | 1,448 | 2,352 | 5,280 | 14,400 | 96,036 | 0 | 0.37 |
| host | echo | 64 | 17,262 | 3,376 | 5,344 | 9,408 | 16,768 | 151,073 | 0 | 0.49 |
| host | chain | 1 | 1,345 | 636 | 924 | 2,448 | 7,072 | 41,700 | 0 | 0.08 |
| host | chain | 16 | 6,067 | 2,288 | 3,984 | 8,032 | 18,048 | 312,411 | 0 | 0.25 |
| host | chain | 64 | 9,413 | 6,112 | 10,560 | 17,536 | 37,120 | 300,587 | 0 | 0.29 |
| node | echo | 1 | 13,569 | 61 | 104 | 205 | 996 | 23,616 | 0 | 0.48 |
| node | echo | 16 | 49,041 | 237 | 588 | 1,864 | 4,128 | 51,012 | 0 | 1.90 |
| node | echo | 64 | 77,005 | 596 | 1,688 | 4,512 | 7,648 | 174,492 | 0 | 2.23 |
| node | chain | 1 | 3,904 | 231 | 334 | 820 | 2,384 | 84,659 | 0 | 0.20 |
| node | chain | 16 | 17,055 | 756 | 1,656 | 3,696 | 6,304 | 110,963 | 0 | 0.83 |
| node | chain | 64 | 21,891 | 2,416 | 5,536 | 9,408 | 14,912 | 113,069 | 0 | 0.90 |

## Pod failover

| scenario | delete -> replacement Ready ms | delete -> requests OK again ms | outage window ms (first to last failed probe) | failed probes (each fails fast or after 0.5 s timeout) / loadgen errors |
|---|---|---|---|---|
| 1-replica-forced | 1,419 (1,085-1,822) | 2,433 (1,399-2,470) | 1,816 (822-1,842) | 19 (11-20) |
| 1-replica-graceful | 1,490 (1,167-1,631) | 2,283 (1,259-2,370) | 1,690 (663-1,734) | 13 (10-15) |
| 3-replicas-delete-1-under-300rps | - | - | - | 7 (7-8) errors of ~3594 reqs |

## Rolling update, 10 replicas, echo v1<->v2

| metric | median (min-max) |
|---|---|
| `rollout status` complete, s | 4.74 (4.26-5.25) |
| until old pods fully gone, s | 5.60 (4.99-6.17) |
| failed probes during update (no readiness probe) | 22 (19-29) |

## 3-node: worker failure (what Kubernetes actually does, seconds after failure)

| mode | node_notready_s | node_tainted_unreachable_s | pods_marked_notready_on_victim_s | endpoints_reduced_s | first_pod_terminating_s | replacement_pod_created_s | all_replicas_ready_on_healthy_nodes_s | probe fail ratio | waited s |
|---|---|---|---|---|---|---|---|---|---|
| pause | 48.7 | 48.7 | 48.7 | 48.7 | 349.5 | 349.5 | 349.5 | 0.023 | 369 |
| stop | 52.1 | 52.1 | 52.1 | 52.1 | 352.9 | 352.9 | 354.1 | 0.027 | 371 |

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
| containerd store inside node | 1177 MiB |
| etcd data dir (idle) | 156 MiB |
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
