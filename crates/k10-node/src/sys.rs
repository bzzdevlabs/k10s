//! Thin, safe-ish wrappers over the Linux syscalls the runtime needs (via `libc`).
//!
//! Every function returns `io::Result` (errno → `io::Error`). File descriptors are wrapped in [`Fd`], which closes
//! on drop. Nothing here allocates on the hot path.

use std::io;
use std::os::fd::RawFd;
use std::ptr;

pub const PAGE: usize = 4096;

/// Runs a long-lived runtime thread's body, aborting the whole process if it panics.
///
/// A worker thread (shard, ticker, mesh, applier, KV flusher) holds raw pointers into shared state and other
/// threads assume it is alive; if it unwound and died, the node would keep running as a zombie (gossip stops, the
/// mesh inbox grows unbounded, a shard's instances leak, the ticker stops preempting). Fail-stop is safer than a
/// silently broken node, so a panic here aborts. The body is `AssertUnwindSafe` because the process ends on panic:
/// nothing observes any half-updated state afterwards.
pub fn run_or_abort(name: &str, body: impl FnOnce()) {
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)).is_err() {
        eprintln!("k10s: thread `{name}` panicked — aborting the process (fail-stop)");
        std::process::abort();
    }
}

#[inline]
pub fn cvt(r: libc::c_int) -> io::Result<libc::c_int> {
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r)
    }
}

#[inline]
pub fn cvt_isize(r: isize) -> io::Result<usize> {
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r as usize)
    }
}

/// An owned file descriptor.
#[derive(Debug)]
pub struct Fd(pub RawFd);

impl Fd {
    pub fn raw(&self) -> RawFd {
        self.0
    }
}

impl Drop for Fd {
    fn drop(&mut self) {
        if self.0 >= 0 {
            // SAFETY: we own the descriptor.
            unsafe { libc::close(self.0) };
        }
    }
}

// SAFETY: a file descriptor number is just an integer; the kernel object is thread-safe.
unsafe impl Send for Fd {}
unsafe impl Sync for Fd {}

/// Monotonic clock in nanoseconds (vDSO, ~20 ns).
#[inline]
pub fn mono_ns() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: valid pointer to a timespec.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

/// Wall clock, Unix milliseconds.
pub fn wall_ms() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: valid pointer to a timespec.
    unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, &mut ts) };
    ts.tv_sec as u64 * 1000 + ts.tv_nsec as u64 / 1_000_000
}

/// 64 random bits from the kernel (used to seed per-shard generators).
pub fn getrandom_u64() -> u64 {
    let mut v = 0u64;
    // SAFETY: writes at most 8 bytes into `v`.
    let r = unsafe { libc::getrandom(&mut v as *mut u64 as *mut libc::c_void, 8, 0) };
    if r != 8 {
        // Fall back to time-based entropy (getrandom never fails for 8 bytes on Linux ≥ 3.17 once seeded).
        v = mono_ns() ^ wall_ms().rotate_left(32) ^ (std::process::id() as u64).rotate_left(17);
    }
    v
}

/// Fills `buf` from `getrandom(2)` (keys, nonces): deliberately no fallback.
pub fn fill_random(buf: &mut [u8]) -> io::Result<()> {
    let mut done = 0;
    while done < buf.len() {
        let rest = &mut buf[done..];
        // SAFETY: writes at most `rest.len()` bytes into `rest`.
        let r = unsafe { libc::getrandom(rest.as_mut_ptr() as *mut libc::c_void, rest.len(), 0) };
        if r < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        done += r as usize;
    }
    Ok(())
}

// ------------------------------------------------------------------------------------------------------------------
// Memory
// ------------------------------------------------------------------------------------------------------------------

/// Reserves `len` bytes of read/write address space without committing memory (`MAP_NORESERVE`).
pub fn mmap_reserve(len: usize) -> io::Result<*mut u8> {
    // SAFETY: anonymous mapping at a kernel-chosen address; no existing memory is affected.
    let p = unsafe {
        libc::mmap(
            ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
            -1,
            0,
        )
    };
    if p == libc::MAP_FAILED {
        Err(io::Error::last_os_error())
    } else {
        Ok(p as *mut u8)
    }
}

/// Unmaps a region.
///
/// # Safety
/// `[p, p+len)` must be a mapping owned by the caller that nothing references any more.
pub unsafe fn munmap(p: *mut u8, len: usize) {
    libc::munmap(p as *mut libc::c_void, len);
}

/// Drops the pages of `[p, p+len)`: anonymous memory reads back as zeros, private file mappings revert to the file.
///
/// # Safety
/// The range must be mapped and owned by the caller, with no outstanding references into it.
pub unsafe fn madv_dontneed(p: *mut u8, len: usize) -> io::Result<()> {
    if len == 0 {
        return Ok(());
    }
    cvt(libc::madvise(p as *mut libc::c_void, len, libc::MADV_DONTNEED)).map(|_| ())
}

/// Opts a region out of transparent huge pages. With THP `always`, the first touch of any byte in a 2 MiB-aligned
/// window would commit a whole huge page: for instance slots that means ~30 neighbouring instances' worth of RSS.
///
/// # Safety
/// The range must be mapped and owned by the caller.
pub unsafe fn madv_nohugepage(p: *mut u8, len: usize) {
    libc::madvise(p as *mut libc::c_void, len, libc::MADV_NOHUGEPAGE);
}

/// Replaces `[p, p+len)` with fresh zero anonymous memory (`MAP_FIXED`), e.g. to undo a file mapping.
///
/// # Safety
/// The range must lie inside a reservation owned by the caller, with no outstanding references into it.
pub unsafe fn mmap_fixed_anon(p: *mut u8, len: usize) -> io::Result<()> {
    let r = libc::mmap(
        p as *mut libc::c_void,
        len,
        libc::PROT_READ | libc::PROT_WRITE,
        libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE | libc::MAP_FIXED,
        -1,
        0,
    );
    if r == libc::MAP_FAILED {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Maps `len` bytes of `fd` (from offset 0) copy-on-write over `[p, p+len)` (`MAP_PRIVATE|MAP_FIXED`).
///
/// # Safety
/// As [`mmap_fixed_anon`].
pub unsafe fn mmap_fixed_file_private(p: *mut u8, len: usize, fd: RawFd) -> io::Result<()> {
    let r = libc::mmap(
        p as *mut libc::c_void,
        len,
        libc::PROT_READ | libc::PROT_WRITE,
        libc::MAP_PRIVATE | libc::MAP_FIXED,
        fd,
        0,
    );
    if r == libc::MAP_FAILED {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Makes `[p, p+len)` inaccessible (guard pages).
///
/// # Safety
/// The range must be owned by the caller and unused.
pub unsafe fn mprotect_none(p: *mut u8, len: usize) -> io::Result<()> {
    cvt(libc::mprotect(p as *mut libc::c_void, len, libc::PROT_NONE)).map(|_| ())
}

/// Which pages of `[p, p+len)` are resident (one bool per page).
///
/// # Safety
/// The range must be mapped.
pub unsafe fn mincore(p: *mut u8, len: usize, out: &mut Vec<u8>) -> io::Result<()> {
    let pages = len.div_ceil(PAGE);
    out.clear();
    out.resize(pages, 0);
    cvt(libc::mincore(p as *mut libc::c_void, len, out.as_mut_ptr() as *mut libc::c_uchar)).map(|_| ())
}

/// Creates an anonymous memory file (`memfd_create`).
pub fn memfd(name: &str) -> io::Result<Fd> {
    let c = std::ffi::CString::new(name).unwrap_or_default();
    // SAFETY: valid C string.
    let fd = unsafe { libc::memfd_create(c.as_ptr(), libc::MFD_CLOEXEC) };
    cvt(fd).map(Fd)
}

pub fn ftruncate(fd: &Fd, len: u64) -> io::Result<()> {
    // SAFETY: plain syscall on an owned fd.
    cvt(unsafe { libc::ftruncate(fd.0, len as libc::off_t) }).map(|_| ())
}

pub fn pwrite_all(fd: &Fd, mut buf: &[u8], mut off: u64) -> io::Result<()> {
    while !buf.is_empty() {
        // SAFETY: buf is a valid slice.
        let n = unsafe { libc::pwrite(fd.0, buf.as_ptr() as *const libc::c_void, buf.len(), off as libc::off_t) };
        let n = match cvt_isize(n) {
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "pwrite returned 0"));
        }
        buf = &buf[n..];
        off += n as u64;
    }
    Ok(())
}

// ------------------------------------------------------------------------------------------------------------------
// Process statistics
// ------------------------------------------------------------------------------------------------------------------

/// Resident set size of this process in bytes. Uses `/proc/self/smaps_rollup` (a page-table walk: exact), falling
/// back to `/proc/self/statm` (cheap, but on Linux < 6.2 per-thread RSS counters are cached and can lag by 64 pages
/// per thread).
pub fn rss_bytes() -> u64 {
    match rss_pss_bytes() {
        (0, _) => {
            let s = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
            let resident: u64 = s.split_whitespace().nth(1).and_then(|v| v.parse().ok()).unwrap_or(0);
            resident * PAGE as u64
        }
        (rss, _) => rss,
    }
}

/// (Rss, Pss) in bytes from `/proc/self/smaps_rollup` (Pss counts pages shared between mappings fractionally).
pub fn rss_pss_bytes() -> (u64, u64) {
    let s = std::fs::read_to_string("/proc/self/smaps_rollup").unwrap_or_default();
    let mut rss = 0;
    let mut pss = 0;
    for line in s.lines() {
        let mut it = line.split_whitespace();
        let k = it.next().unwrap_or("");
        let v: u64 = it.next().and_then(|v| v.parse().ok()).unwrap_or(0);
        match k {
            "Rss:" => rss = v * 1024,
            "Pss:" => pss = v * 1024,
            _ => {}
        }
    }
    (rss, pss)
}

/// User + system CPU time consumed by this process, in nanoseconds.
pub fn process_cpu_ns() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: valid pointer.
    unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

/// Total physical memory in bytes.
pub fn phys_mem_bytes() -> u64 {
    // SAFETY: sysconf has no memory-safety preconditions.
    let pages = unsafe { libc::sysconf(libc::_SC_PHYS_PAGES) };
    if pages <= 0 {
        8 << 30
    } else {
        pages as u64 * PAGE as u64
    }
}

/// `vm.max_map_count` (limits the number of VMAs, hence of snapshot-mapped instances).
pub fn max_map_count() -> u64 {
    std::fs::read_to_string("/proc/sys/vm/max_map_count").ok().and_then(|s| s.trim().parse().ok()).unwrap_or(65530)
}

// ------------------------------------------------------------------------------------------------------------------
// epoll / eventfd
// ------------------------------------------------------------------------------------------------------------------

pub struct Epoll {
    fd: Fd,
}

pub const EPOLLIN: u32 = libc::EPOLLIN as u32;
pub const EPOLLOUT: u32 = libc::EPOLLOUT as u32;
pub const EPOLLET: u32 = libc::EPOLLET as u32;
pub const EPOLLRDHUP: u32 = libc::EPOLLRDHUP as u32;
pub const EPOLLERR: u32 = libc::EPOLLERR as u32;
pub const EPOLLHUP: u32 = libc::EPOLLHUP as u32;

impl Epoll {
    pub fn new() -> io::Result<Epoll> {
        // SAFETY: plain syscall.
        let fd = cvt(unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) })?;
        Ok(Epoll { fd: Fd(fd) })
    }

    pub fn add(&self, fd: RawFd, events: u32, token: u64) -> io::Result<()> {
        let mut ev = libc::epoll_event { events, u64: token };
        // SAFETY: valid pointer to an epoll_event.
        cvt(unsafe { libc::epoll_ctl(self.fd.0, libc::EPOLL_CTL_ADD, fd, &mut ev) }).map(|_| ())
    }

    pub fn del(&self, fd: RawFd) {
        let mut ev = libc::epoll_event { events: 0, u64: 0 };
        // SAFETY: valid pointer; errors (fd already closed) are irrelevant.
        unsafe { libc::epoll_ctl(self.fd.0, libc::EPOLL_CTL_DEL, fd, &mut ev) };
    }

    /// Waits for events; `timeout_ms < 0` blocks. EINTR is reported as zero events.
    pub fn wait(&self, events: &mut [libc::epoll_event], timeout_ms: i32) -> io::Result<usize> {
        // SAFETY: the buffer is valid for `events.len()` entries.
        let n = unsafe { libc::epoll_wait(self.fd.0, events.as_mut_ptr(), events.len() as i32, timeout_ms) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                return Ok(0);
            }
            return Err(e);
        }
        Ok(n as usize)
    }
}

/// An `eventfd` used to wake a shard's epoll loop from other threads.
pub struct EventFd {
    fd: Fd,
}

impl EventFd {
    pub fn new() -> io::Result<EventFd> {
        // SAFETY: plain syscall.
        let fd = cvt(unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) })?;
        Ok(EventFd { fd: Fd(fd) })
    }
    pub fn raw(&self) -> RawFd {
        self.fd.0
    }
    pub fn notify(&self) {
        let one: u64 = 1;
        // SAFETY: writes 8 bytes from a valid u64. EAGAIN (counter saturated) is harmless.
        unsafe { libc::write(self.fd.0, &one as *const u64 as *const libc::c_void, 8) };
    }
    pub fn drain(&self) {
        let mut v: u64 = 0;
        // SAFETY: reads 8 bytes into a valid u64.
        unsafe { libc::read(self.fd.0, &mut v as *mut u64 as *mut libc::c_void, 8) };
    }
}

// ------------------------------------------------------------------------------------------------------------------
// Sockets
// ------------------------------------------------------------------------------------------------------------------

fn sockaddr_from(addr: &std::net::SocketAddr) -> (libc::sockaddr_storage, libc::socklen_t) {
    // SAFETY: an all-zero sockaddr_storage is a valid value.
    let mut ss: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let len = match addr {
        std::net::SocketAddr::V4(a) => {
            let sin = libc::sockaddr_in {
                sin_family: libc::AF_INET as libc::sa_family_t,
                sin_port: a.port().to_be(),
                sin_addr: libc::in_addr { s_addr: u32::from_ne_bytes(a.ip().octets()) },
                sin_zero: [0; 8],
            };
            // SAFETY: sockaddr_storage is large enough and suitably aligned for sockaddr_in.
            unsafe { ptr::write(&mut ss as *mut _ as *mut libc::sockaddr_in, sin) };
            std::mem::size_of::<libc::sockaddr_in>()
        }
        std::net::SocketAddr::V6(a) => {
            let sin6 = libc::sockaddr_in6 {
                sin6_family: libc::AF_INET6 as libc::sa_family_t,
                sin6_port: a.port().to_be(),
                sin6_flowinfo: a.flowinfo(),
                sin6_addr: libc::in6_addr { s6_addr: a.ip().octets() },
                sin6_scope_id: a.scope_id(),
            };
            // SAFETY: as above.
            unsafe { ptr::write(&mut ss as *mut _ as *mut libc::sockaddr_in6, sin6) };
            std::mem::size_of::<libc::sockaddr_in6>()
        }
    };
    (ss, len as libc::socklen_t)
}

fn setsockopt_int(fd: RawFd, level: i32, opt: i32, v: i32) -> io::Result<()> {
    // SAFETY: valid pointer to an int of the given size.
    cvt(unsafe {
        libc::setsockopt(fd, level, opt, &v as *const i32 as *const libc::c_void, std::mem::size_of::<i32>() as u32)
    })
    .map(|_| ())
}

/// A non-blocking TCP listener with `SO_REUSEADDR` + `SO_REUSEPORT` (one per shard on the same port).
pub fn tcp_listen_reuseport(addr: &std::net::SocketAddr, backlog: i32) -> io::Result<Fd> {
    let fam = if addr.is_ipv4() { libc::AF_INET } else { libc::AF_INET6 };
    // SAFETY: plain syscall.
    let fd = Fd(cvt(unsafe { libc::socket(fam, libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC, 0) })?);
    setsockopt_int(fd.0, libc::SOL_SOCKET, libc::SO_REUSEADDR, 1)?;
    setsockopt_int(fd.0, libc::SOL_SOCKET, libc::SO_REUSEPORT, 1)?;
    let (ss, len) = sockaddr_from(addr);
    // SAFETY: ss/len describe a valid socket address.
    cvt(unsafe { libc::bind(fd.0, &ss as *const _ as *const libc::sockaddr, len) })?;
    // SAFETY: plain syscall.
    cvt(unsafe { libc::listen(fd.0, backlog) })?;
    Ok(fd)
}

/// A non-blocking TCP listener with `SO_REUSEADDR` only (one owner: the cluster peer listener).
pub fn tcp_listen(addr: &std::net::SocketAddr, backlog: i32) -> io::Result<Fd> {
    let fam = if addr.is_ipv4() { libc::AF_INET } else { libc::AF_INET6 };
    // SAFETY: plain syscall.
    let fd = Fd(cvt(unsafe { libc::socket(fam, libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC, 0) })?);
    setsockopt_int(fd.0, libc::SOL_SOCKET, libc::SO_REUSEADDR, 1)?;
    let (ss, len) = sockaddr_from(addr);
    // SAFETY: ss/len describe a valid socket address.
    cvt(unsafe { libc::bind(fd.0, &ss as *const _ as *const libc::sockaddr, len) })?;
    // SAFETY: plain syscall.
    cvt(unsafe { libc::listen(fd.0, backlog) })?;
    Ok(fd)
}

/// Starts a non-blocking TCP connect (`TCP_NODELAY`). Completion is signalled by writability; check it with
/// [`socket_error`].
pub fn tcp_connect(addr: &std::net::SocketAddr) -> io::Result<Fd> {
    let fam = if addr.is_ipv4() { libc::AF_INET } else { libc::AF_INET6 };
    // SAFETY: plain syscall.
    let fd = Fd(cvt(unsafe { libc::socket(fam, libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC, 0) })?);
    let _ = setsockopt_int(fd.0, libc::IPPROTO_TCP, libc::TCP_NODELAY, 1);
    let (ss, len) = sockaddr_from(addr);
    // SAFETY: ss/len describe a valid socket address.
    let r = unsafe { libc::connect(fd.0, &ss as *const _ as *const libc::sockaddr, len) };
    if r < 0 {
        let e = io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::EINPROGRESS) {
            return Err(e);
        }
    }
    Ok(fd)
}

/// The pending error of a socket (`SO_ERROR`), e.g. the outcome of a non-blocking connect.
pub fn socket_error(fd: RawFd) -> io::Result<()> {
    let mut v: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: valid pointers to an int and its length.
    cvt(unsafe { libc::getsockopt(fd, libc::SOL_SOCKET, libc::SO_ERROR, &mut v as *mut _ as *mut libc::c_void, &mut len) })?;
    if v == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(v))
    }
}

/// The local address a socket is bound to.
pub fn local_addr(fd: RawFd) -> io::Result<std::net::SocketAddr> {
    sock_name(fd, false)
}

/// The address of a connected socket's peer.
pub fn peer_addr(fd: RawFd) -> io::Result<std::net::SocketAddr> {
    sock_name(fd, true)
}

/// The connected peer is on a loopback address (IPv4-mapped included).
pub fn peer_is_loopback(fd: RawFd) -> bool {
    peer_addr(fd).is_ok_and(|a| match a.ip() {
        std::net::IpAddr::V4(ip) => ip.is_loopback(),
        std::net::IpAddr::V6(ip) => ip.is_loopback() || ip.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback()),
    })
}

fn sock_name(fd: RawFd, peer: bool) -> io::Result<std::net::SocketAddr> {
    // SAFETY: zeroed sockaddr_storage is valid; getsockname/getpeername write at most `len` bytes.
    let mut ss: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    let sa = &mut ss as *mut _ as *mut libc::sockaddr;
    cvt(unsafe { if peer { libc::getpeername(fd, sa, &mut len) } else { libc::getsockname(fd, sa, &mut len) } })?;
    match ss.ss_family as i32 {
        libc::AF_INET => {
            // SAFETY: the kernel filled a sockaddr_in.
            let sin = unsafe { &*(&ss as *const _ as *const libc::sockaddr_in) };
            let ip = std::net::Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
            Ok(std::net::SocketAddr::new(ip.into(), u16::from_be(sin.sin_port)))
        }
        libc::AF_INET6 => {
            // SAFETY: the kernel filled a sockaddr_in6.
            let sin6 = unsafe { &*(&ss as *const _ as *const libc::sockaddr_in6) };
            let ip = std::net::Ipv6Addr::from(sin6.sin6_addr.s6_addr);
            Ok(std::net::SocketAddr::new(ip.into(), u16::from_be(sin6.sin6_port)))
        }
        _ => Err(io::Error::other("unknown address family")),
    }
}

/// A log line printed at most once per 10 s; repeats in between are counted and reported with the next line.
#[derive(Debug, Default)]
pub struct RateLog {
    last: u64,
    suppressed: u64,
}

impl RateLog {
    const PERIOD_NS: u64 = 10_000_000_000;

    /// Prints `msg()` to stderr unless a line was printed less than 10 s ago (`now` = [`mono_ns`]).
    pub fn log(&mut self, now: u64, msg: impl FnOnce() -> String) {
        if self.last != 0 && now.saturating_sub(self.last) < Self::PERIOD_NS {
            self.suppressed += 1;
            return;
        }
        match std::mem::take(&mut self.suppressed) {
            0 => eprintln!("{}", msg()),
            n => eprintln!("{} ({n} similar message(s) suppressed)", msg()),
        }
        self.last = now.max(1);
    }
}

/// Back-off of a level-triggered listening socket whose `accept` fails. A failing accept leaves the connection in
/// the backlog, so the listener stays readable and `epoll_wait` would return at once, forever: a busy loop (and a
/// log line per iteration) for as long as the process is out of file descriptors (`EMFILE`/`ENFILE`). The owner
/// instead removes the listener from its epoll set until [`AcceptPause::until`] (10 ms, doubling up to 1 s while
/// accepts keep failing); pending clients wait in the backlog meanwhile.
#[derive(Debug, Default)]
pub struct AcceptPause {
    /// While non-zero, the listener is out of the epoll set until this time ([`mono_ns`]).
    pub until: u64,
    delay_ns: u64,
    /// Number of pauses so far.
    pub pauses: u64,
    pub log: RateLog,
}

impl AcceptPause {
    const MIN_NS: u64 = 10_000_000;
    const MAX_NS: u64 = 1_000_000_000;

    /// `accept` failed with `e` at `now`: starts (or extends) a pause and logs (rate-limited). Returns the end of
    /// the pause.
    pub fn failed(&mut self, now: u64, what: &str, e: &io::Error) -> u64 {
        self.delay_ns = (self.delay_ns * 2).clamp(Self::MIN_NS, Self::MAX_NS);
        self.until = now + self.delay_ns;
        self.pauses += 1;
        let ms = self.delay_ns / 1_000_000;
        self.log.log(now, || format!("k10s: {what}: accept: {e}; not accepting connections for {ms} ms"));
        self.until
    }

    /// An `accept` succeeded: the next failure starts again from the shortest pause.
    pub fn succeeded(&mut self) {
        self.delay_ns = 0;
    }

    /// The pause is over at `now`: the owner re-adds the listener (returns true once per pause).
    pub fn resume_due(&mut self, now: u64) -> bool {
        if self.until != 0 && now >= self.until {
            self.until = 0;
            return true;
        }
        false
    }
}

/// Accepts one connection (non-blocking, close-on-exec). `Ok(None)` when the queue is empty.
pub fn accept(fd: RawFd) -> io::Result<Option<Fd>> {
    loop {
        // SAFETY: null address pointers are allowed.
        let r = unsafe {
            libc::accept4(fd, ptr::null_mut(), ptr::null_mut(), libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC)
        };
        if r >= 0 {
            let c = Fd(r);
            let _ = setsockopt_int(c.0, libc::IPPROTO_TCP, libc::TCP_NODELAY, 1);
            return Ok(Some(c));
        }
        let e = io::Error::last_os_error();
        match e.raw_os_error() {
            Some(libc::EAGAIN) => return Ok(None),
            Some(libc::EINTR) | Some(libc::ECONNABORTED) => continue,
            _ => return Err(e),
        }
    }
}

/// `read(2)`: `Ok(0)` = EOF, `Err(WouldBlock)` when nothing is available.
#[inline]
pub fn read(fd: RawFd, buf: &mut [u8]) -> io::Result<usize> {
    loop {
        // SAFETY: buf is valid for writes of buf.len() bytes.
        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        match cvt_isize(n) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            r => return r,
        }
    }
}

/// `writev(2)` of up to two buffers (MSG_NOSIGNAL semantics are provided by ignoring SIGPIPE process-wide).
#[inline]
pub fn writev2(fd: RawFd, a: &[u8], b: &[u8]) -> io::Result<usize> {
    let iov = [
        libc::iovec { iov_base: a.as_ptr() as *mut libc::c_void, iov_len: a.len() },
        libc::iovec { iov_base: b.as_ptr() as *mut libc::c_void, iov_len: b.len() },
    ];
    let cnt = if b.is_empty() { 1 } else { 2 };
    loop {
        // SAFETY: the iovecs point to valid slices.
        let n = unsafe { libc::writev(fd, iov.as_ptr(), cnt) };
        match cvt_isize(n) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            r => return r,
        }
    }
}

/// Ignores SIGPIPE for the whole process (writes to closed sockets then fail with EPIPE instead of killing us).
pub fn ignore_sigpipe() {
    // SAFETY: setting a signal disposition to SIG_IGN is always sound.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_IGN) };
}

/// Pins the calling thread to one CPU (best effort).
pub fn pin_to_cpu(cpu: usize) {
    // SAFETY: cpu_set_t is plain data; CPU_SET writes inside it.
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_SET(cpu % libc::CPU_SETSIZE as usize, &mut set);
        libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set);
    }
}
