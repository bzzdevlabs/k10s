# k10s — research: what each layer really costs

Purpose: put sourced numbers behind the claims in `docs/DESIGN.md` (section 1 table and section 2 choices),
and be explicit about where the evidence is weak or points the other way.

Compiled 2026-10-19 from primary papers and official docs wherever reachable. PDFs were downloaded and the
numbers below were read from the text, not from summaries, unless tagged `[S]`.

## Legend and ground rules

| Tag | Meaning |
|---|---|
| `[P]` | Measured in a peer-reviewed paper or official benchmark page; conditions given |
| `[D]` | Official documentation (stated limit or recommendation, not necessarily a measurement) |
| `[V]` | Vendor marketing/blog claim, no independent verification, methodology often missing |
| `[S]` | Secondary: blog, or a number relayed to me by a search summary of a source I did not open; treat as weak |
| `[M]` | From memory of the literature, NOT re-verified in this pass (only used where flagged) |
| `[N]` | Looked for it, did not find a credible source. No number is given on purpose |

Nothing here is invented. Where a number is missing the cell says "not found". Many Kubernetes per-component
numbers (kubelet, containerd-shim, pause, Cilium/Calico agent) are NOT available from primary sources; see
section 1 and the "Gaps" list at the end.

---

## 0. Summary table: what each layer costs

| Layer | Number | Conditions | Source tag | Section |
|---|---|---|---|---|
| K3s server, idle-ish, with packaged addons | 1.4-1.6 GB RAM, 5-6% of a core | p95, v1.26.5, AWS c6id.xlarge (Xeon 8375C) | `[D]` | 1.2 |
| K3s agent (kubelet+containerd+kube-proxy+flannel in one binary) | ~275 MB RAM, 3% of a core | same | `[D]` | 1.2 |
| Typical etcd box | 2-4 cores, 8 GB RAM "generally enough" | etcd v3.5 docs | `[D]` | 1.1 |
| kube-apiserver ceiling seen in the field | up to 16 GB / 3 cores per replica considered "safe" (Gardener); ~20 GB -> ~2 GB with watch-list streaming (K8s blog) | large clusters, version-dependent | `[S]` | 1.1 |
| Kubernetes supported scale | <=110 pods/node, <=5000 nodes, <=150k pods, <=300k containers | K8s v1.37 docs | `[D]` | 1.6 |
| Pod startup SLO | p99 <= 5 s, **excluding image pull and init containers**, churn <= 20/s | sig-scalability SLOs | `[D]` | 1.5 |
| Pod startup, measured, image pre-pulled | p50 0.90-1.94 s, p99 1.59-4.32 s (10%-100% full cluster) | K8s v1.0, 100 nodes, n1-standard-1, 2015 | `[P]` | 1.5 |
| Borg task start | median ~25 s, ~80% of it package installation | Google, 2015 | `[P]` | 1.5 |
| Envoy sidecar | ~60 MB, ~0.20 vCPU at 1000 rps (Istio 1.24); 154.6 MB (Istio 1.10, 2021) | see 2.2 | `[D]`/`[P]` | 2.2 |
| Linkerd proxy | 17.8 MB, 10 ms CPU-time (2000 rps) | Linkerd 2.10.2, 2021 | `[P]` (vendor-run) | 2.2 |
| Mesh added latency | Linkerd +9..+11 ms p50, Istio +15..+20 ms p50 on a 6 ms baseline | 2021 benchmark, vendor-run | `[P]` (vendor-run) | 2.2 |
| iptables rule update at scale | 2 ms (1 svc), 11 min (5000 svc), 5 h (20000 svc); IPVS 2 ms flat | Huawei, KubeCon EU 2017 | `[S]` | 2.1 |
| Datacenter tax | 22-27% of all cycles; memcpy/memmove 4-5%; kernel ~1/5 of cycles | >20,000 Google machines, 3 years | `[P]` | 5.2 |
| Fleet CPU utilization | Twitter <20% (reservations up to 80%); Google 25-35%; Alibaba 10-30% for >80% of time | 2014-2018 | `[P]` | 5.1 |
| Firecracker microVM overhead | ~3 MB (VMM only, excludes guest kernel/userspace); 125 ms boot claim; 150 microVMs/s/host | m5d.metal, NSDI 2020 | `[P]` | 3.1 |
| Unikraft | image ~1 MB, <10 MB RAM, ~1 ms boot on top of VMM (3-40 ms total) | EuroSys 2021 | `[P]` | 3.3 |
| LightVM unikernel | 2.3 ms boot, 3.6 MB running, 8000 VMs/host; Docker ~200 ms; fork/exec 3.5 ms | Xen, SOSP 2017 | `[P]` | 3.3 |
| Wasmtime instantiation | 5 us (SpiderMonkey.wasm), writes only "a few kilobytes" | Bytecode Alliance, 2023, hw not stated | `[P]`-ish blog | 3.4 |
| Wasm guard-region scaling limit | 16K instances/address space (2^47/2^33); 15x more with ColorGuard | ASPLOS 2025 | `[P]` | 3.5 |
| SFI runtime overhead | LFI 7% (ARM64, SPEC17 subset); Wasm geomean cut by 44.7% with Segue; NaCl <5% ARM, <7% x86-64 | 2010-2025 | `[P]` | 3.5 |
| Singularity SIP vs Linux | call 80 vs 437 cycles; msg ping-pong 1,040 vs 5,800; process create 388k vs 719k cycles | Athlon 64 1.8 GHz, 2006 | `[P]` | 3.6 |
| Cost of hardware isolation, IPC-heavy | 6.3% (TLB on) -> 18.9% -> 33% -> 37.7% slowdown as protection grows | Singularity WebFiles, 2006 | `[P]` | 3.6 |
| Linux syscall | 45 ns (no mitigations) -> 170-200 ns (KPTI) -> 190-230 ns (KPTI+retpoline); 270-330 ns without PCID | Xeon E5-2667v4, 2018 | `[P]` | 3.8 |
| TLB shootdown | 6 us (16 cores, 2 sockets) up to 80 us (120 cores, 8 sockets) | LATR, ASPLOS 2018 | `[P]` | 3.8 |
| Linux net stack per packet | 3.36 us, ~70% in network stack; kernel-bypass server-side 1.44 us (Arrakis/P), 0.38 us (Arrakis/N) | Arrakis, OSDI 2014 | `[P]` | 4 |
| Kernel-bypass one-way latency | IX 5.7 us vs Linux 24 us; up to 10x Linux throughput | IX, OSDI 2014 | `[P]` | 4 |
| Lambda container start at scale | up to 15,000 new containers/s/customer; only 6.4% of image data needed at start | AWS, ATC 2023 | `[P]` | 5.3 |

---

## 1. Kubernetes overhead

### 1.1 Control plane (etcd, kube-apiserver, scheduler, controller-manager)

There is **no single authoritative table** of control-plane RSS by cluster size. What exists:

| Item | Value | Conditions | Source | Year | Tag |
|---|---|---|---|---|---|
| etcd CPU | "typical clusters need two to four cores" | etcd v3.5 hardware guide | https://etcd.io/docs/v3.5/op-guide/hardware/ | n/a | `[D]` |
| etcd RAM | "8 GB is generally sufficient", dominated by KV cache and watcher tracking | same | same | n/a | `[D]` |
| etcd disk | "fast disks are the most critical factor"; measure with fio | same | same | n/a | `[D]` |
| K3s server sizing | 2 vCPU/4 GB for 0-350 agents; 4/8 GB for 351-900; 8/16 GB for 901-1800; 16+/32 GB beyond | K3s server (apiserver+scheduler+CM+datastore in one process), standard workload | https://docs.k3s.io/installation/requirements | current | `[D]` |
| K3s HA | 3 servers lower CPU by ~30-50%, RAM unchanged | K3s v1.31.0 sizing test | https://docs.k3s.io/reference/resource-profiling | current | `[D]` |
| kube-apiserver "safe" ceiling | sustained up to 3 cores and 16 GB per replica; etcd above 16 GB is "a clear warning sign" | Gardener operational guidance, large clusters | https://gardener.cloud/docs/guides/administer-shoots/_print (relayed via search summary) | n/a | `[S]` |
| kube-apiserver with list streaming | memory stabilizes at ~2 GB with WatchList vs ~20 GB without | K8s blog, v1.32 era; not a 5000-node test | https://kubernetes.io/blog/2024/12/17/kube-apiserver-api-streaming/ (relayed via search summary) | 2024 | `[S]` |
| etcd trouble at scale | after ~500 nodes, regular kubectl timeouts; GKE used a single 32-core VM for 500 nodes | OpenAI, 2,500-node cluster | https://openai.com/index/scaling-kubernetes-to-2500-nodes | 2018 | `[S]` |
| Control-plane instance guidance | one or two control-plane instances per failure zone; scale vertically first | K8s v1.37 large-cluster page | https://kubernetes.io/docs/setup/best-practices/cluster-large/ | current | `[D]` |
| Separate etcd for Events | recommended for performance | same | same | current | `[D]` |

Not found: per-process idle RSS of kube-scheduler and kube-controller-manager, CoreDNS RSS. The 0.5-2 GB figure in
DESIGN.md section 1 is consistent with the K3s numbers below (a *small* cluster); for *medium* clusters the
documented sizing is 4-16 GB per server node, i.e. DESIGN.md understates medium-cluster control-plane cost.

### 1.2 Minimal distributions (k3s, k0s, MicroK8s)

K3s resource profile, https://docs.k3s.io/reference/resource-profiling. Version K3s v1.26.5, packaged components on
(Traefik, Klipper LB, local-path), 95th percentile of steady state, Prometheus v2.43.0 used to measure.
Hardware: AWS c6id.xlarge (4 vCPU Xeon 8375C @ 2.90 GHz, 8 GB, NVMe), Ubuntu 22.04; and Raspberry Pi 4B. `[D]`

| Component | x86 CPU | x86 RAM (SQLite / etcd) | Pi4 CPU | Pi4 RAM |
|---|---|---|---|---|
| Server with a workload | 6% of a core | 1596 / 1606 MB | 30% | 1588 / 1613 MB |
| Cluster with 1 agent (server side) | 5% | 1428 / 1450 MB | 25% | 1215 / 1413 MB |
| Agent | 3% | 275 MB | 10% (page is inconsistent: 5% elsewhere) | 268 MB |

Disk requirement for server: SQLite 10 IOPS, <10 ms; embedded etcd 50 IOPS, <5 ms.

Minimum requirements (docs, not measurements): K3s server 2 cores/2 GB, agent 1 core/512 MB
(https://docs.k3s.io/installation/requirements). k0s: ~1 GB controller, ~0.5 GB worker (https://docs.k0sproject.io/main/system-requirements/,
via search summary) `[D]`. MicroK8s: "runs in as little as 540 MB", 4 GB / 20 GB recommended for workloads
(https://canonical.com/microk8s/docs/getting-started) `[D]`. A mirror of an older k0s doc reports ~510 MB for an idle worker
`[S]`, I could not confirm it on the official site.

Takeaway: the **floor of a real Kubernetes node agent is ~270 MB** even in the most aggressively merged distribution
(one process for kubelet/containerd/kube-proxy/CNI), and the **floor of a server is ~1.2-1.6 GB**. DESIGN.md's "150-500 MB per
node" is plausible at the low end for k3s and for stock kubeadm nodes is not contradicted, but I found no primary
measurement of stock kubelet+containerd idle RSS.

### 1.3 Node agents: kubelet, containerd, containerd-shim, pause

Not found: credible primary measurements of kubelet RSS, containerd RSS, per-pod containerd-shim RSS, pause-container RSS
`[N]`. One forum post puts total system overhead (kubelet + runtime) on a Bottlerocket/containerd 1.5.11 node at ~3.5 GB
but that includes everything and is anecdotal `[S]`. How to measure: `container_memory_rss{id="/system.slice/kubelet.service"}`
and `ps` on shim/pause. This is a gap to close with our own measurement (see `bench/`).

What *is* documented is how much memory clouds **reserve** for these daemons, which is the practical cost.
GKE (https://docs.cloud.google.com/kubernetes-engine/docs/concepts/plan-node-sizes, current) `[D]`:

| Node memory | Reserved |
|---|---|
| <1 GiB | 255 MiB |
| first 4 GiB | 25% |
| next 4 GiB (to 8) | 20% |
| next 8 GiB (to 16) | 10% |
| next 112 GiB (to 128) | 6% |
| above 128 GiB | 2% |
| plus | 100 MiB eviction threshold |

CPU reserved (GKE <=1.36): 6% of core 1, 1% of core 2, 0.5% of cores 3-4, 0.25% of each core above 4. For GKE 1.37+ the
reservation is min(old formula, 15 MiB x max pods + 500 MiB (COS) or 700 MiB (Ubuntu)). So Google itself budgets **~0.5-0.7 GiB of
base overhead per node plus ~15 MiB per potential pod**.

### 1.4 kube-proxy and CNI agents

- kube-proxy iptables vs IPVS: see 2.1 (rule-programming time and CPU).
- CNI agent memory: the official Cilium scalability report (https://docs.cilium.io/en/stable/operations/performance/scalability/report/)
  says agent memory does not rise significantly at 1,000 nodes but gives no MB per node `[D]`. A third-party blog
  (https://sanj.dev/post/kubernetes-cni-benchmark-guide/, c5.4xlarge, methodology not described) reports Cilium 1.17: 180/280/450 MB
  at 100/250/500+ pods and Calico 3.31: 120/160/220 MB `[S]`; **unverified, do not rely on it**. No primary Calico number found `[N]`.

### 1.5 Pod startup latency

| Measurement | Value | Conditions | Source | Year | Tag |
|---|---|---|---|---|---|
| Official SLO | p99 <= 5 s per cluster-day; excludes image pull and init containers; cluster churn <= 20 (pod creates/updates/deletes + user requests per s); events in separate etcd; appropriately sized masters | stateless pods | https://github.com/kubernetes/community/blob/master/sig-scalability/slos/slos.md | current | `[D]` |
| API latency SLO | mutating single-object: p99 <= 1 s; read-only resource scope: p99 <= 1 s; namespace/cluster scope: p99 <= 30 s | default install, excl. CRDs | same | current | `[D]` |
| Measured pod start, 100 nodes | p50 0.90 / 1.08 / 1.33 / 1.94 s; p90 1.29 / 1.49 / 1.72 / 2.50 s; p99 1.59 / 1.86 / 2.56 / 4.32 s at 10/25/50/100% full (30 pods/node = 3,000 pods) | K8s v1.0, master n1-standard-4, nodes n1-standard-1, **pause image pre-pulled**, single-container pod | https://kubernetes.io/blog/2015/09/Kubernetes-Performance-Measurements-And/ | 2015 | `[P]` |
| Borg task start | median ~25 s, highly variable; package installation ~80% of it (disk contention) | Google Borg cells, median cell ~10k machines | https://static.googleusercontent.com/media/research.google.com/en//pubs/archive/43438.pdf | 2015 | `[P]` |

Not found `[N]`: a rigorous modern (containerd, v1.2x+) p50/p99 comparison of cached vs pulled-image startup. The only
published "pull" ranges I saw (5-60 s+) are an unsourced blog `[S]`. The SLO's explicit exclusion of image pull is itself
informative: Kubernetes does not promise anything about the dominant term when images are not cached.

The 2015 numbers use a no-op pause container; any real runtime/language initialization (JVM, Python imports) adds on
top. DESIGN.md's "1-5 s start" is supported for the cached case and understated for the pulled case.

### 1.6 Max pods per node (110) and cluster limits

- Stated design envelope (K8s v1.37 docs): <=110 pods/node, <=5,000 nodes, <=150,000 pods, <=300,000 containers.
  https://kubernetes.io/docs/setup/best-practices/cluster-large/ `[D]`. The page gives **no rationale** for 110.
- The practical reason found: each node is given a /24 pod CIDR (256 addresses); Kubernetes needs headroom so IPs are not
  reused too quickly while old pods terminate and new ones start; GKE states the allocated range is always at least double max pods
  (https://docs.cloud.google.com/kubernetes-engine/docs/how-to/flexible-pod-cidr, https://support.d2iq.com/hc/en-us/articles/18977411706900-How-many-pods-can-run-per-node) `[D]`/`[S]`.
  I did not find the original design decision in a KEP/issue, so "110 = /24 minus headroom" is an inference from vendor docs, not proof of intent.
- GKE allows raising it to 256 per node `[D]`.
- Mesh-scale reference: Istio test mesh was 1,000 services, 2,000 pods, 70,000 rps (https://istio.io/latest/docs/ops/deployment/performance-and-scalability/) `[D]`.

---

## 2. Request-path overhead

### 2.1 kube-proxy: iptables vs IPVS

Huawei, "Scale Kubernetes to Support 50,000 Services", KubeCon EU 2017 (slides:
https://raw.githack.com/mm-k8s-ug/kubecon-slides/master/slides/2017-kubecon-eu/Scale%20Kubernetes%20to%20Support%2050,000%20Services%20%5BI%5D%20-%20Haibin%20Xie%20&%20Quinton%20Hoole,%20Huawei%20Technologies%20-%20Scale%20Kubernetes%20to%20Support%2050000%20Services.pdf).
Relayed via search summary, slides not opened by me `[S]`:

| Services | iptables, time to add a rule | IPVS |
|---|---|---|
| 1 | 2 ms | 2 ms |
| 5,000 | 11 min | 2 ms |
| 20,000 | 5 h | 2 ms |

Tigera/Calico benchmark (https://www.projectcalico.org/comparing-kube-proxy-modes-iptables-or-ipvs, 1,000 rps, up to 10,000 services): round-trip
difference between iptables and IPVS "trivially insignificant" until beyond ~1,000 services (10,000 backends), and only visible without keepalive;
at 10,000 services / 100,000 backends CPU is ~35% of a core (iptables) vs ~8% (IPVS) `[S]` (CPU figures relayed by a blog summary).
Conclusion supported: kube-proxy cost is about **rule programming and CPU at large service counts**, not steady-state per-packet latency below ~1,000
services. Note: I saw a claim that IPVS mode is deprecated in recent Kubernetes in favor of nftables; I did **not** verify it.

### 2.2 Service-mesh sidecars

Istio (https://istio.io/latest/docs/ops/deployment/performance-and-scalability/) `[D]`. Istio 1.24, CNCF test infra, 5 bare-metal M3 Large machines
(Equinix), Flannel CNI, HTTP/1.1, 1 KB payload, mTLS on, 1000 rps, 2 proxy workers:

| Component | vCPU | Memory |
|---|---|---|
| Sidecar (Envoy) | ~0.20 | ~60 MB |
| Waypoint (ambient L7) | ~0.25 | ~60 MB |
| ztunnel (ambient L4, per node) | ~0.06 | ~12 MB |

Memory depends on the config pushed to the proxy (listeners/clusters/routes). The page gives the latency curves only as images, no numbers.

Linkerd's benchmark (vendor-run, Kinvolk harness; https://linkerd.io/2021/05/27/linkerd-vs-istio-benchmarks/, 2021-05-27) `[P]` with vendor bias:
Linkerd 2.10.2 default vs Istio 1.10.0 "minimal"; K8s 1.19 (Lokomotive) on Equinix Metal, 6 workers (Xeon 4214, 24 cores @ 2.2 GHz, 192 GB);
mTLS + metrics on; 6 x 10 min runs per load, worst discarded.

| Load | Baseline p50 | Linkerd added p50 | Istio added p50 |
|---|---|---|---|
| 20 rps | 6 ms | +11 ms | +20 ms |
| 200 rps | 6 ms | +11 ms | +19 ms |
| 2000 rps | 6 ms | +9 ms | +15 ms |

p99 total ~70 ms (Linkerd) vs ~200 ms (Istio) at 20 rps. Per proxy at 2000 rps: Linkerd 17.8 MB / 10 ms CPU; Envoy 154.6 MB / 88 ms CPU.
These latencies cover **two proxies per hop** (client-side and server-side) and are end-to-end client-observed.
DESIGN.md says "30-60 MB per sidecar": matches Istio 1.24 (60 MB) but the 2021 measurement was 155 MB, and Linkerd's Rust proxy is 18 MB.
Per-hop latency should be read as "single-digit to ~20 ms at the median in that 2021 setup", not as an intrinsic proxy cost; modern tuning is lower but I have no primary number.

### 2.3 Overlay networks (VXLAN) and CNI datapath

No rigorous primary percent-overhead for VXLAN found `[N]`. Weak data: a blog reports TCP_RR latency of Cilium 42 us, Calico-eBPF 45 us, Flannel 75 us
and Flannel-VXLAN 6.8 Gbps TCP `[S]` (no methodology; do not cite as evidence). An imec paper compares Flannel UDP vs VXLAN backends
(https://imec-publications.be/bitstreams/d3cbd54f-3a8c-4f56-aee5-7cf5384a67e8/download) and finds VXLAN consistently faster than UDP, but I did not extract numbers.

### 2.4 Serialization (JSON vs protobuf) and RPC

- Go micro-benchmarks (VictoriaMetrics blog, https://victoriametrics.com/blog/go-protobuf/, `[S]`): proto.Marshal ~133 ns vs encoding/json ~249 ns; unmarshal ~259 ns vs ~1318 ns.
  Dgraph blog: 1000-entity decode 489,585 ns (protobuf) vs 1,279,297 ns (JSON), ~2.6x. A protojson test shows ~2,227 ns vs 521 ns for encoding/json
  (official protojson is slower than stdlib JSON). Highly library- and message-dependent; **no gRPC per-call microsecond overhead found** `[N]`.
- Fleet-scale view: see 5.2 (protobuf + RPC + memmove are among the largest tax components at Google).

---

## 3. Isolation alternatives

### 3.1 Firecracker (NSDI 2020)

Source: Agache et al., "Firecracker: Lightweight Virtualization for Serverless Applications", NSDI 2020,
https://www.usenix.org/system/files/nsdi20-paper-agache.pdf. `[P]` Hardware: EC2 m5d.metal, 2x Intel Xeon Platinum 8175M.

| Metric | Value |
|---|---|
| Headline claims (abstract) | <5 MB memory overhead per microVM, boots to application code in <125 ms, up to 150 microVMs/s per host |
| Measured VMM memory overhead | ~3 MB (Firecracker), ~13 MB (Cloud Hypervisor), ~131 MB (QEMU), constant across VM sizes. Measured as pmap non-shared segments of the VMM process; **excludes the VMM binary and, crucially, the guest kernel + guest userspace memory, which live inside the configured VM memory** |
| Boot, 1,000 microVMs, 50 concurrent | pre-configured Firecracker p99 146 ms vs Cloud Hypervisor 158 ms |
| Boot detail | static network interface adds ~20 ms (Firecracker/CH), ~35 ms (QEMU); disabling serial console saves up to 70 ms; recommended guest kernel 4.0 MB compressed (no modules) vs Ubuntu 18.04 kernel 6.7 MB + 44 MB modules; unmodified-Linux boot in literature 180 ms |
| Density/overhead claim | thousands of microVMs per host, as low as 3% memory overhead, negligible CPU overhead |

Caveats: "<5 MB" is the VMM only. A microVM with Linux inside needs the guest kernel plus an init and the app; see 6.
Snapshot/restore numbers were not in this paper `[N]` (see 5.3).

### 3.2 gVisor and Kata

- gVisor performance guide (https://gvisor.dev/docs/architecture_guide/performance/) gives **no numbers in text** (data is in linked CSVs) `[N]`. Qualitative: ptrace
  platform has the highest structural syscall cost and is described as a worst case; KVM platform runs poorly under nested virtualization; Systrap recommended.
  Conditions: GCE n1-standard-4 (Broadwell), Debian 9, kernel 4.19. Memory overhead is "a small, mostly fixed amount" per sandbox (no value). Network and file-I/O overhead is
  described as mostly implementation-dependent, with high relative overhead on synthetic small-request benchmarks.
- Kata: no primary benchmark found. Weak: a blog says ~450 MB per Kata pod vs ~200 MB runc `[S]` (no methodology); an arXiv paper
  (https://arxiv.org/pdf/2501.04580, Edera, 2025, via search summary) reports full-container start 1.9 s Kata and 2.4 s Firecracker vs 0.57 s Docker `[S]`.

### 3.3 Unikernels

| System | Number | Conditions | Source | Year | Tag |
|---|---|---|---|---|---|
| Unikraft | images ~1 MB, <10 MB RAM, boot ~1 ms on top of VMM (total 2-40 ms depending on VMM); 1.7-2.7x performance vs Linux guests (nginx, SQLite, Redis); images under 2 MB for all tested apps | QEMU/KVM, Xen, Firecracker, Solo5 | https://arxiv.org/pdf/2104.12721 | EuroSys 2021 | `[P]` |
| LightVM (Xen) | unikernel boots in 2.3 ms (abstract), 3 ms in detailed run (first unikernel created in 80 ms then 3 ms boot); 480 KB image, 3.6 MB running; up to 8000 VMs/host; Tinyx image 9.5 MB: 360 ms create + 180 ms boot; Debian VM 1.1 GB: ~500 ms create + 1.5 s boot; Tinyx runtime memory 1.6 MB vs 8 MB for the Debian tested | Xen, same-host | https://www.cs.utexas.edu/~witchel/S25-380L/papers/manco17sosp-lightvm.pdf | SOSP 2017 | `[P]` |
| LightVM, comparison points | Docker container start ~200 ms; fork/exec 3.5 ms avg (9 ms p90), independent of how many already exist | same | same | 2017 | `[P]` |
| LightVM, Lambda-like unikernels | ~1 MB images, 8 MB RAM | same | same | 2017 | `[P]` |
| OSv | "sub-second boot": Memcached serving requests 0.6 s after boot, guest image 11 MB | OSv paper | https://www.usenix.org/system/files/conference/atc14/atc14-paper-kivity.pdf | ATC 2014 | `[P]` |
| MirageOS, Nanos, IncludeOS | no primary number retrieved | | | | `[N]` |

Ecosystem cost (counter-evidence): Unikraft's own paper notes the historical blocker is porting effort and that single-address-space
unikernels trade away multi-process features; LightVM notes porting Linux syscall users to Mini-OS is "cumbersome".

### 3.4 WebAssembly runtimes and edge isolates

| System | Number | Source | Tag |
|---|---|---|---|
| Wasmtime instantiation | SpiderMonkey.wasm: ~2 ms eager -> **5 us** after lazy init + CoW memory images + madvise reuse (~400x); heap init writes went from tens of MB -> hundreds of KB -> "just a few kilobytes" per instantiation. Hardware not stated. https://bytecodealliance.org/articles/wasmtime-10-performance (2023) | blog by runtime authors | `[P]`-ish |
| Wasmtime pooling allocator defaults | 1000 total memories/tables/core instances; max_memory_size 4 GiB (page also says 6 GiB reserved per memory by default - inconsistent); ~4 GB of virtual address space per linear memory slot, "caps many systems at roughly 32k memory slots"; deallocation is a single madvise, next instantiation may need one mprotect. https://docs.wasmtime.dev/api/wasmtime/struct.PoolingAllocationConfig.html | official docs | `[D]` |
| Wasm instance spin-up (cited in HFI paper) | 30 us, vs "tens to hundreds of ms" for container/VM | https://www.cfallin.org/pubs/asplos2023_hfi.pdf (ASPLOS 2023, citing others) | `[P]` (cited) |
| Cloudflare Workers | startup ~5 ms (and hidden behind the TLS handshake); ~3 MB per basic runtime "shared across isolates"; ~35 MB for a basic Node Lambda; "order of magnitude" less memory than a Node process; hundreds-thousands of isolates per process; context switch for processes "up to 100 us" (claim). https://blog.cloudflare.com/cloud-computing-without-containers/ and https://blog.cloudflare.com/eliminating-cold-starts-with-cloudflare-workers/ | vendor blog | `[V]` |
| Cloudflare Workers limits | **128 MB memory per isolate** (JS heap + Wasm), 1 s startup, 10 ms CPU free / up to 5 min paid. https://developers.cloudflare.com/workers/platform/limits/ | official docs | `[D]` |
| Fastly Compute (Lucet/Wasmtime) | 35.4 us startup, "100x faster than anything else" claim; no per-instance memory figure. https://www.fastly.com/edge-cloud-network/serverless | vendor | `[V]` |
| Fermyon Spin / SpinKube | <1 ms (0.5 ms) cold start; "50-100x" pod density, ">5000 apps per Kubernetes node" via a vendor platform | vendor press (devclass, thenewstack, tfir) | `[V]`; no methodology |
| Lucet | Fastly's predecessor of Wasmtime-based Compute; no independent measurement found | | `[N]` |

Interpretation: independent evidence (Bytecode Alliance blog, HFI paper) puts Wasm instantiation at **single-digit to tens of microseconds** with
**kilobytes written per instance**; edge vendors' claims (5 ms, 35 us) are consistent in order of magnitude. Per-instance resident memory for real
modules is not independently reported; Cloudflare's own 128 MB cap is the safe upper bound they enforce.

### 3.5 Software fault isolation: cost and scaling

| Result | Value | Conditions | Source | Year | Tag |
|---|---|---|---|---|---|
| Wahbe et al., SFI | ~4% overhead for write+jump sandboxing, more with read protection | SPEC92, Alpha/MIPS | not retrievable here (ACM 403; no mirror) | 1993 | `[M]` unverified, **do not quote** |
| NaCl x86-32 (Yee et al.) | ~5% average control-flow sandboxing; older SFI code showed ~25% for x86-32 control+store | as quoted in Sehr et al. | https://www.scs.stanford.edu/~zyedidia/docs/papers/nacl2.pdf | 2009/2010 | `[P]` (relayed inside Sehr) |
| NaCl ARM / x86-64 (Sehr et al.) | <5% (ARM) and <7% (x86-64) average, loads+stores+control flow; ARM SFI "under 10%" | SPEC2000 | same | USENIX Sec 2010 | `[P]` |
| Wasm bounds checks, naive | "can easily slow code by 2x" with explicit conditional checks; even fast Wasm "can easily impose a 40% overhead"; SFI 20-30% or more is common | HFI paper background | https://www.cfallin.org/pubs/asplos2023_hfi.pdf | 2023 | `[P]` (cited) |
| Wasm guard-region scaling | each linear memory = 4 GiB + 4 GiB guard = 8 GiB virtual; with 2^47 user address space -> **16K instances max per process** | Segue paper | https://cseweb.ucsd.edu/~dstefan/pubs/narayan:2025:segue.pdf | ASPLOS 2025 | `[P]` |
| Segue (x86-64 segment regs) | removes **44.7%** of Wasm's overhead on a Wasm-compatible SPEC CPU 2006 subset (geomean 8.3 points lower); Firefox font rendering overhead -75%; XML parsing -68%; also cuts binary size by a median 5.9% | Wasm2c, Clang-17 | same | 2025 | `[P]` |
| Segue on LFI | baseline SFI geomean overhead **17.4%** -> 9.4% (46% of overhead removed) | GCC 13.2.0 LTO | same | 2025 | `[P]` |
| ColorGuard (MPK colors) | up to **15x** more concurrent Wasm instances per address space (16K -> 256K); up to ~29% throughput improvement in benchmarks | same | same | 2025 | `[P]` |
| LFI (Yedidia) | **7%** runtime and 14% code-size overhead, SPEC 2017 compatible subset, ARM64; prior multi-sandbox SFI >20%; tens of thousands of 4 GiB sandboxes per address space | https://www.scs.stanford.edu/~zyedidia/docs/papers/lfi.pdf (abstract via search summary; DOI 10.1145/3620665.3640408) | ASPLOS 2024 | `[P]` |
| HFI (hardware-assisted SFI, emulated/gem5) | near-zero sandbox setup/teardown, context switch "on the order of a function call", arbitrary sandbox count; Wasm context switches are "in the low 10s of cycles"; FaaS tail latency +0-2% with Spectre protection (HFI) vs +9-42% (Swivel software); real workload gains 14-37% on image rendering | https://www.cfallin.org/pubs/asplos2023_hfi.pdf | ASPLOS 2023 | `[P]` (emulated; **no shipping silicon**) |

Note on masking: Wasm engines moved from masking to guard regions because an out-of-bounds access wrapping around changes
semantics (Segue footnote). DESIGN.md's `base + ((addr+off) & mask)` is fine for *fault* isolation because the platform defines the semantics,
but the published numbers above are for guard-region/segment/LFI schemes, not for DESIGN.md's exact masking sequence. Closest published relative is LFI (7%).

### 3.6 Microsoft Singularity (SIPs) and Midori

Singularity, Hunt & Larus, "Singularity: Rethinking the Software Stack" (OSR 2007; paper text: https://www.microsoft.com/en-us/research/wp-content/uploads/2016/02/osr2007_rethinkingsoftwarestack.pdf). `[P]`
Table 1, AMD Athlon 64 3000+ (1.8 GHz), nForce4, cycles:

| | API call | Thread yield | Message ping/pong | Process creation |
|---|---|---|---|---|
| Singularity (SIP) | 80 | 365 | 1,040 | 388,000 |
| Linux | 437 | 906 | 5,800 | 719,000 |
| FreeBSD | 878 | 911 | 13,300 | 1,030,000 |
| Windows | 627 | 753 | 6,340 | 5,380,000 |

- SIP-to-SIP user-code switch "in as few as 394 cycles" (quantum inheritance).
- "Unsafe code tax" (WebFiles benchmark, IPC-heavy, described by authors as possibly worst case): 6.3% slowdown merely turning the TLB on with a single
  4 KB-page address space; 18.9% with a separate protection domain (still ring 0); 33.0% moving the client to ring 3; 37.7% with each of the three SIPs
  in a separate ring-3 domain. Safe-code runtime-check cost: under 5% (measured by disabling array-bound and other checks).
- Caveats: 2006-era hardware (no PCID, no KPTI, no Spectre), a Sing#/C# managed-language system with its own GC per SIP, verified IL, and exchange heap with linear ownership.
  The process-creation gap largely reflects page-table construction; "SIP creation" still cost ~388k cycles (~215 us at 1.8 GHz), not microseconds-for-free.

Midori (Joe Duffy's blog series, https://joeduffyblog.com/2015/11/03/blogging-about-midori/ and sub-posts): `[S]`-primary (the author's own account, no peer review).
- Safe native code (https://joeduffyblog.com/2015/12/19/safe-native-code/): no explicit "within X% of C++" claim; it describes bounds checks as "an extra field fetch, compare, and conditional branch". Code-size lessons: vtables ~40% of image size; static-init checks 10% of code; removing reflection saved >=30% of the C# image; PGO gave 30-40% on browser benchmarks.
- Async everything (https://joeduffyblog.com/2015/11/19/asynchronous-everything/): programs split into "dozens of small processes"; stack switching cost faded "into the sub-0.5% noise" after 1-2 person-years of work; linked stacks started at 128 bytes; per-process single-threaded event loops; pooled stacks O(P) not O(T).
- Error model (https://joeduffyblog.com/2016/02/07/the-error-model/): abandonment (fail-fast) at process granularity, fail-fast : typed exception usage ratio 10:1; kernel services (scheduler, FS, network, drivers) ran as isolated user-mode processes.
- Lessons that hurt: work floods behaved like fork bombs ("really thought we would die from this one"); distributed messages are unordered which surprised developers; races remain at await points; Gen0 allocation is not free; the async state-machine rewrite hurt inlining. The project took "over a decade"; its end was "not entirely technology-driven, and sadly, not even entirely business-driven." No official cost-of-isolation numbers were published.

### 3.7 Theseus OS (OSDI 2020)

Boos et al., https://www.usenix.org/system/files/osdi20-boos.pdf. Single address space, single privilege level, Rust. `[P]`
- Intralingual (safe) heap bookkeeping: **up to 22.5% overhead** vs an unsafe heap version (allocator microbenchmarks).
- Fault injection (QEMU bit flips, skipped instructions): of 665 manifested faults, recovered 69%; failed on 31%, mainly because Rust/LLVM lack asynchronous unwinding. Restarting the task sufficed in 11% of recoveries; 89% needed reloading one or more cells.
- IPC: 1-byte round trip 687 cycles for a safe-Rust atomic fastpath vs seL4 one-way fastpath 401 cycles (no Meltdown mitigations, same hardware); Theseus asynchronous channel RTT 1,664 cycles, vs Singularity's reported 1,415-cycle channel RTT.
- Claim: differences vs Linux "stem from eliminating the overhead of switching between hardware protection modes and address spaces", no significant performance drawbacks seen in their workloads (36-core/72-thread Supermicro 119u-7).
- Relevance: a same-address-space safe-language OS recovers **less than 70%** of injected faults and safe IPC fastpaths cost hundreds of cycles, not "a function call".

### 3.8 Linux syscall, context switch, TLB shootdown

| Item | Value | Conditions | Source | Year | Tag |
|---|---|---|---|---|---|
| getppid() syscall | None 45 ns; retpoline 50-65 ns; KPTI 170-200 ns; KPTI+retpoline 190-230 ns; KPTI+retpoline without PCID 270-330 ns; IBRS on older kernels 670-790 ns (RHEL7 3.10 / SLES 4.4) | Xeon E5-2667v4 Broadwell, 2 sockets, bare metal, microcode 0xb000021, kernels 4.4.114 and 4.14.19 | https://imagefactory.otc.t-systems.com/home/meltdown-spectre/benchmarks | 2018 | `[P]` |
| Pipe context switch cost | IBRS alone 3.5-5.5x slower; IBRS+KPTI 4.2-6.4x; retpoline+KPTI up to ~2x worst case; sibling threads 1.66-2x | same | same | 2018 | `[P]` |
| KPTI at workload level | 0.5% (PCID, kernel 4.14) to 2.1% (no PCID) at ~5k syscalls/s/CPU, 100 MB working set; MySQL OLTP 75k syscalls/s/CPU 5% (8 CPU), 3% (64 CPU); stress test 210k syscalls/s + 27k ctx-switches/s/CPU: 25% measured; author range "1% to over 800%" | Netflix/Gregg | https://www.brendangregg.com/blog/2018-02-09/kpti-kaiser-meltdown-performance.html | 2018 | `[P]` |
| PCID effect on DTLB walks | 2.91% of cycles without PCID -> 0.22% with -> 0.00% with PCID+THP; worst no-PCID case 27% of cycles, IPC 0.86 -> 0.10 | same | same | 2018 | `[P]` |
| Syscall cost, later kernel | getpid ~0.08 us without PTI, ~0.20 us with PTI (kernel 5.10-rc4) | LKML posting | https://lkml.iu.edu/hypermail/linux/kernel/2011.2/04453.html (via search summary) | 2020 | `[S]` |
| Linux syscall in cycles | API call 437, thread yield 906 | Athlon 64 1.8 GHz, 2006 | Singularity paper (3.6) | 2007 | `[P]` |
| Thread context switch | ~1.2-1.5 us pinned to one core, ~2.2 us unpinned (direct cost); indirect cache cost often larger | blog, relayed | https://eli.thegreenplace.net/2018/measuring-context-switching-and-memory-overheads-for-linux-threads/ | 2018 | `[S]` |
| TLB shootdown | ~6 us for 16 cores (2 sockets), up to **80 us for 120 cores (8 sockets)**; an IPI alone 2.7 us (16 cores) to 6.6 us (120 cores); LATR cuts munmap latency by 70.8% on a 2-socket box | LATR | https://gts3.org/assets/papers/2018/kumar:latr.pdf | ASPLOS 2018 | `[P]` |

Reading: the "mitigation tax" is real but modest for syscall-light workloads (<=2-5%) and large for syscall/context-switch-heavy ones
(25% in a stress test, multi-x on pipe ping-pong). It argues for avoiding the kernel on the hot path, which is what DESIGN.md does,
but also shows Linux-based competitors with PCID/huge pages already recover most of the loss.

---

## 4. Kernel bypass and data plane

| System | Result | Conditions | Source | Year | Tag |
|---|---|---|---|---|---|
| Arrakis | Linux spends 3.36 us/packet, ~70% in the network stack, ~5% in the scheduler; Arrakis/P server-side overhead 1.44 us (57% less than Linux), Arrakis/N 0.38 us; UDP echo 2.3x Linux throughput; memcached 1.7x Linux on 1 core, near line rate at 4 cores | 6-core Xeon E5-2430 @ 2.2 GHz, Ubuntu 13.04, 10GbE | https://www.usenix.org/system/files/conference/osdi14/osdi14-paper-peter_simon.pdf | OSDI 2014 | `[P]` |
| IX | unloaded one-way latency 5.7 us (2 IX servers) vs 24 us Linux (4x); up to 10x Linux and 1.9x mTCP throughput on short messages at 10GbE; scales to 4x10GbE; 5 Gbps goodput with 20 KB messages vs 385 KB for Linux | Xeon E5-2665 @ 2.4 GHz, 256 GB, Intel x520 10GbE | https://www.usenix.org/system/files/conference/osdi14/osdi14-paper-belay.pdf | OSDI 2014 | `[P]` |
| Demikernel | per-I/O processing budget <1 us, targeting ~50 ns per I/O; Catnip TCP stack processes an incoming packet and dispatches to the application coroutine in 53 ns; coroutine scheduling "ns-scale overheads" | kernel-bypass libOS (DPDK/RDMA), Microsoft Research | https://irenezhang.net/papers/demikernel-sosp21.pdf | SOSP 2021 | `[P]` |
| io_uring vs epoll (DBMS) | **naive replacement does not necessarily help**: only 1.06x (buffer manager) and 1.10x (network shuffle) improvement for straightforward adoption; carefully tuned io_uring (registered buffers, SQPoll, passthrough, batching, zero-copy) gives 2.05x on the buffer manager and 3.4-3.5x on storage writes vs libaio; epoll vs io_uring without zero-copy "only marginally slower" despite more syscalls; a single core saturates a PCIe 5.0 SSD array at up to 90 GiB/s | ConnectX-7 400GbE, PCIe 5.0 NVMe (Kioxia CM7-R), PVLDB 19(9) | https://arxiv.org/pdf/2512.04859 | 2026 | `[P]` |
| DPDK | no primary number retrieved in this pass (IX/Arrakis/Demikernel above are the measured bypass systems) | | | | `[N]` |
| Seastar / ScyllaDB shard-per-core | vendor claims of 10-37x vs Cassandra (Samsung benchmark, 24-core), independent tests "sometimes ~2x"; guide rule of thumb >=12.5K ops/s/core; no ns-level inter-shard messaging cost found | ScyllaDB sources, relayed | https://go.scylladb.com/product/benchmarks/aws-i3-metal-benchmark/, https://seastar.io/shared-nothing/ | various | `[V]`/`[S]` |

Reading: per-packet kernel stack cost is ~3 us and ~70% of it is the stack itself (Arrakis, 2014 hardware); removing it brings 2-10x on small-message
workloads. The newest and most careful data point (io_uring) is a **warning**: simply switching syscall interface yields ~1.1x; the big gains need architecture change
(batching, zero-copy, polling) - the same lever k10s pulls with in-process calls and polled virtio.

---

## 5. Datacenter efficiency

### 5.1 Utilization (Borg, Twitter, Alibaba)

| Finding | Value | Conditions | Source | Year | Tag |
|---|---|---|---|---|---|
| Twitter (Mesos) | aggregate CPU utilization consistently **<20%** while reservations reach up to **80%** of capacity; majority of servers never above 50% on any week; memory 40-50%; 70% of workloads overestimate reservations by up to 10x, 20% underestimate by up to 5x | cluster with thousands of servers, 30 days | https://www.csl.cornell.edu/~delimitrou/papers/2014.asplos.quasar.pdf | ASPLOS 2014 | `[P]` |
| Google (via Reiss et al.) | 12,000-server cluster: aggregate CPU 25-35%, memory 40%; reserved >75% CPU and >60% memory | cited in Quasar | same | 2014 | `[P]` (cited) |
| Industry-wide estimates | 6% and 12% utilization estimates; EC2 servers 3-17% | cited in Quasar | same | 2014 | `[P]` (cited) |
| Borg prod vs usage | prod jobs get ~70% of CPU allocation and ~55% of memory allocation, represent ~60% of CPU usage and ~85% of memory usage; median cell ~10k machines | Google, 2015 | https://static.googleusercontent.com/media/research.google.com/en//pubs/archive/43438.pdf | 2015 | `[P]` |
| Alibaba co-located trace | CPU utilization 40-50% for the first 4 of 12 hours then lower; >80% of the time machines run at 10-30% CPU; memory >50% most of the time; ~60% of long-running containers have <1% average CPU | 1,313 machines, 12 hours | https://arxiv.org/pdf/1808.02919 | 2018 | `[P]` |

These show **reservation vs use** gap: capacity is reserved because isolation units (VMs/containers) are sized statically. Relevance to k10s is indirect
(per-invocation CPU budget + fair share); the evidence supports "idle reservations dominate", not that SFI would raise utilization by itself.

### 5.2 "Profiling a warehouse-scale computer" (datacenter tax)

Kanev et al., ISCA 2015, https://static.googleusercontent.com/media/research.google.com/en//pubs/archive/44271.pdf. `[P]` >20,000 Google machines, three years, thousands of applications.
- "Tax cycles" consistently **22-27%** of all cycles over 11 months (components: protobuf management, RPC, hashing, compression, memory allocation, data movement).
- memcpy/memmove calls alone: **4-5%** of datacenter cycles (conservative; inlined copies not counted).
- RPC library work (load balancing, encryption, failure detection) is "approximately a third of RPC tax cycles"; the rest is data movement of payloads.
- Kernel: "almost a fifth of CPU cycles" in the kernel, with the scheduler a significant part; 90th-percentile machine runs ~4,500 threads.
- Front-end stalls 15-30% of pipeline slots; IPC ~2x lower than SPECint geomean; half of cycles stalled on caches.
- Diversity: top 50 binaries covered <60% of cycles after 3 years (was 80%), declining ~5 percentage points per year. **Implication: there is no single hot path to optimize; platform overhead is the common denominator.**
The paper's per-component bar values are in Figure 4 (image); I did not extract per-component percentages.

### 5.3 Serverless cold start

Brooker et al., "On-demand Container Loading in AWS Lambda", USENIX ATC 2023, https://www.usenix.org/system/files/atc23-brooker.pdf. `[P]`
- Lambda can start up to **15,000 containers/s for a single customer**; cold-start goal "as low as 50 ms"; images up to 10 GiB.
- On average **only 6.4%** of container image data is needed at startup (sparse loading).
- ~80% of newly uploaded functions have no unique chunks (deduplication); of the rest, mean upload 4.3% unique chunks (median 2.5%).
- Chunk fetch: median 550 us from the AZ-level cache vs 36 ms from origin (S3); p99.9 3.7 ms vs 175 ms; 67% of chunks from on-worker cache, 32% from the AZ cache, 0.06% from origin.
Lambda SnapStart: AWS docs say startup "as low as sub-second"; the AWS blog says single-digit ms for a 512 KB chunk from the L2 cache, up to hundreds of ms from S3
(https://aws.amazon.com/blogs/compute/under-the-hood-how-aws-lambda-snapstart-optimizes-function-startup-latency, via search summary) `[S]`. No Firecracker snapshot-restore latency table found `[N]`;
Catalyzer/REAP/FaaSnap not retrieved.
Cloudflare compares Lambda cold start "500 ms to 10 s" `[V]` (their own number for competitor).

---

## 6. Minimal Linux baselines

| Item | Value | Conditions | Source | Tag |
|---|---|---|---|---|
| Firecracker-oriented guest kernel | 4.0 MB compressed (no modules) vs 6.7 MB + 44 MB modules for stock Ubuntu 18.04 | NSDI 2020 | Firecracker paper (3.1) | `[P]` |
| Debian guest VM (Xen) | image 1.1 GB; "images a few tens of MB and ~30 MB RAM to boot" for typical Linux VMs; Debian runtime memory 8 MB (the build tested) vs Tinyx 1.6 MB | LightVM | LightVM paper (3.3) | `[P]` |
| Tinyx | 9.5 MB image; 1.6 MB runtime memory | same | same | `[P]` |
| Alpine idle | ~47-59 MB in use (2012 post); ~162 MB used at idle in a recent Proxmox VM test, boots with 128 MB | forum/blog | https://lists.alpinelinux.org/~alpine/devel/ (2012), tests via search summary | `[S]` |
| Firecracker guest RSS | max RSS 82 MB (Firecracker) vs 163 MB (QEMU), init unspecified | Alpaquita docs | https://docs.bell-sw.com/alpaquita-linux/latest/virtualization/firecracker-qemu-vm/ | `[S]` |
| systemd baseline | no credible primary measurement found; one generic estimate "50-100 MB for systemd + core services" is unsourced | | | `[N]` |
| Linux "VM node" overhead in DESIGN.md (100-300 MB) | consistent with the above ranges (kernel + init + page tables + caches) but **not directly sourced** | | | `[?]` estimate |

---

## 7. Implications for k10s (against docs/DESIGN.md)

### 7.1 Where the evidence supports the design

1. **Process-level isolation is expensive mainly through the kernel boundary.** Singularity's IPC-heavy benchmark shows 6.3% -> 37.7% slowdown as hardware
   protection is layered back on (3.6); Linux syscall is 45-230 ns and context switches ~1-2 us, with 2-6x worse pipe ping-pong under KPTI+IBRS (3.8); the
   datacenter tax is 22-27% of Google cycles with the kernel ~1/5 (5.2); Linux spends ~3.4 us/packet, 70% in the stack (4). Calls inside one address
   space (DESIGN 2.1, 2.6) attack exactly these terms.
2. **Instance creation can be microseconds and kilobytes.** Wasmtime 5 us/instantiation, a few KB written (3.4); Wasm 30 us spin-up vs tens-hundreds ms for
   containers/VMs (HFI); LightVM unikernel 2.3 ms/3.6 MB (3.3); Unikraft <10 MB, ~1 ms (3.3). Firecracker, the best VM, is ~3 MB VMM overhead + guest, ~125 ms
   boot, 150/s (3.1) - one to three orders of magnitude away from DESIGN's "kilobytes and microseconds". Supports DESIGN 2.2 (scale-to-zero by default).
3. **SFI instrumentation is affordable.** LFI 7% runtime/14% code size (ARM64), NaCl <5-7%, Segue-optimized Wasm geomean down 8 points, Singularity safe-code checks
   <5% (3.5-3.6). DESIGN's single `and` per access compares favorably to guard-region Wasm at 20-40% (HFI paper), though see 7.2.
4. **Packaging as tiny content-addressed modules.** Lambda needs only 6.4% of container image bytes at start and has to build deduplication, caching, erasure coding to hide
   the other 94% (5.3); 80% of uploads have no unique chunks, i.e. dedup is a big lever (DESIGN 2.5).
5. **Control-plane weight.** K3s server ~1.2-1.6 GB at 6% of a core; documented sizing 4-16 GB per apiserver/etcd for larger clusters; etcd wants 2-4 cores/8 GB and fast
   disks; scale limits 110 pods/node (1.1-1.6). Replicated state + pure placement (DESIGN 2.7) removes the etcd/apiserver load, though no number here proves
   gossip's own cost (not measured in this research).
6. **Sidecar/mesh costs** are real: 18-155 MB per proxy, +9..+20 ms p50 in a 2021 vendor benchmark (2.2); in-process calls delete them.
7. **Idle reservation** dominates fleet inefficiency (Twitter <20% used vs 80% reserved; Alibaba machines 10-30% CPU >80% of the time) (5.1): "idle instance ~ 0 RAM"
   (DESIGN 2.2) targets this directly.

### 7.2 Where the evidence warns us

1. **Spectre/side channels.** DESIGN scopes them out and requires a hardware boundary per tenant. The HFI paper shows software Spectre hardening of SFI (Swivel)
   costs +9-42% tail latency vs +0-2% for a hardware scheme that does not exist in shipping silicon (3.5). So the out-of-scope decision is well-founded, but
   same-org multi-tenant nodes must not be sold as isolation against a malicious tenant. Also note that mitigation costs the baseline too: removing KPTI/retpoline cost
   from the Linux comparison would narrow k10s's relative gain only for syscall-heavy loads (3.8).
2. **SFI implementation complexity and verification.** NaCl/LFI-class results depend on a small verified validator; DESIGN generates code on the node with a single-pass
   JIT, so the *compiler* is in the TCB. Published results that are cheap (LFI 7%) verify machine code *after* generation; DESIGN's "compiler guarantees" shifts trust
   to the JIT. Mitigation: verify emitted x86-64 (LFI/NaCl style). I found a 2025 paper on formal verification of the LFI verifier (arXiv 2508.15898) but did not read it.
3. **Safe-language OSes are not magic.** Theseus: safe heap bookkeeping up to 22.5% overhead; only 69% of injected faults recovered; safe IPC fastpath 687 cycles vs seL4 401 (3.7).
   Singularity: SIP creation still ~388k cycles (3.6). A "service call = function call + memcpy" is a design target; the real cost with ownership transfer, safepoints, stack
   switching (Midori: stack switching took 1-2 person-years to reach <0.5%) must be measured, not assumed.
4. **Midori lessons.** A decade, a whole new language/runtime/toolchain; work floods ("fork-bomb-like") were the hardest resource-management problem; message ordering and
   await-point races surprised developers; the project ended for reasons "not entirely technology-driven" (3.6). DESIGN's CPU budgets/safepoints and backpressure need to be first-class, and the
   Ten language + toolchain is the ecosystem cost: all workloads must be rewritten or translated; the Wasm->TIR translator is the hedge, but SFI overhead on translated code (Wasm bounds checks/guard regions) is the 20-40% regime, not 7%.
5. **Address-space limits.** Wasm guard-region design caps at 16K instances per process (3.5); DESIGN's power-of-two masked slots avoid guard regions but each slot's virtual footprint and lazy-commit
   behavior (mmap/madvise/page faults, TLB pressure) still go through the kernel in hosted mode. TLB shootdowns (6-80 us) appear when memory is unmapped/protected across cores
   (3.8); decommit patterns matter.
6. **Preemption.** Safepoint-based preemption cannot interrupt native hostcalls or a stuck long-running host function; the WSC study shows tail behavior dominates, so hostcall time budgets are needed (no published number to support a specific budget).
7. **I/O.** io_uring gives only ~1.1x when swapped naively (4). If hosted-mode k10s uses epoll/sockets for ingress and peer links, real gains come from batching and zero-copy; the full bypass benefits (2-10x on small messages) apply to "metal" mode only.
8. **Counter-evidence on startup claims.** Unikraft/LightVM show a unikernel boot is 2-3 ms even with full VM semantics; Cloudflare/Fastly show Wasm isolates at 5 ms / 35 us in production at scale. k10s's microsecond instance creation is competitive with, not unprecedented relative to,
   Wasm pooling allocators - the differentiator would be the *whole-stack* (placement, no control plane, in-process calls), not instantiation alone.
9. **Kubernetes baselines may be overstated in DESIGN section 1.** Real data: node agent floor ~270 MB (k3s) vs DESIGN's 150-500 MB (plausible); pod start p99 1.6-4.3 s *cached* (consistent with 1-5 s), but no primary pulled-image numbers; sidecar 18-155 MB vs DESIGN's 30-60 MB; guest Linux per VM
   node (100-300 MB) is estimate-grade. A fair benchmark (DESIGN section 5) should compare against k3s/kind and tuned Linux (PCID, no sidecars), not against stock worst cases.
10. **Control-plane reality check.** Small clusters run fine on ~1-1.6 GB (k3s). The k10s advantage is largest at scale (4-16 GB apiserver+etcd, 110 pods/node) and for operational simplicity; at 3-node scale it is ~1 GB per server saved, not tens of GB.

### 7.3 Suggested measurements to close the evidence gaps (for `bench/`)

- Idle RSS of kubelet, containerd, shim, pause, kube-proxy, Cilium and Calico on a pinned kubeadm/kind version, with `container_memory_rss` and `smem -t`.
- Pod startup, containerd, same node: cached vs uncached 100 MB / 1 GB images, with p50/p99 and `kubelet_pod_start_duration_seconds`.
- k10s same-node call round trip in cycles, with and without ownership copy, vs Theseus 687 cycles and Singularity 1,040 cycles (3.6, 3.7).
- SFI cost of the actual masked-access codegen on SPEC-like kernels vs LFI's 7% and Wasmtime guard-region baseline.
- Gossip bandwidth/CPU per node at 100/1,000 nodes (no primary comparison number found for SWIM-style at these sizes in this pass).

---

## 8. Gaps: things I looked for and did not find (do not fill with guesses)

- Per-process idle memory of kube-scheduler, kube-controller-manager, CoreDNS, kubelet, containerd, containerd-shim, pause.
- Primary Cilium/Calico per-node agent memory (only an unverified blog).
- Rigorous modern pod startup p50/p99, image pulled vs cached.
- Primary VXLAN overhead percent; gRPC per-call microsecond overhead.
- gVisor numeric overhead (page has CSVs, not text); Kata primary memory/startup.
- MirageOS, Nanos, IncludeOS boot/memory numbers.
- Wahbe et al. 1993 numbers (source blocked); Wahbe figures are NOT quoted above on purpose.
- Fastly/Spin independent measurements; Lucet independent measurement.
- DPDK per-packet figures; Seastar inter-shard messaging cost; Seastar independent throughput-per-core.
- Firecracker/Lambda snapshot restore latency table.
- systemd baseline memory; a clean "minimal Linux guest RSS" table.
- Midori official cost-of-isolation numbers (none published).
