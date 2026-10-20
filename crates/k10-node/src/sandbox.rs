//! Process sandbox of `k10s node` (Linux): defense in depth under SFI, bounding what code that escapes the JIT
//! sandbox can do to the host. Order (main.rs): [`early`], [`before_threads`] (`--user` fs identity; Landlock, which
//! is per thread, so before any thread exists), node start, [`after_start`] (privilege drop; seccomp with TSYNC on
//! every thread). Only the daemon installs it; library users (tests, `k10s run`) never do. See SECURITY.md.

use std::ffi::CString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// `--sandbox on|log|off`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Everything enforced; a denied syscall kills the process (`SIGSYS`).
    On,
    /// Dry run: as `On` but no Landlock (no permissive mode) and seccomp violations logged (audit type=1326), not denied.
    Log,
    /// Nothing at all (escape hatch).
    Off,
}

impl Mode {
    pub fn parse(s: &str) -> Option<Mode> {
        match s {
            "on" => Some(Mode::On),
            "log" => Some(Mode::Log),
            "off" => Some(Mode::Off),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Mode::On => "on",
            Mode::Log => "log",
            Mode::Off => "off",
        }
    }

    /// `K10S_SANDBOX` (`on` | `log` | `off`) if set, else [`Mode::On`].
    pub fn from_env() -> Result<Mode, String> {
        match std::env::var("K10S_SANDBOX") {
            Err(_) => Ok(Mode::On),
            Ok(v) => Mode::parse(v.trim()).ok_or_else(|| format!("K10S_SANDBOX=`{v}`: expected on|log|off")),
        }
    }
}

fn cvt(r: libc::c_long) -> io::Result<libc::c_long> {
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r)
    }
}

// ------------------------------------------------------------------------------------------------------------------
// 1. Process hygiene
// ------------------------------------------------------------------------------------------------------------------

/// Closes inherited descriptors ≥ 3 and sets `PR_SET_NO_NEW_PRIVS` (needed by Landlock/seccomp). Call first.
pub fn early() -> io::Result<()> {
    close_inherited_fds();
    // SAFETY: plain prctl with integer arguments.
    cvt(unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } as libc::c_long)?;
    Ok(())
}

fn close_inherited_fds() {
    // SAFETY: close_range only closes descriptors; nothing in this process uses any above 2 yet.
    if unsafe { libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, 0u32) } == 0 {
        return;
    }
    // Linux < 5.9: collect /proc/self/fd first (closing while iterating would close the walker's fd).
    let fds: Vec<i32> = match std::fs::read_dir("/proc/self/fd") {
        Ok(d) => d.filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok()).filter(|&fd| fd > 2).collect(),
        Err(_) => return,
    };
    for fd in fds {
        // SAFETY: as above (the read_dir fd is already closed: closing it again fails harmlessly).
        unsafe { libc::close(fd) };
    }
}

// ------------------------------------------------------------------------------------------------------------------
// 2./4. Privilege drop (`--user USER[:GROUP]`)
// ------------------------------------------------------------------------------------------------------------------

/// The unprivileged identity the node runs as.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct User {
    pub name: String,
    pub uid: libc::uid_t,
    pub gid: libc::gid_t,
}

fn getpw(name: &str) -> Option<(libc::uid_t, libc::gid_t)> {
    let c = CString::new(name).ok()?;
    let mut buf = vec![0u8; 16 << 10];
    // SAFETY: an all-zero passwd is a valid out-parameter; getpwnam_r writes strings into `buf`.
    let mut pw: libc::passwd = unsafe { std::mem::zeroed() };
    let mut res: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: valid C string, out-pointers and buffer of the given length.
    let r = unsafe { libc::getpwnam_r(c.as_ptr(), &mut pw, buf.as_mut_ptr() as *mut libc::c_char, buf.len(), &mut res) };
    (r == 0 && !res.is_null()).then_some((pw.pw_uid, pw.pw_gid))
}

fn getgr(name: &str) -> Option<libc::gid_t> {
    let c = CString::new(name).ok()?;
    let mut buf = vec![0u8; 64 << 10];
    // SAFETY: as in getpw.
    let mut gr: libc::group = unsafe { std::mem::zeroed() };
    let mut res: *mut libc::group = std::ptr::null_mut();
    // SAFETY: valid C string, out-pointers and buffer of the given length.
    let r = unsafe { libc::getgrnam_r(c.as_ptr(), &mut gr, buf.as_mut_ptr() as *mut libc::c_char, buf.len(), &mut res) };
    (r == 0 && !res.is_null()).then_some(gr.gr_gid)
}

impl User {
    /// `USER[:GROUP]`, names or numeric ids; the group defaults to the user's primary group (required for a bare uid).
    pub fn resolve(spec: &str) -> Result<User, String> {
        let (u, g) = match spec.split_once(':') {
            Some((u, g)) => (u, Some(g)),
            None => (spec, None),
        };
        if u.is_empty() {
            return Err(format!("--user `{spec}`: empty user"));
        }
        let (uid, primary) = match u.parse::<libc::uid_t>() {
            Ok(uid) => (uid, None),
            Err(_) => match getpw(u) {
                Some((uid, gid)) => (uid, Some(gid)),
                None => return Err(format!("--user `{spec}`: unknown user `{u}`")),
            },
        };
        let gid = match g {
            Some(g) => match g.parse::<libc::gid_t>() {
                Ok(gid) => gid,
                Err(_) => getgr(g).ok_or_else(|| format!("--user `{spec}`: unknown group `{g}`"))?,
            },
            None => match primary {
                Some(gid) => gid,
                None => return Err(format!("--user `{spec}`: numeric uid needs a group (UID:GID)")),
            },
        };
        if uid == 0 || gid == 0 {
            return Err(format!("--user `{spec}`: refusing to run as uid/gid 0"));
        }
        Ok(User { name: u.to_string(), uid, gid })
    }

    /// Before any thread: file access as the user (setfsuid/setfsgid, groups), euid still 0 for privileged ports.
    /// A missing `data_dir` is created and chowned; an existing one must already belong to the user.
    pub fn assume_fs_identity(&self, data_dir: Option<&Path>) -> Result<(), String> {
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } != 0 {
            return Err(format!("--user {}: the node must be started as root to switch users", self.name));
        }
        if let Some(d) = data_dir {
            if !d.exists() {
                std::fs::create_dir_all(d).map_err(|e| format!("cannot create {}: {e}", d.display()))?;
                let c = CString::new(d.as_os_str().as_bytes()).map_err(|_| "bad data dir path".to_string())?;
                // SAFETY: valid C string.
                if unsafe { libc::chown(c.as_ptr(), self.uid, self.gid) } != 0 {
                    return Err(format!("chown {}: {}", d.display(), io::Error::last_os_error()));
                }
            }
            use std::os::unix::fs::MetadataExt;
            let md = std::fs::metadata(d).map_err(|e| format!("{}: {e}", d.display()))?;
            if md.uid() != self.uid {
                return Err(format!(
                    "data directory {} belongs to uid {}, not to --user {} (uid {}): `chown -R {}: {}` first",
                    d.display(),
                    md.uid(),
                    self.name,
                    self.uid,
                    self.name,
                    d.display()
                ));
            }
        }
        let groups = [self.gid];
        // SAFETY: plain syscalls; one thread only (glibc's setgroups would broadcast anyway).
        unsafe {
            if libc::setgroups(1, groups.as_ptr()) != 0 {
                return Err(format!("setgroups: {}", io::Error::last_os_error()));
            }
            libc::setfsgid(self.gid);
            libc::setfsuid(self.uid);
            // setfs*id return the previous id: read back to check.
            if libc::setfsuid(u32::MAX) as libc::uid_t != self.uid || libc::setfsgid(u32::MAX) as libc::gid_t != self.gid {
                return Err("setfsuid/setfsgid did not take effect".into());
            }
        }
        Ok(())
    }

    /// After binding: all uids/gids become the user's in every thread (glibc broadcasts setres*id); caps are cleared.
    pub fn drop_privileges(&self) -> Result<(), String> {
        // SAFETY: plain libc calls with integer arguments / out-pointers to locals.
        unsafe {
            if libc::setresgid(self.gid, self.gid, self.gid) != 0 {
                return Err(format!("setresgid({}): {}", self.gid, io::Error::last_os_error()));
            }
            if libc::setresuid(self.uid, self.uid, self.uid) != 0 {
                return Err(format!("setresuid({}): {}", self.uid, io::Error::last_os_error()));
            }
            let (mut r, mut e, mut s) = (0, 0, 0);
            libc::getresuid(&mut r, &mut e, &mut s);
            if [r, e, s] != [self.uid; 3] {
                return Err(format!("uid is still {r}/{e}/{s} after setresuid"));
            }
        }
        Ok(())
    }
}

// ------------------------------------------------------------------------------------------------------------------
// 3. Landlock
// ------------------------------------------------------------------------------------------------------------------

/// Landlock UAPI (`include/uapi/linux/landlock.h`); the `libc` crate has the syscall numbers only.
mod ll {
    pub const CREATE_RULESET_VERSION: u32 = 1 << 0;
    pub const RULE_PATH_BENEATH: u32 = 1;
    pub const RULE_NET_PORT: u32 = 2;

    pub const FS_EXECUTE: u64 = 1 << 0;
    pub const FS_WRITE_FILE: u64 = 1 << 1;
    pub const FS_READ_FILE: u64 = 1 << 2;
    pub const FS_READ_DIR: u64 = 1 << 3;
    pub const FS_REMOVE_DIR: u64 = 1 << 4;
    pub const FS_REMOVE_FILE: u64 = 1 << 5;
    pub const FS_MAKE_CHAR: u64 = 1 << 6;
    pub const FS_MAKE_DIR: u64 = 1 << 7;
    pub const FS_MAKE_REG: u64 = 1 << 8;
    pub const FS_MAKE_SOCK: u64 = 1 << 9;
    pub const FS_MAKE_FIFO: u64 = 1 << 10;
    pub const FS_MAKE_BLOCK: u64 = 1 << 11;
    pub const FS_MAKE_SYM: u64 = 1 << 12;
    /// ABI 2 (Linux 5.19): link/rename across directories.
    pub const FS_REFER: u64 = 1 << 13;
    /// ABI 3 (Linux 6.2): truncate(2), ftruncate on files opened without it, O_TRUNC.
    pub const FS_TRUNCATE: u64 = 1 << 14;
    /// ABI 5 (Linux 6.10): ioctl on character/block devices.
    pub const FS_IOCTL_DEV: u64 = 1 << 15;

    /// ABI 4 (Linux 6.7).
    pub const NET_BIND_TCP: u64 = 1 << 0;

    /// ABI 6 (Linux 6.12).
    pub const SCOPE_ABSTRACT_UNIX_SOCKET: u64 = 1 << 0;
    pub const SCOPE_SIGNAL: u64 = 1 << 1;

    #[repr(C)]
    pub struct RulesetAttr {
        pub handled_access_fs: u64,
        pub handled_access_net: u64,
        pub scoped: u64,
    }

    #[repr(C, packed)]
    pub struct PathBeneathAttr {
        pub allowed_access: u64,
        pub parent_fd: i32,
    }

    #[repr(C)]
    pub struct NetPortAttr {
        pub allowed_access: u64,
        pub port: u64,
    }

    /// Every filesystem right a kernel with this ABI knows.
    pub fn fs_rights(abi: u32) -> u64 {
        let mut r = (1 << 13) - 1; // ABI 1: EXECUTE ..= MAKE_SYM
        if abi >= 2 {
            r |= FS_REFER;
        }
        if abi >= 3 {
            r |= FS_TRUNCATE;
        }
        if abi >= 5 {
            r |= FS_IOCTL_DEV;
        }
        r
    }

    /// Data directory: regular files and directories only (no exec, special files or symlinks).
    pub const DATA_RW: u64 = FS_READ_FILE
        | FS_WRITE_FILE
        | FS_READ_DIR
        | FS_REMOVE_DIR
        | FS_REMOVE_FILE
        | FS_MAKE_DIR
        | FS_MAKE_REG
        | FS_REFER
        | FS_TRUNCATE;
    pub const RO: u64 = FS_READ_FILE | FS_READ_DIR;

    // Never granted (documents intent).
    #[allow(dead_code)]
    pub const NEVER: u64 = FS_EXECUTE | FS_MAKE_CHAR | FS_MAKE_BLOCK | FS_MAKE_SOCK | FS_MAKE_FIFO | FS_MAKE_SYM | FS_IOCTL_DEV;
}

/// What the node may touch once it runs.
#[derive(Clone, Debug, Default)]
pub struct Policy {
    /// Read-write ([`ll::DATA_RW`]).
    pub data_dir: Option<PathBuf>,
    /// Extra read-write directories (e.g. `K10_JIT_DUMP`).
    pub write_dirs: Vec<PathBuf>,
    /// Extra read-only files or directories (e.g. key/token files read after start-up).
    pub read_paths: Vec<PathBuf>,
    /// Landlock ABI ≥ 4: the only TCP ports `bind(2)` may use; empty or containing 0 (ephemeral) = unrestricted.
    pub tcp_bind_ports: Vec<u16>,
}

/// Read-only files every node reads after start-up.
fn base_read_paths() -> Vec<PathBuf> {
    let mut v = vec![
        // RSS/PSS for status and metrics (smaps_rollup, statm).
        PathBuf::from("/proc/self"),
        // Snapshot-mapping VMA budget (Node::start).
        PathBuf::from("/proc/sys/vm/max_map_count"),
        // glibc malloc on arena creation: get_nprocs() and the heap-shrink policy.
        PathBuf::from("/sys/devices/system/cpu"),
        PathBuf::from("/proc/sys/vm/overcommit_memory"),
    ];
    // Panic backtraces read the binary's debug info.
    if let Ok(exe) = std::env::current_exe() {
        v.push(exe);
    }
    v
}

/// Outcome of [`landlock`], for the start-up log line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LandlockStatus {
    /// Applied; `net`: TCP bind restricted, `scoped`: signals and abstract sockets scoped.
    Applied { abi: u32, net: bool, scoped: bool },
    /// No Landlock in this kernel (or disabled).
    Unsupported(String),
}

/// A ruleset with everything allocated, so [`Prepared::apply`] only makes syscalls (fork-safe, for the tests).
pub struct Prepared {
    abi: u32,
    rules: Vec<(CString, u64)>,
    bind_ports: Vec<u16>,
}

/// The Landlock ABI version of the running kernel (0 = unsupported).
pub fn landlock_abi() -> u32 {
    // SAFETY: the documented version query (NULL attr, size 0).
    let r = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<ll::RulesetAttr>(),
            0usize,
            ll::CREATE_RULESET_VERSION,
        )
    };
    if r < 0 {
        0
    } else {
        r as u32
    }
}

impl Prepared {
    pub fn new(p: &Policy) -> Prepared {
        let abi = landlock_abi();
        let mut rules = Vec::new();
        let mut add = |path: &Path, access: u64| {
            if let Ok(c) = CString::new(path.as_os_str().as_bytes()) {
                rules.push((c, access));
            }
        };
        if let Some(d) = &p.data_dir {
            add(d, ll::DATA_RW);
        }
        for d in &p.write_dirs {
            add(d, ll::DATA_RW);
        }
        for f in base_read_paths().iter().chain(&p.read_paths) {
            add(f, ll::RO);
        }
        let restrict_bind = !p.tcp_bind_ports.is_empty() && !p.tcp_bind_ports.contains(&0);
        Prepared { abi, rules, bind_ports: if restrict_bind { p.tcp_bind_ports.clone() } else { Vec::new() } }
    }

    /// Applies the ruleset to the calling thread and the threads it creates afterwards.
    pub fn apply(&self) -> io::Result<LandlockStatus> {
        if self.abi == 0 {
            return Ok(LandlockStatus::Unsupported("not supported by this kernel (Linux >= 5.13 with landlock in the lsm= list)".into()));
        }
        let fs = ll::fs_rights(self.abi);
        let net = self.abi >= 4 && !self.bind_ports.is_empty();
        let scoped = self.abi >= 6;
        let attr = ll::RulesetAttr {
            handled_access_fs: fs,
            handled_access_net: if net { ll::NET_BIND_TCP } else { 0 },
            scoped: if scoped { ll::SCOPE_SIGNAL | ll::SCOPE_ABSTRACT_UNIX_SOCKET } else { 0 },
        };
        // Pass only the fields this ABI knows (older kernels reject unknown ones).
        let size = match self.abi {
            1..=3 => 8,
            4..=5 => 16,
            _ => 24,
        };
        // SAFETY: attr is valid for `size` bytes.
        let rs = cvt(unsafe { libc::syscall(libc::SYS_landlock_create_ruleset, &attr as *const ll::RulesetAttr, size as usize, 0u32) })?
            as i32;
        let r = (|| -> io::Result<()> {
            for (path, access) in &self.rules {
                // SAFETY: valid C string; O_PATH opens without reading.
                let fd = unsafe { libc::open(path.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
                if fd < 0 {
                    continue; // optional path missing (e.g. some /proc files in containers)
                }
                // A rule on a non-directory may only carry file rights.
                // SAFETY: an all-zero stat is a valid out-parameter.
                let mut st: libc::stat = unsafe { std::mem::zeroed() };
                // SAFETY: valid fd and out-pointer.
                let is_dir = unsafe { libc::fstat(fd, &mut st) } == 0 && st.st_mode & libc::S_IFMT == libc::S_IFDIR;
                let file_rights = ll::FS_EXECUTE | ll::FS_WRITE_FILE | ll::FS_READ_FILE | ll::FS_TRUNCATE | ll::FS_IOCTL_DEV;
                let allowed = access & fs & if is_dir { u64::MAX } else { file_rights };
                let rule = ll::PathBeneathAttr { allowed_access: allowed, parent_fd: fd };
                // SAFETY: valid ruleset fd and rule pointer.
                let r = unsafe {
                    libc::syscall(libc::SYS_landlock_add_rule, rs, ll::RULE_PATH_BENEATH, &rule as *const ll::PathBeneathAttr, 0u32)
                };
                let e = io::Error::last_os_error();
                // SAFETY: our fd.
                unsafe { libc::close(fd) };
                if r < 0 {
                    return Err(io::Error::new(e.kind(), format!("landlock rule {}: {e}", path.to_string_lossy())));
                }
            }
            if net {
                for &port in &self.bind_ports {
                    let rule = ll::NetPortAttr { allowed_access: ll::NET_BIND_TCP, port: port as u64 };
                    // SAFETY: valid ruleset fd and rule pointer.
                    cvt(unsafe {
                        libc::syscall(libc::SYS_landlock_add_rule, rs, ll::RULE_NET_PORT, &rule as *const ll::NetPortAttr, 0u32)
                    })?;
                }
            }
            // SAFETY: valid ruleset fd; no_new_privs is set by `early`.
            cvt(unsafe { libc::syscall(libc::SYS_landlock_restrict_self, rs, 0u32) })?;
            Ok(())
        })();
        // SAFETY: our fd.
        unsafe { libc::close(rs) };
        r.map(|()| LandlockStatus::Applied { abi: self.abi, net, scoped })
    }
}

/// Landlock for the calling thread and its future threads: call before any other thread exists.
pub fn landlock(p: &Policy) -> io::Result<LandlockStatus> {
    Prepared::new(p).apply()
}

// ------------------------------------------------------------------------------------------------------------------
// 5. seccomp-bpf
// ------------------------------------------------------------------------------------------------------------------

/// What happens to a call that a [`Rule`] rejects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Deny {
    /// The filter's default action: kill the process (`On`) / log and allow (`Log`).
    Kill,
    /// Fail with this errno (for calls a library may probe and handle).
    Errno(u16),
}

/// What the filter does with one syscall.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rule {
    /// Allowed whatever the arguments: served by the kernel's action cache (≥ 5.11) without running the filter.
    /// Keep every hot-path syscall in this form.
    Allow,
    /// Always fails with this errno (steers libc to a checkable fallback: `clone3` → `clone`).
    Errno(u16),
    /// Allowed when no bit of `mask` is set in argument `arg` (low 32 bits).
    NoBits { arg: u8, mask: u32, deny: Deny },
    /// Allowed unless *all* bits of `mask` are set in argument `arg` (low 32 bits).
    NotAllBits { arg: u8, mask: u32, deny: Deny },
    /// Allowed when every bit of `all` and no bit of `none` is set in argument `arg` (low 32 bits).
    Bits { arg: u8, all: u32, none: u32, deny: Deny },
    /// Allowed when argument `arg` (low 32 bits) is one of `vals`.
    OneOf { arg: u8, vals: &'static [u32], deny: Deny },
    /// Allowed when argument `arg` (low 32 bits) is this process's pid.
    OwnPid { arg: u8, deny: Deny },
    /// Allowed when the whole 64-bit argument `arg` is 0 (a NULL pointer).
    Null { arg: u8, deny: Deny },
}

/// One allowlist entry.
pub struct Sys {
    pub nr: i64,
    pub name: &'static str,
    pub rule: Rule,
}

const fn allow(nr: libc::c_long, name: &'static str) -> Sys {
    Sys { nr, name, rule: Rule::Allow }
}

const fn sys(nr: libc::c_long, name: &'static str, rule: Rule) -> Sys {
    Sys { nr, name, rule }
}

const PROT_WRITE: u32 = libc::PROT_WRITE as u32;
const PROT_EXEC: u32 = libc::PROT_EXEC as u32;
/// Namespace-creating `clone` flags (a user namespace would give an attacker namespaced capabilities).
const CLONE_NEW_ANY: u32 = (libc::CLONE_NEWNS
    | libc::CLONE_NEWCGROUP
    | libc::CLONE_NEWUTS
    | libc::CLONE_NEWIPC
    | libc::CLONE_NEWUSER
    | libc::CLONE_NEWPID
    | libc::CLONE_NEWNET
    | libc::CLONE_NEWTIME) as u32;
const EPERM: u16 = libc::EPERM as u16;

/// The syscall allowlist of a running node (x86-64); anything else kills the process. Built from code inspection
/// and a ptrace recording of the k10s test suites (both backends, clusters, chaos/abuse/soak) and bench workloads.
/// Add a feature's syscall here (one line). Listeners (HTTP, peer, admin) are bound before the filter, so
/// `bind`/`listen` stay denied: a compromised node cannot open a new port.
pub const SYSCALLS: &[Sys] = &[
    // ---- event loops, HTTP, peer links, gossip (hot path: argument-independent) ----------------------------------
    allow(libc::SYS_epoll_wait, "epoll_wait"),                 // shard and mesh loops
    allow(libc::SYS_epoll_pwait, "epoll_pwait"),               // (libc variants of the same)
    allow(libc::SYS_epoll_pwait2, "epoll_pwait2"),
    allow(libc::SYS_epoll_ctl, "epoll_ctl"),                   // connections in/out of the epoll sets
    allow(libc::SYS_epoll_create1, "epoll_create1"),           // shard/mesh start-up (may run after the filter)
    allow(libc::SYS_read, "read"),                             // sockets, eventfds, files
    allow(libc::SYS_write, "write"),                           // eventfd wake-ups, stderr, files
    allow(libc::SYS_readv, "readv"),
    allow(libc::SYS_writev, "writev"),                         // HTTP responses (head + body)
    allow(libc::SYS_pread64, "pread64"),
    allow(libc::SYS_pwrite64, "pwrite64"),                     // snapshot memfds
    allow(libc::SYS_close, "close"),
    allow(libc::SYS_accept4, "accept4"),                       // HTTP and peer listeners
    allow(libc::SYS_accept, "accept"),
    allow(libc::SYS_connect, "connect"),                       // peer dials (TCP)
    allow(libc::SYS_recvfrom, "recvfrom"),                     // gossip (UDP)
    allow(libc::SYS_sendto, "sendto"),
    allow(libc::SYS_recvmsg, "recvmsg"),
    allow(libc::SYS_sendmsg, "sendmsg"),
    allow(libc::SYS_shutdown, "shutdown"),
    allow(libc::SYS_getsockopt, "getsockopt"),                 // SO_ERROR after a non-blocking connect
    allow(libc::SYS_setsockopt, "setsockopt"),                 // TCP_NODELAY on accepted/dialed sockets
    allow(libc::SYS_getsockname, "getsockname"),
    allow(libc::SYS_getpeername, "getpeername"),
    allow(libc::SYS_eventfd2, "eventfd2"),                     // shard inboxes
    // ---- memory (slots, CoW snapshots, fiber stacks, JIT code, malloc) ------------------------------------------
    // W^X: no executable mmap; JIT code goes RW then mprotect(RX), never writable and executable at once.
    sys(libc::SYS_mmap, "mmap", Rule::NoBits { arg: 2, mask: PROT_EXEC, deny: Deny::Kill }),
    sys(libc::SYS_mprotect, "mprotect", Rule::NotAllBits { arg: 2, mask: PROT_WRITE | PROT_EXEC, deny: Deny::Kill }),
    allow(libc::SYS_munmap, "munmap"),
    allow(libc::SYS_madvise, "madvise"),                       // slot free / `fresh` reset (per request), THP opt-out
    allow(libc::SYS_mremap, "mremap"),                         // realloc of large buffers
    allow(libc::SYS_brk, "brk"),                               // malloc
    allow(libc::SYS_mincore, "mincore"),                       // snapshot capture (resident pages)
    allow(libc::SYS_memfd_create, "memfd_create"),             // snapshots
    allow(libc::SYS_ftruncate, "ftruncate"),                   // snapshot memfds, KV log truncation
    // ---- threads, synchronization, scheduling ----------------------------------------------------------------------
    // Threads only (apply helpers, KV compaction): no fork, no namespaces.
    sys(libc::SYS_clone, "clone", Rule::Bits { arg: 0, all: libc::CLONE_THREAD as u32, none: CLONE_NEW_ANY, deny: Deny::Kill }),
    // clone3's flags are in memory (invisible to the filter): ENOSYS makes glibc fall back to clone.
    sys(libc::SYS_clone3, "clone3", Rule::Errno(libc::ENOSYS as u16)),
    allow(libc::SYS_futex, "futex"),                           // Mutex/Condvar/park, thread join
    allow(libc::SYS_set_robust_list, "set_robust_list"),       // thread start (glibc)
    allow(libc::SYS_rseq, "rseq"),                             // thread start (glibc >= 2.35)
    allow(libc::SYS_sched_yield, "sched_yield"),
    allow(libc::SYS_sched_getaffinity, "sched_getaffinity"),
    // `--pin`: shards pin themselves (pid 0 = caller), possibly after the filter is installed.
    sys(libc::SYS_sched_setaffinity, "sched_setaffinity", Rule::OneOf { arg: 0, vals: &[0], deny: Deny::Errno(EPERM) }),
    allow(libc::SYS_gettid, "gettid"),
    allow(libc::SYS_getpid, "getpid"),
    // Thread names (PR_SET_NAME) and glibc's anonymous-mapping names (PR_SET_VMA); nothing else.
    sys(libc::SYS_prctl, "prctl", Rule::OneOf {
        arg: 0,
        vals: &[libc::PR_SET_NAME as u32, libc::PR_GET_NAME as u32, 0x5356_4d41 /* PR_SET_VMA */],
        deny: Deny::Errno(EPERM),
    }),
    // ---- time ------------------------------------------------------------------------------------------------------
    allow(libc::SYS_clock_gettime, "clock_gettime"),           // vDSO fallback, CLOCK_PROCESS_CPUTIME_ID
    allow(libc::SYS_clock_getres, "clock_getres"),
    allow(libc::SYS_gettimeofday, "gettimeofday"),
    allow(libc::SYS_clock_nanosleep, "clock_nanosleep"),       // thread::sleep (ticker, flusher, shutdown)
    allow(libc::SYS_nanosleep, "nanosleep"),
    // ---- randomness ------------------------------------------------------------------------------------------------
    allow(libc::SYS_getrandom, "getrandom"),                   // shard RNG seeds, HashMap keys, nonces
    // ---- files (Landlock confines them to the data directory) -----------------------------------------------------
    allow(libc::SYS_openat, "openat"),
    allow(libc::SYS_open, "open"),
    allow(libc::SYS_newfstatat, "newfstatat"),
    allow(libc::SYS_fstat, "fstat"),
    allow(libc::SYS_statx, "statx"),
    allow(libc::SYS_lseek, "lseek"),
    allow(libc::SYS_fsync, "fsync"),                           // persisted bundle/manifest, directory syncs
    allow(libc::SYS_fdatasync, "fdatasync"),                   // KV group commit
    allow(libc::SYS_getdents64, "getdents64"),                 // blob store / KV directory scans
    allow(libc::SYS_rename, "rename"),                         // tmp + rename
    allow(libc::SYS_renameat, "renameat"),
    allow(libc::SYS_renameat2, "renameat2"),
    allow(libc::SYS_unlink, "unlink"),                         // blob GC, temp files
    allow(libc::SYS_unlinkat, "unlinkat"),
    allow(libc::SYS_mkdir, "mkdir"),
    allow(libc::SYS_mkdirat, "mkdirat"),
    allow(libc::SYS_fcntl, "fcntl"),
    allow(libc::SYS_readlink, "readlink"),                     // current_exe (panic backtraces)
    // Only FIONBIO/FIOCLEX (std's set_nonblocking / cloexec); no terminal or device ioctls.
    sys(libc::SYS_ioctl, "ioctl", Rule::OneOf {
        arg: 1,
        vals: &[libc::FIONBIO as u32, libc::FIOCLEX as u32],
        deny: Deny::Errno(libc::ENOTTY as u16),
    }),
    // ---- signals and exit -----------------------------------------------------------------------------------------
    allow(libc::SYS_rt_sigreturn, "rt_sigreturn"),
    allow(libc::SYS_rt_sigprocmask, "rt_sigprocmask"),         // pthread_create, sigwait set-up
    allow(libc::SYS_rt_sigaction, "rt_sigaction"),             // abort()
    allow(libc::SYS_rt_sigtimedwait, "rt_sigtimedwait"),       // main thread: sigwait(SIGINT|SIGTERM)
    allow(libc::SYS_sigaltstack, "sigaltstack"),               // std's stack-overflow handler, per thread
    allow(libc::SYS_restart_syscall, "restart_syscall"),
    // abort() → raise() → tgkill(own pid, own tid, SIGABRT); never another process.
    sys(libc::SYS_tgkill, "tgkill", Rule::OwnPid { arg: 0, deny: Deny::Kill }),
    allow(libc::SYS_exit, "exit"),                             // thread exit
    allow(libc::SYS_exit_group, "exit_group"),
    // ---- process information ----------------------------------------------------------------------------------------
    // getrlimit only (glibc reads RLIMIT_STACK for the main thread's stack bounds); no limit is ever changed.
    sys(libc::SYS_prlimit64, "prlimit64", Rule::Null { arg: 2, deny: Deny::Errno(EPERM) }),
    allow(libc::SYS_getrlimit, "getrlimit"),
    allow(libc::SYS_sysinfo, "sysinfo"),                       // sysconf(_SC_PHYS_PAGES)
    // ---- sockets: IPv4/IPv6 only (no netlink, packet, unix, alg, vsock ...) ---------------------------------------
    sys(libc::SYS_socket, "socket", Rule::OneOf {
        arg: 0,
        vals: &[libc::AF_INET as u32, libc::AF_INET6 as u32],
        deny: Deny::Errno(libc::EAFNOSUPPORT as u16),
    }),
];

// ---- classic BPF ----------------------------------------------------------------------------------------------------

const AUDIT_ARCH_X86_64: u32 = 0xC000_003E;
const X32_SYSCALL_BIT: u32 = 0x4000_0000;
const OFF_NR: u32 = 0;
const OFF_ARCH: u32 = 4;
const fn off_arg_lo(i: u8) -> u32 {
    16 + 8 * i as u32
}
const fn off_arg_hi(i: u8) -> u32 {
    20 + 8 * i as u32
}

const BPF_LD_W_ABS: u16 = 0x20; // BPF_LD | BPF_W | BPF_ABS
const BPF_JEQ_K: u16 = 0x15; // BPF_JMP | BPF_JEQ | BPF_K
const BPF_JSET_K: u16 = 0x45; // BPF_JMP | BPF_JSET | BPF_K
const BPF_JA: u16 = 0x05; // BPF_JMP | BPF_JA
const BPF_AND_K: u16 = 0x54; // BPF_ALU | BPF_AND | BPF_K
const BPF_RET_K: u16 = 0x06; // BPF_RET | BPF_K

const RET_KILL_PROCESS: u32 = 0x8000_0000;
const RET_ERRNO: u32 = 0x0005_0000;
const RET_LOG: u32 = 0x7ffc_0000;
const RET_ALLOW: u32 = 0x7fff_0000;

/// A jump target while assembling.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum T {
    Next,
    Allow,
    Default,
    Errno(u16),
    /// Skip to the next allowlist entry.
    Skip(usize),
}

struct Asm {
    code: Vec<(u16, T, T, u32)>,
    /// Position of each `Skip(i)` label.
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
    fn deny(d: Deny) -> T {
        match d {
            Deny::Kill => T::Default,
            Deny::Errno(e) => T::Errno(e),
        }
    }
}

/// Builds the filter for `mode` (`pid` = this process, for [`Rule::OwnPid`]):
/// ```text
///         ld   arch ; jeq AUDIT_ARCH_X86_64, 0, DEFAULT   ; i386 (int 0x80) etc.
///         ld   nr   ; jset 0x40000000, DEFAULT, 0        ; x32 ABI
///         jeq  nr_a, ALLOW, 0                            ; Rule::Allow: no argument load (cacheable)
///         jeq  nr_b, 0, skip_b ; ld args[i] ; ...        ; argument rule: its block ends in a return
/// skip_b: ... ; ja DEFAULT
/// ALLOW: ret ALLOW ; DEFAULT: ret KILL_PROCESS (Log: LOG) ; ERRNO_e: ret ERRNO|e
/// ```
pub fn build_filter(mode: Mode, pid: u32) -> Vec<libc::sock_filter> {
    let default = match mode {
        Mode::Log => RET_LOG,
        _ => RET_KILL_PROCESS,
    };
    let mut a = Asm { code: Vec::new(), skips: Vec::new() };
    a.ld(OFF_ARCH);
    a.jeq(AUDIT_ARCH_X86_64, T::Next, T::Default);
    a.ld(OFF_NR);
    a.jset(X32_SYSCALL_BIT, T::Default, T::Next);
    // Plain allows first: the common case on kernels without the action cache.
    for s in SYSCALLS.iter().filter(|s| s.rule == Rule::Allow) {
        a.jeq(s.nr as u32, T::Allow, T::Next);
    }
    for (i, s) in SYSCALLS.iter().enumerate().filter(|(_, s)| s.rule != Rule::Allow) {
        let nr = s.nr as u32;
        match s.rule {
            Rule::Allow => unreachable!(),
            Rule::Errno(e) => a.jeq(nr, T::Errno(e), T::Next),
            Rule::NoBits { arg, mask, deny } => {
                a.jeq(nr, T::Next, T::Skip(i));
                a.ld(off_arg_lo(arg));
                a.jset(mask, Asm::deny(deny), T::Allow);
            }
            Rule::NotAllBits { arg, mask, deny } => {
                a.jeq(nr, T::Next, T::Skip(i));
                a.ld(off_arg_lo(arg));
                a.and(mask);
                a.jeq(mask, Asm::deny(deny), T::Allow);
            }
            Rule::Bits { arg, all, none, deny } => {
                a.jeq(nr, T::Next, T::Skip(i));
                a.ld(off_arg_lo(arg));
                a.jset(none, Asm::deny(deny), T::Next);
                a.and(all);
                a.jeq(all, T::Allow, Asm::deny(deny));
            }
            Rule::OneOf { arg, vals, deny } => {
                a.jeq(nr, T::Next, T::Skip(i));
                a.ld(off_arg_lo(arg));
                for &v in vals {
                    a.jeq(v, T::Allow, T::Next);
                }
                a.ja(Asm::deny(deny));
            }
            Rule::OwnPid { arg, deny } => {
                a.jeq(nr, T::Next, T::Skip(i));
                a.ld(off_arg_lo(arg));
                a.jeq(pid, T::Allow, Asm::deny(deny));
            }
            Rule::Null { arg, deny } => {
                a.jeq(nr, T::Next, T::Skip(i));
                a.ld(off_arg_lo(arg));
                a.jeq(0, T::Next, Asm::deny(deny));
                a.ld(off_arg_hi(arg));
                a.jeq(0, T::Allow, Asm::deny(deny));
            }
        }
        let p = a.code.len();
        a.skips.push((i, p));
    }
    a.ja(T::Default);
    // Return blocks.
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
    let default_at = allow_at + 1;
    let errno_at = |e: u16| default_at + 1 + errnos.iter().position(|&x| x == e).unwrap();
    let resolve = |pc: usize, t: T| -> usize {
        match t {
            T::Next => pc + 1,
            T::Allow => allow_at,
            T::Default => default_at,
            T::Errno(e) => errno_at(e),
            T::Skip(i) => a.skips.iter().find(|&&(j, _)| j == i).unwrap().1,
        }
    };
    let mut out = Vec::with_capacity(a.code.len() + 2 + errnos.len());
    for (pc, &(code, jt, jf, k)) in a.code.iter().enumerate() {
        if code == BPF_JA {
            let off = resolve(pc, jt) - (pc + 1);
            out.push(libc::sock_filter { code, jt: 0, jf: 0, k: off as u32 });
            continue;
        }
        let rel = |t: T| -> u8 {
            let d = resolve(pc, t) - (pc + 1);
            assert!(d <= 255, "seccomp filter: jump too far ({d}) — reorder SYSCALLS or use BPF_JA");
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

/// Installs `prog` on every thread (TSYNC); syscalls only (fork-safe). Needs `PR_SET_NO_NEW_PRIVS`.
pub fn install_filter(prog: &[libc::sock_filter], log_violations: bool) -> io::Result<()> {
    let fprog = libc::sock_fprog { len: prog.len() as u16, filter: prog.as_ptr() as *mut libc::sock_filter };
    let mut flags = libc::SECCOMP_FILTER_FLAG_TSYNC;
    if log_violations {
        flags |= libc::SECCOMP_FILTER_FLAG_LOG; // also log the errno-returning rules
    }
    // SAFETY: fprog points to `prog`, which outlives the call (the kernel copies it).
    let r = unsafe { libc::syscall(libc::SYS_seccomp, libc::SECCOMP_SET_MODE_FILTER, flags, &fprog as *const libc::sock_fprog) };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    if r > 0 {
        return Err(io::Error::other(format!("seccomp: thread {r} could not be synchronized")));
    }
    Ok(())
}

/// Installs [`SYSCALLS`] on every current and future thread.
pub fn seccomp(mode: Mode) -> io::Result<()> {
    if mode == Mode::Off {
        return Ok(());
    }
    // READ_IMPLIES_EXEC would make readable mappings executable behind the W^X rules.
    // SAFETY: personality(0xffffffff) only queries.
    let pers = unsafe { libc::personality(0xffff_ffff) };
    if pers >= 0 && pers & 0x0040_0000 != 0 {
        return Err(io::Error::other("READ_IMPLIES_EXEC personality is set: refusing to run the JIT under it"));
    }
    // SAFETY: getpid has no preconditions.
    let prog = build_filter(mode, unsafe { libc::getpid() } as u32);
    install_filter(&prog, mode == Mode::Log)
}

// ------------------------------------------------------------------------------------------------------------------
// The daemon's sequence
// ------------------------------------------------------------------------------------------------------------------

/// State carried from [`before_threads`] to [`after_start`].
pub struct Setup {
    mode: Mode,
    user: Option<User>,
    landlock: Option<LandlockStatus>,
}

/// Main thread, before any thread exists: `--user` fs identity and Landlock (`On` only). Creates the data
/// directory first (a Landlock rule needs an existing path).
pub fn before_threads(mode: Mode, user: Option<User>, policy: &Policy) -> Result<Setup, String> {
    if let Some(u) = &user {
        u.assume_fs_identity(policy.data_dir.as_deref())?;
    } else if let Some(d) = &policy.data_dir {
        std::fs::create_dir_all(d).map_err(|e| format!("cannot create {}: {e}", d.display()))?;
    }
    for d in &policy.write_dirs {
        let _ = std::fs::create_dir_all(d); // best effort (K10_JIT_DUMP)
    }
    let landlock = match mode {
        Mode::On => Some(landlock(policy).map_err(|e| format!("landlock: {e} (use --sandbox off to run without it)"))?),
        _ => None,
    };
    Ok(Setup { mode, user, landlock })
}

/// Once everything is bound and running: drop to `--user`, seccomp on all threads. Returns the log line.
pub fn after_start(s: Setup) -> Result<String, String> {
    if let Some(u) = &s.user {
        u.drop_privileges()?;
    }
    // SAFETY: geteuid has no preconditions (asked before the filter, which does not allow it).
    let root = unsafe { libc::geteuid() } == 0;
    seccomp(s.mode).map_err(|e| format!("seccomp: {e} (use --sandbox off to run without it)"))?;
    let mut line = format!("k10s: sandbox {}", s.mode.name());
    match s.mode {
        Mode::Off => line.push_str(": nothing restricted"),
        Mode::On | Mode::Log => {
            line.push_str(": no_new_privs, inherited fds closed");
            match &s.landlock {
                Some(LandlockStatus::Applied { abi, net, scoped }) => {
                    line.push_str(&format!(
                        "; landlock ABI {abi}: files = data dir rw + /proc/self ro, tcp bind {}{}",
                        if *net { "limited to the node's ports" } else { "not restricted (needs ABI 4 / Linux 6.7, and fixed ports)" },
                        if *scoped { ", signals scoped" } else { "" }
                    ));
                }
                Some(LandlockStatus::Unsupported(why)) => line.push_str(&format!("; landlock {why}: files NOT restricted")),
                None => line.push_str("; landlock skipped (no permissive mode)"),
            }
            let n = SYSCALLS.len();
            line.push_str(&match s.mode {
                Mode::On => format!("; seccomp: {n} syscalls allowed on every thread, anything else kills the process"),
                _ => format!("; seccomp: {n} syscalls allowed, violations logged (audit type=1326) not enforced"),
            });
        }
    }
    match &s.user {
        Some(u) => line.push_str(&format!("; running as {} ({}:{})", u.name, u.uid, u.gid)),
        None if root => line.push_str("; WARNING: running as root (use --user USER)"),
        None => {}
    }
    Ok(line)
}

// ------------------------------------------------------------------------------------------------------------------
// Tests
// ------------------------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// A classic-BPF interpreter for the subset `build_filter` emits (the kernel's semantics).
    fn run(prog: &[libc::sock_filter], nr: u32, arch: u32, args: [u64; 6]) -> u32 {
        let mut data = [0u8; 64];
        data[0..4].copy_from_slice(&nr.to_ne_bytes());
        data[4..8].copy_from_slice(&arch.to_ne_bytes());
        for (i, a) in args.iter().enumerate() {
            data[16 + 8 * i..24 + 8 * i].copy_from_slice(&a.to_ne_bytes());
        }
        let mut acc = 0u32;
        let mut pc = 0usize;
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
        let mut reg = 0u32;
        let mut pc = 0usize;
        while pc < prog.len() {
            let f = &prog[pc];
            match f.code {
                BPF_LD_W_ABS => match f.k {
                    OFF_NR => reg = nr,
                    OFF_ARCH => reg = AUDIT_ARCH_X86_64,
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

    const X64: u32 = AUDIT_ARCH_X86_64;

    #[test]
    fn table_has_no_duplicates_and_known_dangerous_syscalls_are_absent() {
        let mut nrs: Vec<i64> = SYSCALLS.iter().map(|s| s.nr).collect();
        nrs.sort();
        let n = nrs.len();
        nrs.dedup();
        assert_eq!(n, nrs.len(), "duplicate syscall in SYSCALLS");
        for (nr, name) in [
            (libc::SYS_execve, "execve"),
            (libc::SYS_execveat, "execveat"),
            (libc::SYS_fork, "fork"),
            (libc::SYS_vfork, "vfork"),
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
            (libc::SYS_kexec_file_load, "kexec_file_load"),
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
            (libc::SYS_tkill, "tkill"),
            (libc::SYS_setuid, "setuid"),
            (libc::SYS_setresuid, "setresuid"),
            (libc::SYS_seccomp, "seccomp"),
            (libc::SYS_pkey_mprotect, "pkey_mprotect"),
            (libc::SYS_modify_ldt, "modify_ldt"),
            (libc::SYS_iopl, "iopl"),
            (libc::SYS_ioperm, "ioperm"),
            (libc::SYS_name_to_handle_at, "name_to_handle_at"),
            (libc::SYS_open_by_handle_at, "open_by_handle_at"),
            (libc::SYS_landlock_create_ruleset, "landlock_create_ruleset"),
        ] {
            assert!(!nrs.contains(&nr), "{name} must not be allowed");
        }
    }

    #[test]
    fn filter_semantics() {
        let pid = 4242;
        for mode in [Mode::On, Mode::Log] {
            let deny = if mode == Mode::On { RET_KILL_PROCESS } else { RET_LOG };
            let p = build_filter(mode, pid);
            let r = |nr: libc::c_long, args: [u64; 6]| run(&p, nr as u32, X64, args);
            let z = [0u64; 6];
            // Every plain entry is allowed, whatever the arguments.
            for s in SYSCALLS.iter().filter(|s| s.rule == Rule::Allow) {
                assert_eq!(run(&p, s.nr as u32, X64, [u64::MAX; 6]), RET_ALLOW, "{}", s.name);
                assert_eq!(run(&p, s.nr as u32, X64, z), RET_ALLOW, "{}", s.name);
            }
            // Not listed: default action.
            for nr in [libc::SYS_execve, libc::SYS_ptrace, libc::SYS_bind, libc::SYS_getppid, libc::SYS_io_uring_setup, 999] {
                assert_eq!(r(nr, z), deny, "nr {nr}");
            }
            // Wrong architecture (i386 via int 0x80) and x32 numbers.
            assert_eq!(run(&p, libc::SYS_read as u32, 0x4000_0003 /* AUDIT_ARCH_I386 */, z), deny);
            assert_eq!(run(&p, X32_SYSCALL_BIT | libc::SYS_read as u32, X64, z), deny);
            // W^X.
            let rw = (libc::PROT_READ | libc::PROT_WRITE) as u64;
            let rx = (libc::PROT_READ | libc::PROT_EXEC) as u64;
            let rwx = (libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC) as u64;
            assert_eq!(r(libc::SYS_mmap, [0, 4096, rw, 0x22, u64::MAX, 0]), RET_ALLOW);
            assert_eq!(r(libc::SYS_mmap, [0, 4096, rx, 0x22, u64::MAX, 0]), deny);
            assert_eq!(r(libc::SYS_mmap, [0, 4096, rwx, 0x22, u64::MAX, 0]), deny);
            assert_eq!(r(libc::SYS_mprotect, [0, 4096, rx, 0, 0, 0]), RET_ALLOW);
            assert_eq!(r(libc::SYS_mprotect, [0, 4096, rw, 0, 0, 0]), RET_ALLOW);
            assert_eq!(r(libc::SYS_mprotect, [0, 4096, 0, 0, 0, 0]), RET_ALLOW);
            assert_eq!(r(libc::SYS_mprotect, [0, 4096, rwx, 0, 0, 0]), deny);
            assert_eq!(r(libc::SYS_mprotect, [0, 4096, (libc::PROT_WRITE | libc::PROT_EXEC) as u64, 0, 0, 0]), deny);
            // Threads only.
            let thread = (libc::CLONE_VM
                | libc::CLONE_FS
                | libc::CLONE_FILES
                | libc::CLONE_SIGHAND
                | libc::CLONE_THREAD
                | libc::CLONE_SYSVSEM
                | libc::CLONE_SETTLS
                | libc::CLONE_PARENT_SETTID
                | libc::CLONE_CHILD_CLEARTID) as u64;
            assert_eq!(r(libc::SYS_clone, [thread, 0, 0, 0, 0, 0]), RET_ALLOW);
            assert_eq!(r(libc::SYS_clone, [libc::SIGCHLD as u64, 0, 0, 0, 0, 0]), deny, "fork-like clone");
            assert_eq!(r(libc::SYS_clone, [thread | libc::CLONE_NEWUSER as u64, 0, 0, 0, 0, 0]), deny);
            assert_eq!(r(libc::SYS_clone, [thread | libc::CLONE_NEWNET as u64, 0, 0, 0, 0, 0]), deny);
            assert_eq!(r(libc::SYS_clone3, z), RET_ERRNO | libc::ENOSYS as u32);
            // Sockets.
            assert_eq!(r(libc::SYS_socket, [libc::AF_INET as u64, 1, 0, 0, 0, 0]), RET_ALLOW);
            assert_eq!(r(libc::SYS_socket, [libc::AF_INET6 as u64, 2, 0, 0, 0, 0]), RET_ALLOW);
            for fam in [libc::AF_UNIX, libc::AF_NETLINK, libc::AF_PACKET, libc::AF_ALG, libc::AF_VSOCK] {
                assert_eq!(r(libc::SYS_socket, [fam as u64, 1, 0, 0, 0, 0]), RET_ERRNO | libc::EAFNOSUPPORT as u32);
            }
            // prctl / ioctl / sched_setaffinity / prlimit64 / tgkill.
            assert_eq!(r(libc::SYS_prctl, [libc::PR_SET_NAME as u64, 0, 0, 0, 0, 0]), RET_ALLOW);
            assert_eq!(r(libc::SYS_prctl, [libc::PR_SET_SECCOMP as u64, 0, 0, 0, 0, 0]), RET_ERRNO | libc::EPERM as u32);
            assert_eq!(r(libc::SYS_prctl, [libc::PR_SET_DUMPABLE as u64, 1, 0, 0, 0, 0]), RET_ERRNO | libc::EPERM as u32);
            assert_eq!(r(libc::SYS_ioctl, [3, libc::FIONBIO, 0, 0, 0, 0]), RET_ALLOW);
            assert_eq!(r(libc::SYS_ioctl, [0, libc::TIOCSTI, 0, 0, 0, 0]), RET_ERRNO | libc::ENOTTY as u32);
            assert_eq!(r(libc::SYS_sched_setaffinity, z), RET_ALLOW);
            assert_eq!(r(libc::SYS_sched_setaffinity, [1, 0, 0, 0, 0, 0]), RET_ERRNO | libc::EPERM as u32);
            assert_eq!(r(libc::SYS_prlimit64, [0, 3, 0, 0x1000, 0, 0]), RET_ALLOW);
            assert_eq!(r(libc::SYS_prlimit64, [0, 3, 0x1000, 0, 0, 0]), RET_ERRNO | libc::EPERM as u32);
            assert_eq!(r(libc::SYS_prlimit64, [0, 3, 0x1_0000_0000, 0, 0, 0]), RET_ERRNO | libc::EPERM as u32, "high word");
            assert_eq!(r(libc::SYS_tgkill, [pid as u64, 77, 6, 0, 0, 0]), RET_ALLOW);
            assert_eq!(r(libc::SYS_tgkill, [1, 1, 9, 0, 0, 0]), deny);
        }
    }

    #[test]
    fn plain_allows_are_served_by_the_kernel_action_cache() {
        let p = build_filter(Mode::On, 1);
        for s in SYSCALLS {
            assert_eq!(const_allow(&p, s.nr as u32), s.rule == Rule::Allow, "{}", s.name);
        }
        assert!(!const_allow(&p, libc::SYS_execve as u32));
        // The hot path in particular.
        for nr in [
            libc::SYS_epoll_wait,
            libc::SYS_read,
            libc::SYS_write,
            libc::SYS_writev,
            libc::SYS_accept4,
            libc::SYS_recvfrom,
            libc::SYS_sendto,
            libc::SYS_futex,
            libc::SYS_sched_yield,
            libc::SYS_clock_gettime,
            libc::SYS_madvise,
        ] {
            assert!(const_allow(&p, nr as u32), "nr {nr}");
        }
    }

    // ---- real kernel: forked children ---------------------------------------------------------------------------

    /// Runs `f` in a forked child (syscalls only in there) and returns its wait status.
    fn in_child(f: impl FnOnce() -> i32) -> i32 {
        // SAFETY: the child only makes raw syscalls and leaves with _exit; the parent waits for it.
        unsafe {
            let pid = libc::fork();
            assert!(pid >= 0);
            if pid == 0 {
                let code = f();
                libc::_exit(code);
            }
            let mut st = 0;
            assert_eq!(libc::waitpid(pid, &mut st, 0), pid);
            st
        }
    }

    fn killed_by_sigsys(st: i32) -> bool {
        libc::WIFSIGNALED(st) && libc::WTERMSIG(st) == libc::SIGSYS
    }

    fn exited(st: i32, code: i32) -> bool {
        libc::WIFEXITED(st) && libc::WEXITSTATUS(st) == code
    }

    fn sandboxed_child(mode: Mode, f: impl FnOnce() -> i32) -> i32 {
        // SAFETY: getpid has no preconditions (OwnPid gets the parent's pid: unused by these tests).
        let prog = build_filter(mode, unsafe { libc::getpid() } as u32);
        in_child(move || {
            // SAFETY: plain prctl.
            if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
                return 90;
            }
            if install_filter(&prog, false).is_err() {
                return 91;
            }
            f()
        })
    }

    #[test]
    fn denied_syscall_kills_the_process() {
        // An allowed syscall works; a denied one kills with SIGSYS before it runs.
        let st = sandboxed_child(Mode::On, || {
            // SAFETY: raw syscalls.
            unsafe {
                if libc::syscall(libc::SYS_getpid) <= 0 {
                    return 1;
                }
                libc::syscall(libc::SYS_ptrace, libc::PTRACE_TRACEME, 0, 0, 0);
            }
            0
        });
        assert!(killed_by_sigsys(st), "status {st:#x}");
        // execve
        let st = sandboxed_child(Mode::On, || {
            let argv = [c"/bin/true".as_ptr(), std::ptr::null()];
            // SAFETY: valid argv/envp.
            unsafe { libc::syscall(libc::SYS_execve, c"/bin/true".as_ptr(), argv.as_ptr(), argv[1..].as_ptr()) };
            0
        });
        assert!(killed_by_sigsys(st), "status {st:#x}");
        // x32 syscall number
        let st = sandboxed_child(Mode::On, || {
            // SAFETY: raw syscall.
            unsafe { libc::syscall(X32_SYSCALL_BIT as libc::c_long | libc::SYS_getpid) };
            0
        });
        assert!(killed_by_sigsys(st), "status {st:#x}");
        // i386 entry point (int 0x80, eax = 20 = getpid): SIGSYS, or SIGSEGV if the kernel lacks IA32 emulation.
        let st = sandboxed_child(Mode::On, || {
            // SAFETY: int 0x80 with a harmless syscall number; only eax is clobbered.
            unsafe { std::arch::asm!("int 0x80", inout("eax") 20u32 => _) };
            0
        });
        assert!(killed_by_sigsys(st) || (libc::WIFSIGNALED(st) && libc::WTERMSIG(st) == libc::SIGSEGV), "status {st:#x}");
    }

    #[test]
    fn wx_mappings_are_refused() {
        let rw = libc::PROT_READ | libc::PROT_WRITE;
        let anon = libc::MAP_PRIVATE | libc::MAP_ANONYMOUS;
        // RW map, write, then RX: the JIT's sequence works.
        let st = sandboxed_child(Mode::On, move || {
            // SAFETY: raw mmap/mprotect on a fresh private mapping.
            unsafe {
                let p = libc::mmap(std::ptr::null_mut(), 4096, rw, anon, -1, 0);
                if p == libc::MAP_FAILED {
                    return 1;
                }
                *(p as *mut u8) = 0xC3; // ret
                if libc::mprotect(p, 4096, libc::PROT_READ | libc::PROT_EXEC) != 0 {
                    return 2;
                }
                let f: extern "C" fn() = std::mem::transmute(p);
                f();
            }
            0
        });
        assert!(exited(st, 0), "status {st:#x}");
        for (what, map_prot, protect) in [
            ("mmap rwx", rw | libc::PROT_EXEC, None),
            ("mmap rx", libc::PROT_READ | libc::PROT_EXEC, None),
            ("mprotect rwx", rw, Some(rw | libc::PROT_EXEC)),
        ] {
            let st = sandboxed_child(Mode::On, move || {
                // SAFETY: raw mmap/mprotect on a fresh private mapping.
                unsafe {
                    let p = libc::mmap(std::ptr::null_mut(), 4096, map_prot, anon, -1, 0);
                    if let Some(prot) = protect {
                        libc::mprotect(p, 4096, prot);
                    }
                }
                0
            });
            assert!(killed_by_sigsys(st), "{what}: status {st:#x}");
        }
    }

    #[test]
    fn steering_rules_return_errors() {
        let st = sandboxed_child(Mode::On, || {
            // SAFETY: raw syscalls with harmless arguments.
            unsafe {
                if libc::syscall(libc::SYS_clone3, std::ptr::null::<u8>(), 0usize) != -1
                    || *libc::__errno_location() != libc::ENOSYS
                {
                    return 1;
                }
                if libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) != -1 || *libc::__errno_location() != libc::EAFNOSUPPORT {
                    return 2;
                }
                let s = libc::socket(libc::AF_INET, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
                if s < 0 {
                    return 3;
                }
                libc::close(s);
                if libc::prctl(libc::PR_SET_DUMPABLE, 1, 0, 0, 0) != -1 || *libc::__errno_location() != libc::EPERM {
                    return 4;
                }
                // A thread can still be created (clone with CLONE_THREAD after the clone3 fallback).
                extern "C" fn body(_: *mut libc::c_void) -> *mut libc::c_void {
                    std::ptr::null_mut()
                }
                let mut t: libc::pthread_t = 0;
                if libc::pthread_create(&mut t, std::ptr::null(), body, std::ptr::null_mut()) != 0 {
                    return 5;
                }
                libc::pthread_join(t, std::ptr::null_mut());
            }
            0
        });
        assert!(exited(st, 0), "status {st:#x}");
    }

    #[test]
    fn log_mode_does_not_kill() {
        let st = sandboxed_child(Mode::Log, || {
            // SAFETY: getppid is harmless; it is not on the allowlist.
            let r = unsafe { libc::syscall(libc::SYS_getppid) };
            if r > 0 {
                0
            } else {
                1
            }
        });
        assert!(exited(st, 0), "status {st:#x}");
    }

    #[test]
    fn landlock_confines_the_filesystem() {
        if landlock_abi() == 0 {
            eprintln!("landlock not supported by this kernel: skipped");
            return;
        }
        let dir = std::env::temp_dir().join(format!("k10s-landlock-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let outside = std::env::temp_dir().join(format!("k10s-landlock-outside-{}", std::process::id()));
        std::fs::write(&outside, b"secret").unwrap();
        let prep = Prepared::new(&Policy { data_dir: Some(dir.clone()), ..Policy::default() });
        let inside_new = CString::new(dir.join("new").as_os_str().as_bytes()).unwrap();
        let sub = CString::new(dir.join("sub").as_os_str().as_bytes()).unwrap();
        let out_c = CString::new(outside.as_os_str().as_bytes()).unwrap();
        let st = in_child(move || {
            // SAFETY: plain syscalls on valid C strings.
            unsafe {
                if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                    return 90;
                }
                if !matches!(prep.apply(), Ok(LandlockStatus::Applied { .. })) {
                    return 91;
                }
                let fd = libc::open(inside_new.as_ptr(), libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC, 0o600);
                if fd < 0 {
                    return 1;
                }
                libc::close(fd);
                if libc::mkdir(sub.as_ptr(), 0o700) != 0 {
                    return 2;
                }
                if libc::open(out_c.as_ptr(), libc::O_RDONLY) >= 0 || *libc::__errno_location() != libc::EACCES {
                    return 3;
                }
                if libc::open(c"/etc/passwd".as_ptr(), libc::O_RDONLY) >= 0 {
                    return 4;
                }
                // Our own /proc entries stay readable.
                let fd = libc::open(c"/proc/self/statm".as_ptr(), libc::O_RDONLY);
                if fd < 0 {
                    return 5;
                }
                libc::close(fd);
                // Nothing can be executed, even from the data directory.
                let argv = [c"/bin/true".as_ptr(), std::ptr::null()];
                libc::execve(c"/bin/true".as_ptr(), argv.as_ptr(), argv[1..].as_ptr());
                if *libc::__errno_location() != libc::EACCES {
                    return 6;
                }
            }
            0
        });
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(&outside);
        assert!(exited(st, 0), "status {st:#x}");
    }

    #[test]
    fn user_spec_parsing() {
        assert!(User::resolve("").is_err());
        assert!(User::resolve("0:0").is_err(), "root is refused");
        assert!(User::resolve("1234").is_err(), "numeric uid needs a group");
        assert_eq!(User::resolve("1234:5678").unwrap(), User { name: "1234".into(), uid: 1234, gid: 5678 });
        assert!(User::resolve("no-such-user-k10s").is_err());
        if let Some((uid, gid)) = getpw("nobody") {
            let u = User::resolve("nobody").unwrap();
            assert_eq!((u.uid, u.gid), (uid, gid));
        }
    }

    /// Filter cost: cached allow (getpid) and argument-checked (mmap+munmap), without/with the filter (--ignored).
    #[test]
    #[ignore]
    fn syscall_overhead() {
        // Best of 20 rounds (robust to a busy machine), ns per call.
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
        fn bench(label: &str) {
            // SAFETY: raw getpid.
            let a = best(100_000, || unsafe {
                libc::syscall(libc::SYS_getpid);
            });
            // SAFETY: fresh private mapping, unmapped at once.
            let b = best(10_000, || unsafe {
                let p = libc::mmap(std::ptr::null_mut(), 4096, 3, libc::MAP_PRIVATE | libc::MAP_ANONYMOUS, -1, 0);
                libc::munmap(p, 4096);
            });
            eprintln!("{label}: getpid {a:.1} ns, mmap+munmap {b:.1} ns");
        }
        // A filter reading an argument makes nothing cacheable: stacked under ours, both always run.
        let defeat = vec![
            libc::sock_filter { code: BPF_LD_W_ABS, jt: 0, jf: 0, k: off_arg_lo(0) },
            libc::sock_filter { code: BPF_RET_K, jt: 0, jf: 0, k: RET_ALLOW },
        ];
        for (label, progs) in [
            ("no filter", vec![]),
            ("filter (action cache)", vec![build_filter(Mode::On, 0)]),
            ("filter (cache defeated)", vec![defeat, build_filter(Mode::On, 0)]),
        ] {
            let st = in_child(move || {
                // SAFETY: plain prctl.
                unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
                for p in &progs {
                    install_filter(p, false).unwrap();
                }
                bench(label);
                0
            });
            assert!(exited(st, 0));
        }
    }
}
