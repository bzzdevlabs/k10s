//! Process sandbox of `k10s node`, entered between binding the sockets and starting the node (SECURITY.md).
#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
compile_error!("the k10s node sandbox supports Linux on x86-64 only");

mod landlock;
mod seccomp;
mod user;

pub use user::User;

use std::io;
use std::path::PathBuf;

/// `--sandbox`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Landlock and seccomp; a denied syscall kills the process (`SIGSYS`).
    On,
    /// seccomp without Landlock (kernels without it, by explicit choice).
    SeccompOnly,
    /// No Landlock; syscalls the filter would kill are logged (audit type=1326) and run. The filter's errno rules
    /// (clone3, socket families, prctl options, …) still apply: not a pure dry run.
    Log,
    /// Neither Landlock nor seccomp (`--user` still applies).
    Off,
}

impl Mode {
    pub fn parse(s: &str) -> Option<Mode> {
        match s {
            "on" => Some(Mode::On),
            "seccomp-only" => Some(Mode::SeccompOnly),
            "log" => Some(Mode::Log),
            "off" => Some(Mode::Off),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Mode::On => "on",
            Mode::SeccompOnly => "seccomp-only",
            Mode::Log => "log",
            Mode::Off => "off",
        }
    }

    /// `K10S_SANDBOX` if set, else [`Mode::On`].
    pub fn from_env() -> Result<Mode, String> {
        match std::env::var("K10S_SANDBOX") {
            Err(_) => Ok(Mode::On),
            Ok(v) => Mode::parse(v.trim()).ok_or_else(|| format!("K10S_SANDBOX=`{v}`: expected on|seccomp-only|log|off")),
        }
    }
}

/// What the node may write once sandboxed.
#[derive(Clone, Debug, Default)]
pub struct Policy {
    pub data_dir: Option<PathBuf>,
    /// Extra read-write directories (`K10_JIT_DUMP`).
    pub write_dirs: Vec<PathBuf>,
}

/// A syscall with integer arguments only.
fn sc(nr: libc::c_long, a: [libc::c_long; 5]) -> io::Result<libc::c_long> {
    // SAFETY: integer arguments, no pointer is dereferenced by the kernel.
    let r = unsafe { libc::syscall(nr, a[0], a[1], a[2], a[3], a[4]) };
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r)
    }
}

/// First thing in `k10s node`: closes inherited descriptors ≥ 3 and sets no_new_privs.
pub fn early() -> io::Result<()> {
    if sc(libc::SYS_close_range, [3, u32::MAX as _, 0, 0, 0]).is_err() {
        // Linux < 5.9: collect first (closing while iterating would close the walker's descriptor).
        let fds: Vec<i32> = std::fs::read_dir("/proc/self/fd")?
            .filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok())
            .filter(|&fd| fd > 2)
            .collect();
        for fd in fds {
            let _ = sc(libc::SYS_close, [fd as _, 0, 0, 0, 0]);
        }
    }
    sc(libc::SYS_prctl, [libc::PR_SET_NO_NEW_PRIVS as _, 1, 0, 0, 0]).map(|_| ())
}

fn threads() -> Result<usize, String> {
    std::fs::read_dir("/proc/self/task").map(|d| d.count()).map_err(|e| format!("/proc/self/task: {e}"))
}

/// `--user` needs root, and root needs `--user` (it keeps the files it owns even without capabilities); with
/// `--sandbox off` root may run as itself.
fn check_identity(mode: Mode, user: Option<&User>, euid: u32) -> Result<(), String> {
    match (user, euid) {
        (Some(u), e) if e != 0 => Err(format!("--user {}: the node must be started as root to switch users", u.name)),
        (None, 0) if mode != Mode::Off => Err("running as root: pass --user USER (or --sandbox off)".into()),
        _ => Ok(()),
    }
}

/// Between binding the sockets and starting the node, single-threaded: data directories, `--user` (groups, gid,
/// uid), capabilities, Landlock, seccomp. `--user` applies with `--sandbox off` too. Returns the start-up log line.
pub fn enter(mode: Mode, user: Option<&User>, policy: &Policy) -> Result<String, String> {
    let jit_dump = policy.write_dirs.first().map_or("-".into(), |d| d.display().to_string());
    let euid = user::euid();
    if mode == Mode::Off && user.is_none() {
        return Ok(format!("k10s: sandbox mode=off user={euid} jit_dump={jit_dump}"));
    }
    match threads()? {
        1 => {}
        n => return Err(format!("the sandbox must be entered before any thread starts ({n} threads)")),
    }
    check_identity(mode, user, euid)?;
    let dirs = policy.data_dir.iter().chain(&policy.write_dirs);
    match user {
        Some(u) => {
            for d in dirs {
                u.own_dir(d)?;
            }
            u.switch()?;
        }
        None => {
            for d in dirs {
                std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
            }
        }
    }
    user::drop_capabilities()?;
    let landlock = match mode {
        Mode::On => format!("abi{}", landlock::apply(policy)?),
        _ => "off".into(),
    };
    let seccomp = match mode {
        Mode::Off => "off",
        Mode::Log => "log",
        Mode::On | Mode::SeccompOnly => "kill",
    };
    if mode != Mode::Off {
        seccomp::install(mode)?;
    }
    let who = user.map_or_else(|| euid.to_string(), |u| format!("{}({}:{})", u.name, u.uid, u.gid));
    Ok(format!("k10s: sandbox mode={} user={who} caps=none landlock={landlock} seccomp={seccomp} jit_dump={jit_dump}", mode.name()))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Runs `f` in a forked child (syscalls and simple std calls only) and returns its wait status.
    pub fn in_child(f: impl FnOnce() -> i32) -> i32 {
        // SAFETY: the child leaves with _exit; the parent waits for it.
        unsafe {
            let pid = libc::fork();
            assert!(pid >= 0);
            if pid == 0 {
                libc::_exit(f());
            }
            let mut st = 0;
            assert_eq!(libc::waitpid(pid, &mut st, 0), pid);
            st
        }
    }

    pub fn killed_by(st: i32, sig: i32) -> bool {
        libc::WIFSIGNALED(st) && libc::WTERMSIG(st) == sig
    }

    pub fn exited(st: i32, code: i32) -> bool {
        libc::WIFEXITED(st) && libc::WEXITSTATUS(st) == code
    }

    #[test]
    fn modes() {
        for m in [Mode::On, Mode::SeccompOnly, Mode::Log, Mode::Off] {
            assert_eq!(Mode::parse(m.name()), Some(m));
        }
        assert_eq!(Mode::parse("maybe"), None);
    }

    #[test]
    fn refuses_a_started_process() {
        // The test harness runs threads: nothing may be half-applied.
        let _t = std::thread::spawn(|| std::thread::sleep(std::time::Duration::from_secs(2)));
        let e = enter(Mode::On, None, &Policy::default()).unwrap_err();
        assert!(e.contains("before any thread"), "{e}");
    }

    #[test]
    fn identity_rules() {
        let u = User { name: "k10s".into(), uid: 1000, gid: 1000 };
        for m in [Mode::On, Mode::SeccompOnly, Mode::Log] {
            assert!(check_identity(m, None, 0).unwrap_err().contains("running as root"), "{m:?}");
            assert_eq!(check_identity(m, None, 1000), Ok(()));
            assert_eq!(check_identity(m, Some(&u), 0), Ok(()));
        }
        assert_eq!(check_identity(Mode::Off, None, 0), Ok(()));
        for m in [Mode::On, Mode::Off] {
            assert!(check_identity(m, Some(&u), 1000).unwrap_err().contains("started as root"));
        }
    }

    /// `enter(mode)` in a child with a fresh data directory; `probe` runs inside the sandbox.
    fn enter_in_child(mode: Mode, tag: &str, probe: fn(&std::path::Path, &str) -> bool) -> i32 {
        let d = std::env::temp_dir().join(format!("k10s-sandbox-{tag}-{}", std::process::id()));
        let dc = d.clone();
        let st = in_child(move || {
            let r = early().map_err(|e| e.to_string()).and_then(|()| enter(mode, None, &Policy { data_dir: Some(dc.clone()), write_dirs: vec![] }));
            match r {
                Ok(line) if probe(&dc, &line) => 0,
                Ok(_) => 1,
                Err(e) if user::euid() == 0 && e.contains("running as root") => 0,
                Err(_) => 2,
            }
        });
        let _ = std::fs::remove_dir_all(&d);
        st
    }

    #[test]
    fn enters_single_threaded() {
        if landlock::abi() == 0 {
            return;
        }
        let st = enter_in_child(Mode::On, "on", |d, line| {
            line.contains("mode=on") && line.contains("caps=none") && line.contains("seccomp=kill") && std::fs::write(d.join("f"), b"x").is_ok()
        });
        assert!(exited(st, 0), "status {st:#x}");
    }

    #[test]
    fn seccomp_only_skips_landlock() {
        let st = enter_in_child(Mode::SeccompOnly, "seccomp-only", |d, line| {
            line.contains("landlock=off seccomp=kill")
                && std::fs::write(d.join("f"), b"x").is_ok()
                && std::fs::read("/etc/passwd").is_ok()
                && sc(libc::SYS_socket, [libc::AF_UNIX as _, libc::SOCK_STREAM as _, 0, 0, 0]).is_err()
        });
        assert!(exited(st, 0), "status {st:#x}");
    }
}
