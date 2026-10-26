# OCI workloads on k10s — design

k10s runs two kinds of workloads on the same node kernel: native services (TIR modules, SFI-isolated, JIT-compiled,
ring 0) and unmodified Linux programs from Docker/OCI images (MMU-isolated ring-3 processes behind a Linux-ABI
personality of the native kernel, `metal/`). Third-party tenants deploy their existing images first and migrate parts
of them to native services later. This document is the plan: execution model, kernel subsystems, image format and
storage, acquisition, networking, manifest and packaging, migration tooling, measurement, tests and sessions.
Linux-namespace containers on Tier 1 and hosted nodes are an optional variant (§15); Tier 1 is frozen.

Axioms: lightness and performance first; security above both; no regression of anything that exists (native
isolation, performance, tests, image sizes beyond measured and accepted deltas); production grade; no existing
technology trusted by default (containerd, runc, CNI, netfilter, Helm, TLS stacks, registry conventions and Linux
namespaces are each judged on evidence, as everything else in k10s was).

## 0. Feasibility and challenges (verdicts)

| Item | Verdict |
|---|---|
| Running unmodified OCI images on the native kernel | **Feasible, and the largest effort in the project.** It is a Linux-compatible OS personality: user address spaces, demand paging, CoW fork, threads, signals, a VFS with a page cache, pipes, sockets, epoll, procfs, an ELF loader. Estimate 50–80 kLoC of kernel code (the kernel is 16.9 kLoC today) plus as much test code: 28 planned sessions (§14), 30–45 once the large ones split. Compatibility is reached image by image against a corpus, never "all of Docker Hub". Precedents bound the risk both ways: gVisor and Asterinas run real server software through a reimplemented Linux ABI; WSL1 shipped the same idea and was replaced by a real Linux kernel (WSL2) because of compatibility and file-system performance gaps. |
| Execution model | **A (ring-3 processes with a Linux personality) for execution, C's migration half for tenants.** B (binary translation into SFI code) is rejected for unmodified images: §2. |
| "A fraction of Kubernetes/Docker resources" | **Holds for platform overhead, not for the application's own memory.** Node fixed overhead (native kernel idle: ≈ 3–4 MiB vs 583 MiB for a kind node, REPORT.md §3), per-container overhead (no containerd/shim/pause/runc/veth/conntrack: target ≤ 256 KiB vs several MiB), cold start (target < 5 ms vs 0.3–5 s) and image bytes (file-level dedupe) can be 10–100× smaller. A JVM heap or a Python interpreter costs what it costs; file-level page-cache sharing helps density, nothing more. Network throughput is a risk: the native kernel's single smoltcp stack on CPU 0 (metal/README.md "Limitations") is below Linux for bulk traffic until the stack is per-CPU (§8.6). |
| Security | **Changes the threat model.** Native services run code our compiler emits (masked, no `rdtsc`, fuzzy clock). OCI tenants run arbitrary machine code: every transient-execution attack applies (Meltdown, Spectre v2/BHI, Retbleed/SRSO, MDS/TAA/RFDS/TSA, GDS) and a new 50–80 kLoC kernel surface is reachable from attacker code. Answers: a CPU-gated mitigation set (§5), a safe-Rust personality with fallible allocation and no panicking paths (§9), a syscall surface without mount/bpf/io_uring/ptrace/raw sockets/modules, differential testing and fuzzing (§13), and **one tenant per node by default** (DESIGN.md §1 already says a hardware boundary per tenant is the real answer; placement enforces it, §10.3). |
| Stateless services (HTTP/TCP app tier, workers) | **First production milestone (M1).** Static and dynamic musl/glibc programs, shells and scripts, nginx, redis (fork for BGSAVE included), Python, Node.js, Go binaries. |
| JVM | **M2.** Needs huge `PROT_NONE` reservations, `mprotect` storms, SIGSEGV-based safepoints with exact `ucontext`, `/sys/fs/cgroup` and `/proc` sizing files: all in the M1 subsystems, but conformance breadth takes a milestone of its own. |
| Databases and other durable state (postgres, MySQL) | **M3, hardest part.** A POSIX file layer with `fsync` semantics on K10S-DATA (§7.5) is a file system; correctness under power loss needs the kvlog crash model's rigor. Until M3, stateful containers keep their data in external databases or in k10s KV via native services. |
| Out of scope (cut) | Privileged containers, host network/PID/IPC modes, devices and GPUs, Docker-in-Docker, systemd as PID 1 in a container, ptrace/debuggers inside containers, FUSE, kernel modules, eBPF and perf tooling (Falco/Datadog-style agents), io_uring, 32-bit (x32/ia32) binaries, user namespaces inside containers, `CAP_SYS_ADMIN`/`CAP_NET_ADMIN`/`CAP_NET_RAW` semantics. They are refused at conversion or return `EPERM`/`ENOSYS`, with a report. |
| Image acquisition and registries | **CLI-side only; nodes never talk to registries.** Import from `docker save`/OCI archives/OCI layouts needs no network code; registry pull over plain HTTP for tests; TLS 1.3 client hand-written in a CLI-only crate (§6.2). The kernel stays without TLS. |
| Ingress, service calls, egress without veth/bridge/NAT | **Feasible and better than Kubernetes:** the kernel owns the TCP stack and the processes, so ingress hands accepted connections to the container's listener (real client address, no proxy copy), calls are authorized by the kernel (no spoofable source address), egress uses the node's address (no conntrack). |
| Migration from compose/Kubernetes/Helm | **Feasible as lossy importers with exact reports.** Helm charts are rendered by `helm template` (Go templates are not reimplemented), then imported. Container → native service: realistic for request/response components rewritten in Ten or compiled through Wasm→TIR (to build); automatic conversion of arbitrary containers is not realistic. |
| Helm-like packaging | **Feasible, independent of OCI, schedule early.** Typed, total templating over the manifest's own structure, signed content-addressed packages, releases tracked in the desired state (§11). |
| arm64 | **After x86-64.** The native kernel has no arm64 arch layer yet (M5). The personality is arch-neutral by design (§9); arm64 adds the asm-generic syscall table, signal frames and the arm64 entry/mitigations. |

Reframed goal for M1: "a tenant's stateless service images run on k10s native nodes, signed, placed, routed and
limited like native services", not "Docker compatibility".

## 1. Requirements and threat model

- Workloads: OCI image manifests and indexes (`application/vnd.oci.image.*`, Docker schema 2), layers tar with gzip,
  zstd or none, whiteouts; image config (Entrypoint, Cmd, Env, WorkingDir, User, ExposedPorts, StopSignal,
  Healthcheck, Volumes).
- Tenants are third parties: their code is hostile to other tenants and to the node. Inside one tenant, containers
  and native services trust each other as much as Kubernetes pods in one namespace do (no side-channel guarantee).
- The operator (admin key holder) is trusted; images and packages are signed by keys the nodes trust (§10.4). A
  compromised node process cannot start unsigned code.
- A container can never: read or write another container's or the kernel's memory, name a file outside its own
  tree, reach a service it is not granted (`calls`), listen on node ports it was not given, send raw frames, change
  node state (time, mounts, modules, network configuration), or exceed its memory, CPU, PID, FD and socket limits.

## 2. Execution model: options and choice

| | A: ring-3 processes + Linux personality | B: binary translation to SFI code | C: hybrid |
|---|---|---|---|
| Unmodified images | yes: the CPU executes the binaries' own code | needs disassembly of arbitrary x86 (variable length, data in code, overlapping streams) and of code JITs emit at run time (V8, HotSpot, LuaJIT, .NET): dynamic translation with a code cache, W^X flips trapped | A |
| Speed | native; a syscall costs ~0.1–0.3 µs plus mitigations | DBT base cost (DynamoRIO-class: 10–30 %) plus a mask per memory access and a lookup per indirect branch (SFI on x86 needs NaCl-style bundles, impossible on unmodified code without full translation) | A |
| Isolation | MMU (architectural), SMEP/SMAP, plus mitigations for transient attacks | SFI masks are architectural only; translated attacker code keeps its speculative gadgets unless the translator fences them (more cost) | A |
| Linux semantics | needed (syscalls, signals, fork, threads) | **also needed**: translation removes the trap, not the syscall semantics; and threads share one slot (atomics, memory model), fork copies slots, 32–64 GiB reservations (JVM) exceed slot sizes | — |
| Kernel changes | user mode, page tables per process, paging | translator (≈ a second JIT for x86/arm64 input), shared-memory threads in SFI | — |
| Migration toward native | none by itself | none: translated code is still the Linux program | C adds it |

B dominates nothing: it needs the same personality, runs slower, isolates worse, and fails on JIT-heavy runtimes.
**Choice: A for execution. C's migration half is source-level** (Ten rewrites, Wasm→TIR for languages that target
WASI, §12), not machine-code conversion.

Rejected alternatives, for the record: microVMs per container (a hypervisor in the native kernel plus a Linux guest
per container: 5–30 MiB and a Linux kernel per container, the opposite of lightness); Linux namespaces on the Tier 1
kernel (§15: full compatibility now, but Linux's container boundary, netfilter, cgroups and a privileged runner on
every OCI node; kept as an optional variant); library-OS Linux (LKL) inside the native kernel (imports Linux).

**Which kernel is "the same lightweight kernel"**: the native kernel. It already runs native services, owns
placement, storage (k10-kvlog) and the cluster protocol, boots in milliseconds and serves in 3 MiB. The personality
adds code only to OCI-capable builds' text (it is never in a native service's path) and costs no RAM until a
container runs. Tier 1's Linux would bring compatibility for free but every container feature would be Linux's
(namespaces, cgroups, netfilter: §15 measures their size), and the same machine would run two different
isolation stacks.

## 3. Sizing the syscall surface

Evidence:
- Static scan of this machine's binaries (Debian 12 glibc builds; undefined dynamic symbols of each binary and its
  libraries except libc/ld.so/libm that name an x86-64 syscall; `nm -D`, kernel 6.18 `syscall_64.tbl`, 383 entries):
  python3 154, busybox 121 (multi-call, admin applets included), node 120, perl 92, curl 75 (31 libraries), libjvm
  62; union 201. Every one of them also calls `syscall()` directly (futex, gettid, getrandom, statx, membarrier,
  io_uring probes). This undercounts what libc does internally (ld.so: `openat`, `mmap`, `mprotect`, `pread64`,
  `arch_prctl`; threads: `clone3`/`clone`, `set_robust_list`, `rseq`, `rt_sigprocmask`; `malloc`: `brk`, `madvise`;
  `stat`: `newfstatat`/`statx`; `readdir`: `getdents64`) and overcounts what runs (Loupe, ASPLOS 2024: dynamic
  analysis needs markedly fewer calls than static analysis, and many can be stubbed or faked without breaking apps;
  Tsai et al., EuroSys 2016: usage is very skewed, a few dozen calls are used by everything).
- Implementations that run real server software: gVisor (Go, most of the table, many partial), Asterinas (Rust,
  210+ syscalls per its documentation, runs Redis/Nginx/SQLite-class software), FreeBSD's Linuxulator (decades,
  still partial), Kerla (≈ 50 calls: BusyBox and a toy server, abandoned).

Target: ≈ 180–200 implemented (≈ 60 of them partially or as documented stubs), ≈ 25 refused by policy (`mount`,
`umount2`, `pivot_root`, `chroot` beyond the container's own tree, `setns`, `unshare`, `reboot`, `swapon/off`,
`syslog`, `settimeofday`, `clock_settime`, `adjtimex`, `sethostname`, `init_module`, `kexec_*`, `bpf`,
`perf_event_open`, `io_uring_*`, `userfaultfd`, `ptrace`, `process_vm_*`, `keyctl`/`add_key`, `pkey_*`,
`personality` except `PER_LINUX`), everything else `ENOSYS`.

Measured (P0.1: `bench/oci/trace.sh`, the per-call table in `bench/oci/syscalls.md`, the plan in docs/LX.md): the
nine corpus images, traced with ptrace under their own entrypoints and a realistic workload, make 34 (static Go) to
102 (postgres) distinct calls, 139 in all; the seven M1 images make 120, of which M1 implements 115 and 5 get an errno
their callers handle (`clone3`, `rseq` and `io_setup` ENOSYS, `io_uring_setup/enter` EPERM). The same images
reference 277 calls statically. The plan of docs/LX.md covers all 383 x86-64 calls: 239 in M1 (4 as stubs), 35 in
M2/M3, 75 refused, 34 ENOSYS.

Classes (x86-64 names; the personality is table-driven per arch):

| Class | Calls | Notes |
|---|---|---|
| process | `clone` (threads, fork, vfork; `CLONE_NEW*` → `EPERM`), `clone3` (`ENOSYS` first: libc falls back), `execve(at)`, `exit(_group)`, `wait4`, `waitid`, `getpid/ppid/tid`, `set_tid_address`, `prctl` (subset: name, pdeathsig, no_new_privs, dumpable, speculation ctrl, child subreaper), `arch_prctl` (FS/GS base), `setpgid/getpgid/setsid/getsid`, `getrlimit/setrlimit/prlimit64`, `getrusage`, `uname`, `sched_*` (affinity within the container's CPUs, yield, policies reported) | posix_spawn uses `CLONE_VM|CLONE_VFORK`: no page-table copy |
| credentials | `get*id`, `set*id`, `setgroups`, `getgroups`, `capget/capset` (bounded set), `umask` | ids are the container's; root has no authority outside it |
| memory | `mmap`, `munmap`, `mprotect`, `mremap`, `madvise` (DONTNEED, FREE, WILLNEED, DONTFORK, HUGEPAGE as no-op), `brk`, `msync`, `mincore`, `mlock*` (accounted, no pinning semantics), `membarrier` (private expedited via IPIs) | overcommit off: commit charged at mmap of writable private memory |
| signals | `rt_sigaction`, `rt_sigprocmask`, `rt_sigreturn`, `rt_sigpending`, `rt_sigtimedwait`, `rt_sigsuspend`, `sigaltstack`, `kill`, `tgkill`, `tkill`, `rt_(tg)sigqueueinfo`, `pidfd_open`, `pidfd_send_signal`, `alarm`, `setitimer/getitimer`, `timer_create` family | exact `siginfo`/`ucontext`/XSAVE frame layout (Go, JVM, V8 read and write it) |
| sync, time | `futex` (wait, wake, requeue, PI later), `futex_waitv` (`ENOSYS`), `set_robust_list`, `rseq` (`ENOSYS`: glibc continues), `nanosleep`, `clock_nanosleep`, `clock_gettime/getres`, `gettimeofday`, `time`, `getrandom` | vDSO for the clocks |
| files | `openat(2)`, `open`, `close`, `close_range`, `read(v)`, `write(v)`, `pread64`, `pwrite64`, `preadv(2)`, `pwritev(2)`, `lseek`, `dup(2,3)`, `fcntl` (flags, locks, dupfd, pipe size), `flock`, `ioctl` (FIONBIO, FIONREAD, FIOCLEX, TCGETS→`ENOTTY`, a few socket ioctls), `newfstatat`, `fstat`, `stat`, `lstat`, `statx`, `statfs/fstatfs`, `getdents64`, `getcwd`, `chdir/fchdir`, `mkdir(at)`, `rmdir`, `unlink(at)`, `rename(at,at2)`, `link(at)`, `symlink(at)`, `readlink(at)`, `chmod`/`fchmod(at)`, `chown`/`fchown(at)`/`lchown`, `utimensat`, `access`/`faccessat(2)`, `truncate/ftruncate`, `fallocate`, `fsync`, `fdatasync`, `sync_file_range`, `sendfile`, `copy_file_range`, `splice`/`tee` (later), `*xattr` (user., security.capability read), `memfd_create`, `inotify_*` (M2), `fadvise64` | paths resolved by the personality's VFS, never by host paths |
| pipes, events | `pipe(2)`, `eventfd(2)`, `timerfd_*`, `signalfd(4)`, `epoll_create(1)`, `epoll_ctl`, `epoll_wait`, `epoll_pwait(2)`, `poll`, `ppoll`, `select`, `pselect6` | one readiness core for all |
| sockets | `socket` (AF_UNIX, AF_INET, AF_INET6; STREAM, DGRAM, SEQPACKET for unix; no RAW/PACKET/NETLINK except a minimal NETLINK_ROUTE `RTM_GET*` answer for `getifaddrs`), `socketpair`, `bind`, `listen`, `accept(4)`, `connect`, `shutdown`, `send*`/`recv*` (`mmsg` included), `get/setsockopt` (subset), `getsockname`, `getpeername` | §8 |
| IPC | SysV `shmget/shmat/shmdt/shmctl`, `semget/semop/semctl` (postgres, M3), POSIX shm via `/dev/shm` | |

Policy for anything not implemented: `ENOSYS` (or `EPERM` for refused classes), counted per container and service
in `/_k10s/status` with the call number, so the corpus run shows what to build next.

## 4. Kernel architecture changes

### 4.1 Address spaces
The kernel identity-maps physical memory in the lower canonical half (metal/README.md "The identity map"); Linux
programs need that half (`0x400000` executables, PIE at `0x5555…`, mmap top-down below `0x7fff…`). The kernel moves
to the upper half: a direct map of all RAM at a fixed upper base (`PHYS_OFFSET`), the kernel image linked high, every
physical/virtual conversion explicit (`DmaRegion`s and virtio/NVMe/AHCI descriptors take physical addresses, slots and
the heap take direct-map addresses). The upper-half PML4 entries are shared by every address space; each process has
its own lower half. Native slots stay in the direct map: TIR instances never run in a process address space.
- PCID: 12-bit ASIDs, recycled with a generation; INVPCID single-context on reuse.
- TLB coherence: the lazy generation scheme (enough for kernel-only W^X sealing) is insufficient once user mappings
  change while other CPUs run the same address space. Each address space keeps a CPU mask; `munmap`, `mprotect`
  that removes rights, CoW breaks and page reclaim send shootdown IPIs to that mask and wait (batched per call);
  freed frames are reused only after every CPU in the mask acknowledged. This is security-critical: a stale entry
  is a cross-tenant read.
- 4-level paging (47-bit user half); LA57 not used.

### 4.2 Entry, exit, privilege
GDT gains user code/data segments; `syscall`/`sysret` (STAR/LSTAR/SFMASK, with the sysret non-canonical RIP check),
interrupts and exceptions from ring 3 through the TSS's `rsp0` per thread; `swapgs` discipline; user state saved in
a per-thread frame. SMEP, SMAP (STAC/CLAC only inside `copy_from_user`/`copy_to_user`, with exception-table fixups
turning a fault into `EFAULT`), UMIP, NX everywhere, `CR4.TSD` optional (§5). Mitigations: §5.

### 4.3 FPU state
The kernel is soft-float; JIT code uses SSE in ring 0 (no vector value live across safepoints). Each Linux thread
owns an XSAVE area (XSAVES/XRSTORS compacted; size from CPUID 0xD; AMX refused: `ARCH_REQ_XCOMP_PERM` → `EPERM`).
Saved on switch-out of a thread, restored lazily at return to ring 3; before a JIT invocation runs on a CPU whose
registers hold a user thread's state, that state is saved first (one flag per CPU). `AT_MINSIGSTKSZ` and the signal
frame follow the enabled XSAVE features as on Linux.

### 4.4 Threads and scheduling
One scheduler for native invocations and Linux threads, extending today's per-CPU run queues:
- A Linux thread is a task with a 16 KiB kernel stack (the coroutine machinery: a blocked syscall suspends the
  task, the CPU runs something else). Long kernel operations (fork of a large address space, big reads, exec,
  page-table teardown) have explicit preemption points, as TIR safepoints.
- Ring 3 is preempted by the LAPIC timer at slice end (2 ms, as native long tasks). Weighted fair scheduling
  between containers with per-container CPU bandwidth (quota per 100 ms period from `cpu`); native short requests
  keep their priority path so a busy container cannot delay native traffic beyond one slice.
- CPU 0 keeps the NIC and protocol work (§8.6); container threads are placed on other CPUs first when there are
  any.
- Process groups, sessions, `SIGCHLD`, zombies, reparenting to the container's PID 1; the container's PID 1 gets
  Linux's PID-namespace semantics (unhandled SIGTERM/SIGINT ignored; `init = true` adds a reaper, §10.2).

### 4.5 Memory management
- Per-process VMAs (sorted tree), demand-zero anonymous memory, file-backed private and shared mappings from the
  page cache, CoW on fork (refcounted frames, write-protect, break on fault), `MAP_SHARED|MAP_ANONYMOUS` shared
  across fork (nginx, postgres), `brk`, guard gaps, stack growth with a guard gap.
- ASLR: mmap base, stack, PIE base, vDSO randomized from RDRAND (28+ bits on x86-64).
- No swap. Non-present PTEs are always zero (no swap or migration entries): L1TF has nothing to read.
- Accounting (one node ledger, §10.3): anonymous pages, page tables, kernel stacks, XSAVE areas, socket and pipe
  buffers and scratch files are charged to the container. Clean image pages are charged to the node's image cache
  (evictable, shared by every container). The container's limit is a hard cap: a fault over it first reclaims the
  container's own clean pages, then the container is killed (OOM) and restarted with backoff; other containers and
  native services are never touched. Commit charge is checked at `mmap`/`brk` time for writable private memory, so
  most allocation failures are `ENOMEM` instead of OOM kills (JVM and Go handle `PROT_NONE` reservations, which are
  not charged).
- Page cache: per object (§7) a sparse page index; pages filled from K10S-DATA, verified against the object's Merkle
  leaves before they are mapped; reclaim by a clock over clean pages when the image-cache budget or free memory runs
  short. Read-ahead: sequential detection plus the object's recorded hot ranges (§7.2).
- THP: none in M1; 2 MiB pages for large anonymous regions later (measured).

### 4.6 ELF, vDSO, exec
- ELF64 loader (ET_EXEC and ET_DYN, PT_LOAD with page-aligned file offsets, PT_INTERP, PT_GNU_STACK, PT_GNU_RELRO
  left to ld.so, PT_TLS untouched), validated before anything is mapped; `#!` scripts (one level of interpreter,
  as Linux's binfmt_script with `BINPRM_BUF_SIZE` 256).
- Initial stack and auxv as Linux: `AT_PHDR`, `AT_PHENT`, `AT_PHNUM`, `AT_PAGESZ`, `AT_BASE`, `AT_ENTRY`,
  `AT_UID…`, `AT_SECURE` (0), `AT_RANDOM`, `AT_HWCAP(2)`, `AT_CLKTCK`, `AT_SYSINFO_EHDR`, `AT_MINSIGSTKSZ`,
  `AT_EXECFN`, `AT_PLATFORM`.
- vDSO written in our own no_std code (a small ELF built with the kernel): `clock_gettime`, `gettimeofday`, `time`,
  `getcpu`, reading a per-node clock page (TSC scale and offset, as kvmclock-style data; coarse or fuzzy per §5).
- `execve` replaces the address space, closes CLOEXEC descriptors, resets signal handlers, keeps the PID.

### 4.7 VFS
- One mount tree per container, built by the kernel from the manifest: `/` = the image tree (read-only objects) with
  a writable scratch overlay (copy-up on write, whiteouts), volumes (§7.5), synthetic `/proc`, `/sys` (a subset),
  `/dev`, `/dev/shm` and `/tmp` (scratch), and generated `/etc/hosts`, `/etc/resolv.conf`, `/etc/hostname` (§8.3).
  No `mount` syscall: the tree is fixed at container creation.
- Path walk with symlinks (40-link limit), `..` confined at the container root, `openat2` resolve flags later;
  permissions by mode, uid/gid and supplementary groups; `CAP_DAC_OVERRIDE`-style bypass for container root only
  inside its own tree.
- Locks: `flock` and POSIX record locks (`fcntl` F_SETLK/F_OFD_*), per inode, released on close/exit.
- `/proc`: `self`, `<pid>/{stat,status,statm,maps,smaps_rollup,cmdline,environ,exe,cwd,root,fd/,fdinfo/,mountinfo,
  cgroup,limits,auxv,task/}`, `meminfo`, `cpuinfo`, `stat`, `uptime`, `loadavg`, `filesystems`, `mounts`,
  `sys/kernel/{hostname,osrelease,pid_max,random/uuid,random/boot_id}`, `sys/net/core/somaxconn`,
  `sys/vm/{overcommit_memory,max_map_count}`; values describe the container (its PIDs, its memory limit as
  MemTotal, its CPUs).
- `/sys`: `devices/system/cpu/{online,possible}`, `fs/cgroup/{memory.max,memory.current,cpu.max,pids.max,
  cgroup.controllers}` (JVM, Go, .NET and Node size heaps and thread pools from these), `kernel/mm/
  transparent_hugepage/enabled` (`never`).
- `/dev`: `null`, `zero`, `full`, `random`, `urandom`, `tty` (`ENXIO`), `fd`, `stdin/out/err`, `ptmx`/`pts` refused
  (no interactive containers in M1; `k10s exec` with a pty is a later item), `shm`.

### 4.8 Pipes, unix sockets, readiness
Pipes (64 KiB default, `F_SETPIPE_SZ` up to a per-container cap), unix stream/dgram/seqpacket sockets with
`SCM_RIGHTS` and `SCM_CREDENTIALS` (container-local pids), abstract names per container, filesystem names in the
container's tree; eventfd, timerfd, signalfd; one wait-queue/readiness core under epoll (level and edge,
EPOLLONESHOT, EPOLLEXCLUSIVE), poll and select.

### 4.9 Code size and boot cost
Estimated additions to the native kernel: address spaces, entry and mitigations 6–9 kLoC; MM and page cache 8–12;
scheduler/threads/signals/futex 5–8; VFS, image fs, scratch, procfs/sysfs/devfs 10–15; pipes/unix/epoll/events 4–6;
sockets over smoltcp and the in-kernel service fabric 8–12; ELF/vDSO/exec 2–3; volumes 8–12 (M3). Kernel file
1.3 MB → an estimated 2.3–3.0 MB for an OCI-capable build. Builds without OCI (a cargo feature of `k10-metal`) stay
byte-identical in behavior and size. Boot time is unchanged: nothing runs until a container is placed.

## 5. Kernel/user isolation and transient-execution mitigations

The CPU gating table (`user/cpu_bugs.rs`) maps vendor/family/model/stepping, microcode revision and
`IA32_ARCH_CAPABILITIES`/CPUID bits to the mitigations the CPU needs. A node advertises the `oci` capability only if
every needed mitigation is implemented and active; otherwise it stays a native-only node and says why in its status.

| Issue | Mitigation in the native kernel | First version |
|---|---|---|
| Meltdown (RDCL) | KPTI: user page tables map only the entry trampoline, per-CPU entry stacks and the vDSO; PCID pairs | gate: require `RDCL_NO` or a non-affected vendor; KPTI as a later session if fleets need pre-2019 Intel |
| Spectre v2 user→kernel | eIBRS (Intel) or AutoIBRS (AMD Zen 4+); retpolines would need rustc's unstable `-Zretpoline` | gate: require eIBRS/AutoIBRS; RSB stuffing on context switch; PBRSB sequence after VM-exit irrelevant (no VMs) |
| Spectre v2 user→user | IBPB on switch between address spaces of different services; STIBP when SMT siblings run different services | yes |
| BHI | `BHI_DIS_S` where supported, else the BHB-clearing sequence at syscall/interrupt entry | yes |
| Retbleed, SRSO (AMD Zen 1–4), ITS (Intel) | untrained return thunk / safe RET / IBPB-on-entry per model; ITS thunks for indirect branches in the lower cache-line half on affected Intel parts | gate to models whose cheapest mitigation is implemented; Zen 3/4 safe-RET as one session |
| MDS, TAA, MMIO stale data, RFDS, TSA | `VERW` (MD_CLEAR) on every return to ring 3 and before idle on affected CPUs | yes |
| L1TF | non-present PTEs always zero; no nested VMs | by construction |
| Spectre v1 in syscalls | every user-controlled index into a kernel table (syscall table, fd table, epoll sets, futex hash, sockets) clamped with `k10-nospec` | yes |
| SSB | SSBD on request (`PR_SET_SPECULATION_CTRL`), off by default as Linux | yes |
| GDS (Downfall) | microcode required; else `GDS_CTRL` mitigation; else gate | gate |
| SMT | one tenant per node (placement); core scheduling only if a node ever mixes tenants | placement |
| Timing | `rdtsc` allowed by default (performance, single tenant per node); `oci_fuzzy_clock = true` sets `CR4.TSD`, emulates `rdtsc(p)` with the guest fuzzy clock and gives the vDSO the same clock | option |
| Kernel pointers | UMIP, no `/proc/kallsyms`, `/proc/*/stat` kernel fields zeroed, no `dmesg` | yes |

Native TIR services stay in ring 0 with SFI; ring-3 code never maps kernel pages (SMEP/SMAP, supervisor bit).

## 6. Image acquisition

### 6.1 Who talks to registries
| Option | Cost | Verdict |
|---|---|---|
| Nodes pull | TLS, registry auth, gzip/zstd, tar and the converter in the native kernel (~+15–25 kLoC of internet-facing parsing in ring 0), credentials on every node, Docker Hub rate limits per node | rejected |
| CLI pulls or imports, converts, uploads objects to one node, the cluster spreads them | conversion work on the operator machine; the uplink carries each changed object once | **chosen** |

`k10s oci import FILE` reads `docker save` archives (Docker's `manifest.json` + layer tars), OCI archives and OCI
layout directories (no network code). `k10s oci pull REF` speaks the OCI distribution API: manifest and index
resolution (`Accept` for OCI and Docker media types), multi-arch index → the platform of each target arch
(`amd64`, `arm64` later; variant and os checked), blob GETs with `Range` resume, bearer-token auth (`WWW-
Authenticate: Bearer realm,service,scope`: Docker Hub, GHCR, Harbor, ECR/GAR/ACR through their token endpoints),
basic auth, credentials from `~/.docker/config.json` (`auths` with `auth` fields; credential helpers are external
programs and are not executed). Pulls by tag resolve to a digest, recorded in the manifest; `--insecure-http` for
local registries (tests).

### 6.2 TLS
| Option | CLI binary size | Risk | Verdict |
|---|---|---|---|
| A vetted crate (rustls + ring/aws-lc-rs) behind a cargo feature of the CLI | +1.5–3 MB, C/assembly dependencies | external code in the operator tool only; nodes unaffected | fallback only |
| Hand-written TLS 1.3 client in a CLI-only crate (`k10-tls`) | ≈ +150–250 KB | client-only, verification-only public-key code: X25519 and P-256 ECDHE, AES-128/256-GCM (AES-NI/PMULL, constant-time software fallback), ChaCha20-Poly1305, HKDF and SHA-2 exist in k10-mesh, ECDSA P-256/P-384 and RSA-PSS/PKCS#1 v1.5 verification, X.509 path building with name constraints and EKU, system or operator CA bundle; 8–12 kLoC | **chosen** |
Integrity never depends on TLS: every manifest and blob is checked against its digest, and a pull pinned by digest
is safe over a hostile network; TLS protects credentials, tag resolution and private content. The kernel has no TLS
(ingress TLS is passthrough by SNI, §8.2); a TLS server for ingress would reuse `k10-tls` later.

### 6.3 Decompression and conversion
gzip (RFC 1951/1952 inflate, ≈ 600 lines) and zstd (frame decoder with Huffman/FSE, dictionaries refused, window ≤
128 MiB, ≈ 2.5 kLoC) hand-written in the CLI's `k10-oci` crate, fuzzed and checked against the reference tools'
outputs on a corpus. Nodes never decompress: the converter (§7.2) produces the k10s image format, so the kernel
parses only our own canonical, signed metadata.

### 6.4 Transport into the cluster
Today blobs travel whole (`GetBlob`/`Blob` frames up to 64 MiB, `MAX_BLOB_BYTES`; bundles ≤ 256 MiB per apply on
hosted nodes) and are kept in memory by both node kinds (`cluster/store.rs`, `metal/kernel/src/cluster.rs`). Image
objects reach GiBs per image and must go straight to disk:
- **Objects** are content-addressed by their Merkle root (§7.1). Peer protocol additions (negotiated in `Hello`):
  `GetRange{root, offset, len ≤ 1 MiB}`, `Range{root, offset, bytes}`, `RangeMissing`. A fetcher verifies each 4 KiB
  leaf against the object's leaf hashes (fetched first, small) as ranges arrive, writes to the object area, and
  fetches different ranges from up to 4 peers that hold the object (swarm), within the per-link budget
  (`LINK_BUDGET`) so gossip is never starved. A corrupt range is re-fetched from another peer and the sender is
  logged.
- **Operator upload**: `POST /_k10s/objects/missing` (list of roots → the missing ones), `PUT /_k10s/objects/<root>`
  with `Content-Range`, ≤ 16 MiB per signed request (the HMAC covers the body hash), resumable, streamed to disk on
  the native kernel. Then the bundle (manifest, image metadata blobs, packages) is applied; the apply node refuses
  it while an object it references is missing.
- Placement-driven prefetch: a node fetches the objects of images placed on it (and of the next rollout wave), not
  of every image in the desired state. Images are the only blobs not replicated everywhere.

## 7. Image format and storage

### 7.1 The k10s image (`K10I`)
A flattened, canonical, signed description of one image for one arch; layers do not survive conversion
(deduplication is per file, which shares more than per layer).
```text
"K10I" | format:u8 (=1) | arch:u8 | config | tree | small-file data
config = entrypoint:[str] cmd:[str] env:[(k,v)] sorted by k | workdir | uid gid groups:[u32] | stop_signal:u8
         | exposed:[(port,proto)] | health? | volumes:[path] | labels subset
tree   = entries in path order: name | parent | mode | uid | gid | mtime_s | size | nlink-group
         | content: inline (≤ 2 KiB, in the small-file data) | object root:[32] | symlink target
         | xattrs: security.capability (file caps) and user.* only
```
- Objects: regular files over 2 KiB, stored once per node whatever the number of images using them, addressed by
  the root of a SHA-256 Merkle tree with 4 KiB leaves (a leaf is a page: the page cache verifies exactly what it
  maps). Leaf hashes are a small companion record, so verification costs one SHA-256 per 4 KiB filled (≈ 2–3 µs
  with SHA-NI).
- ELF files get a recorded layout at conversion (validated headers, PT_LOAD ranges, page-aligned offsets checked)
  and a hot-range list (the pages the loader and ld.so touch first) for read-ahead. Files are never rewritten or
  pre-relocated: ASLR stays intact and the bytes stay the image's.
- Device nodes are refused (except overlay whiteouts, consumed by flattening); setuid/setgid bits are kept but give
  no authority outside the container; hard links are preserved within an image.
- The image hash is `sha256(K10I bytes)`; the manifest references it. The CLI signs packages that reference images
  (§11.3); the kernel verifies the signature and every hash before a container starts.
- Determinism: converting the same OCI image gives the same bytes (sorted entries, tar mtimes kept, no conversion
  timestamps), so images converted by two operators deduplicate.

### 7.2 Converter (CLI)
Pull/import → verify OCI digests → decompress → parse tar (PAX and GNU long names, hard links, sparse files refused,
paths normalized and confined, size caps) → apply layers in order with whiteouts and opaque directories → build the
tree, objects and leaf hashes → K10I + objects in a local cache (`~/.cache/k10s/objects`, content-addressed). The
`User` field is resolved against the image's own `/etc/passwd`/`/etc/group` at conversion. A conversion report lists
everything dropped (devices, unsupported xattrs, refused features).

### 7.3 Where bytes live on the native kernel
K10S-DATA is a k10-kvlog store whose log is compacted by checkpoints that rewrite live data: hundreds of MiB of
immutable objects must not live in the log. **Store layout version 2** adds an object area: the store's 4 MiB
segments are either log segments or object segments; objects are written as extents of 4 KiB units into object
segments (data first, flushed), then their index record (root → extents, size, leaf hashes' location, refcount) is
appended to the log in a batch. A crash leaves either an indexed whole object or unreferenced extents, which replay
reclaims. Compaction of object segments moves live extents out of sparse segments in the background, rate-limited,
off CPU 0 (§14 P3.1). Version 1 stores open unchanged; the first object write upgrades the superblock (the native
kernel and hosted nodes both read version 2; Tier 1 nodes never write objects). The same layout serves hosted
nodes' `--store` files.

### 7.4 Writable scratch
Copy-up overlay per container: files written by the container live in a scratch filesystem, memory-backed and
charged to the container (default cap `memory/4`, `scratch = SIZE`); `/tmp` and `/dev/shm` are scratch too. Lost on
restart, like a container's writable layer after `docker rm`. Disk-backed scratch uses the volume layer (§7.5) once it
exists.

### 7.5 Volumes (M3)
`volumes = { "/var/lib/postgresql/data" = "pgdata:20GiB" }`: a named, per-service-replica persistent tree on the
node. Implementation: a log-structured file layer on the store: inodes and directory entries as records of a
per-volume namespace in the log (group commit), file data in 4 KiB-granular extents of object segments with
copy-on-write, `fsync`/`fdatasync`/`O_DSYNC` as a flushed batch, quotas from the declared size. Its crash model
extends k10-kvlog's (power cuts at every write, prefix semantics per file with every acknowledged `fsync`). Volumes
are node-local, like KV: a replica that moves starts empty unless the service replicates its own data (as databases
do). Not M1.

### 7.6 GC
Mark: images of the desired state, of the last N release revisions (rollback, §11.4) and of running containers;
sweep after a grace period (10 min) and only with free space under a threshold; objects are refcounted across
images, so a dropped image frees only its unique files.

## 8. Networking

No veth, bridge, netns, NAT, conntrack or iptables: the kernel's socket layer gives each container a **socket view**
over the node's single stack.

### 8.1 Socket views
- A container's sockets are kernel objects tagged with the container. `bind`/`listen` go into the container's own
  port table: two containers can both listen on port 80; nothing is bound on the NIC.
- The container's addresses: `127.0.0.1`/`::1` (container-local loopback: in-kernel streams between its own
  sockets) and one virtual address per container from the node's virtual range (default inside 100.64.0.0/10,
  configurable; IPv6 from a ULA prefix), which `getsockname` and the generated `/etc/hosts` report. `getpeername` on
  an ingress connection returns the real client address.
- No `AF_PACKET`, `SOCK_RAW`, `IP_TRANSPARENT`, `SO_BINDTODEVICE`, multicast joins (M1); `NETLINK_ROUTE` answers
  `RTM_GETLINK/GETADDR` with the container's view only (`getifaddrs`).

### 8.2 Ingress
- HTTP routes: `routes = ["api.example.com", "example.com/api"]` (host, optional path prefix) for image services
  (and native ones, as a bonus). The kernel parses the request head (it does today), picks the service, and for a
  local replica **hands the connection itself** — the smoltcp socket plus the bytes already read — to one of the
  container's accepted-connection queues for its `http` port. The container then owns the TCP connection: no proxy,
  no copy, no second connection. The route decision is per connection (an HTTP/1.1 connection carrying another
  Host later stays with the same container; browsers keep separate connections per origin).
- Remote replica: the node streams the connection to the hosting node over the peer link (§8.4), which hands it to
  its container. Native `/<service>/…` routing is unchanged.
- TLS: passthrough by SNI (`routes = ["tls:api.example.com"]`): the kernel reads the ClientHello's SNI only and hands
  over the connection; the container terminates TLS (its own certificates). A TLS-terminating ingress in the kernel
  is a later item reusing `k10-tls`.
- Raw TCP: `public = [{ port = 5432, to = 5432 }]` reserves node ports cluster-wide (placement rejects conflicts) and
  hands accepted connections over the same way. UDP public ports later.

### 8.3 Service names and calls
- Generated per container: `/etc/hosts` lists `localhost`, the container's hostname and every service in its
  `calls` allowlist with that service's virtual address on this node; `/etc/resolv.conf` points at a stub address
  answered inside the kernel (no packets): allowed service names → virtual addresses, other names → forwarded to the
  node's resolvers only when `egress` allows DNS, else `NXDOMAIN`. Both files are synthetic: they change atomically
  with the desired state.
- `connect(service_vip, port)`: the kernel checks the caller container's `calls` allowlist (the caller's identity is
  the socket's owner, not a source address: nothing to spoof), then: a local replica of an image service → an
  in-kernel stream to its listener on `port` (memcpy between buffers, no TCP); a native service → the stream is
  served by the kernel's HTTP path as ingress is (request → invocation → response); a remote replica → a peer-link
  stream. Denied or unknown → `ECONNREFUSED`, identical for both (as native `call`).
- Native → image service: `call("api/path", body)` becomes an HTTP/1.1 request on a pooled in-kernel stream to the
  service's `http` port (method, path, `Content-Length` body); the response's status and body return to the caller.
  Native callers stay request/response; streaming is for image services among themselves.

### 8.4 Peer-link streams
New peer messages (negotiated in `Hello`): `StreamOpen{id, service, port, caller}`, `StreamData{id, bytes}`,
`StreamWindow{id, credit}`, `StreamClose{id, how}`; per-stream credit (256 KiB default), the link's per-iteration
budget shared fairly between calls, streams and object ranges. Inter-node container traffic is encrypted by the
existing sealed links; no overlay network.

### 8.5 Egress
`egress = ["0.0.0.0/0:443", "10.1.2.0/24:5432", "dns"]` (default: none). Outgoing connections to allowed
destinations use the node's address and a port from the node's ephemeral range, through smoltcp; nothing else
leaves. No NAT table: the stack is the node's.

### 8.6 Network stack capacity
smoltcp runs on CPU 0 with one NIC queue. Container socket calls from other CPUs become batched messages to CPU 0
(as dispatch does today); that bounds throughput per node to what one core moves (expected a few Gbit/s, to be
measured). Plan: multi-queue NICs (virtio-net MQ, igb/igc/e1000e RSS where available) with one stack instance per
queue/CPU, flows steered by RSS hash, the listener and route tables shared read-mostly; the native services' ingress
benefits as well. Until then, M1's network targets are per-request latency and small-response throughput, not bulk
bandwidth.

## 9. Code structure and `unsafe`

- `crates/k10-lx` (new, `no_std + alloc`, `#![forbid(unsafe_code)]`): the Linux personality's semantics —
  syscall table and argument decoding per arch, fd tables, VFS (path walk, overlay, locks, synthetic trees), signals
  (frames are built as bytes through a `UserMem` trait), futex, epoll and wait queues, pipes, unix sockets, socket
  views over a `NetStack` trait, credentials, ELF validation and exec plans, procfs content. It talks to the kernel
  only through traits: `UserMem` (fallible copy in/out), `AddressSpace` (map/unmap/protect plans), `Sched`, `Clock`,
  `PageCache`, `NetStack`, `Ledger`.
- Rules for k10-lx: fallible allocation everywhere (`try_reserve`, no aborting collections), no indexing that can
  panic (clippy `indexing_slicing`, `arithmetic_side_effects`, `unwrap_used` denied), every user-supplied index
  clamped with `k10-nospec`. A kernel panic remains fail-stop (reboot), so the personality has no panicking paths.
- `metal/kernel/src/user/` (new): the only `unsafe` for user mode — entry/exit assembly, page tables and PCID,
  shootdowns, `copy_*_user` with exception fixups, XSAVE, MSRs and mitigation sequences, the vDSO page. Reviewed
  line by line; Kani proofs for the page-table and copy-range arithmetic.
- `crates/k10-image` (new, `no_std` core): K10I encode/decode/validate, Merkle trees; shared by the CLI (writer) and
  the kernel (reader).
- `crates/k10-oci` (new, std, CLI only): OCI layout/archive/docker-save readers, distribution client, inflate, zstd,
  tar, converter. `crates/k10-tls` (new, CLI only). `crates/k10-pkg` (new): packages (§11).
- k10-lx builds and runs on hosted Linux too: the differential harness (§13) drives it in-process against a model
  machine, so most personality bugs are found without booting a kernel.

## 10. Manifest, placement, lifecycle

### 10.1 Manifest format 4
Text form (`images` names `K10I` digests per architecture; from P5.3 the CLI also resolves `image` to them,
uploading objects first; a block with `images` (or, from P5.3, `image`) is an image service, keys of native services
are errors there):
```text
service api {
  image = "oci:ghcr.io/acme/api@sha256:…"      # P5.3; or "oci-archive:api.tar", "oci-layout:dir:tag"
  images = { x86_64 = "sha256:…" }             # what `image` resolves to, accepted as is
  replicas = 3
  memory = 384MiB                              # exact limit (4 KiB granularity) for images
  cpu = 0.5                                    # cores: quota per period; optional
  args = ["--port", "8080"]                    # replaces Cmd; entrypoint = [...] replaces Entrypoint
  env = { LOG_LEVEL = "info" }
  secrets = { DB_PASSWORD = "db-password" }    # sealed values (§10.4)
  http = 8080                                  # port for routes and native callers
  routes = ["api.example.com"]
  public = [{ port = 5432, to = 5432 }]
  calls = ["db", "auth"]
  egress = ["0.0.0.0/0:443", "dns"]
  health = "http:8080/healthz"                 # or "tcp:8080", "exec:/bin/check", default: process alive
  stop_grace = 10s
  scratch = 64MiB                              # default memory/4
  restart = always                             # or on-failure, never
  init = false
  workdir = "/srv"                             # user = "app:app"; both default to the image's
  pids_max = 1024                              # fds_max = 4096
  tenant = "acme"                              # default none (§10.3)
}
```
Binary: format 4 = format 3 with native and image services in one name order, each `name | kind:u8 | body` (0
native: the existing fields; 1 image: `images:[(arch:u8, hash:[32])]` sorted by arch, `replicas`, memory in 4 KiB
pages, `cpu_milli`, `pids_max`, `fds_max`, flags (entrypoint set, args set, init, calls any), restart, the
entrypoint and args lists (order kept), workdir, user, env and secrets sorted by name, `http`, routes sorted, public
ports sorted, egress rules sorted, health, stop grace (ms), scratch (pages), tenant, `calls` indices), then the OS
plan, which may be empty. Lists are canonical in memory too (`ImageService::canonicalize`; `validate` refuses any
other order or a duplicate), so the decoder accepts exactly one encoding per manifest. Routes and public ports are
claimed by one service each. The encoder emits format 4 only when a manifest has image services, so existing
manifests keep their hashes (the rule formats 1–3 follow). `calls` stays index-based and applies to both kinds.
Releases (§11.4) come with packages (P6.3) as a later format.

### 10.2 Lifecycle
- Start: the kernel builds the container (address space, VFS tree, socket view, ledger entry), execs Entrypoint+Cmd
  with the image's env (plus `env`, decrypted `secrets`, `HOSTNAME`, `K10S_SERVICE`, `K10S_REPLICA`) as the image's
  uid/gid in `workdir`; stdin `/dev/null`; stdout/stderr go to the service's log ring (the native `log` ring, line
  framed, rate-limited, `k10s logs`); `init = true` runs a minimal reaper (built into the kernel's exec path, not a
  file in the image) as PID 1 that forwards signals and reaps orphans.
- Stop: SIGTERM (or the image's StopSignal) to PID 1, SIGKILL to the whole container after `stop_grace`; ingress
  stops routing to it first (drain).
- Restart: on exit or OOM, exponential backoff 100 ms → 5 min, reset after 10 min healthy; repeated crashes are
  reported in status and gossip (the service's replica state).
- Health: `http`/`tcp` probes run inside the kernel over in-kernel streams (no packets, µs cost); `exec` probes run a
  process in the container; readiness gates routing and rollouts.
- Rollouts: an image change starts the new container next to the old one when memory allows (surge), switches routes
  when it is ready, then stops the old one; cluster-wide pacing reuses `k10_mesh::rollout`'s waves (1, 2, 4…) gated
  by readiness, and a failing version stops the rollout.
- Later, opt-in (`snapshot = true`): a post-ready snapshot of the container (the kernel owns every piece of its
  state: address space, fds, sockets in listen state only) from which new replicas start in microseconds, as native
  services start from their post-`init` snapshot. Clones share RNG state, stack canaries and ASLR layout, so it stays
  opt-in per service and documented as such.

### 10.3 Capabilities and placement
- Gossip member extension record `EXT_CAPS` (sent only to extension-capable members, like `EXT_OS`; nodes that
  predate it skip it): arch, word size, largest slot (`max_mem_log2`), JIT flag, Linux-personality level (0: no image
  services, 1 M1, 2 M2, 3 M3) and the reason it is 0 (hosted node, kernel built without the personality, CPU gate,
  architecture), image-cache and object-area capacity, and the tenant the node is dedicated to. Hosted and Tier 1
  nodes announce level 0 ("hosted node"); the native kernel announces level 0 ("without the Linux personality")
  until Phases 1–4 land.
- Placement version 3: a service is eligible on a node only if the node satisfies its requirements (image arch
  present in `images`, Linux-personality level ≥ 1, `mem_log2 ≤ max_mem_log2` for native services — **this fixes the 32-bit open
  item: a 32-bit node is never given a service it cannot host, instead of refusing the whole desired state**),
  tenant match (§1: an image service of tenant T only on nodes dedicated to T, one without a tenant only on nodes
  without one), public-port conflicts (a node whose own peer or HTTP port is one of the service's public ports is not
  eligible; two services never claim the same public port); image services charge `mem_bytes + 256 KiB` (kernel structures) against
  the same `mem_bytes` capacity native slots use. A manifest using format 4 is accepted only when every live member
  announces placement version 3 (otherwise 409 with the members to upgrade), so mixed clusters never disagree on
  placement.
- `tier=auto` is unchanged: k10boot picks the native kernel where it drives the NIC and K10S-DATA's controller; the
  native kernel then advertises `oci` when its CPU passes the gate. Tier 1 nodes advertise no `oci` and receive only
  native services.

### 10.4 Trust
- Image services must be signed: the CLI signs the package (or, without packages, the manifest's image set) with an
  Ed25519 key (the `osimage` signature code); nodes trust keys from `k10s.conf` `oci_key` (and the release key for
  k10s-published packages). The kernel verifies before it starts a container: a node process compromised through the
  network can neither start nor alter code (the precedent: Tier 1 init verifies OS images itself).
- Secrets: values sealed by the CLI (`k10s apply --cluster-key-file`) with XChaCha20-Poly1305 under a key derived from
  the cluster key (HKDF, label "k10s secrets v1"), bound to the service and variable, as `key id | nonce | ciphertext |
  tag` with the nonce a keyed hash of the inputs (SIV: an unchanged manifest keeps its hash, only equality of two
  values of one variable shows); nodes refuse (422) a secret sealed with a key they do not hold; decrypted only when a
  container starts; never in status, logs or gossip. Exposure equals the cluster key's.

## 11. Packages (Helm-like, k10s-native)

What exists: the manifest text is the whole desired state; `apply` replaces it; versions are `highest + 1`;
concurrent applies resolve by `(version, hash)` and the loser is dropped (docs/NODE.md "Known limitations"); bundles
are unsigned. Packages build on that without text templating.

### 11.1 Package
A directory compiled by `k10s pkg build` into one content-addressed, signed blob:
```text
package api 1.4.2
params {
  replicas: int = 2 (1..50)
  memory: size = 256MiB
  host: string
  cache: bool = false
}
requires { postgres = "^15.2" }        # resolved and pinned by hash in pkg.lock
service api {
  image = "oci:ghcr.io/acme/api@sha256:…"
  replicas = param.replicas
  memory = param.memory
  routes = [param.host]
  calls = [dep.postgres.service, if param.cache { "cache" }]
}
if param.cache { service cache { image = "oci:redis@sha256:…" memory = 128MiB } }
exports { service = "api" }
```
- The language is the manifest syntax plus typed parameters, `if` over whole blocks and list items, bounded `for`
  over list parameters, string interpolation into string fields only, and nothing else: total, no recursion, no
  I/O, no whitespace semantics. Rendering = (package hash, canonical values) → a canonical manifest fragment;
  equal inputs give equal bytes on every machine.
- Parameter types: `int`, `bool`, `string` (with a regex or enum), `size`, `duration`, lists, records; defaults and
  ranges checked before rendering; unknown values rejected.
- Dependencies: semver ranges resolved against a package index (a directory or an HTTP index of signed packages),
  pinned in `pkg.lock` by hash; a dependency renders as its own release (§11.4) and exports names to its dependents.

### 11.2 Commands
`k10s pkg build|sign|verify|render|diff|install|upgrade|rollback|uninstall|list|history`. `render` prints the
canonical text; `diff` compares the rendered fragment with the release in the cluster, field by field (canonical
encoding makes it exact).

### 11.3 Signatures
Ed25519 over the package blob; trust lists in `k10s.conf` (`pkg_key`), checked by the CLI and by every node (§10.4).
Packages embed the image hashes, so signing a package signs its images.

### 11.4 Releases in the desired state
- A manifest format after 4 (with packages, P6.3) groups services into releases: `release = name | package hash | values hash | revision |
  services`. A service is identified by `(release, name)`; inside a release `calls`, routes and hosts files use the
  bare name, across releases `release/name` (exports make the common case bare); ingress `/<service>/…` keeps
  addressing release `default`.
- `install`/`upgrade`/`uninstall` change one release; the admin API takes the expected current version (CAS): a
  stale apply gets 409 instead of being silently dropped. The node writes the previous revision's rendered fragment
  to its store (last 10 revisions), so `rollback` re-applies a previous revision without the operator's files.
- A plain `k10s apply` of a text manifest keeps working: it is release `default`.

## 12. Migration tooling

- `k10s import compose FILE` → package: services (image, command/entrypoint, environment and env_file, ports →
  `http`/`public`, expose, healthcheck, deploy.replicas, deploy.resources.limits, volumes → volumes/scratch,
  networks → `calls` between members of a shared network, secrets/configs → secrets/files). Reported as unsupported:
  `build` (build the image first), `privileged`, `cap_add`, `devices`, `network_mode`, `pid`, `ipc`, `depends_on`
  ordering (k10s starts everything; services must retry), `extra_hosts`, `sysctls`, `ulimits` beyond fd/pid caps.
- `k10s import k8s FILE…` (YAML, a strict subset parser of our own: block mappings and sequences, flow
  collections, plain/quoted scalars, multiple documents; anchors, tags and merge keys refused with a message):
  Deployment/StatefulSet (stateless shape only, M3 for volumeClaimTemplates)/DaemonSet (→ replicas = nodes)/Job
  (refused in M1), container image/command/args/env/envFrom, resources.limits, probes → health, Service (ClusterIP →
  name and port; NodePort/LoadBalancer → `public`), Ingress (hosts/paths → `routes`; TLS → SNI passthrough with a
  note), ConfigMap (→ files in scratch or env), Secret (→ secrets), NetworkPolicy (→ `calls`/`egress` when
  expressible). Init containers run before the main process as one-shot processes in the same container; sidecars
  become separate services with a report.
- `k10s import helm CHART --values …` runs the operator's `helm template` (an external program the operator
  installed) and imports its output; Go templates and Sprig are not reimplemented.
- Every importer emits a k10s package plus a report of each dropped or approximated field; nothing is silently
  ignored.
- Container → native service: (1) strangler routing — native services take over route prefixes one at a time while
  the container keeps the rest (routes are per host/prefix, §8.2); (2) rewrite hot request/response components in
  Ten (examples/ show the shape); (3) a Wasm→TIR translator (to build) for code that compiles to WASI (Rust, C/C++,
  Go `wasip1`, TinyGo, Zig) with a `wasi:http`-shaped adapter onto the host API. Long-running servers with threads
  and sockets do not convert; they stay containers.

## 13. Precision strategy

- **Spec**: `docs/LX.md` lists every syscall, flag and file of `/proc`/`/sys` with its status (full, partial with the
  difference, stub, refused) and the errno for each refused combination.
- **Differential, call level**: k10-lx runs in-process on hosted Linux against a model machine; a generator (syzkaller-
  style descriptions of our syscall subset) produces sequences that run both on Linux 6.18 (in a QEMU guest booted
  from the Tier 1 kernel, pinned) and on k10-lx; return values, errnos, out-parameters, file trees and signal
  deliveries are compared after normalization (pids, addresses, times).
- **Differential, binary level**: the same static test binaries (our `lxprobe` programs, musl libc-test, the gVisor
  syscall test suite, an LTP subset) run on the native kernel under QEMU and on Linux under QEMU; stdout, exit status
  and an strace-format trace (the kernel's `k10.lx_trace`; on Linux a ptrace tracer of our own) are compared.
- **Application level**: the test suites of the corpus images (busybox's testsuite, redis `runtest` subset, nginx's
  perl tests, CPython `regrtest` subset, Node's `test/parallel` subset, a JVM jtreg subset in M2, postgres
  `make check` in M3) inside the converted images.
- **Fuzzing**: syscall fuzzing with coverage on the kernel under QEMU/KVM (crash = kernel panic or isolation
  canary hit); isolation probes (a process scanning for other containers' canary pages and for kernel addresses;
  transient-execution PoCs for each mitigation of §5 on CI hardware that has the bug).
- **Corpus gate**: a pinned set of public images (by digest) converted and run in CI; their ENOSYS/EPERM counters
  must stay at their recorded values.

## 14. Implementation plan

Rules for every session: one commit, independently tested, no regression (`bench/k10s/ab.py --same`, the existing
suites, metal-selftest, image sizes), docs updated as current state. Native-only builds stay byte-for-byte
equivalent in behavior until an OCI feature is enabled.

**Phase 0 — tooling and evidence**
- P0.1 Syscall sizing: a ptrace tracer (`bench/oci/lxtrace`, also the `/init` of the QEMU guest whose initramfs is
  the image) and an ELF static scanner (`bench/oci/elfscan.py`) over the corpus (busybox/alpine, nginx, redis,
  python, node, temurin, postgres, Go and Rust static binaries) on the pinned Debian kernel; output: the
  call/flag/path table `bench/oci/syscalls.md` that drives `docs/LX.md`. `bench/oci/trace.sh` reproduces it; CI
  checks that the checked-in table, docs/LX.md and the plan agree (`bench/oci/table.py --check`).
- P0.2 Test images: `scripts/oci-testimages.sh` builds OCI layouts and `docker save` archives with no Docker, from
  musl 1.2.5 and busybox 1.37.0 built from pinned sources: static musl `lxprobe` (deterministic probes of the M1
  calls, `bench/oci/lxprobe`), the same program dynamically linked with musl's loader, the Go bench workloads
  (CGO off; `echo` as a linux/amd64 + linux/arm64 index), a busybox shell-script entrypoint serving HTTP;
  `os/tools/registry.py` (stdlib-only plain-HTTP distribution API over an OCI layout, bearer-token emulation,
  `--self-test` in CI).

**Phase 1 — user mode in the native kernel** (metal/kernel; sequential)
- P1.1 Higher-half kernel and direct map (all physical/virtual conversions explicit); no behavior change; full
  metal and os tier suites.
- P1.2 Address spaces, PCID, shootdown IPIs, ring 3 entry/exit, SMEP/SMAP/UMIP, CPU gating table and the M1
  mitigations; a static binary prints and exits.
- P1.3 Demand paging, VMAs, mmap/munmap/mprotect/brk, XSAVE per thread, preemption of ring 3, unified scheduler.
- P1.4 k10-lx skeleton (traits, syscall table, fd table) with the hosted differential harness.

**Phase 2 — processes** (k10-lx + metal; sequential)
- P2.1 Threads, futex, signals with exact frames, sigaltstack, timers.
- P2.2 fork/vfork/CoW, execve, wait, process groups, ELF PT_INTERP, auxv, vDSO, ASLR, `#!`.
- P2.3 Pipes, eventfd/timerfd/signalfd, epoll/poll/select, unix sockets with SCM_RIGHTS.

**Phase 3 — files** (parallel with Phase 2 where k10-lx allows)
- P3.1 k10-kvlog layout v2 object area (crash model extended), storage off CPU 0 and NVMe multi-queue.
- P3.2 k10-image (K10I, Merkle) + page cache + image fs + scratch overlay + locks.
- P3.3 Synthetic `/proc`, `/sys`, `/dev`, generated `/etc` files; busybox shell scripts and musl libc-test pass.

**Phase 4 — network**
- P4.1 Socket views over smoltcp (TCP/UDP, both families), container loopback, NETLINK_ROUTE subset.
- P4.2 Ingress handoff (HTTP routes, SNI passthrough, public TCP ports), in-kernel streams, native ↔ image bridging.
- P4.3 Peer-link streams, egress allowlists, stub resolver.
- P4.4 Multi-queue NICs and per-CPU stacks (also for native services).

**Phase 5 — control plane** (crates/k10-mesh, crates/k10-node, CLI; parallel with Phases 1–4)
- P5.1 Manifest format 4 (image services, routes, exact memory), `EXT_CAPS`, placement v3 (fixes the 32-bit item).
- P5.2 Objects: peer range protocol, swarm fetch, upload API, prefetch by placement, GC (hosted nodes first).
- P5.3 `k10-oci` import (docker-save/OCI archive/layout) + converter + inflate/zstd + CLI upload; signatures and
  trust lists.
- P5.4 Lifecycle in the kernel: start/stop/restart/backoff/health/logs/ledger/rollouts.
- **M1 exit**: x86-64 native nodes run the signed converted corpus of stateless services (static Go, busybox/alpine
  scripts, nginx, redis with BGSAVE, Python and Node HTTP servers) with routes, calls, egress, limits, restarts and
  rolling updates, in a mixed cluster with hosted and Tier 1 nodes; corpus, differential and isolation suites green;
  efficiency report (§16).

**Phase 6 — ecosystem** (parallel with Phase 5 after P5.1)
- P6.1 Registry pull over plain HTTP, auth, multi-arch; P6.2 `k10-tls` client + CI pulls of real public images.
- P6.3 Packages (`k10-pkg`), releases with CAS and history; P6.4 importers (compose, k8s, helm-rendered).

**Phase 7 — breadth**
- P7.1 JVM (M2): signal/ucontext exactness, inotify, membarrier, cgroup files, large reservations; jtreg subset.
- P7.2 KPTI and the remaining CPU models of §5 as fleets need them.
- P7.3 Volumes (M3): the log-structured file layer, SysV IPC, postgres.
- P7.4 arm64 personality once the native arm64 arch layer (M5) exists.

Parallelism: P0.x, Phase 5 and Phase 6 are userspace Rust on hosted Linux and run alongside the kernel phases; P3.1
(storage) is independent of Phases 1–2; the kernel phases 1 → 2 → 3.2/3.3 → 4 are sequential.

## 15. Optional variant: Linux-namespace containers on Tier 1 and hosted nodes

Kept as a documented option (Tier 1 is frozen): a privileged runner (a child of PID 1 on Tier 1, forked before the
`--user` drop on hosted nodes) speaking a versioned, capped protocol over a socketpair; namespaces (mnt, pid, ipc,
uts, net, cgroup), cgroup v2 limits, per-layer EROFS images mounted file-backed (`EROFS_FS_BACKED_BY_FILE`, mounted
once per node and shared as overlay lowers: one page cache per layer), overlayfs, veth with routed /31s and
nftables masquerade for egress, the node proxying calls; layers on a `K10S-OCI` ext4 partition (K10S-DATA is a raw
store). Measured on the 6.18.57 `vm` profile (bzImage 3,113,984 bytes today): see the table below. Tier 1's kernel
also lacks what general Linux programs expect (`BINFMT_SCRIPT`, `FILE_LOCKING`, `INOTIFY_USER`, `SYSVIPC`,
`POSIX_MQUEUE`, `AIO`, `MEMBARRIER`, `RSEQ`), so the variant changes Tier 1's kernel for every node unless it is a
separate profile.

| Fragment added to `vm` (cumulative, built with os/kbuild.sh's merge of fragments) | bzImage (zstd) | Δ | vmlinux text+data+bss Δ |
|---|---|---|---|
| none (today) | 3,113,984 B | — | — |
| general Linux ABI: BINFMT_SCRIPT, FILE_LOCKING, INOTIFY_USER, SYSVIPC, POSIX_MQUEUE, AIO, MEMBARRIER, RSEQ, TMPFS_XATTR, UNIX98_PTYS, PROC_PAGE_MONITOR | 3,159,040 B | +44 KiB | +83 KB |
| + namespaces (uts, ipc, pid, net), cgroups (memcg, CFS bandwidth, pids), PSI, overlayfs, veth | 3,281,920 B | +120 KiB | +251 KB |
| + EROFS with file-backed mounts (`MISC_FILESYSTEMS`) | 3,298,304 B | +16 KiB | +30 KB |
| + user namespaces (measured on the previous row without EROFS) | +8 KiB | +8 KiB | +12 KB |
| + nftables NAT/masquerade and conntrack | +140 KiB | +140 KiB | +295 KB |
| all of the above | ≈ 3.44 MB | **≈ +330 KiB (+10.8 %)** | ≈ +0.67 MB resident |

The `x86_64-vm` image (5.0 MB, budget 5.5 MB) would stay within budget. Runtime costs not in the table: memcg
charging on every page fault of native slots too (root-cgroup charges skip the counters but keep the statistics;
to be measured with `ab.py --same` before any adoption), and the runner process (≈ 1–2 MiB).

## 16. Efficiency targets and measurement

| Metric | Target (native kernel, M1) | Compared with |
|---|---|---|
| Node fixed overhead, OCI-capable, idle | ≤ 16 MiB RAM, ≈ 0 % CPU | kind node 583 MiB / 21.9 % (REPORT.md); Docker Engine + containerd idle |
| Per-container overhead beyond the app's RSS | ≤ 256 KiB | Docker: shim + runc + veth + netns; Kubernetes: + pause + kubelet bookkeeping |
| Cold start, image cached (placement → first accept) | ≤ 5 ms static binary, ≤ ld.so time + 5 ms dynamic | `docker run` (0.3–1 s), pod start (1–5 s) |
| Cold start, image not on node | object transfer at link speed from peers, only missing files | layer pulls from a registry |
| Density | containers per GiB for the Go echo image | same image under Docker and kind |
| Image bytes on node and in transit | file-level dedupe across images and versions | layer-level |
| Ingress throughput and latency (small responses, c=1/16/64) | ≥ Docker with port publishing; ≥ kind with ingress-nginx | bench/k8s scripts |
| Bulk throughput | measured; parity with Linux only after P4.4 | Docker host network |

Method: `bench/oci/` drives the same converted images on k10s native nodes (QEMU/KVM on a bench host and bare metal
when available), on Docker and on kind (the existing `bench/k8s` scripts, on CI runners or a dedicated bench host,
never this development machine); RSS from the kernel ledger vs `smem`/cgroup `memory.current`; per-container
overhead = node memory with N idle containers minus N × the app's own RSS. Results in `bench/results/` and
REPORT.md.

## 17. Test strategy

- This machine forbids namespaces and privileges: every runtime test runs in QEMU/KVM guests (`metal/test.sh`
  patterns for the native kernel, `os/test.py` patterns for disks and clusters); the hosted differential harness for
  k10-lx runs unprivileged on the host.
- Test images come from our scripts (P0.2): no Docker on this machine, no owner images. CI-only jobs pull pinned
  public images by digest through `k10s oci pull` (and convert them) for the corpus gate.
- Suites: `metal/lx-test.sh` (kernel: boot, run the probes, isolation, mitigations present per CPU model QEMU can
  emulate), `os/test.py oci` (a mixed cluster: native OCI nodes, hosted and Tier 1 nodes: placement by capability,
  routes, calls across nodes, rollouts, power cuts during object writes), crash model of the object area and volumes
  (k10-kvlog), importer golden files, package render/diff golden files, TLS interop (CI: Docker Hub, GHCR) and
  vectors (Wycheproof-style).

## 18. Open questions

- The CPU gate's coverage of the fleet (which machines would need KPTI or retpolines first).
- smoltcp's suitability for many long-lived container connections (memory per socket, SACK absence on loss); a
  per-CPU stack may need TCP features smoltcp lacks.
- Whether stateful tenants accept M3's node-local volumes, or need replicated volumes (out of scope today).
- io_uring: refused; libuv and some databases fall back today, a future runtime may not.
