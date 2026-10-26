"""x86-64 Linux ABI tables for the syscall sizing: syscall names (Linux 6.18 syscall_64.tbl, x32 entries omitted),
errno and signal names, and decoders for the flag arguments lxtrace records (lxtrace/src/main.rs `spec`)."""

SYSCALLS = {
    0: "read", 1: "write", 2: "open", 3: "close", 4: "stat", 5: "fstat", 6: "lstat", 7: "poll", 8: "lseek",
    9: "mmap", 10: "mprotect", 11: "munmap", 12: "brk", 13: "rt_sigaction", 14: "rt_sigprocmask", 15: "rt_sigreturn",
    16: "ioctl", 17: "pread64", 18: "pwrite64", 19: "readv", 20: "writev", 21: "access", 22: "pipe", 23: "select",
    24: "sched_yield", 25: "mremap", 26: "msync", 27: "mincore", 28: "madvise", 29: "shmget", 30: "shmat",
    31: "shmctl", 32: "dup", 33: "dup2", 34: "pause", 35: "nanosleep", 36: "getitimer", 37: "alarm", 38: "setitimer",
    39: "getpid", 40: "sendfile", 41: "socket", 42: "connect", 43: "accept", 44: "sendto", 45: "recvfrom",
    46: "sendmsg", 47: "recvmsg", 48: "shutdown", 49: "bind", 50: "listen", 51: "getsockname", 52: "getpeername",
    53: "socketpair", 54: "setsockopt", 55: "getsockopt", 56: "clone", 57: "fork", 58: "vfork", 59: "execve",
    60: "exit", 61: "wait4", 62: "kill", 63: "uname", 64: "semget", 65: "semop", 66: "semctl", 67: "shmdt",
    68: "msgget", 69: "msgsnd", 70: "msgrcv", 71: "msgctl", 72: "fcntl", 73: "flock", 74: "fsync", 75: "fdatasync",
    76: "truncate", 77: "ftruncate", 78: "getdents", 79: "getcwd", 80: "chdir", 81: "fchdir", 82: "rename",
    83: "mkdir", 84: "rmdir", 85: "creat", 86: "link", 87: "unlink", 88: "symlink", 89: "readlink", 90: "chmod",
    91: "fchmod", 92: "chown", 93: "fchown", 94: "lchown", 95: "umask", 96: "gettimeofday", 97: "getrlimit",
    98: "getrusage", 99: "sysinfo", 100: "times", 101: "ptrace", 102: "getuid", 103: "syslog", 104: "getgid",
    105: "setuid", 106: "setgid", 107: "geteuid", 108: "getegid", 109: "setpgid", 110: "getppid", 111: "getpgrp",
    112: "setsid", 113: "setreuid", 114: "setregid", 115: "getgroups", 116: "setgroups", 117: "setresuid",
    118: "getresuid", 119: "setresgid", 120: "getresgid", 121: "getpgid", 122: "setfsuid", 123: "setfsgid",
    124: "getsid", 125: "capget", 126: "capset", 127: "rt_sigpending", 128: "rt_sigtimedwait",
    129: "rt_sigqueueinfo", 130: "rt_sigsuspend", 131: "sigaltstack", 132: "utime", 133: "mknod", 134: "uselib",
    135: "personality", 136: "ustat", 137: "statfs", 138: "fstatfs", 139: "sysfs", 140: "getpriority",
    141: "setpriority", 142: "sched_setparam", 143: "sched_getparam", 144: "sched_setscheduler",
    145: "sched_getscheduler", 146: "sched_get_priority_max", 147: "sched_get_priority_min",
    148: "sched_rr_get_interval", 149: "mlock", 150: "munlock", 151: "mlockall", 152: "munlockall", 153: "vhangup",
    154: "modify_ldt", 155: "pivot_root", 156: "_sysctl", 157: "prctl", 158: "arch_prctl", 159: "adjtimex",
    160: "setrlimit", 161: "chroot", 162: "sync", 163: "acct", 164: "settimeofday", 165: "mount", 166: "umount2",
    167: "swapon", 168: "swapoff", 169: "reboot", 170: "sethostname", 171: "setdomainname", 172: "iopl",
    173: "ioperm", 174: "create_module", 175: "init_module", 176: "delete_module", 177: "get_kernel_syms",
    178: "query_module", 179: "quotactl", 180: "nfsservctl", 181: "getpmsg", 182: "putpmsg", 183: "afs_syscall",
    184: "tuxcall", 185: "security", 186: "gettid", 187: "readahead", 188: "setxattr", 189: "lsetxattr",
    190: "fsetxattr", 191: "getxattr", 192: "lgetxattr", 193: "fgetxattr", 194: "listxattr", 195: "llistxattr",
    196: "flistxattr", 197: "removexattr", 198: "lremovexattr", 199: "fremovexattr", 200: "tkill", 201: "time",
    202: "futex", 203: "sched_setaffinity", 204: "sched_getaffinity", 205: "set_thread_area", 206: "io_setup",
    207: "io_destroy", 208: "io_getevents", 209: "io_submit", 210: "io_cancel", 211: "get_thread_area",
    212: "lookup_dcookie", 213: "epoll_create", 214: "epoll_ctl_old", 215: "epoll_wait_old", 216: "remap_file_pages",
    217: "getdents64", 218: "set_tid_address", 219: "restart_syscall", 220: "semtimedop", 221: "fadvise64",
    222: "timer_create", 223: "timer_settime", 224: "timer_gettime", 225: "timer_getoverrun", 226: "timer_delete",
    227: "clock_settime", 228: "clock_gettime", 229: "clock_getres", 230: "clock_nanosleep", 231: "exit_group",
    232: "epoll_wait", 233: "epoll_ctl", 234: "tgkill", 235: "utimes", 236: "vserver", 237: "mbind",
    238: "set_mempolicy", 239: "get_mempolicy", 240: "mq_open", 241: "mq_unlink", 242: "mq_timedsend",
    243: "mq_timedreceive", 244: "mq_notify", 245: "mq_getsetattr", 246: "kexec_load", 247: "waitid", 248: "add_key",
    249: "request_key", 250: "keyctl", 251: "ioprio_set", 252: "ioprio_get", 253: "inotify_init",
    254: "inotify_add_watch", 255: "inotify_rm_watch", 256: "migrate_pages", 257: "openat", 258: "mkdirat",
    259: "mknodat", 260: "fchownat", 261: "futimesat", 262: "newfstatat", 263: "unlinkat", 264: "renameat",
    265: "linkat", 266: "symlinkat", 267: "readlinkat", 268: "fchmodat", 269: "faccessat", 270: "pselect6",
    271: "ppoll", 272: "unshare", 273: "set_robust_list", 274: "get_robust_list", 275: "splice", 276: "tee",
    277: "sync_file_range", 278: "vmsplice", 279: "move_pages", 280: "utimensat", 281: "epoll_pwait",
    282: "signalfd", 283: "timerfd_create", 284: "eventfd", 285: "fallocate", 286: "timerfd_settime",
    287: "timerfd_gettime", 288: "accept4", 289: "signalfd4", 290: "eventfd2", 291: "epoll_create1", 292: "dup3",
    293: "pipe2", 294: "inotify_init1", 295: "preadv", 296: "pwritev", 297: "rt_tgsigqueueinfo",
    298: "perf_event_open", 299: "recvmmsg", 300: "fanotify_init", 301: "fanotify_mark", 302: "prlimit64",
    303: "name_to_handle_at", 304: "open_by_handle_at", 305: "clock_adjtime", 306: "syncfs", 307: "sendmmsg",
    308: "setns", 309: "getcpu", 310: "process_vm_readv", 311: "process_vm_writev", 312: "kcmp", 313: "finit_module",
    314: "sched_setattr", 315: "sched_getattr", 316: "renameat2", 317: "seccomp", 318: "getrandom",
    319: "memfd_create", 320: "kexec_file_load", 321: "bpf", 322: "execveat", 323: "userfaultfd", 324: "membarrier",
    325: "mlock2", 326: "copy_file_range", 327: "preadv2", 328: "pwritev2", 329: "pkey_mprotect", 330: "pkey_alloc",
    331: "pkey_free", 332: "statx", 333: "io_pgetevents", 334: "rseq", 335: "uretprobe", 336: "uprobe",
    424: "pidfd_send_signal", 425: "io_uring_setup", 426: "io_uring_enter", 427: "io_uring_register",
    428: "open_tree", 429: "move_mount", 430: "fsopen", 431: "fsconfig", 432: "fsmount", 433: "fspick",
    434: "pidfd_open", 435: "clone3", 436: "close_range", 437: "openat2", 438: "pidfd_getfd", 439: "faccessat2",
    440: "process_madvise", 441: "epoll_pwait2", 442: "mount_setattr", 443: "quotactl_fd",
    444: "landlock_create_ruleset", 445: "landlock_add_rule", 446: "landlock_restrict_self", 447: "memfd_secret",
    448: "process_mrelease", 449: "futex_waitv", 450: "set_mempolicy_home_node", 451: "cachestat", 452: "fchmodat2",
    453: "map_shadow_stack", 454: "futex_wake", 455: "futex_wait", 456: "futex_requeue", 457: "statmount",
    458: "listmount", 459: "lsm_get_self_attr", 460: "lsm_set_self_attr", 461: "lsm_list_modules", 462: "mseal",
    463: "setxattrat", 464: "getxattrat", 465: "listxattrat", 466: "removexattrat", 467: "open_tree_attr",
    468: "file_getattr", 469: "file_setattr",
}
NR = {v: k for k, v in SYSCALLS.items()}

ERRNO = {
    1: "EPERM", 2: "ENOENT", 3: "ESRCH", 4: "EINTR", 5: "EIO", 6: "ENXIO", 7: "E2BIG", 8: "ENOEXEC", 9: "EBADF",
    10: "ECHILD", 11: "EAGAIN", 12: "ENOMEM", 13: "EACCES", 14: "EFAULT", 15: "ENOTBLK", 16: "EBUSY", 17: "EEXIST",
    18: "EXDEV", 19: "ENODEV", 20: "ENOTDIR", 21: "EISDIR", 22: "EINVAL", 23: "ENFILE", 24: "EMFILE", 25: "ENOTTY",
    26: "ETXTBSY", 27: "EFBIG", 28: "ENOSPC", 29: "ESPIPE", 30: "EROFS", 31: "EMLINK", 32: "EPIPE", 33: "EDOM",
    34: "ERANGE", 35: "EDEADLK", 36: "ENAMETOOLONG", 37: "ENOLCK", 38: "ENOSYS", 39: "ENOTEMPTY", 40: "ELOOP",
    42: "ENOMSG", 43: "EIDRM", 44: "ECHRNG", 45: "EL2NSYNC", 46: "EL3HLT", 47: "EL3RST", 48: "ELNRNG", 49: "EUNATCH",
    50: "ENOCSI", 51: "EL2HLT", 52: "EBADE", 53: "EBADR", 54: "EXFULL", 55: "ENOANO", 56: "EBADRQC", 57: "EBADSLT",
    59: "EBFONT", 60: "ENOSTR", 61: "ENODATA", 62: "ETIME", 63: "ENOSR", 64: "ENONET", 65: "ENOPKG", 66: "EREMOTE",
    67: "ENOLINK", 68: "EADV", 69: "ESRMNT", 70: "ECOMM", 71: "EPROTO", 72: "EMULTIHOP", 73: "EDOTDOT",
    74: "EBADMSG", 75: "EOVERFLOW", 76: "ENOTUNIQ", 77: "EBADFD", 78: "EREMCHG", 79: "ELIBACC", 80: "ELIBBAD",
    81: "ELIBSCN", 82: "ELIBMAX", 83: "ELIBEXEC", 84: "EILSEQ", 85: "ERESTART", 86: "ESTRPIPE", 87: "EUSERS",
    88: "ENOTSOCK", 89: "EDESTADDRREQ", 90: "EMSGSIZE", 91: "EPROTOTYPE", 92: "ENOPROTOOPT", 93: "EPROTONOSUPPORT",
    94: "ESOCKTNOSUPPORT", 95: "EOPNOTSUPP", 96: "EPFNOSUPPORT", 97: "EAFNOSUPPORT", 98: "EADDRINUSE",
    99: "EADDRNOTAVAIL", 100: "ENETDOWN", 101: "ENETUNREACH", 102: "ENETRESET", 103: "ECONNABORTED",
    104: "ECONNRESET", 105: "ENOBUFS", 106: "EISCONN", 107: "ENOTCONN", 108: "ESHUTDOWN", 109: "ETOOMANYREFS",
    110: "ETIMEDOUT", 111: "ECONNREFUSED", 112: "EHOSTDOWN", 113: "EHOSTUNREACH", 114: "EALREADY",
    115: "EINPROGRESS", 116: "ESTALE", 117: "EUCLEAN", 118: "ENOTNAM", 119: "ENAVAIL", 120: "EISNAM",
    121: "EREMOTEIO", 122: "EDQUOT", 123: "ENOMEDIUM", 124: "EMEDIUMTYPE", 125: "ECANCELED", 126: "ENOKEY",
    127: "EKEYEXPIRED", 128: "EKEYREVOKED", 129: "EKEYREJECTED", 130: "EOWNERDEAD", 131: "ENOTRECOVERABLE",
    132: "ERFKILL",
    512: "ERESTARTSYS", 513: "ERESTARTNOINTR", 514: "ERESTARTNOHAND", 516: "ERESTART_RESTARTBLOCK",
}

SIGNALS = {1: "SIGHUP", 2: "SIGINT", 3: "SIGQUIT", 4: "SIGILL", 5: "SIGTRAP", 6: "SIGABRT", 7: "SIGBUS", 8: "SIGFPE",
           9: "SIGKILL", 10: "SIGUSR1", 11: "SIGSEGV", 12: "SIGUSR2", 13: "SIGPIPE", 14: "SIGALRM", 15: "SIGTERM",
           16: "SIGSTKFLT", 17: "SIGCHLD", 18: "SIGCONT", 19: "SIGSTOP", 20: "SIGTSTP", 21: "SIGTTIN", 22: "SIGTTOU",
           23: "SIGURG", 24: "SIGXCPU", 25: "SIGXFSZ", 26: "SIGVTALRM", 27: "SIGPROF", 28: "SIGWINCH", 29: "SIGIO",
           30: "SIGPWR", 31: "SIGSYS"}


def signame(n):
    return SIGNALS.get(n) or (f"SIGRTMIN+{n - 32}" if 32 <= n <= 64 else str(n))


def bits(v, table, rest_hex=True):
    out = []
    for bit, name in table:
        if v & bit == bit and bit:
            out.append(name)
            v &= ~bit
    if v and rest_hex:
        out.append(hex(v))
    return "|".join(out) or "0"


def enum(table):
    return lambda v: table.get(v, hex(v))


FD_FLAGS = [(0x800, "NONBLOCK"), (0x80000, "CLOEXEC")]
O_FLAGS = [(0o20200000, "O_TMPFILE"), (0o4010000, "O_SYNC"), (0o100, "O_CREAT"), (0o200, "O_EXCL"),
           (0o400, "O_NOCTTY"), (0o1000, "O_TRUNC"), (0o2000, "O_APPEND"), (0o4000, "O_NONBLOCK"),
           (0o10000, "O_DSYNC"), (0o20000, "O_ASYNC"), (0o40000, "O_DIRECT"), (0o100000, "O_LARGEFILE"),
           (0o200000, "O_DIRECTORY"), (0o400000, "O_NOFOLLOW"), (0o1000000, "O_NOATIME"), (0o2000000, "O_CLOEXEC"),
           (0o10000000, "O_PATH")]


def oflags(v):
    acc = ["O_RDONLY", "O_WRONLY", "O_RDWR", "O_ACCMODE3"][v & 3]
    rest = bits(v & ~3, O_FLAGS)
    return acc if rest == "0" else f"{acc}|{rest}"


PROT = [(1, "READ"), (2, "WRITE"), (4, "EXEC"), (8, "SEM"), (0x01000000, "GROWSDOWN"), (0x02000000, "GROWSUP")]
MAP = [(0x10, "FIXED"), (0x20, "ANONYMOUS"), (0x40, "32BIT"), (0x100, "GROWSDOWN"), (0x800, "DENYWRITE"),
       (0x1000, "EXECUTABLE"), (0x2000, "LOCKED"), (0x4000, "NORESERVE"), (0x8000, "POPULATE"),
       (0x10000, "NONBLOCK"), (0x20000, "STACK"), (0x40000, "HUGETLB"), (0x80000, "SYNC"),
       (0x100000, "FIXED_NOREPLACE"), (0x4000000, "UNINITIALIZED")]


def prot(v):
    return bits(v, PROT) if v else "NONE"


def mapflags(v):
    kind = {1: "SHARED", 2: "PRIVATE", 3: "SHARED_VALIDATE", 8: "DROPPABLE"}.get(v & 0xf, hex(v & 0xf))
    huge = (v >> 26) & 0x3f
    rest = bits(v & ~0xf & ~(0x3f << 26), MAP)
    return kind + ("" if rest == "0" else "|" + rest) + (f"|HUGE_2^{huge}" if huge else "")


CLONE = [(0x100, "VM"), (0x200, "FS"), (0x400, "FILES"), (0x800, "SIGHAND"), (0x1000, "PIDFD"), (0x2000, "PTRACE"),
         (0x4000, "VFORK"), (0x8000, "PARENT"), (0x10000, "THREAD"), (0x20000, "NEWNS"), (0x40000, "SYSVSEM"),
         (0x80000, "SETTLS"), (0x100000, "PARENT_SETTID"), (0x200000, "CHILD_CLEARTID"), (0x400000, "DETACHED"),
         (0x800000, "UNTRACED"), (0x1000000, "CHILD_SETTID"), (0x2000000, "NEWCGROUP"), (0x4000000, "NEWUTS"),
         (0x8000000, "NEWIPC"), (0x10000000, "NEWUSER"), (0x20000000, "NEWPID"), (0x40000000, "NEWNET"),
         (0x80000000, "IO"), (0x100000000, "CLEAR_SIGHAND"), (0x200000000, "INTO_CGROUP")]


def clone(v):
    sig = v & 0xff
    rest = bits(v & ~0xff, CLONE)
    return rest + (f"|{signame(sig)}" if sig else "")


def clone3(v):
    return bits(v, [(0x80, "NEWTIME")] + CLONE)


FCNTL = {0: "F_DUPFD", 1: "F_GETFD", 2: "F_SETFD", 3: "F_GETFL", 4: "F_SETFL", 5: "F_GETLK", 6: "F_SETLK",
         7: "F_SETLKW", 8: "F_SETOWN", 9: "F_GETOWN", 10: "F_SETSIG", 11: "F_GETSIG", 15: "F_SETOWN_EX",
         16: "F_GETOWN_EX", 36: "F_OFD_GETLK", 37: "F_OFD_SETLK", 38: "F_OFD_SETLKW", 1024: "F_SETLEASE",
         1025: "F_GETLEASE", 1026: "F_NOTIFY", 1030: "F_DUPFD_CLOEXEC", 1031: "F_SETPIPE_SZ", 1032: "F_GETPIPE_SZ",
         1033: "F_ADD_SEALS", 1034: "F_GET_SEALS"}
IOCTL = {0x5401: "TCGETS", 0x5402: "TCSETS", 0x5403: "TCSETSW", 0x5404: "TCSETSF", 0x5405: "TCGETA",
         0x5409: "TCSBRK", 0x540a: "TCXONC", 0x540b: "TCFLSH", 0x540c: "TIOCEXCL", 0x540e: "TIOCSCTTY",
         0x540f: "TIOCGPGRP", 0x5410: "TIOCSPGRP", 0x5411: "TIOCOUTQ", 0x5412: "TIOCSTI", 0x5413: "TIOCGWINSZ",
         0x5414: "TIOCSWINSZ", 0x5415: "TIOCMGET", 0x541b: "FIONREAD", 0x5421: "FIONBIO", 0x5422: "TIOCNOTTY",
         0x5429: "TIOCGSID", 0x5441: "TIOCGPTPEER", 0x5450: "FIONCLEX", 0x5451: "FIOCLEX", 0x5452: "FIOASYNC",
         0x5460: "FIOQSIZE", 0x802c542a: "TCGETS2", 0x80045430: "TIOCGPTN", 0x40045431: "TIOCSPTLCK",
         0x8905: "SIOCATMARK", 0x8906: "SIOCGSTAMP", 0x8910: "SIOCGIFNAME", 0x8912: "SIOCGIFCONF",
         0x8913: "SIOCGIFFLAGS", 0x8914: "SIOCSIFFLAGS", 0x8915: "SIOCGIFADDR", 0x891b: "SIOCGIFNETMASK",
         0x8921: "SIOCGIFMTU", 0x8927: "SIOCGIFHWADDR", 0x8933: "SIOCGIFINDEX", 0x80086601: "FS_IOC_GETFLAGS",
         0x40086602: "FS_IOC_SETFLAGS", 0x40049409: "FICLONE", 0xc020660b: "FS_IOC_FIEMAP",
         0x80081272: "BLKGETSIZE64", 0x80045200: "RNDGETENTCNT", 0x80045440: "TIOCGEXCL"}
DOMAIN = {0: "AF_UNSPEC", 1: "AF_UNIX", 2: "AF_INET", 10: "AF_INET6", 16: "AF_NETLINK", 17: "AF_PACKET",
          38: "AF_ALG", 40: "AF_VSOCK"}


def socktype(v):
    t = {1: "STREAM", 2: "DGRAM", 3: "RAW", 4: "RDM", 5: "SEQPACKET", 10: "PACKET"}.get(v & 0xf, hex(v & 0xf))
    rest = bits(v & ~0xf, FD_FLAGS)
    return t + ("" if rest == "0" else "|" + rest)


PROTO = {0: "0", 1: "ICMP", 6: "TCP", 17: "UDP", 58: "ICMPV6", 255: "RAW"}
LEVEL = {0: "IPPROTO_IP", 1: "SOL_SOCKET", 6: "IPPROTO_TCP", 17: "IPPROTO_UDP", 41: "IPPROTO_IPV6",
         270: "SOL_NETLINK", 263: "SOL_PACKET", 274: "SOL_ALG"}
OPTNAME = {
    1: {1: "SO_DEBUG", 2: "SO_REUSEADDR", 3: "SO_TYPE", 4: "SO_ERROR", 5: "SO_DONTROUTE", 6: "SO_BROADCAST",
        7: "SO_SNDBUF", 8: "SO_RCVBUF", 9: "SO_KEEPALIVE", 10: "SO_OOBINLINE", 12: "SO_PRIORITY", 13: "SO_LINGER",
        15: "SO_REUSEPORT", 16: "SO_PASSCRED", 17: "SO_PEERCRED", 18: "SO_RCVLOWAT", 19: "SO_SNDLOWAT",
        20: "SO_RCVTIMEO_OLD", 21: "SO_SNDTIMEO_OLD", 25: "SO_BINDTODEVICE", 26: "SO_ATTACH_FILTER",
        29: "SO_TIMESTAMP_OLD", 30: "SO_ACCEPTCONN", 31: "SO_PEERSEC", 32: "SO_SNDBUFFORCE", 33: "SO_RCVBUFFORCE",
        34: "SO_PASSSEC", 35: "SO_TIMESTAMPNS_OLD", 36: "SO_MARK", 38: "SO_PROTOCOL", 39: "SO_DOMAIN",
        40: "SO_RXQ_OVFL", 42: "SO_PEEK_OFF", 46: "SO_BUSY_POLL", 47: "SO_MAX_PACING_RATE", 49: "SO_INCOMING_CPU",
        50: "SO_ATTACH_BPF", 51: "SO_ATTACH_REUSEPORT_CBPF", 59: "SO_PEERGROUPS", 60: "SO_ZEROCOPY",
        66: "SO_RCVTIMEO_NEW", 67: "SO_SNDTIMEO_NEW"},
    6: {1: "TCP_NODELAY", 2: "TCP_MAXSEG", 3: "TCP_CORK", 4: "TCP_KEEPIDLE", 5: "TCP_KEEPINTVL", 6: "TCP_KEEPCNT",
        7: "TCP_SYNCNT", 8: "TCP_LINGER2", 9: "TCP_DEFER_ACCEPT", 10: "TCP_WINDOW_CLAMP", 11: "TCP_INFO",
        12: "TCP_QUICKACK", 13: "TCP_CONGESTION", 18: "TCP_USER_TIMEOUT", 23: "TCP_FASTOPEN",
        25: "TCP_NOTSENT_LOWAT", 30: "TCP_FASTOPEN_CONNECT", 31: "TCP_ULP"},
    0: {1: "IP_TOS", 2: "IP_TTL", 3: "IP_HDRINCL", 8: "IP_PKTINFO", 10: "IP_MTU_DISCOVER", 11: "IP_RECVERR",
        14: "IP_MTU", 15: "IP_FREEBIND", 19: "IP_TRANSPARENT", 24: "IP_BIND_ADDRESS_NO_PORT", 32: "IP_MULTICAST_IF",
        33: "IP_MULTICAST_TTL", 34: "IP_MULTICAST_LOOP", 35: "IP_ADD_MEMBERSHIP", 36: "IP_DROP_MEMBERSHIP"},
    41: {16: "IPV6_UNICAST_HOPS", 17: "IPV6_MULTICAST_IF", 18: "IPV6_MULTICAST_HOPS", 19: "IPV6_MULTICAST_LOOP",
         20: "IPV6_ADD_MEMBERSHIP", 21: "IPV6_DROP_MEMBERSHIP", 23: "IPV6_MTU_DISCOVER", 25: "IPV6_RECVERR",
         26: "IPV6_V6ONLY", 49: "IPV6_RECVPKTINFO", 50: "IPV6_PKTINFO", 67: "IPV6_TCLASS"},
    270: {1: "NETLINK_ADD_MEMBERSHIP", 3: "NETLINK_PKTINFO", 11: "NETLINK_CAP_ACK", 12: "NETLINK_EXT_ACK",
          13: "NETLINK_GET_STRICT_CHK"},
}
PRCTL = {1: "PR_SET_PDEATHSIG", 2: "PR_GET_PDEATHSIG", 3: "PR_GET_DUMPABLE", 4: "PR_SET_DUMPABLE",
         7: "PR_GET_KEEPCAPS", 8: "PR_SET_KEEPCAPS", 15: "PR_SET_NAME", 16: "PR_GET_NAME", 21: "PR_GET_SECCOMP",
         22: "PR_SET_SECCOMP", 23: "PR_CAPBSET_READ", 24: "PR_CAPBSET_DROP", 27: "PR_GET_SECUREBITS",
         28: "PR_SET_SECUREBITS", 35: "PR_SET_MM",
         36: "PR_SET_CHILD_SUBREAPER", 37: "PR_GET_CHILD_SUBREAPER", 38: "PR_SET_NO_NEW_PRIVS",
         39: "PR_GET_NO_NEW_PRIVS", 40: "PR_GET_TID_ADDRESS", 41: "PR_SET_THP_DISABLE", 42: "PR_GET_THP_DISABLE",
         47: "PR_CAP_AMBIENT", 52: "PR_GET_SPECULATION_CTRL", 53: "PR_SET_SPECULATION_CTRL",
         59: "PR_SET_SYSCALL_USER_DISPATCH", 62: "PR_SCHED_CORE", 65: "PR_SET_MDWE", 66: "PR_GET_MDWE",
         0x59616d61: "PR_SET_PTRACER", 0x53564d41: "PR_SET_VMA"}
ARCH_PRCTL = {0x1001: "ARCH_SET_GS", 0x1002: "ARCH_SET_FS", 0x1003: "ARCH_GET_FS", 0x1004: "ARCH_GET_GS",
              0x1011: "ARCH_GET_CPUID", 0x1012: "ARCH_SET_CPUID", 0x1021: "ARCH_GET_XCOMP_SUPP",
              0x1022: "ARCH_GET_XCOMP_PERM", 0x1023: "ARCH_REQ_XCOMP_PERM", 0x2001: "ARCH_MAP_VDSO_X32",
              0x2002: "ARCH_MAP_VDSO_32", 0x2003: "ARCH_MAP_VDSO_64", 0x3001: "ARCH_CET_STATUS",
              0x4001: "ARCH_GET_UNTAG_MASK", 0x4002: "ARCH_ENABLE_TAGGED_ADDR", 0x5001: "ARCH_SHSTK_ENABLE",
              0x5002: "ARCH_SHSTK_DISABLE", 0x5005: "ARCH_SHSTK_STATUS"}
FUTEX = {0: "WAIT", 1: "WAKE", 2: "FD", 3: "REQUEUE", 4: "CMP_REQUEUE", 5: "WAKE_OP", 6: "LOCK_PI", 7: "UNLOCK_PI",
         8: "TRYLOCK_PI", 9: "WAIT_BITSET", 10: "WAKE_BITSET", 11: "WAIT_REQUEUE_PI", 12: "CMP_REQUEUE_PI",
         13: "LOCK_PI2"}


def futex(v):
    s = "FUTEX_" + FUTEX.get(v & 0x7f, hex(v & 0x7f))
    return s + ("|PRIVATE" if v & 128 else "") + ("|CLOCK_REALTIME" if v & 256 else "")


CLOCK = {0: "REALTIME", 1: "MONOTONIC", 2: "PROCESS_CPUTIME_ID", 3: "THREAD_CPUTIME_ID", 4: "MONOTONIC_RAW",
         5: "REALTIME_COARSE", 6: "MONOTONIC_COARSE", 7: "BOOTTIME", 8: "REALTIME_ALARM", 9: "BOOTTIME_ALARM",
         11: "TAI"}


def clockid(v):
    if v >= 1 << 31:
        return "CPUCLOCK(pid/tid)"
    return "CLOCK_" + CLOCK.get(v, hex(v))


RLIMIT = {0: "CPU", 1: "FSIZE", 2: "DATA", 3: "STACK", 4: "CORE", 5: "RSS", 6: "NPROC", 7: "NOFILE", 8: "MEMLOCK",
          9: "AS", 10: "LOCKS", 11: "SIGPENDING", 12: "MSGQUEUE", 13: "NICE", 14: "RTPRIO", 15: "RTTIME"}
MADVISE = {0: "NORMAL", 1: "RANDOM", 2: "SEQUENTIAL", 3: "WILLNEED", 4: "DONTNEED", 8: "FREE", 9: "REMOVE",
           10: "DONTFORK", 11: "DOFORK", 12: "MERGEABLE", 13: "UNMERGEABLE", 14: "HUGEPAGE", 15: "NOHUGEPAGE",
           16: "DONTDUMP", 17: "DODUMP", 18: "WIPEONFORK", 19: "KEEPONFORK", 20: "COLD", 21: "PAGEOUT",
           22: "POPULATE_READ", 23: "POPULATE_WRITE", 24: "DONTNEED_LOCKED", 25: "COLLAPSE", 102: "GUARD_INSTALL",
           103: "GUARD_REMOVE"}
MEMBARRIER = {0: "QUERY", 1: "GLOBAL", 2: "GLOBAL_EXPEDITED", 4: "REGISTER_GLOBAL_EXPEDITED", 8: "PRIVATE_EXPEDITED",
              16: "REGISTER_PRIVATE_EXPEDITED", 32: "PRIVATE_EXPEDITED_SYNC_CORE",
              64: "REGISTER_PRIVATE_EXPEDITED_SYNC_CORE", 128: "PRIVATE_EXPEDITED_RSEQ",
              256: "REGISTER_PRIVATE_EXPEDITED_RSEQ", 512: "GET_REGISTRATIONS"}
AT = [(0x100, "AT_SYMLINK_NOFOLLOW"), (0x200, "AT_REMOVEDIR/EACCESS"), (0x400, "AT_SYMLINK_FOLLOW"),
      (0x800, "AT_NO_AUTOMOUNT"), (0x1000, "AT_EMPTY_PATH"), (0x2000, "AT_STATX_FORCE_SYNC"),
      (0x4000, "AT_STATX_DONT_SYNC"), (0x8000, "AT_RECURSIVE")]
STATX = [(0xfff, "ALL"), (0x7ff, "BASIC_STATS"), (1, "TYPE"), (2, "MODE"), (4, "NLINK"), (8, "UID"), (0x10, "GID"),
         (0x20, "ATIME"), (0x40, "MTIME"), (0x80, "CTIME"), (0x100, "INO"), (0x200, "SIZE"), (0x400, "BLOCKS"),
         (0x800, "BTIME"), (0x1000, "MNT_ID"), (0x2000, "DIOALIGN"), (0x4000, "MNT_ID_UNIQUE"), (0x8000, "SUBVOL")]
MSG = [(1, "OOB"), (2, "PEEK"), (4, "DONTROUTE"), (8, "CTRUNC"), (0x20, "TRUNC"), (0x40, "DONTWAIT"), (0x80, "EOR"),
       (0x100, "WAITALL"), (0x2000, "ERRQUEUE"), (0x4000, "NOSIGNAL"), (0x8000, "MORE"), (0x10000, "WAITFORONE"),
       (0x4000000, "ZEROCOPY"), (0x20000000, "FASTOPEN"), (0x40000000, "CMSG_CLOEXEC")]
WAIT = [(1, "WNOHANG"), (2, "WUNTRACED"), (4, "WEXITED"), (8, "WCONTINUED"), (0x1000000, "WNOWAIT"),
        (0x20000000, "__WNOTHREAD"), (0x40000000, "__WALL"), (0x80000000, "__WCLONE")]
IPC = [(0o1000, "IPC_CREAT"), (0o2000, "IPC_EXCL"), (0o4000, "SHM_HUGETLB")]


def ipcflags(v):
    rest = bits(v & ~0o777, IPC)
    return f"{rest}|0{v & 0o777:o}" if rest != "0" else f"0{v & 0o777:o}"


def signum(v):
    return signame(v) if v else "0"


def access_mode(v):
    return bits(v, [(4, "R_OK"), (2, "W_OK"), (1, "X_OK")]) if v else "F_OK"


def sched(v):
    policy = {0: "SCHED_OTHER", 1: "SCHED_FIFO", 2: "SCHED_RR", 3: "SCHED_BATCH", 5: "SCHED_IDLE", 6: "SCHED_DEADLINE"}
    return policy.get(v & ~0x40000000, hex(v)) + ("|RESET_ON_FORK" if v & 0x40000000 else "")


SIGHOW = {0: "SIG_BLOCK", 1: "SIG_UNBLOCK", 2: "SIG_SETMASK"}
DECODERS = {
    "oflags": oflags, "prot": prot, "mapflags": mapflags, "clone": clone, "clone3": clone3,
    "fcntl": enum(FCNTL), "ioctl": enum(IOCTL), "domain": enum(DOMAIN), "socktype": socktype, "proto": enum(PROTO),
    "level": enum(LEVEL), "prctl": enum(PRCTL), "arch_prctl": enum(ARCH_PRCTL), "futex": futex,
    "sighow": enum(SIGHOW), "signal": signum, "wait": lambda v: bits(v, WAIT),
    "idtype": enum({0: "P_ALL", 1: "P_PID", 2: "P_PGID", 3: "P_PIDFD"}),
    "fdflags": lambda v: bits(v, FD_FLAGS), "efdflags": lambda v: bits(v, [(1, "SEMAPHORE")] + FD_FLAGS),
    "pipeflags": lambda v: bits(v, [(0x4000, "O_DIRECT"), (0x80, "O_NOTIFICATION_PIPE")] + FD_FLAGS),
    "clockid": clockid, "abstime": lambda v: bits(v, [(1, "ABSTIME"), (2, "CANCEL_ON_SET")]),
    "epollop": enum({1: "EPOLL_CTL_ADD", 2: "EPOLL_CTL_DEL", 3: "EPOLL_CTL_MOD"}),
    "grnd": lambda v: bits(v, [(1, "GRND_NONBLOCK"), (2, "GRND_RANDOM"), (4, "GRND_INSECURE")]),
    "membarrier": enum(MEMBARRIER), "rlimit": lambda v: "RLIMIT_" + RLIMIT.get(v, hex(v)),
    "falloc": lambda v: bits(v, [(1, "KEEP_SIZE"), (2, "PUNCH_HOLE"), (8, "COLLAPSE_RANGE"), (0x10, "ZERO_RANGE"),
                                 (0x20, "INSERT_RANGE"), (0x40, "UNSHARE_RANGE")]),
    "fadvise": enum({0: "NORMAL", 1: "RANDOM", 2: "SEQUENTIAL", 3: "WILLNEED", 4: "DONTNEED", 5: "NOREUSE"}),
    "flock": lambda v: bits(v, [(1, "LOCK_SH"), (2, "LOCK_EX"), (4, "LOCK_NB"), (8, "LOCK_UN")]),
    "whence": enum({0: "SEEK_SET", 1: "SEEK_CUR", 2: "SEEK_END", 3: "SEEK_DATA", 4: "SEEK_HOLE"}),
    "how": enum({0: "SHUT_RD", 1: "SHUT_WR", 2: "SHUT_RDWR"}), "msg": lambda v: bits(v, MSG),
    "at": lambda v: bits(v, AT), "statx": lambda v: bits(v, STATX), "access": access_mode,
    "rename": lambda v: bits(v, [(1, "NOREPLACE"), (2, "EXCHANGE"), (4, "WHITEOUT")]),
    "madvise": lambda v: "MADV_" + MADVISE.get(v, hex(v)),
    "msync": lambda v: bits(v, [(1, "MS_ASYNC"), (2, "MS_INVALIDATE"), (4, "MS_SYNC")]),
    "mremap": lambda v: bits(v, [(1, "MAYMOVE"), (2, "FIXED"), (4, "DONTUNMAP")]),
    "ipcflags": ipcflags, "shmat": lambda v: bits(v, [(0o10000, "SHM_RDONLY"), (0o20000, "SHM_RND"),
                                                     (0o40000, "SHM_REMAP"), (0o100000, "SHM_EXEC")]),
    "shmctl": enum({0: "IPC_RMID", 1: "IPC_SET", 2: "IPC_STAT", 3: "IPC_INFO", 11: "SHM_LOCK", 12: "SHM_UNLOCK",
                    13: "SHM_STAT", 14: "SHM_INFO", 15: "SHM_STAT_ANY"}),
    "semctl": enum({0: "IPC_RMID", 1: "IPC_SET", 2: "IPC_STAT", 3: "IPC_INFO", 11: "GETPID", 12: "GETVAL",
                    13: "GETALL", 14: "GETNCNT", 15: "GETZCNT", 16: "SETVAL", 17: "SETALL"}),
    "memfd": lambda v: bits(v, [(1, "MFD_CLOEXEC"), (2, "MFD_ALLOW_SEALING"), (4, "MFD_HUGETLB"),
                                (8, "MFD_NOEXEC_SEAL"), (0x10, "MFD_EXEC")]),
    "sched": sched,
    "seccomp": enum({0: "SET_MODE_STRICT", 1: "SET_MODE_FILTER", 2: "GET_ACTION_AVAIL", 3: "GET_NOTIF_SIZES"}),
    "itimer": enum({0: "ITIMER_REAL", 1: "ITIMER_VIRTUAL", 2: "ITIMER_PROF"}),
    "rusage": enum({0: "RUSAGE_SELF", 0xffffffffffffffff: "RUSAGE_CHILDREN", 0xffffffff: "RUSAGE_CHILDREN",
                    1: "RUSAGE_THREAD"}),
    "prio": enum({0: "PRIO_PROCESS", 1: "PRIO_PGRP", 2: "PRIO_USER"}),
    "personality": enum({0: "PER_LINUX", 0xffffffff: "query", 0x0040000: "ADDR_NO_RANDOMIZE"}),
    "pidfd": lambda v: bits(v, [(0x800, "PIDFD_NONBLOCK"), (0x80, "PIDFD_THREAD")]),
    "umount": lambda v: bits(v, [(1, "MNT_FORCE"), (2, "MNT_DETACH"), (4, "MNT_EXPIRE"), (8, "UMOUNT_NOFOLLOW")]),
    "mount": lambda v: bits(v, [(1, "RDONLY"), (2, "NOSUID"), (4, "NODEV"), (8, "NOEXEC"), (0x20, "REMOUNT"),
                                (0x1000, "BIND"), (0x2000, "MOVE"), (0x4000, "REC"), (0x40000, "PRIVATE"),
                                (0x80000, "SLAVE"), (0x100000, "SHARED")]),
    "closerange": lambda v: bits(v, [(2, "UNSHARE"), (4, "CLOEXEC")]),
    "rwf": lambda v: bits(v, [(1, "HIPRI"), (2, "DSYNC"), (4, "SYNC"), (8, "NOWAIT"), (0x10, "APPEND")]),
    "mlock": lambda v: bits(v, [(1, "MLOCK_ONFAULT")]),
    "family": lambda v: DOMAIN.get(v, str(v)) if v >= 0 else "NULL",
    "hex": hex, "dec": str,
}

# (syscall, argument key as lxtrace writes it) -> decoder; arguments missing here print as recorded.
ARGS = {
    ("open", "a1"): "oflags", ("openat", "a2"): "oflags", ("openat2", "a2.0"): "oflags",
    ("lseek", "a2"): "whence", ("mmap", "a2"): "prot", ("mmap", "a3"): "mapflags", ("mprotect", "a2"): "prot",
    ("pkey_mprotect", "a2"): "prot", ("msync", "a2"): "msync", ("madvise", "a2"): "madvise",
    ("mremap", "a3"): "mremap", ("mlock2", "a2"): "mlock", ("shmget", "a2"): "ipcflags", ("shmat", "a2"): "shmat",
    ("shmctl", "a1"): "shmctl", ("semget", "a2"): "ipcflags", ("semctl", "a2"): "semctl",
    ("rt_sigaction", "a0"): "signal", ("rt_sigprocmask", "a0"): "sighow", ("getitimer", "a0"): "itimer",
    ("setitimer", "a0"): "itimer", ("getrlimit", "a0"): "rlimit", ("setrlimit", "a0"): "rlimit",
    ("prlimit64", "a1"): "rlimit", ("getrusage", "a0"): "rusage", ("ptrace", "a0"): "hex", ("syslog", "a0"): "dec",
    ("personality", "a0"): "personality", ("getpriority", "a0"): "prio", ("setpriority", "a0"): "prio",
    ("prctl", "a0"): "prctl", ("arch_prctl", "a0"): "arch_prctl", ("timer_create", "a0"): "clockid",
    ("clock_gettime", "a0"): "clockid", ("clock_getres", "a0"): "clockid", ("clock_nanosleep", "a0"): "clockid",
    ("clock_nanosleep", "a1"): "abstime", ("keyctl", "a0"): "dec", ("unshare", "a0"): "clone",
    ("epoll_create1", "a0"): "fdflags", ("inotify_init1", "a0"): "fdflags", ("bpf", "a0"): "dec",
    ("userfaultfd", "a0"): "fdflags", ("membarrier", "a0"): "membarrier", ("ioctl", "a1"): "ioctl",
    ("access", "a1"): "access", ("shutdown", "a1"): "how", ("kill", "a1"): "signal", ("tkill", "a1"): "signal",
    ("flock", "a1"): "flock", ("sched_setscheduler", "a1"): "sched", ("futex", "a1"): "futex",
    ("epoll_ctl", "a1"): "epollop", ("fallocate", "a1"): "falloc", ("timerfd_settime", "a1"): "abstime",
    ("eventfd2", "a1"): "efdflags", ("pipe2", "a1"): "pipeflags", ("prlimit64", "a1"): "rlimit",
    ("setns", "a1"): "clone", ("memfd_create", "a1"): "memfd", ("pidfd_send_signal", "a1"): "signal",
    ("pidfd_open", "a1"): "pidfd", ("socket", "a0"): "domain", ("socket", "a1"): "socktype", ("socket", "a2"): "proto",
    ("socketpair", "a0"): "domain", ("socketpair", "a1"): "socktype", ("connect", "a1.family"): "family",
    ("bind", "a1.family"): "family", ("sendto", "a3"): "msg", ("recvfrom", "a3"): "msg", ("fadvise64", "a3"): "fadvise",
    ("accept4", "a3"): "fdflags", ("signalfd4", "a3"): "fdflags", ("recvmmsg", "a3"): "msg", ("sendmmsg", "a3"): "msg",
    ("sendmsg", "a2"): "msg", ("recvmsg", "a2"): "msg", ("tgkill", "a2"): "signal", ("dup3", "a2"): "fdflags",
    ("rseq", "a2"): "hex", ("close_range", "a2"): "closerange", ("setsockopt", "a1"): "level",
    ("getsockopt", "a1"): "level", ("clone", "a0"): "clone", ("wait4", "a2"): "wait", ("fcntl", "a1"): "fcntl",
    ("mount", "a3"): "mount", ("umount2", "a1"): "umount", ("reboot", "a2"): "hex", ("waitid", "a0"): "idtype",
    ("waitid", "a3"): "wait", ("inotify_add_watch", "a2"): "hex", ("fchownat", "a4"): "at", ("linkat", "a4"): "at",
    ("renameat2", "a4"): "rename", ("execveat", "a4"): "at", ("newfstatat", "a3"): "at", ("utimensat", "a3"): "at",
    ("fchmodat2", "a3"): "at", ("unlinkat", "a2"): "at", ("faccessat", "a2"): "access", ("splice", "a5"): "hex",
    ("copy_file_range", "a5"): "hex", ("timerfd_create", "a0"): "clockid", ("timerfd_create", "a1"): "fdflags",
    ("seccomp", "a0"): "seccomp", ("seccomp", "a1"): "hex", ("getrandom", "a2"): "grnd", ("preadv2", "a4"): "rwf",
    ("pwritev2", "a4"): "rwf", ("statx", "a2"): "at", ("statx", "a3"): "statx", ("clone3", "a0.0"): "clone3",
    ("faccessat2", "a2"): "access", ("faccessat2", "a3"): "at",
}


def decode(name, key):
    """'a2=0x80000 a1.family=2' -> 'O_RDONLY|O_CLOEXEC ...' using ARGS (setsockopt names need the level)."""
    toks = dict(t.split("=", 1) for t in key.split(" ") if "=" in t)
    out = []
    for k, v in toks.items():
        if k == "fd":
            out.append(f"fd:{v}")
            continue
        if not v.startswith(("0x", "-")) and not v.lstrip("-").isdigit():
            out.append(v)
            continue
        n = int(v, 0)
        if (name, k) in (("setsockopt", "a2"), ("getsockopt", "a2")):
            lvl = int(toks.get("a1", "0"), 0)
            out.append(OPTNAME.get(lvl, {}).get(n, hex(n)))
        elif name == "fcntl" and k == "a2":
            cmd = int(toks["a1"], 0)
            out.append(oflags(n) if cmd == 4 else ("FD_CLOEXEC" if cmd == 2 and n == 1 else hex(n)))
        else:
            out.append(DECODERS[ARGS.get((name, k), "hex")](n))
    return " ".join(out)
