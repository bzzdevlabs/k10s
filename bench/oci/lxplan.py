"""The k10-lx syscall plan (docs/LX.md is generated from it by table.py): status and note per x86-64 syscall.

Statuses: M1 (implemented in the first milestone, the note says what is partial), stub (M1, a documented fixed
answer), M2 / M3 (a later milestone; ENOSYS until then), refused (an errno by policy, EPERM unless the note names
another), ENOSYS (not planned: every call missing here). The corpus check in table.py requires every call an M1
corpus image makes to be M1, stub or refused, or a later milestone whose note starts with "M1:" (the M1 answer and
the caller's fallback)."""

M1, STUB, M2, M3, REFUSED = "M1", "stub", "M2", "M3", "refused"


def _group(status, names, note=""):
    return {n: (status, note) for n in names.split()}


PLAN = {}
# process
PLAN.update(_group(M1, "fork vfork execve execveat exit exit_group wait4 waitid getpid getppid gettid set_tid_address "
                       "setpgid getpgid getpgrp setsid getsid getrlimit setrlimit prlimit64 getrusage uname times "
                       "sysinfo sched_yield sched_getaffinity sched_setaffinity getcpu getpriority setpriority "
                       "sched_getparam sched_setparam sched_getscheduler sched_get_priority_max "
                       "sched_get_priority_min sched_rr_get_interval restart_syscall"))
PLAN.update({
    "clone": (M1, "threads, fork and vfork shapes (CLONE_VM|CLONE_VFORK for posix_spawn without a page-table copy); "
                  "CLONE_NEW* EPERM, CLONE_PTRACE/CLONE_UNTRACED ignored"),
    "clone3": (REFUSED, "ENOSYS: glibc (pthread_create, posix_spawn) and Go fall back to clone"),
    "prctl": (M1, "PR_SET/GET_NAME, PR_SET/GET_PDEATHSIG, PR_SET/GET_DUMPABLE, PR_SET/GET_NO_NEW_PRIVS, "
                  "PR_SET/GET_CHILD_SUBREAPER, PR_SET/GET_KEEPCAPS, PR_GET/SET_SECUREBITS, PR_CAPBSET_READ/DROP and "
                  "PR_CAP_AMBIENT within the bounded set, PR_SET/GET_THP_DISABLE, PR_GET/SET_SPECULATION_CTRL, "
                  "PR_SET_VMA (accepted); others EINVAL"),
    "arch_prctl": (M1, "ARCH_SET/GET_FS, ARCH_SET/GET_GS; CET, XCOMP permission and tagging requests EINVAL"),
    "sched_setscheduler": (M1, "SCHED_OTHER, SCHED_BATCH, SCHED_IDLE accepted; real-time policies EPERM"),
    "sched_getattr": (M1, "reports SCHED_OTHER"), "sched_setattr": (M1, "as sched_setscheduler"),
    "personality": (M1, "query and PER_LINUX; any other persona EINVAL"),
    "ioprio_get": (STUB, "reports the default class"), "ioprio_set": (STUB, "accepted, no effect"),
})
# credentials
PLAN.update(_group(M1, "getuid geteuid getgid getegid getresuid getresgid getgroups setuid setgid setreuid "
                       "setregid setresuid setresgid setgroups setfsuid setfsgid umask",
                   "ids are the container's; root has no authority outside it"))
PLAN.update({"capget": (M1, "the container's bounded capability set"),
             "capset": (M1, "can only drop within the bounded set")})
# memory
PLAN.update(_group(M1, "munmap brk mincore msync munlock munlockall"))
PLAN.update({
    "mmap": (M1, "private/shared, anonymous/file, FIXED, FIXED_NOREPLACE, NORESERVE, STACK, POPULATE, GROWSDOWN; "
                 "HUGETLB ENOMEM (no huge pages reserved, callers fall back); DROPPABLE EINVAL (only used with a vDSO "
                 "getrandom, which the vDSO does not export); writable private memory charged at mmap (no overcommit)"),
    "mprotect": (M1, "PROT_GROWSDOWN accepted"), "mremap": (M1, "MAYMOVE, FIXED, DONTUNMAP"),
    "madvise": (M1, "DONTNEED, FREE, WILLNEED, DONTFORK/DOFORK, WIPEONFORK/KEEPONFORK, NORMAL/RANDOM/SEQUENTIAL, "
                    "POPULATE_READ/WRITE, (NO)HUGEPAGE and DONTDUMP as no-ops; GUARD_INSTALL and others EINVAL"),
    "mlock": (M1, "accounted against RLIMIT_MEMLOCK, no pinning semantics"), "mlock2": (M1, "as mlock"),
    "mlockall": (M1, "as mlock"),
    "membarrier": (M2, "QUERY answers 0 (no command) in M1; PRIVATE_EXPEDITED(_SYNC_CORE) via IPIs for the JVM"),
    "mbind": (M2, "single NUMA node: MPOL_DEFAULT only"), "set_mempolicy": (M2, "single NUMA node"),
    "get_mempolicy": (M2, "single NUMA node"),
})
# signals
PLAN.update(_group(M1, "rt_sigaction rt_sigprocmask rt_sigreturn rt_sigpending rt_sigtimedwait rt_sigsuspend "
                       "sigaltstack kill tgkill tkill rt_sigqueueinfo rt_tgsigqueueinfo pidfd_open "
                       "pidfd_send_signal alarm setitimer getitimer timer_create timer_settime timer_gettime "
                       "timer_getoverrun timer_delete pause",
                   "exact siginfo/ucontext/XSAVE frames"))
# sync, time
PLAN.update(_group(M1, "set_robust_list get_robust_list nanosleep clock_nanosleep clock_getres getrandom"))
PLAN.update({
    "futex": (M1, "WAIT, WAKE, REQUEUE, CMP_REQUEUE, WAKE_OP, WAIT_BITSET, WAKE_BITSET, PRIVATE, CLOCK_REALTIME; "
                  "PI operations M2"),
    "futex_waitv": (REFUSED, "ENOSYS"),
    "rseq": (REFUSED, "ENOSYS: glibc continues without restartable sequences"),
    "clock_gettime": (M1, "vDSO; the syscall for CPU-time clocks"), "gettimeofday": (M1, "vDSO"),
    "time": (M1, "vDSO"),
})
# files
PLAN.update(_group(M1, "open openat close close_range read readv write writev pread64 pwrite64 preadv pwritev lseek "
                       "dup dup2 dup3 flock newfstatat fstat stat lstat statx statfs fstatfs getdents64 getdents "
                       "getcwd chdir fchdir mkdir mkdirat rmdir unlink unlinkat rename renameat link linkat symlink "
                       "symlinkat readlink readlinkat chmod fchmod fchmodat fchmodat2 chown fchown fchownat lchown "
                       "utimensat utime utimes futimesat access faccessat faccessat2 truncate ftruncate fsync "
                       "fdatasync sync syncfs sync_file_range sendfile copy_file_range memfd_create creat",
                   "paths resolved by the personality's VFS"))
PLAN.update({
    "openat2": (M1, "RESOLVE_NO_SYMLINKS, RESOLVE_BENEATH, RESOLVE_IN_ROOT, RESOLVE_NO_XDEV; others EINVAL"),
    "renameat2": (M1, "NOREPLACE, EXCHANGE; WHITEOUT EPERM"),
    "preadv2": (M1, "RWF_HIPRI/DSYNC/SYNC/APPEND; RWF_NOWAIT EOPNOTSUPP"), "pwritev2": (M1, "as preadv2"),
    "fcntl": (M1, "DUPFD(_CLOEXEC), GET/SETFD, GET/SETFL, GETLK/SETLK/SETLKW and OFD locks, GET/SETPIPE_SZ, "
                  "ADD/GET_SEALS, SETOWN/GETOWN (no SIGIO delivery: M2); leases and F_NOTIFY EINVAL"),
    "ioctl": (M1, "FIONBIO, FIONREAD, FIOCLEX, FIONCLEX, FIOASYNC (EINVAL); TCGETS, TIOCGWINSZ, TIOCGPGRP "
                  "ENOTTY on non-terminals; SIOCGIFCONF/FLAGS/INDEX/MTU on the container's interfaces; others ENOTTY"),
    "fallocate": (M1, "mode 0 and KEEP_SIZE; PUNCH_HOLE and ZERO_RANGE M3; others EOPNOTSUPP"),
    "fadvise64": (STUB, "accepted, no effect"), "readahead": (STUB, "accepted, no effect"),
    "mknod": (M1, "FIFOs and regular files; device nodes EPERM"), "mknodat": (M1, "as mknod"),
    "getxattr": (M1, "user.* and security.capability read; system.posix_acl_* EOPNOTSUPP (no ACLs); others "
                     "ENODATA"), "lgetxattr": (M1, "as getxattr"),
    "fgetxattr": (M1, "as getxattr"), "listxattr": (M1, "user.*"), "llistxattr": (M1, "user.*"),
    "flistxattr": (M1, "user.*"),
    "setxattr": (M1, "user.* stored; system.posix_acl_* EOPNOTSUPP (sed -i and cp -p then fall back to chmod); "
                     "security.* and trusted.* EPERM"),
    "lsetxattr": (M1, "as setxattr"), "fsetxattr": (M1, "as setxattr"), "removexattr": (M1, "user.*"),
    "lremovexattr": (M1, "user.*"),
    "fremovexattr": (M1, "user.*"),
    "inotify_init": (M2, ""), "inotify_init1": (M2, ""), "inotify_add_watch": (M2, ""), "inotify_rm_watch": (M2, ""),
    "splice": (M2, ""), "tee": (M2, ""), "vmsplice": (M2, ""),
    "name_to_handle_at": (REFUSED, "EOPNOTSUPP"),
    "chroot": (REFUSED, "a chroot inside the container's own tree is a later candidate"),
})
# pipes, events
PLAN.update(_group(M1, "pipe pipe2 eventfd eventfd2 timerfd_create timerfd_settime timerfd_gettime signalfd signalfd4 "
                       "epoll_create epoll_create1 epoll_ctl epoll_wait epoll_pwait epoll_pwait2 poll ppoll select "
                       "pselect6", "one readiness core for all"))
# sockets
PLAN.update(_group(M1, "socketpair bind listen accept accept4 connect shutdown sendto recvfrom sendmsg recvmsg "
                       "sendmmsg recvmmsg getsockname getpeername",
                   "AF_UNIX (STREAM, DGRAM, SEQPACKET, SCM_RIGHTS, SCM_CREDENTIALS), AF_INET and AF_INET6 "
                   "(STREAM, DGRAM) over the container's socket view"))
PLAN.update({
    "socket": (M1, "AF_UNIX, AF_INET, AF_INET6; AF_NETLINK/NETLINK_ROUTE answers RTM_GETLINK/GETADDR/GETROUTE; "
                   "RAW, PACKET and other netlink families EPERM, other families EAFNOSUPPORT"),
    "setsockopt": (M1, "SO_REUSEADDR/REUSEPORT/KEEPALIVE/RCVBUF/SNDBUF/LINGER/RCVTIMEO/SNDTIMEO/PASSCRED, TCP_NODELAY/"
                       "KEEPIDLE/KEEPINTVL/KEEPCNT/CORK/DEFER_ACCEPT/USER_TIMEOUT/FASTOPEN, IPV6_V6ONLY, IP_TOS, "
                       "IP_RECVERR; others ENOPROTOOPT"),
    "getsockopt": (M1, "SO_ERROR, SO_TYPE, SO_DOMAIN, SO_PROTOCOL, SO_ACCEPTCONN, SO_PEERCRED, SO_RCVBUF/SNDBUF, "
                       "TCP_INFO (subset), the set options above"),
})
# IPC (postgres, M3)
PLAN.update(_group(M3, "shmget shmat shmdt shmctl semget semop semtimedop semctl msgget msgsnd msgrcv msgctl",
                   "SysV IPC, per container namespace (postgres)"))
PLAN.update(_group(M3, "io_setup io_destroy io_getevents io_pgetevents io_submit io_cancel",
                   "M1: ENOSYS (nginx logs it and serves files without native AIO; libaio users use threads)"))
PLAN.update(_group(M2, "mq_open mq_unlink mq_timedsend mq_timedreceive mq_notify mq_getsetattr", "POSIX queues"))
# refused by policy
PLAN.update(_group(REFUSED, "mount umount2 pivot_root setns swapon swapoff syslog settimeofday clock_settime "
                            "clock_adjtime adjtimex sethostname setdomainname init_module finit_module delete_module "
                            "kexec_load kexec_file_load bpf perf_event_open userfaultfd ptrace process_vm_readv "
                            "process_vm_writev keyctl add_key request_key reboot acct quotactl quotactl_fd iopl ioperm "
                            "vhangup open_by_handle_at fanotify_init fanotify_mark fsopen fsconfig fsmount fspick "
                            "open_tree open_tree_attr move_mount mount_setattr statmount listmount kcmp pidfd_getfd "
                            "process_madvise process_mrelease lookup_dcookie migrate_pages move_pages "
                            "set_mempolicy_home_node modify_ldt",
                   "outside the container boundary (oci.md §0, §3)"))
PLAN.update({
    "unshare": (REFUSED, "CLONE_NEW* EPERM; CLONE_FILES, CLONE_FS and CLONE_SYSVSEM are M1"),
    "io_uring_setup": (REFUSED, "EPERM as under Docker's default seccomp profile; libuv and others fall back"),
    "io_uring_enter": (REFUSED, ""), "io_uring_register": (REFUSED, ""),
    "pkey_alloc": (REFUSED, "ENOSPC, as on a CPU without protection keys"), "pkey_free": (REFUSED, "EINVAL"),
    "pkey_mprotect": (REFUSED, "EINVAL"),
    "seccomp": (REFUSED, "ENOSYS: the personality is the filter"),
    "landlock_create_ruleset": (REFUSED, "ENOSYS"), "landlock_add_rule": (REFUSED, "ENOSYS"),
    "landlock_restrict_self": (REFUSED, "ENOSYS"),
    "memfd_secret": (REFUSED, "ENOSYS"), "map_shadow_stack": (REFUSED, "ENOSYS (shadow stacks: later)"),
    "mseal": (REFUSED, "ENOSYS"),
})
NOTES_ENOSYS = "not implemented: ENOSYS, counted per container in /_k10s/status"


def status(name):
    return PLAN.get(name, ("ENOSYS", ""))
