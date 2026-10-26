//! Syscall tracer of the k10-lx sizing (bench/oci/trace.sh). Static (musl), no shell needed in the image.
//!
//! * `lxtrace trace OUT -- PROG ARGS...`: runs PROG under ptrace (forks, threads and execs followed) and writes the
//!   JSON summary to OUT.
//! * As PID 1 of a QEMU guest whose initramfs is an image's root file system: mounts what a container runtime
//!   provides, brings `lo` up, runs `/k10s/plan` (the traced workload plus untraced driver steps), writes the summary
//!   to /dev/ttyS1 and powers off.
//!
//! The summary counts, per program (executable path) and syscall number: calls, errnos, the flag arguments listed in
//! `spec` (raw hex, decoded by bench/oci/linuxabi.py) and the /proc, /sys, /dev and /etc paths named.

use std::collections::{BTreeMap, HashMap};
use std::ffi::CString;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const PTRACE_GET_SYSCALL_INFO: u32 = 0x420e;
const OP_ENTRY: u8 = 1;
const OP_EXIT: u8 = 2;
const AUDIT_ARCH_X86_64: u32 = 0xc000_003e;
const MAX_PATHS: usize = 256;
const MAX_KEYS: usize = 512;
const CLONE_THREAD: u64 = 0x10000;

static ALARM: AtomicBool = AtomicBool::new(false);

#[repr(C)]
#[derive(Default)]
struct SyscallInfo {
    op: u8,
    pad: [u8; 3],
    arch: u32,
    ip: u64,
    sp: u64,
    data: [u64; 8],
}

#[derive(Default)]
struct Stat {
    n: u64,
    err: BTreeMap<i64, u64>,
    args: BTreeMap<String, u64>,
    more_args: u64,
    paths: BTreeMap<String, u64>,
    more_paths: u64,
}

struct Prog {
    name: String,
    execs: u64,
    forks: u64,
    threads: u64,
    sys: BTreeMap<u64, Stat>,
    signals: BTreeMap<i32, u64>,
}

struct Pending {
    nr: u64,
    prog: usize,
    arg0: u64,
}

struct Task {
    prog: usize,
    pending: Option<Pending>,
    fresh: bool,
}

struct Tracer {
    progs: Vec<Prog>,
    by_name: HashMap<String, usize>,
    tasks: HashMap<i32, Task>,
    root: i32,
    started: bool,
    root_status: Option<i32>,
    foreign_arch: u64,
}

#[derive(Clone, Copy)]
enum Arg {
    Flag(usize),
    Path(usize),
    Word(usize),
    Family(usize),
    Fd(usize),
    Str(usize),
}

/// The arguments recorded per x86-64 syscall number (decoded by bench/oci/linuxabi.py `ARGS`).
fn spec(nr: u64) -> &'static [Arg] {
    use Arg::*;
    match nr {
        2 => &[Path(0), Flag(1)],
        4 | 6 | 76 | 80 | 82 | 83 | 84 | 85 | 86 | 87 | 89 | 90 | 92 | 94 | 132 | 137 | 161 | 194 | 195 | 235 => {
            &[Path(0)]
        }
        8 => &[Flag(2)],
        9 => &[Flag(2), Flag(3)],
        10 | 26 | 28 | 29 | 30 | 64 | 66 | 325 => &[Flag(2)],
        13 | 14 | 36 | 38 | 97 | 98 | 101 | 103 | 135 | 140 | 141 | 157 | 158 | 160 | 222 | 228 | 229 | 250 => {
            &[Flag(0)]
        }
        272 | 291 | 294 | 321 | 323 | 324 => &[Flag(0)],
        16 => &[Flag(1), Fd(0)],
        21 => &[Path(0), Flag(1)],
        25 => &[Flag(3)],
        31 | 48 | 62 | 73 | 144 | 200 | 202 | 233 | 285 | 286 | 290 | 293 | 302 | 308 | 319 | 424 | 434 => &[Flag(1)],
        41 => &[Flag(0), Flag(1), Flag(2)],
        53 => &[Flag(0), Flag(1)],
        42 | 49 => &[Family(1)],
        44 | 45 | 221 | 288 | 289 | 299 | 307 => &[Flag(3)],
        46 | 47 | 234 | 292 | 334 | 436 => &[Flag(2)],
        54 | 55 => &[Flag(1), Flag(2)],
        56 => &[Flag(0)],
        59 => &[Path(0)],
        61 => &[Flag(2)],
        72 => &[Flag(1)],
        88 => &[Path(1)],
        133 => &[Path(0)],
        165 => &[Path(1), Str(2), Flag(3)],
        166 => &[Path(0), Flag(1)],
        169 => &[Flag(2)],
        188 | 189 | 191 | 192 | 197 | 198 => &[Path(0), Str(1)],
        190 | 193 | 199 => &[Str(1)],
        230 => &[Flag(0), Flag(1)],
        247 => &[Flag(0), Flag(3)],
        254 => &[Path(1), Flag(2)],
        257 => &[Path(1), Flag(2)],
        258 | 259 | 264 | 267 | 268 => &[Path(1)],
        260 | 265 | 316 | 322 => &[Path(1), Flag(4)],
        262 | 280 | 452 => &[Path(1), Flag(3)],
        263 | 269 => &[Path(1), Flag(2)],
        266 => &[Path(2)],
        275 | 326 => &[Flag(5)],
        283 | 317 => &[Flag(0), Flag(1)],
        318 => &[Flag(2)],
        327 | 328 => &[Flag(4)],
        332 => &[Path(1), Flag(2), Flag(3)],
        435 => &[Word(0)],
        437 => &[Path(1), Word(2)],
        439 => &[Path(1), Flag(2), Flag(3)],
        _ => &[],
    }
}

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

fn read_mem(pid: i32, addr: u64, buf: &mut [u8]) -> usize {
    let local = libc::iovec { iov_base: buf.as_mut_ptr().cast(), iov_len: buf.len() };
    let remote = libc::iovec { iov_base: addr as *mut libc::c_void, iov_len: buf.len() };
    let r = unsafe { libc::process_vm_readv(pid, &local, 1, &remote, 1, 0) };
    r.max(0) as usize
}

fn read_word(pid: i32, addr: u64) -> Option<u64> {
    let mut b = [0u8; 8];
    (addr != 0 && read_mem(pid, addr, &mut b) == 8).then(|| u64::from_ne_bytes(b))
}

fn read_str(pid: i32, mut addr: u64) -> Option<String> {
    if addr == 0 {
        return None;
    }
    let mut out = Vec::new();
    while out.len() < 4096 {
        let n = (4096 - (addr & 4095)) as usize;
        let mut buf = vec![0u8; n.min(4096 - out.len())];
        let got = read_mem(pid, addr, &mut buf);
        if got == 0 {
            return None;
        }
        if let Some(z) = buf[..got].iter().position(|&c| c == 0) {
            out.extend_from_slice(&buf[..z]);
            return Some(String::from_utf8_lossy(&out).into_owned());
        }
        out.extend_from_slice(&buf[..got]);
        addr += got as u64;
    }
    None
}

/// /proc, /sys, /dev and /etc paths with pids, fds and long numbers folded to `N`; other paths are not kept.
fn norm_path(p: &str) -> Option<String> {
    let special = ["/proc", "/sys", "/dev", "/etc"].iter().any(|r| p == *r || p.starts_with(&format!("{r}/")));
    if !special {
        return None;
    }
    let mut out = String::with_capacity(p.len());
    for (i, c) in p.split('/').enumerate() {
        if i > 0 {
            out.push('/');
        }
        if !c.is_empty() && c.bytes().all(|b| b.is_ascii_digit()) {
            out.push('N');
            continue;
        }
        let mut run = String::new();
        for ch in c.chars().chain(std::iter::once('/')) {
            if ch.is_ascii_digit() {
                run.push(ch);
                continue;
            }
            out.push_str(if run.len() >= 4 { "N" } else { &run });
            run.clear();
            if ch != '/' {
                out.push(ch);
            }
        }
    }
    Some(out)
}

fn fd_kind(pid: i32, fd: u64) -> String {
    let l = std::fs::read_link(format!("/proc/{pid}/fd/{fd}"))
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    if l.is_empty() {
        "bad".into()
    } else if l.starts_with("socket:") {
        "socket".into()
    } else if l.starts_with("pipe:") {
        "pipe".into()
    } else if let Some(a) = l.strip_prefix("anon_inode:") {
        a.trim_matches(|c| c == '[' || c == ']').to_string()
    } else if l.starts_with("/dev/pts/") || l.starts_with("/dev/tty") || l == "/dev/console" || l == "/dev/ptmx" {
        "tty".into()
    } else if l.starts_with("/dev/") {
        l
    } else {
        "file".into()
    }
}

fn exe_of(pid: i32) -> String {
    let p = std::fs::read_link(format!("/proc/{pid}/exe")).map(|p| p.to_string_lossy().into_owned());
    let p = p.unwrap_or_else(|_| "?".into());
    p.strip_suffix(" (deleted)").map(str::to_string).unwrap_or(p)
}

impl Tracer {
    fn new(root: i32) -> Tracer {
        let mut t = Tracer {
            progs: Vec::new(),
            by_name: HashMap::new(),
            tasks: HashMap::new(),
            root,
            started: false,
            root_status: None,
            foreign_arch: 0,
        };
        let p = t.prog("(runner)");
        t.tasks.insert(root, Task { prog: p, pending: None, fresh: false });
        t
    }

    fn prog(&mut self, name: &str) -> usize {
        if let Some(&i) = self.by_name.get(name) {
            return i;
        }
        self.progs.push(Prog {
            name: name.to_string(),
            execs: 0,
            forks: 0,
            threads: 0,
            sys: BTreeMap::new(),
            signals: BTreeMap::new(),
        });
        self.by_name.insert(name.to_string(), self.progs.len() - 1);
        self.progs.len() - 1
    }

    fn task(&mut self, pid: i32) -> &mut Task {
        if !self.tasks.contains_key(&pid) {
            let p = self.prog(&exe_of(pid));
            self.tasks.insert(pid, Task { prog: p, pending: None, fresh: true });
        }
        self.tasks.get_mut(&pid).unwrap()
    }

    fn entry(&mut self, pid: i32, nr: u64, args: &[u64; 6]) {
        let prog = self.task(pid).prog;
        let mut key = String::new();
        let mut path = None;
        let mut arg0 = args[0];
        for a in spec(nr) {
            let part = match *a {
                Arg::Flag(i) if nr == 72 && i == 1 && matches!(args[1], 2 | 4 | 1033) => {
                    format!("a1={:#x} a2={:#x}", args[1], args[2])
                }
                Arg::Flag(i) => format!("a{i}={:#x}", args[i]),
                Arg::Word(i) => {
                    let w = read_word(pid, args[i]).unwrap_or(u64::MAX);
                    if nr == 435 {
                        arg0 = w;
                    }
                    format!("a{i}.0={w:#x}")
                }
                Arg::Family(i) => {
                    let mut b = [0u8; 2];
                    let f = if args[i] != 0 && read_mem(pid, args[i], &mut b) == 2 {
                        u16::from_ne_bytes(b) as i64
                    } else {
                        -1
                    };
                    format!("a{i}.family={f}")
                }
                Arg::Fd(i) => format!("fd={}", fd_kind(pid, args[i])),
                Arg::Str(i) => format!("a{i}={}", read_str(pid, args[i]).unwrap_or_default()),
                Arg::Path(i) => {
                    path = read_str(pid, args[i]);
                    continue;
                }
            };
            if !key.is_empty() {
                key.push(' ');
            }
            key.push_str(&part);
        }
        let st = self.progs[prog].sys.entry(nr).or_default();
        st.n += 1;
        if !key.is_empty() {
            if st.args.len() < MAX_KEYS || st.args.contains_key(&key) {
                *st.args.entry(key).or_insert(0) += 1;
            } else {
                st.more_args += 1;
            }
        }
        if let Some(p) = path {
            match norm_path(&p) {
                Some(p) if st.paths.len() < MAX_PATHS || st.paths.contains_key(&p) => {
                    *st.paths.entry(p).or_insert(0) += 1
                }
                Some(_) => st.more_paths += 1,
                None => {}
            }
        }
        self.task(pid).pending = Some(Pending { nr, prog, arg0 });
    }

    fn exit(&mut self, pid: i32, rval: i64, is_error: bool) {
        let Some(p) = self.task(pid).pending.take() else { return };
        if is_error && p.nr != 15 {
            let st = self.progs[p.prog].sys.entry(p.nr).or_default();
            *st.err.entry(-rval).or_insert(0) += 1;
        }
    }

    fn syscall_stop(&mut self, pid: i32) {
        let mut info = SyscallInfo::default();
        let size = std::mem::size_of::<SyscallInfo>();
        let r = unsafe { libc::ptrace(PTRACE_GET_SYSCALL_INFO as _, pid, size, &mut info as *mut SyscallInfo) };
        if r <= 0 || !self.started {
            return;
        }
        match info.op {
            OP_ENTRY if info.arch != AUDIT_ARCH_X86_64 => self.foreign_arch += 1,
            OP_ENTRY => {
                let a = &info.data;
                self.entry(pid, a[0], &[a[1], a[2], a[3], a[4], a[5], a[6]]);
            }
            OP_EXIT => self.exit(pid, info.data[0] as i64, info.data[1] & 0xff != 0),
            _ => {}
        }
    }

    fn event(&mut self, pid: i32, ev: i32) {
        let mut msg: libc::c_ulong = 0;
        unsafe { libc::ptrace(libc::PTRACE_GETEVENTMSG, pid, 0, &mut msg as *mut libc::c_ulong) };
        match ev {
            libc::PTRACE_EVENT_FORK | libc::PTRACE_EVENT_VFORK | libc::PTRACE_EVENT_CLONE => {
                let child = msg as i32;
                let (prog, flags) = {
                    let t = self.task(pid);
                    (t.prog, t.pending.as_ref().map(|p| if matches!(p.nr, 56 | 435) { p.arg0 } else { 0 }).unwrap_or(0))
                };
                if ev == libc::PTRACE_EVENT_CLONE && flags & CLONE_THREAD != 0 {
                    self.progs[prog].threads += 1;
                } else {
                    self.progs[prog].forks += 1;
                }
                self.tasks.entry(child).or_insert(Task { prog, pending: None, fresh: true }).prog = prog;
            }
            libc::PTRACE_EVENT_EXEC => {
                let former = msg as i32;
                if former != pid {
                    let pending = self.tasks.remove(&former).and_then(|t| t.pending);
                    self.task(pid).pending = pending;
                }
                let p = self.prog(&exe_of(pid));
                self.task(pid).prog = p;
                self.progs[p].execs += 1;
                if pid == self.root {
                    self.started = true;
                }
            }
            _ => {}
        }
    }

    /// Runs until no traced task is left and `keep_going` says the driver is done; returns whether it timed out.
    fn run(&mut self, mut keep_going: impl FnMut(i32) -> bool) -> bool {
        let mut st = 0;
        loop {
            if ALARM.load(Ordering::Relaxed) {
                for &p in self.tasks.keys() {
                    unsafe { libc::kill(p, libc::SIGKILL) };
                }
                ALARM.store(false, Ordering::Relaxed);
                self.root_status.get_or_insert(-libc::SIGKILL);
                return self.drain(keep_going);
            }
            let pid = unsafe { libc::waitpid(-1, &mut st, libc::__WALL) };
            if pid < 0 {
                if errno() == libc::EINTR {
                    continue;
                }
                return false;
            }
            if libc::WIFEXITED(st) || libc::WIFSIGNALED(st) {
                self.tasks.remove(&pid);
                if pid == self.root {
                    self.root_status =
                        Some(if libc::WIFEXITED(st) { libc::WEXITSTATUS(st) } else { -libc::WTERMSIG(st) });
                }
                if !keep_going(pid) && self.tasks.is_empty() {
                    return false;
                }
                continue;
            }
            if !libc::WIFSTOPPED(st) {
                continue;
            }
            let sig = libc::WSTOPSIG(st);
            let mut inject = 0;
            if sig == libc::SIGTRAP | 0x80 {
                self.syscall_stop(pid);
            } else if sig == libc::SIGTRAP && (st >> 16) != 0 {
                self.event(pid, st >> 16);
            } else {
                let fresh = std::mem::replace(&mut self.task(pid).fresh, false);
                let mut si: libc::siginfo_t = unsafe { std::mem::zeroed() };
                let delivery =
                    unsafe { libc::ptrace(libc::PTRACE_GETSIGINFO, pid, 0, &mut si as *mut libc::siginfo_t) } == 0;
                if !(fresh && sig == libc::SIGSTOP) && delivery {
                    inject = sig;
                    if self.started {
                        let p = self.task(pid).prog;
                        *self.progs[p].signals.entry(sig).or_insert(0) += 1;
                    }
                }
            }
            unsafe { libc::ptrace(libc::PTRACE_SYSCALL, pid, 0, inject as libc::c_long) };
        }
    }

    fn drain(&mut self, mut keep_going: impl FnMut(i32) -> bool) -> bool {
        let mut st = 0;
        loop {
            let pid = unsafe { libc::waitpid(-1, &mut st, libc::__WALL) };
            if pid < 0 {
                if errno() == libc::EINTR {
                    continue;
                }
                return true;
            }
            if libc::WIFEXITED(st) || libc::WIFSIGNALED(st) {
                self.tasks.remove(&pid);
                if !keep_going(pid) && self.tasks.is_empty() {
                    return true;
                }
            } else if libc::WIFSTOPPED(st) {
                unsafe { libc::kill(pid, libc::SIGKILL) };
                unsafe { libc::ptrace(libc::PTRACE_CONT, pid, 0, 0) };
            }
        }
    }

    fn json(&self, extra: &str) -> String {
        let mut o = String::from("{\"tracer\":\"lxtrace\",");
        o += &format!("\"uname\":{},", jstr(&uname()));
        o += &format!("\"exit\":{},", self.root_status.map(|s| s.to_string()).unwrap_or("null".into()));
        o += &format!("\"foreign_arch_calls\":{},", self.foreign_arch);
        o += extra;
        o += "\"programs\":";
        let progs = self.progs.iter().filter(|p| !p.sys.is_empty() || p.execs > 0).map(|p| {
            let calls = jobj(p.sys.iter().map(|(nr, s)| {
                let call = format!(
                    "{{\"n\":{},\"err\":{},\"args\":{},\"more_args\":{},\"paths\":{},\"more_paths\":{}}}",
                    s.n,
                    jobj(s.err.iter()),
                    jobj(s.args.iter()),
                    s.more_args,
                    jobj(s.paths.iter()),
                    s.more_paths
                );
                (nr, call)
            }));
            let sigs = jobj(p.signals.iter());
            let body = format!("{{\"execs\":{},\"forks\":{},\"threads\":{},", p.execs, p.forks, p.threads);
            (&p.name, format!("{body}\"signals\":{sigs},\"syscalls\":{calls}}}"))
        });
        o += &jobj(progs);
        o += "}\n";
        o
    }
}

/// A JSON object of already-encoded values, keys as strings.
fn jobj<K: std::fmt::Display, V: std::fmt::Display>(it: impl Iterator<Item = (K, V)>) -> String {
    let parts: Vec<String> = it.map(|(k, v)| format!("{}:{v}", jstr(&k.to_string()))).collect();
    format!("{{{}}}", parts.join(","))
}

fn jstr(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o += "\\\"",
            '\\' => o += "\\\\",
            c if (c as u32) < 0x20 => o += &format!("\\u{:04x}", c as u32),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

fn uname() -> String {
    let mut u: libc::utsname = unsafe { std::mem::zeroed() };
    unsafe { libc::uname(&mut u) };
    let f = |a: &[libc::c_char]| a.iter().take_while(|&&c| c != 0).map(|&c| c as u8 as char).collect::<String>();
    format!("{} {} {}", f(&u.sysname), f(&u.release), f(&u.machine))
}

fn cstr(s: &str) -> CString {
    CString::new(s.as_bytes()).unwrap_or_else(|_| CString::new("?").unwrap())
}

/// The executable an argv[0] names, searched in the workload's PATH as a container runtime does.
fn resolve(argv0: &str, env: &[String]) -> String {
    if argv0.contains('/') {
        return argv0.to_string();
    }
    let path = env
        .iter()
        .find_map(|e| e.strip_prefix("PATH="))
        .unwrap_or("/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin");
    for d in path.split(':') {
        let p = format!("{d}/{argv0}");
        let c = cstr(&p);
        if unsafe { libc::access(c.as_ptr(), libc::X_OK) } == 0 {
            return p;
        }
    }
    argv0.to_string()
}

struct Spawn<'a> {
    argv: &'a [String],
    env: &'a [String],
    cwd: Option<&'a str>,
    ids: Option<(u32, u32)>,
    stdin: i32,
    stdout: i32,
    traced: bool,
}

/// Forks and execs; a traced child stops itself before exec so the caller can set the ptrace options.
fn spawn(s: &Spawn) -> i32 {
    let path = cstr(&resolve(&s.argv[0], s.env));
    let argv: Vec<CString> = s.argv.iter().map(|a| cstr(a)).collect();
    let envp: Vec<CString> = s.env.iter().map(|a| cstr(a)).collect();
    let mut argp: Vec<*const libc::c_char> = argv.iter().map(|a| a.as_ptr()).collect();
    argp.push(std::ptr::null());
    let mut envpp: Vec<*const libc::c_char> = envp.iter().map(|a| a.as_ptr()).collect();
    envpp.push(std::ptr::null());
    let cwd = s.cwd.map(cstr);
    let pid = unsafe { libc::fork() };
    if pid != 0 {
        return pid;
    }
    unsafe {
        if s.traced {
            libc::ptrace(libc::PTRACE_TRACEME, 0, 0, 0);
            libc::raise(libc::SIGSTOP);
        }
        libc::dup2(s.stdin, 0);
        libc::dup2(s.stdout, 1);
        libc::dup2(s.stdout, 2);
        if let Some((uid, gid)) = s.ids {
            libc::setgroups(0, std::ptr::null());
            libc::setgid(gid);
            libc::setuid(uid);
        }
        if let Some(c) = &cwd {
            libc::chdir(c.as_ptr());
        }
        libc::execve(path.as_ptr(), argp.as_ptr(), envpp.as_ptr());
        libc::_exit(127);
    }
}

fn attach(root: i32) -> bool {
    let mut st = 0;
    let r = unsafe { libc::waitpid(root, &mut st, libc::__WALL) };
    if r != root || !libc::WIFSTOPPED(st) {
        return false;
    }
    let opts = libc::PTRACE_O_TRACESYSGOOD
        | libc::PTRACE_O_TRACEFORK
        | libc::PTRACE_O_TRACEVFORK
        | libc::PTRACE_O_TRACECLONE
        | libc::PTRACE_O_TRACEEXEC
        | libc::PTRACE_O_EXITKILL;
    unsafe {
        libc::ptrace(libc::PTRACE_SETOPTIONS, root, 0, opts as libc::c_long) == 0
            && libc::ptrace(libc::PTRACE_SYSCALL, root, 0, 0) == 0
    }
}

fn host_trace(args: &[String]) -> i32 {
    let (Some(out), Some(sep)) = (args.first(), args.iter().position(|a| a == "--")) else {
        eprintln!("usage: lxtrace trace OUT -- PROG ARGS...");
        return 2;
    };
    let argv = &args[sep + 1..];
    if argv.is_empty() {
        eprintln!("usage: lxtrace trace OUT -- PROG ARGS...");
        return 2;
    }
    let env: Vec<String> = std::env::vars().map(|(k, v)| format!("{k}={v}")).collect();
    let root = spawn(&Spawn { argv, env: &env, cwd: None, ids: None, stdin: 0, stdout: 1, traced: true });
    if root < 0 || !attach(root) {
        eprintln!("lxtrace: cannot start {}", argv[0]);
        return 1;
    }
    let mut t = Tracer::new(root);
    let t0 = Instant::now();
    t.run(|_| false);
    let extra = format!("\"elapsed_ms\":{},", t0.elapsed().as_millis());
    if let Err(e) = std::fs::write(out, t.json(&extra)) {
        eprintln!("lxtrace: {out}: {e}");
        return 1;
    }
    t.root_status.map(|s| if s < 0 { 128 - s } else { s }).unwrap_or(1)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if std::process::id() == 1 {
        guest_init();
    }
    match args.first().map(String::as_str) {
        Some("trace") => std::process::exit(host_trace(&args[1..])),
        _ => {
            eprintln!("usage: lxtrace trace OUT -- PROG ARGS...   (as PID 1: runs /k10s/plan)");
            std::process::exit(2);
        }
    }
}

// ---- guest init ----

#[derive(Default)]
struct Plan {
    env: Vec<String>,
    cwd: Option<String>,
    ids: Option<(u32, u32)>,
    hostname: String,
    timeout: u32,
    run: Vec<String>,
    steps: Vec<Vec<String>>,
}

fn parse_plan(text: &str) -> Plan {
    let mut p = Plan { hostname: "k10s-trace".into(), timeout: 300, ..Default::default() };
    for line in text.lines().filter(|l| !l.is_empty() && !l.starts_with('#')) {
        let f: Vec<String> = line.split('\t').map(str::to_string).collect();
        match (f[0].as_str(), f.len()) {
            ("env", 2) => p.env.push(f[1].clone()),
            ("cwd", 2) => p.cwd = Some(f[1].clone()),
            ("user", 3) => p.ids = f[1].parse().ok().zip(f[2].parse().ok()),
            ("hostname", 2) => p.hostname = f[1].clone(),
            ("timeout", 2) => p.timeout = f[1].parse().unwrap_or(300),
            ("run", n) if n > 1 => p.run = f[1..].to_vec(),
            ("step", n) if n > 1 => p.steps.push(f[1..].to_vec()),
            _ => say(&format!("plan: ignored line {line:?}")),
        }
    }
    p
}

fn say(s: &str) {
    let mut o = std::io::stdout();
    let _ = o.write_all(format!("lxtrace: {s}\n").as_bytes());
}

fn mount(src: &str, dst: &str, fs: &str, flags: libc::c_ulong, data: &str) {
    let _ = std::fs::create_dir_all(dst);
    let (s, d, f, o) = (cstr(src), cstr(dst), cstr(fs), cstr(data));
    if unsafe { libc::mount(s.as_ptr(), d.as_ptr(), f.as_ptr(), flags, o.as_ptr().cast()) } != 0 {
        say(&format!("mount {dst}: {}", std::io::Error::last_os_error()));
    }
}

fn lo_up() {
    unsafe {
        let fd = libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0);
        let mut r: libc::ifreq = std::mem::zeroed();
        for (d, s) in r.ifr_name.iter_mut().zip(b"lo") {
            *d = *s as libc::c_char;
        }
        libc::ioctl(fd, libc::SIOCGIFFLAGS as _, &mut r);
        r.ifr_ifru.ifru_flags |= libc::IFF_UP as libc::c_short;
        if libc::ioctl(fd, libc::SIOCSIFFLAGS as _, &r) != 0 {
            say(&format!("lo up: {}", std::io::Error::last_os_error()));
        }
        libc::close(fd);
    }
}

/// What a container runtime gives a container: proc, sys (cgroup2 inside), dev with pts, shm and mqueue, the
/// /dev/fd links, and generated /etc/hosts, /etc/hostname and /etc/resolv.conf.
fn container_mounts(hostname: &str) {
    mount("proc", "/proc", "proc", libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC, "");
    mount("sysfs", "/sys", "sysfs", libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC, "");
    mount("cgroup2", "/sys/fs/cgroup", "cgroup2", libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC, "");
    mount("devtmpfs", "/dev", "devtmpfs", libc::MS_NOSUID, "mode=0755");
    mount("devpts", "/dev/pts", "devpts", libc::MS_NOSUID | libc::MS_NOEXEC, "ptmxmode=0666,mode=0620");
    mount("shm", "/dev/shm", "tmpfs", libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC, "mode=1777,size=65536k");
    mount("mqueue", "/dev/mqueue", "mqueue", libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC, "");
    for (l, t) in [
        ("/dev/fd", "/proc/self/fd"),
        ("/dev/stdin", "/proc/self/fd/0"),
        ("/dev/stdout", "/proc/self/fd/1"),
        ("/dev/stderr", "/proc/self/fd/2"),
    ] {
        let _ = std::os::unix::fs::symlink(t, l);
    }
    let _ = std::fs::create_dir_all("/etc");
    let _ = std::fs::create_dir_all("/tmp");
    let hosts = format!("127.0.0.1\tlocalhost\n::1\tlocalhost ip6-localhost ip6-loopback\n127.0.1.1\t{hostname}\n");
    let _ = std::fs::write("/etc/hosts", hosts);
    let _ = std::fs::write("/etc/hostname", format!("{hostname}\n"));
    let _ = std::fs::write("/etc/resolv.conf", "nameserver 127.0.0.1\n");
    unsafe { libc::sethostname(hostname.as_ptr().cast(), hostname.len()) };
}

extern "C" fn on_alarm(_: libc::c_int) {
    ALARM.store(true, Ordering::Relaxed);
}

fn guest_init() -> ! {
    let plan = parse_plan(&std::fs::read_to_string("/k10s/plan").unwrap_or_default());
    container_mounts(&plan.hostname);
    lo_up();
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = on_alarm as *const () as usize;
        libc::sigaction(libc::SIGALRM, &sa, std::ptr::null_mut());
    }
    let mut pipe = [0; 2];
    unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC) };
    let logger = unsafe { libc::fork() };
    if logger == 0 {
        unsafe { libc::close(pipe[1]) };
        let mut buf = [0u8; 4096];
        loop {
            let n = unsafe { libc::read(pipe[0], buf.as_mut_ptr().cast(), buf.len()) };
            if n <= 0 {
                unsafe { libc::_exit(0) };
            }
            unsafe { libc::write(1, buf.as_ptr().cast(), n as usize) };
        }
    }
    let null = unsafe { libc::open(c"/dev/null".as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
    let t0 = Instant::now();
    let mut extra = String::new();
    let mut killed = false;
    let mut tracer = None;
    if plan.run.is_empty() {
        say("plan: no run line");
    } else {
        let root = spawn(&Spawn {
            argv: &plan.run,
            env: &plan.env,
            cwd: plan.cwd.as_deref(),
            ids: plan.ids,
            stdin: null,
            stdout: pipe[1],
            traced: true,
        });
        unsafe { libc::close(pipe[1]) };
        if root > 0 && attach(root) {
            say(&format!("tracing {:?} as pid {root}", plan.run));
            let driver = unsafe { libc::fork() };
            if driver == 0 {
                unsafe { libc::close(pipe[0]) };
                drive(&plan, root, null);
                unsafe { libc::_exit(0) };
            }
            unsafe { libc::alarm(plan.timeout) };
            let mut t = Tracer::new(root);
            let mut driver_live = true;
            killed = t.run(|pid| {
                if pid == driver {
                    driver_live = false;
                }
                driver_live
            });
            if killed {
                say("timeout: workload killed");
            }
            tracer = Some(t);
        } else {
            say("cannot start the workload");
        }
    }
    unsafe { libc::alarm(0) };
    let mut st = 0;
    unsafe { libc::waitpid(logger, &mut st, 0) };
    extra += &format!("\"elapsed_ms\":{},\"killed\":{killed},", t0.elapsed().as_millis());
    let log = std::fs::read_to_string("/k10s/driver.log").unwrap_or_default();
    extra += &format!("\"driver\":[{}],", log.lines().map(jstr).collect::<Vec<_>>().join(","));
    let json = tracer.map(|t| t.json(&extra)).unwrap_or_else(|| format!("{{{extra}\"programs\":{{}}}}\n"));
    match std::fs::OpenOptions::new().write(true).open("/dev/ttyS1") {
        Ok(mut f) => {
            let _ = f.write_all(b"\n--- lxtrace json ---\n");
            let _ = f.write_all(json.as_bytes());
            let _ = f.write_all(b"--- lxtrace end ---\n");
        }
        Err(e) => say(&format!("/dev/ttyS1: {e}")),
    }
    say("done");
    unsafe {
        libc::sync();
        libc::reboot(libc::LINUX_REBOOT_CMD_POWER_OFF);
    }
    loop {
        unsafe { libc::pause() };
    }
}

// ---- driver (untraced): readiness, requests, helpers, stop signal ----

fn log_step(f: &mut std::fs::File, s: &str) {
    say(s);
    let _ = f.write_all(format!("{s}\n").as_bytes());
}

fn drive(plan: &Plan, root: i32, null: i32) {
    let Ok(mut log) = std::fs::File::create("/k10s/driver.log") else { return };
    for step in &plan.steps {
        let t0 = Instant::now();
        let a = |i: usize| step.get(i).map(String::as_str).unwrap_or("");
        let res = match a(0) {
            "wait-tcp" => wait_tcp(root, a(1).parse().unwrap_or(0), a(2).parse().unwrap_or(60)),
            "http" => http(a(1).parse().unwrap_or(0), a(2), a(3).parse().unwrap_or(1)),
            "exec" => exec(plan, &step[1..], null).1,
            "until" => {
                let tries: u32 = a(1).parse().unwrap_or(1);
                let mut last = String::new();
                for _ in 0..tries {
                    let (code, out) = exec(plan, &step[2..], null);
                    last = out;
                    if code == 0 {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                last
            }
            "signal" => {
                let sig: i32 = a(1).parse().unwrap_or(libc::SIGTERM);
                let r = unsafe { libc::kill(root, sig) };
                format!("kill({root}, {sig}) = {r}")
            }
            other => format!("unknown step {other}"),
        };
        log_step(&mut log, &format!("{} [{} ms]: {}", step.join(" "), t0.elapsed().as_millis(), res));
    }
}

fn wait_tcp(root: i32, port: u16, secs: u64) -> String {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return "ok".into();
        }
        if unsafe { libc::kill(root, 0) } != 0 {
            return "workload gone".into();
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    "timeout".into()
}

fn exec(plan: &Plan, argv: &[String], null: i32) -> (i32, String) {
    if argv.is_empty() {
        return (-1, "empty".into());
    }
    let mut p = [0; 2];
    unsafe { libc::pipe2(p.as_mut_ptr(), libc::O_CLOEXEC) };
    let pid = spawn(&Spawn { argv, env: &plan.env, cwd: None, ids: None, stdin: null, stdout: p[1], traced: false });
    unsafe { libc::close(p[1]) };
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = unsafe { libc::read(p[0], buf.as_mut_ptr().cast(), buf.len()) };
        if n <= 0 {
            break;
        }
        if out.len() < 512 {
            out.extend_from_slice(&buf[..n as usize]);
        }
    }
    unsafe { libc::close(p[0]) };
    let mut st = 0;
    unsafe { libc::waitpid(pid, &mut st, 0) };
    let code = if libc::WIFEXITED(st) { libc::WEXITSTATUS(st) } else { 128 + libc::WTERMSIG(st) };
    out.truncate(512);
    let text: String = String::from_utf8_lossy(&out).chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    (code, format!("exit {code}: {}", text.trim()))
}

/// N requests on fresh connections, then N on one keep-alive connection; returns the status counts.
fn http(port: u16, path: &str, n: usize) -> String {
    let mut fresh: BTreeMap<String, usize> = BTreeMap::new();
    for _ in 0..n {
        let s = TcpStream::connect(("127.0.0.1", port)).and_then(|mut s| {
            s.set_read_timeout(Some(Duration::from_secs(20)))?;
            s.write_all(format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").as_bytes())?;
            let mut b = Vec::new();
            s.read_to_end(&mut b)?;
            Ok(status(&b))
        });
        *fresh.entry(s.unwrap_or_else(|e| format!("error {e}"))).or_insert(0) += 1;
    }
    let mut kept: BTreeMap<String, usize> = BTreeMap::new();
    let mut conn: Option<TcpStream> = None;
    for _ in 0..n {
        let r = (|| -> std::io::Result<String> {
            if conn.is_none() {
                let s = TcpStream::connect(("127.0.0.1", port))?;
                s.set_read_timeout(Some(Duration::from_secs(20)))?;
                conn = Some(s);
            }
            let s = conn.as_mut().unwrap();
            s.write_all(format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes())?;
            let (code, keep) = read_response(s)?;
            if !keep {
                conn = None;
            }
            Ok(code)
        })();
        if r.is_err() {
            conn = None;
        }
        *kept.entry(r.unwrap_or_else(|e| format!("error {e}"))).or_insert(0) += 1;
    }
    format!("fresh {fresh:?} keep-alive {kept:?}")
}

fn status(b: &[u8]) -> String {
    String::from_utf8_lossy(b).split_whitespace().nth(1).unwrap_or("none").to_string()
}

/// One HTTP/1.x response (Content-Length, chunked or close-delimited); returns the status and whether the
/// connection stays open.
fn read_response(s: &mut TcpStream) -> std::io::Result<(String, bool)> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if s.read(&mut byte)? == 0 || head.len() > 65536 {
            return Err(std::io::Error::other("truncated head"));
        }
        head.push(byte[0]);
    }
    let text = String::from_utf8_lossy(&head).to_ascii_lowercase();
    let code = status(&head);
    let hdr = |name: &str| text.lines().find_map(|l| l.strip_prefix(name).map(|v| v.trim().to_string()));
    let close = hdr("connection:").is_some_and(|v| v == "close")
        || text.starts_with("http/1.0") && hdr("connection:").is_none_or(|v| v != "keep-alive");
    if let Some(len) = hdr("content-length:").and_then(|v| v.parse::<u64>().ok()) {
        std::io::copy(&mut s.take(len), &mut std::io::sink())?;
    } else if hdr("transfer-encoding:").is_some_and(|v| v.contains("chunked")) {
        loop {
            let mut line = Vec::new();
            while !line.ends_with(b"\r\n") {
                if s.read(&mut byte)? == 0 || line.len() > 1024 {
                    return Err(std::io::Error::other("truncated chunk"));
                }
                line.push(byte[0]);
            }
            let size = u64::from_str_radix(String::from_utf8_lossy(&line).trim().split(';').next().unwrap_or(""), 16)
                .map_err(|_| std::io::Error::other("bad chunk"))?;
            std::io::copy(&mut s.take(size + 2), &mut std::io::sink())?;
            if size == 0 {
                break;
            }
        }
    } else {
        std::io::copy(s, &mut std::io::sink())?;
        return Ok((code, false));
    }
    Ok((code, !close))
}
