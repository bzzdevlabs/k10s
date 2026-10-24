//! The seccomp-bpf syscall allowlist of a running node.

use super::Mode;
use std::io;

/// What a rejected call gets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Deny {
    /// The filter's default action: kill the process (`Log`: log and allow).
    Kill,
    Errno(u16),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rule {
    /// Any arguments: served by the kernel's action cache (Linux ≥ 5.11) without running the filter. Hot paths only.
    Allow,
    Errno(u16),
    /// No bit of `mask` set in argument `arg` (low 32 bits, as all the rules below).
    NoBits { arg: u8, mask: u32, deny: Deny },
    /// Not all bits of `mask` set in argument `arg`.
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    NotAllBits { arg: u8, mask: u32, deny: Deny },
    /// All bits of `all` and none of `none` set in argument `arg`.
    Bits { arg: u8, all: u32, none: u32, deny: Deny },
    /// Argument `arg` is one of `vals`.
    OneOf { arg: u8, vals: &'static [u32], deny: Deny },
    /// Argument `arg` is this process's pid.
    OwnPid { arg: u8, deny: Deny },
    /// The whole 64-bit argument `arg` is 0.
    Null { arg: u8, deny: Deny },
    /// `socket(AF_INET|AF_INET6, SOCK_STREAM|SOCK_DGRAM [|SOCK_NONBLOCK|SOCK_CLOEXEC], _)`.
    Socket { deny: Deny },
}

pub struct Sys {
    pub nr: libc::c_long,
    pub rule: Rule,
}

const fn allow(nr: libc::c_long) -> Sys {
    Sys { nr, rule: Rule::Allow }
}

const fn sys(nr: libc::c_long, rule: Rule) -> Sys {
    Sys { nr, rule }
}

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
const PROT_WRITE: u32 = libc::PROT_WRITE as u32;
const PROT_EXEC: u32 = libc::PROT_EXEC as u32;
/// Namespace-creating `clone` flags.
const CLONE_NEW_ANY: u32 = (libc::CLONE_NEWNS
    | libc::CLONE_NEWCGROUP
    | libc::CLONE_NEWUTS
    | libc::CLONE_NEWIPC
    | libc::CLONE_NEWUSER
    | libc::CLONE_NEWPID
    | libc::CLONE_NEWNET
    | libc::CLONE_NEWTIME) as u32;
const EPERM: u16 = libc::EPERM as u16;
/// Placeholder number of a syscall this architecture does not have; [`build`] skips it.
const NONE: libc::c_long = -1;

/// Defines `nr::NAME` as `libc::NAME` where `cfg` holds, else as [`NONE`].
macro_rules! nrs {
    ($cfg:meta: $($name:ident),*) => {
        $(
            #[cfg($cfg)]
            pub const $name: libc::c_long = libc::$name;
            #[cfg(not($cfg))]
            pub const $name: libc::c_long = super::NONE;
        )*
    };
}

/// The syscalls only some ABIs have (or musl uses only on some).
#[allow(non_upper_case_globals)]
mod nr {
    // The legacy (non-`*at`, non-`p*`) forms, which AArch64 lacks and musl uses where they exist.
    nrs!(not(target_arch = "aarch64"): SYS_epoll_wait, SYS_poll, SYS_open, SYS_rename, SYS_unlink, SYS_mkdir, SYS_rmdir, SYS_readlink);
    // musl std::fs::metadata and symlink_metadata on x86-64.
    nrs!(target_arch = "x86_64": SYS_stat, SYS_lstat);
    // i386 has no plain accept outside socketcall.
    nrs!(not(target_arch = "x86"): SYS_accept);
    // 64-bit ABIs: 64-bit off_t, uid_t and time_t in the plain calls.
    nrs!(target_pointer_width = "64": SYS_mmap, SYS_newfstatat, SYS_fstat, SYS_fcntl, SYS_ftruncate, SYS_getuid, SYS_geteuid,
        SYS_getgid, SYS_getegid, SYS_getresuid, SYS_getresgid, SYS_getrlimit);
    // 32-bit ABIs, as musl calls them: mmap2, the 64-bit file offset and stat calls, 32-bit ids, sigreturn for
    // handlers without SA_SIGINFO, and the time64 calls (musl still uses the time32 ones, listed with the common
    // calls, when the value fits in 32 bits).
    nrs!(target_pointer_width = "32": SYS_mmap2, SYS_fstat64, SYS_fstatat64, SYS_stat64, SYS_lstat64, SYS__llseek, SYS_fcntl64,
        SYS_ftruncate64, SYS_getuid32, SYS_geteuid32, SYS_getgid32, SYS_getegid32, SYS_getresuid32, SYS_getresgid32,
        SYS_ugetrlimit, SYS_sigreturn);
    #[cfg(target_pointer_width = "64")]
    pub const SYS_rseq: libc::c_long = libc::SYS_rseq;
    #[cfg(target_arch = "x86")]
    pub const SYS_rseq: libc::c_long = 386;
    #[cfg(target_arch = "arm")]
    pub const SYS_rseq: libc::c_long = 398;
    // The time64 calls have the same numbers on every 32-bit ABI (libc does not define them for musl).
    pub const SYS_clock_gettime64: libc::c_long = if cfg!(target_pointer_width = "32") { 403 } else { super::NONE };
    pub const SYS_clock_getres_time64: libc::c_long = if cfg!(target_pointer_width = "32") { 406 } else { super::NONE };
    pub const SYS_clock_nanosleep_time64: libc::c_long = if cfg!(target_pointer_width = "32") { 407 } else { super::NONE };
    pub const SYS_ppoll_time64: libc::c_long = if cfg!(target_pointer_width = "32") { 414 } else { super::NONE };
    pub const SYS_rt_sigtimedwait_time64: libc::c_long = if cfg!(target_pointer_width = "32") { 421 } else { super::NONE };
    pub const SYS_futex_time64: libc::c_long = if cfg!(target_pointer_width = "32") { 422 } else { super::NONE };
    // The AArch64 JIT's context synchronization of every thread (x86-64 keeps instruction fetch coherent).
    nrs!(target_arch = "aarch64": SYS_membarrier);
    // musl on i386 makes every socket call through socketcall (arguments in memory); the node creates its sockets
    // with the direct socket call, which the filter can inspect.
    #[cfg(target_arch = "x86")]
    pub const SYS_socketcall: libc::c_long = libc::SYS_socketcall;
}

/// `socketcall` operations ([`nr::SYS_socketcall`], `linux/net.h`) a running node uses: what the direct calls
/// allow, without socket, bind, listen or socketpair.
#[cfg(target_arch = "x86")]
const SOCKETCALLS: &[u32] = &[
    3,  // connect
    5,  // accept
    6,  // getsockname
    7,  // getpeername
    9,  // send
    10, // recv
    11, // sendto
    12, // recvfrom
    13, // shutdown
    14, // setsockopt
    15, // getsockopt
    16, // sendmsg
    17, // recvmsg
    18, // accept4
];

/// `MEMBARRIER_CMD_PRIVATE_EXPEDITED_SYNC_CORE` and its registration.
const MEMBARRIER_SYNC_CORE: [u32; 2] = [1 << 5, 1 << 6];

/// Syscalls of a running node; anything else kills the process.
pub const SYSCALLS: &[Sys] = &[
    allow(nr::SYS_epoll_wait),
    allow(libc::SYS_epoll_pwait),
    allow(libc::SYS_epoll_pwait2),
    allow(libc::SYS_epoll_ctl),
    allow(libc::SYS_epoll_create1),
    allow(nr::SYS_poll),
    allow(libc::SYS_ppoll),
    allow(nr::SYS_ppoll_time64),
    allow(libc::SYS_read),
    allow(libc::SYS_write),
    allow(libc::SYS_readv),
    allow(libc::SYS_writev),
    allow(libc::SYS_pread64),
    allow(libc::SYS_pwrite64),
    allow(libc::SYS_close),
    allow(libc::SYS_accept4),
    allow(nr::SYS_accept),
    allow(libc::SYS_connect),
    allow(libc::SYS_recvfrom),
    allow(libc::SYS_sendto),
    allow(libc::SYS_recvmsg),
    allow(libc::SYS_sendmsg),
    allow(libc::SYS_shutdown),
    allow(libc::SYS_getsockopt),
    allow(libc::SYS_setsockopt),
    allow(libc::SYS_getsockname),
    allow(libc::SYS_getpeername),
    allow(libc::SYS_eventfd2),
    // W^X: no executable mapping is created; JIT code goes RW, then mprotect(RX), never writable and executable.
    // Without a JIT nothing is ever made executable.
    sys(nr::SYS_mmap, Rule::NoBits { arg: 2, mask: PROT_EXEC, deny: Deny::Kill }),
    sys(nr::SYS_mmap2, Rule::NoBits { arg: 2, mask: PROT_EXEC, deny: Deny::Kill }),
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    sys(libc::SYS_mprotect, Rule::NotAllBits { arg: 2, mask: PROT_WRITE | PROT_EXEC, deny: Deny::Kill }),
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    sys(libc::SYS_mprotect, Rule::NoBits { arg: 2, mask: PROT_EXEC, deny: Deny::Kill }),
    allow(libc::SYS_munmap),
    allow(libc::SYS_madvise),
    allow(libc::SYS_mremap),
    allow(libc::SYS_brk),
    allow(libc::SYS_mincore), // snapshot capture: resident pages
    allow(libc::SYS_memfd_create),
    // AArch64 JIT: every thread context-synchronizes before running newly mapped code; nothing else.
    sys(nr::SYS_membarrier, Rule::OneOf { arg: 0, vals: &MEMBARRIER_SYNC_CORE, deny: Deny::Errno(EPERM) }),
    allow(nr::SYS_ftruncate),
    allow(nr::SYS_ftruncate64),
    // Threads only: no fork, no new namespace.
    sys(libc::SYS_clone, Rule::Bits { arg: 0, all: libc::CLONE_THREAD as u32, none: CLONE_NEW_ANY, deny: Deny::Kill }),
    // clone3's flags are in memory, invisible to the filter: ENOSYS makes glibc fall back to clone.
    sys(libc::SYS_clone3, Rule::Errno(libc::ENOSYS as u16)),
    allow(libc::SYS_futex),
    allow(nr::SYS_futex_time64),
    allow(libc::SYS_set_robust_list),
    allow(nr::SYS_rseq),
    allow(libc::SYS_sched_yield),
    allow(libc::SYS_sched_getaffinity),
    sys(libc::SYS_sched_setaffinity, Rule::OneOf { arg: 0, vals: &[0], deny: Deny::Errno(EPERM) }),
    allow(libc::SYS_gettid),
    allow(libc::SYS_getpid),
    allow(libc::SYS_getppid),
    allow(nr::SYS_getuid),
    allow(nr::SYS_geteuid),
    allow(nr::SYS_getgid),
    allow(nr::SYS_getegid),
    allow(nr::SYS_getresuid),
    allow(nr::SYS_getresgid),
    allow(nr::SYS_getuid32),
    allow(nr::SYS_geteuid32),
    allow(nr::SYS_getgid32),
    allow(nr::SYS_getegid32),
    allow(nr::SYS_getresuid32),
    allow(nr::SYS_getresgid32),
    allow(libc::SYS_uname),
    sys(libc::SYS_prctl, Rule::OneOf {
        arg: 0,
        vals: &[libc::PR_SET_NAME as u32, libc::PR_GET_NAME as u32, 0x5356_4d41 /* PR_SET_VMA */],
        deny: Deny::Errno(EPERM),
    }),
    allow(libc::SYS_clock_gettime),
    allow(nr::SYS_clock_gettime64),
    allow(libc::SYS_clock_getres),
    allow(nr::SYS_clock_getres_time64),
    allow(libc::SYS_gettimeofday),
    allow(libc::SYS_clock_nanosleep),
    allow(nr::SYS_clock_nanosleep_time64),
    allow(libc::SYS_nanosleep),
    allow(libc::SYS_getrandom),
    allow(libc::SYS_openat),
    allow(nr::SYS_open),
    allow(nr::SYS_newfstatat),
    allow(nr::SYS_fstat),
    allow(libc::SYS_statx),
    allow(nr::SYS_stat),
    allow(nr::SYS_lstat),
    allow(nr::SYS_fstat64),
    allow(nr::SYS_fstatat64),
    allow(nr::SYS_stat64),
    allow(nr::SYS_lstat64),
    allow(libc::SYS_lseek),
    allow(nr::SYS__llseek),
    allow(libc::SYS_fsync),
    allow(libc::SYS_fdatasync),
    allow(libc::SYS_getdents64),
    allow(nr::SYS_rename),
    allow(libc::SYS_renameat),
    allow(libc::SYS_renameat2),
    allow(nr::SYS_unlink),
    allow(libc::SYS_unlinkat),
    allow(nr::SYS_mkdir),
    allow(libc::SYS_mkdirat),
    allow(nr::SYS_rmdir),
    allow(nr::SYS_fcntl),
    allow(nr::SYS_fcntl64),
    allow(nr::SYS_readlink), // current_exe() for panic backtraces
    allow(libc::SYS_readlinkat),
    // std's set_nonblocking and cloexec only; no terminal or device ioctl.
    sys(libc::SYS_ioctl, Rule::OneOf {
        arg: 1,
        vals: &[libc::FIONBIO as u32, libc::FIOCLEX as u32],
        deny: Deny::Errno(libc::ENOTTY as u16),
    }),
    allow(libc::SYS_rt_sigreturn),
    allow(nr::SYS_sigreturn),
    allow(libc::SYS_rt_sigprocmask),
    allow(libc::SYS_rt_sigaction),
    allow(libc::SYS_rt_sigtimedwait),
    allow(nr::SYS_rt_sigtimedwait_time64),
    allow(libc::SYS_sigaltstack),
    allow(libc::SYS_restart_syscall),
    // glibc abort(): tgkill(own pid, …); never another process.
    sys(libc::SYS_tgkill, Rule::OwnPid { arg: 0, deny: Deny::Kill }),
    // musl abort(): tkill(own tid, SIGABRT); any other signal is refused.
    sys(libc::SYS_tkill, Rule::OneOf { arg: 1, vals: &[libc::SIGABRT as u32], deny: Deny::Kill }),
    allow(libc::SYS_exit),
    allow(libc::SYS_exit_group),
    // getrlimit only (glibc reads RLIMIT_STACK); no limit is ever changed.
    sys(libc::SYS_prlimit64, Rule::Null { arg: 2, deny: Deny::Errno(EPERM) }),
    allow(nr::SYS_getrlimit),
    allow(nr::SYS_ugetrlimit),
    allow(libc::SYS_sysinfo), // sysconf(_SC_PHYS_PAGES)
    sys(libc::SYS_socket, Rule::Socket { deny: Deny::Errno(EPERM) }),
    #[cfg(target_arch = "x86")]
    sys(nr::SYS_socketcall, Rule::OneOf { arg: 0, vals: SOCKETCALLS, deny: Deny::Errno(EPERM) }),
];

/// `AUDIT_ARCH_X86_64`, `AUDIT_ARCH_AARCH64`, `AUDIT_ARCH_I386`, `AUDIT_ARCH_ARM`: the only ABI the filter admits
/// (no 32-bit or x32 entry points into a 64-bit node, no 64-bit ones from a 32-bit node on a 64-bit kernel).
#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH: u32 = 0xC000_003E;
#[cfg(target_arch = "aarch64")]
const AUDIT_ARCH: u32 = 0xC000_00B7;
#[cfg(target_arch = "x86")]
const AUDIT_ARCH: u32 = 0x4000_0003;
#[cfg(target_arch = "arm")]
const AUDIT_ARCH: u32 = 0x4000_0028;
#[cfg(target_arch = "x86_64")]
const X32_SYSCALL_BIT: u32 = 0x4000_0000;
const OFF_NR: u32 = 0;
const OFF_ARCH: u32 = 4;
const fn off_arg_lo(i: u8) -> u32 {
    16 + 8 * i as u32
}
const fn off_arg_hi(i: u8) -> u32 {
    20 + 8 * i as u32
}

const BPF_LD_W_ABS: u16 = 0x20;
const BPF_JEQ_K: u16 = 0x15;
const BPF_JSET_K: u16 = 0x45;
const BPF_JA: u16 = 0x05;
const BPF_AND_K: u16 = 0x54;
const BPF_RET_K: u16 = 0x06;

const RET_KILL_PROCESS: u32 = 0x8000_0000;
const RET_ERRNO: u32 = 0x0005_0000;
const RET_LOG: u32 = 0x7ffc_0000;
const RET_ALLOW: u32 = 0x7fff_0000;

/// A jump target while assembling.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum T {
    Next,
    /// Skips `n` instructions.
    Fwd(usize),
    Allow,
    Default,
    Errno(u16),
    /// The next allowlist entry.
    Skip(usize),
}

struct Asm {
    code: Vec<(u16, T, T, u32)>,
    skips: Vec<(usize, usize)>,
}

impl Asm {
    fn op(&mut self, code: u16, jt: T, jf: T, k: u32) {
        self.code.push((code, jt, jf, k));
    }
    fn ld(&mut self, off: u32) {
        self.op(BPF_LD_W_ABS, T::Next, T::Next, off);
    }
    fn jeq(&mut self, k: u32, jt: T, jf: T) {
        self.op(BPF_JEQ_K, jt, jf, k);
    }
    fn jset(&mut self, k: u32, jt: T, jf: T) {
        self.op(BPF_JSET_K, jt, jf, k);
    }
    fn and(&mut self, k: u32) {
        self.op(BPF_AND_K, T::Next, T::Next, k);
    }
    fn ja(&mut self, t: T) {
        self.op(BPF_JA, t, T::Next, 0);
    }
}

fn deny(d: Deny) -> T {
    match d {
        Deny::Kill => T::Default,
        Deny::Errno(e) => T::Errno(e),
    }
}

/// The filter for `mode` (`pid` for [`Rule::OwnPid`]): arch check, x32 check (x86-64), one `jeq nr` per plain allow
/// (no argument load, so cacheable), then one block per argument rule ending in a return; returns at the end.
pub fn build(mode: Mode, pid: u32) -> Vec<libc::sock_filter> {
    let default = if mode == Mode::Log { RET_LOG } else { RET_KILL_PROCESS };
    let mut a = Asm { code: Vec::new(), skips: Vec::new() };
    a.ld(OFF_ARCH);
    a.jeq(AUDIT_ARCH, T::Next, T::Default);
    a.ld(OFF_NR);
    #[cfg(target_arch = "x86_64")]
    a.jset(X32_SYSCALL_BIT, T::Default, T::Next);
    for s in SYSCALLS.iter().filter(|s| s.rule == Rule::Allow && s.nr != NONE) {
        a.jeq(s.nr as u32, T::Allow, T::Next);
    }
    for (i, s) in SYSCALLS.iter().enumerate().filter(|(_, s)| s.rule != Rule::Allow && s.nr != NONE) {
        let nr = s.nr as u32;
        if let Rule::Errno(e) = s.rule {
            a.jeq(nr, T::Errno(e), T::Next);
            continue;
        }
        a.jeq(nr, T::Next, T::Skip(i));
        match s.rule {
            Rule::Allow | Rule::Errno(_) => unreachable!(),
            Rule::NoBits { arg, mask, deny: d } => {
                a.ld(off_arg_lo(arg));
                a.jset(mask, deny(d), T::Allow);
            }
            #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
            Rule::NotAllBits { arg, mask, deny: d } => {
                a.ld(off_arg_lo(arg));
                a.and(mask);
                a.jeq(mask, deny(d), T::Allow);
            }
            Rule::Bits { arg, all, none, deny: d } => {
                a.ld(off_arg_lo(arg));
                a.jset(none, deny(d), T::Next);
                a.and(all);
                a.jeq(all, T::Allow, deny(d));
            }
            Rule::OneOf { arg, vals, deny: d } => {
                a.ld(off_arg_lo(arg));
                for &v in vals {
                    a.jeq(v, T::Allow, T::Next);
                }
                a.ja(deny(d));
            }
            Rule::OwnPid { arg, deny: d } => {
                a.ld(off_arg_lo(arg));
                a.jeq(pid, T::Allow, deny(d));
            }
            Rule::Null { arg, deny: d } => {
                a.ld(off_arg_lo(arg));
                a.jeq(0, T::Next, deny(d));
                a.ld(off_arg_hi(arg));
                a.jeq(0, T::Allow, deny(d));
            }
            Rule::Socket { deny: d } => {
                a.ld(off_arg_lo(0));
                a.jeq(libc::AF_INET as u32, T::Fwd(1), T::Next);
                a.jeq(libc::AF_INET6 as u32, T::Next, deny(d));
                a.ld(off_arg_lo(1));
                a.and(!((libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC) as u32));
                a.jeq(libc::SOCK_STREAM as u32, T::Allow, T::Next);
                a.jeq(libc::SOCK_DGRAM as u32, T::Allow, deny(d));
            }
        }
        a.skips.push((i, a.code.len()));
    }
    a.ja(T::Default);
    let mut errnos: Vec<u16> = Vec::new();
    for &(_, jt, jf, _) in &a.code {
        for t in [jt, jf] {
            if let T::Errno(e) = t {
                if !errnos.contains(&e) {
                    errnos.push(e);
                }
            }
        }
    }
    let allow_at = a.code.len();
    let resolve = |pc: usize, t: T| -> usize {
        match t {
            T::Next => pc + 1,
            T::Fwd(n) => pc + 1 + n,
            T::Allow => allow_at,
            T::Default => allow_at + 1,
            T::Errno(e) => allow_at + 2 + errnos.iter().position(|&x| x == e).unwrap(),
            T::Skip(i) => a.skips.iter().find(|&&(j, _)| j == i).unwrap().1,
        }
    };
    let mut out = Vec::with_capacity(a.code.len() + 2 + errnos.len());
    for (pc, &(code, jt, jf, k)) in a.code.iter().enumerate() {
        if code == BPF_JA {
            out.push(libc::sock_filter { code, jt: 0, jf: 0, k: (resolve(pc, jt) - pc - 1) as u32 });
            continue;
        }
        let rel = |t: T| -> u8 {
            let d = resolve(pc, t) - pc - 1;
            assert!(d <= 255, "seccomp filter: jump too far ({d})");
            d as u8
        };
        let (jt, jf) = if code & 0x07 == 0x05 { (rel(jt), rel(jf)) } else { (0, 0) };
        out.push(libc::sock_filter { code, jt, jf, k });
    }
    out.push(libc::sock_filter { code: BPF_RET_K, jt: 0, jf: 0, k: RET_ALLOW });
    out.push(libc::sock_filter { code: BPF_RET_K, jt: 0, jf: 0, k: default });
    for e in errnos {
        out.push(libc::sock_filter { code: BPF_RET_K, jt: 0, jf: 0, k: RET_ERRNO | e as u32 });
    }
    out
}

/// Installs `prog` on every thread (TSYNC); syscalls only, so usable in a forked child. Needs no_new_privs.
pub fn install_filter(prog: &[libc::sock_filter], log_violations: bool) -> io::Result<()> {
    let fprog = libc::sock_fprog { len: prog.len() as u16, filter: prog.as_ptr() as *mut libc::sock_filter };
    let mut flags = libc::SECCOMP_FILTER_FLAG_TSYNC;
    if log_violations {
        flags |= libc::SECCOMP_FILTER_FLAG_LOG; // errno rules are logged too
    }
    // SAFETY: fprog points to `prog`, alive for the call; the kernel copies it.
    let r = unsafe { libc::syscall(libc::SYS_seccomp, libc::SECCOMP_SET_MODE_FILTER, flags, &fprog) };
    match r {
        0 => Ok(()),
        r if r < 0 => Err(io::Error::last_os_error()),
        tid => Err(io::Error::other(format!("thread {tid} could not be synchronized"))),
    }
}

/// Installs [`SYSCALLS`] for `mode`.
pub(super) fn install(mode: Mode) -> Result<(), String> {
    // READ_IMPLIES_EXEC would make readable mappings executable behind the W^X rules.
    if super::sc(libc::SYS_personality, [0xffff_ffff_u32 as libc::c_long, 0, 0, 0, 0]).is_ok_and(|p| p & 0x0040_0000 != 0) {
        return Err("READ_IMPLIES_EXEC personality: refusing to run the JIT under it".into());
    }
    let pid = std::process::id();
    install_filter(&build(mode, pid), mode == Mode::Log).map_err(|e| format!("seccomp: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::tests::{exited, in_child, killed_by};

    /// A classic-BPF interpreter for the subset [`build`] emits.
    fn run(prog: &[libc::sock_filter], nr: u32, arch: u32, args: [u64; 6]) -> u32 {
        let mut data = [0u8; 64];
        data[0..4].copy_from_slice(&nr.to_ne_bytes());
        data[4..8].copy_from_slice(&arch.to_ne_bytes());
        for (i, a) in args.iter().enumerate() {
            data[16 + 8 * i..24 + 8 * i].copy_from_slice(&a.to_ne_bytes());
        }
        let (mut acc, mut pc) = (0u32, 0usize);
        loop {
            let f = &prog[pc];
            pc += 1;
            match f.code {
                BPF_LD_W_ABS => acc = u32::from_ne_bytes(data[f.k as usize..f.k as usize + 4].try_into().unwrap()),
                BPF_AND_K => acc &= f.k,
                BPF_JA => pc += f.k as usize,
                BPF_JEQ_K => pc += if acc == f.k { f.jt } else { f.jf } as usize,
                BPF_JSET_K => pc += if acc & f.k != 0 { f.jt } else { f.jf } as usize,
                BPF_RET_K => return f.k,
                c => panic!("unexpected BPF opcode {c:#x}"),
            }
        }
    }

    /// Kernel `seccomp_is_const_allow`: ALLOW for `nr` without reading arguments, i.e. served by the action cache.
    fn const_allow(prog: &[libc::sock_filter], nr: u32) -> bool {
        let (mut reg, mut pc) = (0u32, 0usize);
        while pc < prog.len() {
            let f = &prog[pc];
            match f.code {
                BPF_LD_W_ABS => match f.k {
                    OFF_NR => reg = nr,
                    OFF_ARCH => reg = AUDIT_ARCH,
                    _ => return false,
                },
                BPF_RET_K => return f.k == RET_ALLOW,
                BPF_JA => pc += f.k as usize,
                BPF_JEQ_K => pc += if reg == f.k { f.jt } else { f.jf } as usize,
                BPF_JSET_K => pc += if reg & f.k != 0 { f.jt } else { f.jf } as usize,
                BPF_AND_K => reg &= f.k,
                _ => return false,
            }
            pc += 1;
        }
        false
    }

    const ARCH: u32 = AUDIT_ARCH;
    /// The other ABI of the same CPU family (`AUDIT_ARCH_I386` and `AUDIT_ARCH_X86_64`, `AUDIT_ARCH_ARM` and
    /// `AUDIT_ARCH_AARCH64`).
    #[cfg(target_arch = "x86_64")]
    const COMPAT: u32 = 0x4000_0003;
    #[cfg(target_arch = "aarch64")]
    const COMPAT: u32 = 0x4000_0028;
    #[cfg(target_arch = "x86")]
    const COMPAT: u32 = 0xC000_003E;
    #[cfg(target_arch = "arm")]
    const COMPAT: u32 = 0xC000_00B7;

    #[test]
    fn table_has_no_duplicates_and_dangerous_syscalls_are_absent() {
        let mut nrs: Vec<libc::c_long> = SYSCALLS.iter().map(|s| s.nr).filter(|&n| n != NONE).collect();
        nrs.sort();
        let n = nrs.len();
        nrs.dedup();
        assert_eq!(n, nrs.len(), "duplicate syscall in SYSCALLS");
        for (nr, name) in [
            (libc::SYS_execve, "execve"),
            (libc::SYS_execveat, "execveat"),
            (libc::SYS_ptrace, "ptrace"),
            (libc::SYS_process_vm_readv, "process_vm_readv"),
            (libc::SYS_process_vm_writev, "process_vm_writev"),
            (libc::SYS_mount, "mount"),
            (libc::SYS_umount2, "umount2"),
            (libc::SYS_pivot_root, "pivot_root"),
            (libc::SYS_chroot, "chroot"),
            (libc::SYS_unshare, "unshare"),
            (libc::SYS_setns, "setns"),
            (libc::SYS_bpf, "bpf"),
            (libc::SYS_perf_event_open, "perf_event_open"),
            (libc::SYS_kexec_load, "kexec_load"),
            (libc::SYS_init_module, "init_module"),
            (libc::SYS_finit_module, "finit_module"),
            (libc::SYS_delete_module, "delete_module"),
            (libc::SYS_keyctl, "keyctl"),
            (libc::SYS_add_key, "add_key"),
            (libc::SYS_request_key, "request_key"),
            (libc::SYS_io_uring_setup, "io_uring_setup"),
            (libc::SYS_io_uring_enter, "io_uring_enter"),
            (libc::SYS_io_uring_register, "io_uring_register"),
            (libc::SYS_personality, "personality"),
            (libc::SYS_userfaultfd, "userfaultfd"),
            (libc::SYS_bind, "bind"),
            (libc::SYS_listen, "listen"),
            (libc::SYS_kill, "kill"),
            (libc::SYS_setuid, "setuid"),
            (libc::SYS_setresuid, "setresuid"),
            (libc::SYS_capset, "capset"),
            (libc::SYS_seccomp, "seccomp"),
            (libc::SYS_pkey_mprotect, "pkey_mprotect"),
            (libc::SYS_open_by_handle_at, "open_by_handle_at"),
            (libc::SYS_landlock_create_ruleset, "landlock_create_ruleset"),
        ] {
            assert!(!nrs.contains(&nr), "{name} must not be allowed");
        }
        // libc's musl bindings for AArch64 lack the number.
        #[cfg(target_arch = "aarch64")]
        assert!(!nrs.contains(&294), "kexec_file_load must not be allowed");
        #[cfg(target_arch = "x86_64")]
        for (nr, name) in [
            (libc::SYS_kexec_file_load, "kexec_file_load"),
            (libc::SYS_fork, "fork"),
            (libc::SYS_vfork, "vfork"),
            (libc::SYS_modify_ldt, "modify_ldt"),
            (libc::SYS_iopl, "iopl"),
            (libc::SYS_ioperm, "ioperm"),
        ] {
            assert!(!nrs.contains(&nr), "{name} must not be allowed");
        }
        #[cfg(target_pointer_width = "32")]
        for (nr, name) in [
            (libc::SYS_fork, "fork"),
            (libc::SYS_vfork, "vfork"),
            (libc::SYS_setuid32, "setuid32"),
            (libc::SYS_setresuid32, "setresuid32"),
            (libc::SYS_setgroups32, "setgroups32"),
        ] {
            assert!(!nrs.contains(&nr), "{name} must not be allowed");
        }
        #[cfg(target_arch = "x86")]
        for (nr, name) in [
            (libc::SYS_modify_ldt, "modify_ldt"),
            (libc::SYS_iopl, "iopl"),
            (libc::SYS_ioperm, "ioperm"),
            (libc::SYS_vm86, "vm86"),
            (libc::SYS_vm86old, "vm86old"),
        ] {
            assert!(!nrs.contains(&nr), "{name} must not be allowed");
        }
    }

    #[test]
    fn filter_semantics() {
        let pid = 4242;
        for mode in [Mode::On, Mode::Log] {
            let deny = if mode == Mode::Log { RET_LOG } else { RET_KILL_PROCESS };
            let p = build(mode, pid);
            let r = |nr: libc::c_long, args: [u64; 6]| run(&p, nr as u32, ARCH, args);
            let z = [0u64; 6];
            for s in SYSCALLS.iter().filter(|s| s.rule == Rule::Allow && s.nr != NONE) {
                assert_eq!(run(&p, s.nr as u32, ARCH, [u64::MAX; 6]), RET_ALLOW, "nr {}", s.nr);
            }
            for nr in [libc::SYS_execve, libc::SYS_ptrace, libc::SYS_bind, libc::SYS_getpgid, libc::SYS_io_uring_setup, 999] {
                assert_eq!(r(nr, z), deny, "nr {nr}");
            }
            assert_eq!(run(&p, libc::SYS_read as u32, COMPAT, z), deny);
            #[cfg(target_arch = "x86_64")]
            assert_eq!(run(&p, X32_SYSCALL_BIT | libc::SYS_read as u32, ARCH, z), deny);
            let (rw, rx) = ((libc::PROT_READ | libc::PROT_WRITE) as u64, (libc::PROT_READ | libc::PROT_EXEC) as u64);
            let mmap = if nr::SYS_mmap != NONE { nr::SYS_mmap } else { nr::SYS_mmap2 };
            assert_eq!(r(mmap, [0, 4096, rw, 0x22, u64::MAX, 0]), RET_ALLOW);
            assert_eq!(r(mmap, [0, 4096, rx, 0x22, u64::MAX, 0]), deny);
            let jit = cfg!(any(target_arch = "x86_64", target_arch = "aarch64"));
            assert_eq!(r(libc::SYS_mprotect, [0, 4096, rx, 0, 0, 0]), if jit { RET_ALLOW } else { deny });
            assert_eq!(r(libc::SYS_mprotect, [0, 4096, rw | rx, 0, 0, 0]), deny);
            #[cfg(target_arch = "x86")]
            {
                assert_eq!(r(nr::SYS_socketcall, [3, 0, 0, 0, 0, 0]), RET_ALLOW, "connect");
                for call in [1, 2, 4, 8, 19, 20] {
                    assert_eq!(r(nr::SYS_socketcall, [call, 0, 0, 0, 0, 0]), RET_ERRNO | libc::EPERM as u32, "socketcall {call}");
                }
            }
            let thread = (libc::CLONE_VM | libc::CLONE_FS | libc::CLONE_FILES | libc::CLONE_SIGHAND | libc::CLONE_THREAD) as u64;
            assert_eq!(r(libc::SYS_clone, [thread, 0, 0, 0, 0, 0]), RET_ALLOW);
            assert_eq!(r(libc::SYS_clone, [libc::SIGCHLD as u64, 0, 0, 0, 0, 0]), deny, "fork-like clone");
            assert_eq!(r(libc::SYS_clone, [thread | libc::CLONE_NEWUSER as u64, 0, 0, 0, 0, 0]), deny);
            assert_eq!(r(libc::SYS_clone3, z), RET_ERRNO | libc::ENOSYS as u32);
            let eperm = RET_ERRNO | libc::EPERM as u32;
            let nb = (libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC) as u64;
            assert_eq!(r(libc::SYS_socket, [libc::AF_INET as u64, 1 | nb, 0, 0, 0, 0]), RET_ALLOW);
            assert_eq!(r(libc::SYS_socket, [libc::AF_INET6 as u64, 2, 0, 0, 0, 0]), RET_ALLOW);
            assert_eq!(r(libc::SYS_socket, [libc::AF_INET as u64, libc::SOCK_RAW as u64, 0, 0, 0, 0]), eperm);
            assert_eq!(r(libc::SYS_socket, [libc::AF_INET as u64, libc::SOCK_SEQPACKET as u64 | nb, 0, 0, 0, 0]), eperm);
            for fam in [libc::AF_UNIX, libc::AF_NETLINK, libc::AF_PACKET, libc::AF_ALG, libc::AF_VSOCK] {
                assert_eq!(r(libc::SYS_socket, [fam as u64, 1, 0, 0, 0, 0]), eperm);
            }
            assert_eq!(r(libc::SYS_prctl, [libc::PR_SET_NAME as u64, 0, 0, 0, 0, 0]), RET_ALLOW);
            assert_eq!(r(libc::SYS_prctl, [libc::PR_SET_DUMPABLE as u64, 1, 0, 0, 0, 0]), eperm);
            assert_eq!(r(libc::SYS_ioctl, [3, libc::FIONBIO as u64, 0, 0, 0, 0]), RET_ALLOW);
            assert_eq!(r(libc::SYS_ioctl, [0, libc::TIOCSTI as u64, 0, 0, 0, 0]), RET_ERRNO | libc::ENOTTY as u32);
            assert_eq!(r(libc::SYS_sched_setaffinity, [1, 0, 0, 0, 0, 0]), eperm);
            assert_eq!(r(libc::SYS_prlimit64, [0, 3, 0x1_0000_0000, 0, 0, 0]), eperm, "high word");
            assert_eq!(r(libc::SYS_tgkill, [pid as u64, 77, 6, 0, 0, 0]), RET_ALLOW);
            assert_eq!(r(libc::SYS_tgkill, [1, 1, 9, 0, 0, 0]), deny);
            assert_eq!(r(libc::SYS_tkill, [77, libc::SIGABRT as u64, 0, 0, 0, 0]), RET_ALLOW);
            assert_eq!(r(libc::SYS_tkill, [77, libc::SIGKILL as u64, 0, 0, 0, 0]), deny);
            #[cfg(target_arch = "aarch64")]
            for (cmd, want) in [(1u64 << 5, RET_ALLOW), (1 << 6, RET_ALLOW), (1 << 3, eperm), (1, eperm)] {
                assert_eq!(r(libc::SYS_membarrier, [cmd, 0, 0, 0, 0, 0]), want, "membarrier {cmd:#x}");
            }
            #[cfg(target_arch = "x86_64")]
            assert_eq!(r(libc::SYS_membarrier, [1 << 5, 0, 0, 0, 0, 0]), deny);
        }
    }

    #[test]
    fn plain_allows_are_served_by_the_kernel_action_cache() {
        let p = build(Mode::On, 1);
        for s in SYSCALLS.iter().filter(|s| s.nr != NONE) {
            assert_eq!(const_allow(&p, s.nr as u32), s.rule == Rule::Allow, "nr {}", s.nr);
        }
    }

    /// Runs `f` in a child under the filter for `mode`.
    fn filtered(mode: Mode, f: impl FnOnce() -> i32) -> i32 {
        let prog = build(mode, 0);
        in_child(move || {
            if super::super::sc(libc::SYS_prctl, [libc::PR_SET_NO_NEW_PRIVS as _, 1, 0, 0, 0]).is_err()
                || install_filter(&prog, false).is_err()
            {
                return 90;
            }
            f()
        })
    }

    fn raw(nr: libc::c_long, a: [libc::c_long; 3]) -> libc::c_long {
        // SAFETY: test syscalls with integer or null arguments.
        unsafe { libc::syscall(nr, a[0], a[1], a[2]) }
    }

    #[test]
    fn denied_syscalls_kill() {
        assert!(killed_by(filtered(Mode::On, || raw(libc::SYS_getpgid, [0, 0, 0]) as i32), libc::SIGSYS));
        assert!(killed_by(filtered(Mode::On, || raw(libc::SYS_ptrace, [libc::PTRACE_TRACEME as _, 0, 0]) as i32), libc::SIGSYS));
        #[cfg(target_arch = "x86_64")]
        assert!(killed_by(filtered(Mode::On, || raw(X32_SYSCALL_BIT as libc::c_long | libc::SYS_getpid, [0, 0, 0]) as i32), libc::SIGSYS));
        #[cfg(target_arch = "x86_64")]
        {
            let st = filtered(Mode::On, || {
                // SAFETY: int 0x80 with eax = 20 (i386 getpid); only eax is clobbered.
                unsafe { std::arch::asm!("int 0x80", inout("eax") 20u32 => _) };
                0
            });
            assert!(killed_by(st, libc::SIGSYS) || killed_by(st, libc::SIGSEGV), "status {st:#x}");
        }
        let st = filtered(Mode::On, || {
            // SAFETY: fresh private anonymous mappings.
            unsafe {
                let p = libc::mmap(std::ptr::null_mut(), 4096, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_PRIVATE | libc::MAP_ANONYMOUS, -1, 0);
                libc::mprotect(p, 4096, libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC);
            }
            0
        });
        assert!(killed_by(st, libc::SIGSYS), "mprotect rwx: {st:#x}");
        // Without a JIT nothing becomes executable.
        #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
        {
            let st = filtered(Mode::On, || {
                // SAFETY: a fresh private anonymous mapping.
                unsafe {
                    let p = libc::mmap(std::ptr::null_mut(), 4096, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_PRIVATE | libc::MAP_ANONYMOUS, -1, 0);
                    libc::mprotect(p, 4096, libc::PROT_READ | libc::PROT_EXEC);
                }
                0
            });
            assert!(killed_by(st, libc::SIGSYS), "mprotect rx: {st:#x}");
        }
        // tkill: SIGABRT only (musl abort()), anything else kills.
        let tid = || raw(libc::SYS_gettid, [0, 0, 0]);
        assert!(killed_by(filtered(Mode::On, || raw(libc::SYS_tkill, [tid(), libc::SIGUSR1 as _, 0]) as i32), libc::SIGSYS));
        assert!(killed_by(filtered(Mode::On, || raw(libc::SYS_tkill, [tid(), libc::SIGABRT as _, 0]) as i32), libc::SIGABRT));
        // In log mode the default action logs instead.
        assert!(exited(filtered(Mode::Log, || (raw(libc::SYS_getpgid, [0, 0, 0]) < 0) as i32), 0));
    }

    #[test]
    fn allowed_and_steered_syscalls() {
        let st = filtered(Mode::On, || {
            let errno = || std::io::Error::last_os_error().raw_os_error();
            if raw(libc::SYS_clone3, [0, 0, 0]) != -1 || errno() != Some(libc::ENOSYS) {
                return 1;
            }
            if raw(libc::SYS_socket, [libc::AF_UNIX as _, libc::SOCK_STREAM as _, 0]) != -1 || errno() != Some(libc::EPERM) {
                return 2;
            }
            if raw(libc::SYS_socket, [libc::AF_INET as _, libc::SOCK_RAW as _, 255]) != -1 || errno() != Some(libc::EPERM) {
                return 3;
            }
            if raw(libc::SYS_socket, [libc::AF_INET6 as _, (libc::SOCK_SEQPACKET | libc::SOCK_NONBLOCK) as _, 0]) != -1
                || errno() != Some(libc::EPERM)
            {
                return 10;
            }
            for ty in [libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC] {
                let s = raw(libc::SYS_socket, [libc::AF_INET as _, ty as _, 0]);
                if s < 0 || raw(libc::SYS_close, [s, 0, 0]) != 0 {
                    return 4;
                }
            }
            let ids = if cfg!(target_pointer_width = "64") {
                [nr::SYS_getuid, nr::SYS_geteuid, nr::SYS_getgid, nr::SYS_getegid]
            } else {
                [nr::SYS_getuid32, nr::SYS_geteuid32, nr::SYS_getgid32, nr::SYS_getegid32]
            };
            for nr in [libc::SYS_getppid].into_iter().chain(ids) {
                if raw(nr, [0, 0, 0]) < 0 {
                    return 5;
                }
            }
            // SAFETY: valid out-pointers.
            unsafe {
                let mut u: libc::utsname = std::mem::zeroed();
                let (mut r, mut e, mut s) = (0, 0, 0);
                let mut pfd = libc::pollfd { fd: 0, events: 0, revents: 0 };
                let ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
                if libc::uname(&mut u) != 0
                    || libc::getresuid(&mut r, &mut e, &mut s) != 0
                    || libc::getresgid(&mut r, &mut e, &mut s) != 0
                    || libc::poll(&mut pfd, 1, 0) < 0
                    || libc::ppoll(&mut pfd, 1, &ts, std::ptr::null()) < 0
                {
                    return 6;
                }
                // A thread still starts (clone after the clone3 fallback).
                extern "C" fn body(_: *mut libc::c_void) -> *mut libc::c_void {
                    std::ptr::null_mut()
                }
                let mut t = std::mem::MaybeUninit::<libc::pthread_t>::uninit();
                if libc::pthread_create(t.as_mut_ptr(), std::ptr::null(), body, std::ptr::null_mut()) != 0 {
                    return 7;
                }
                libc::pthread_join(t.assume_init(), std::ptr::null_mut());
                // The JIT's sequence: RW mapping, written, then RX.
                #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
                {
                    let p = libc::mmap(std::ptr::null_mut(), 4096, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_PRIVATE | libc::MAP_ANONYMOUS, -1, 0);
                    #[cfg(target_arch = "x86_64")]
                    (p as *mut u8).write(0xC3); // ret
                    #[cfg(target_arch = "aarch64")]
                    {
                        (p as *mut u32).write(0xD65F_03C0); // ret
                        std::arch::asm!("dc cvau, {p}", "dsb ish", "ic ivau, {p}", "dsb ish", "isb", p = in(reg) p);
                    }
                    if libc::mprotect(p, 4096, libc::PROT_READ | libc::PROT_EXEC) != 0 {
                        return 8;
                    }
                    let f: extern "C" fn() = std::mem::transmute(p);
                    f();
                }
            }
            let d = std::env::temp_dir().join(format!("k10s-sandbox-rmdir-{}", std::process::id()));
            if std::fs::create_dir(&d).is_err() || std::fs::remove_dir(&d).is_err() {
                return 9;
            }
            0
        });
        assert!(exited(st, 0), "status {st:#x}");
    }

    /// Filter cost: cached allow (getpid) and argument-checked (mmap+munmap), without and with the filter.
    #[test]
    #[ignore]
    fn syscall_overhead() {
        fn best(n: u32, f: impl Fn()) -> f64 {
            (0..20)
                .map(|_| {
                    let t = std::time::Instant::now();
                    for _ in 0..n {
                        f();
                    }
                    t.elapsed().as_nanos() as f64 / n as f64
                })
                .fold(f64::MAX, f64::min)
        }
        for (label, mode) in [("no filter", Mode::Off), ("filter", Mode::On)] {
            let st = in_child(move || {
                if mode == Mode::On {
                    let _ = super::super::sc(libc::SYS_prctl, [libc::PR_SET_NO_NEW_PRIVS as _, 1, 0, 0, 0]);
                    install_filter(&build(mode, 0), false).unwrap();
                }
                let a = best(100_000, || {
                    raw(libc::SYS_getpid, [0, 0, 0]);
                });
                // SAFETY: fresh private mapping, unmapped at once.
                let b = best(10_000, || unsafe {
                    let p = libc::mmap(std::ptr::null_mut(), 4096, 3, libc::MAP_PRIVATE | libc::MAP_ANONYMOUS, -1, 0);
                    libc::munmap(p, 4096);
                });
                eprintln!("{label}: getpid {a:.1} ns, mmap+munmap {b:.1} ns");
                0
            });
            assert!(exited(st, 0));
        }
    }
}
