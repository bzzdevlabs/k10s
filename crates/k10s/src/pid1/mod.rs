//! PID 1 mode: `k10s` as the only userspace program of a Linux image (docs/OS.md). The kernel is a driver layer;
//! there is no shell, udev, systemd or DHCP daemon.
//!
//! The kernel starts `/init` (the static k10s) as PID 1. Init stays privileged and does the system work: mounts,
//! drivers by modalias ([`modules`]), configuration ([`config`]), network ([`net`], [`dhcp`]), time ([`sntp`]), the
//! data partition, the hardware watchdog and the power button. It runs the node as its child, `k10s node … --user
//! 65532:65532`, which enters the normal sandbox (user switch, no capabilities, Landlock, seccomp), restarts it when
//! it exits, and on a power-off request stops it, syncs, unmounts and powers off. If init itself panics, the kernel
//! panics: images boot with `panic=5` to reboot. Signed A/B updates of the image itself: [`update`].

mod config;
mod dhcp;
mod grow;
mod modules;
mod net;
mod sntp;
mod update;

use std::fs;
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use config::Config;
pub use modules::modprobe;
pub use update::release_keys;

/// systemd's convention and the default of the kernel's ACPI "tiny power button": SIGRTMIN+4 asks init to power off.
const SIG_POWEROFF: libc::c_int = 38;
const DATA: &str = "/data";
/// The node gets this long to drain on shutdown before SIGKILL.
const STOP_TIMEOUT: Duration = Duration::from_secs(30);
/// The update agent asks for a reboot into a new slot (or back from a failed trial).
const SIG_UPDATE: libc::c_int = libc::SIGUSR1;
/// The node's end of the update channel (`--os-agent-fd`).
const OS_AGENT_FD: libc::c_int = 3;

pub(crate) fn boot_secs() -> f64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: valid pointer.
    unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut ts) };
    ts.tv_sec as f64 + ts.tv_nsec as f64 / 1e9
}

/// A console line stamped with the time since the kernel started, written with one write(2) so that the node's
/// output (same console) cannot split it.
macro_rules! say {
    ($($a:tt)*) => {{
        let line = format!("k10s-init[{:8.3}]: {}\n", $crate::pid1::boot_secs(), format_args!($($a)*));
        let _ = std::io::Write::write_all(&mut std::io::stderr(), line.as_bytes());
    }};
}
pub(crate) use say;

/// An owned file descriptor.
pub(crate) struct Fd(pub libc::c_int);

impl Drop for Fd {
    fn drop(&mut self) {
        if self.0 >= 0 {
            // SAFETY: we own the descriptor.
            unsafe { libc::close(self.0) };
        }
    }
}

/// The kernel started us as init: PID 1, invoked as `init` (`/init` of the initramfs). k10s as PID 1 of a container
/// (`docker run … k10s node`) keeps its normal command line.
pub fn is_init(argv0: &str) -> bool {
    std::process::id() == 1 && argv0.rsplit('/').next() == Some("init")
}

/// The kernel's usermode modprobe call `/init -q -- MODULE` (`/proc/sys/kernel/modprobe` is `/init`).
pub fn modprobe_request(argv: &[String]) -> Option<&str> {
    match argv {
        [init, q, dd, m] if init.rsplit('/').next() == Some("init") && q == "-q" && dd == "--" => Some(m),
        _ => None,
    }
}

fn mount(src: &str, dst: &str, fstype: &str, flags: libc::c_ulong, data: &str) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    let c = |s: &str| std::ffi::CString::new(s).map_err(io::Error::other);
    let (s, d, t, o) = (c(src)?, c(dst)?, c(fstype)?, c(data)?);
    // SAFETY: valid C strings.
    if unsafe { libc::mount(s.as_ptr(), d.as_ptr(), t.as_ptr(), flags, o.as_ptr().cast()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn signal_set() -> libc::sigset_t {
    // SAFETY: plain sigset manipulation.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for s in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGCHLD, libc::SIGPWR, SIG_POWEROFF, SIG_UPDATE] {
            libc::sigaddset(&mut set, s);
        }
        set
    }
}

/// Entry point when [`is_init`]. Never returns: ends in `reboot(2)`.
pub fn main() -> ! {
    // Blocked in every thread before any starts: signals to PID 1 with the default disposition are dropped, so they
    // are only ever consumed by the sigtimedwait of the supervision loop.
    let set = signal_set();
    // SAFETY: valid sigset.
    unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()) };
    early_mounts();
    // On-demand module loads (request_module) come back to us: the kernel runs `/init -q -- NAME`.
    let _ = fs::write("/proc/sys/kernel/modprobe", "/init");
    // SAFETY: Ctrl-Alt-Del becomes SIGINT to init instead of an immediate reboot.
    unsafe { libc::reboot(libc::LINUX_REBOOT_CMD_CAD_OFF) };
    let mut cfg = Config::from_cmdline(&fs::read_to_string("/proc/cmdline").unwrap_or_default());
    if let Ok(baked) = fs::read_to_string("/etc/k10s.conf") {
        cfg.merge(&baked);
    }
    let debug = cfg.flag("debug");
    say!("k10s {} as PID 1 on Linux {}", crate::VERSION, modules::kernel_release());

    if cfg.get("modules") != Some("none") {
        let t = Instant::now();
        match modules::Modules::open(debug) {
            Some(mut m) => {
                modules::spawn_hotplug(debug);
                let n = m.coldplug();
                say!("modules: {n} loaded by modalias in {:.0} ms", t.elapsed().as_secs_f64() * 1e3);
            }
            None => say!("modules: none shipped (drivers built in)"),
        }
    }
    // Boot partition (GPT name K10S-BOOT, FAT): read-only, root-only; holds k10s.conf and the operator's key files.
    let bootwait = Duration::from_millis(cfg.get("bootwait").and_then(|v| v.parse().ok()).unwrap_or(0));
    let mut boot_mounted = false;
    if let Some(dev) = find_partition("K10S-BOOT", bootwait) {
        modules::load_by_name(&["vfat", "nls_cp437", "nls_iso8859_1"]);
        let ro = libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC;
        // Explicit charsets: kernels differ in their defaults and may lack the default's module.
        match mount(&dev, "/boot", "vfat", ro, "uid=0,gid=0,fmask=0177,dmask=0077,codepage=437,iocharset=iso8859-1") {
            Ok(()) => {
                boot_mounted = true;
                match fs::read_to_string("/boot/k10s.conf") {
                    Ok(text) => {
                        cfg.merge(&text);
                        say!("config: {dev} (K10S-BOOT): k10s.conf merged");
                    }
                    Err(e) => say!("config: {dev} (K10S-BOOT): k10s.conf: {e}"),
                }
            }
            Err(e) => say!("config: mounting {dev} (K10S-BOOT): {e}"),
        }
    }
    // Reading the slots hashes the booted image: alongside the network setup.
    let slots = boot_mounted.then(|| {
        let keys = |k: &str| cfg.get(k).map(str::to_string);
        let (slot, os_key, os_trial) = (keys("slot"), keys("os_key"), keys("os_trial"));
        std::thread::spawn(move || open_slots(slot.as_deref(), os_key.as_deref(), os_trial.as_deref()))
    });
    let lease = Arc::new(dhcp::Shared::default());
    let iface = network(&cfg, &lease);
    set_hostname(&cfg, iface.as_deref());
    mount_data(&cfg);
    hand_data_to_node(&cfg);
    if cfg.flag("prune_modules") {
        say!("modules: {} KiB of unused module files freed", modules::prune() >> 10);
    }
    match cfg.get("ntp") {
        Some("none") => {}
        n => sntp::spawn(n.map(str::to_string), lease.clone()),
    }
    spawn_power_button();
    let health = Arc::new(Health::default());
    spawn_health_probe(cfg.http_probe_addr(), health.clone());
    if cfg.flag("watchdog") {
        spawn_watchdog(health.clone());
    }
    let agent = slots.and_then(|t| t.join().ok().flatten());
    let how = supervise(&cfg, &set, &health, agent.as_ref());
    shutdown(how)
}

/// The image's A/B slots on K10S-BOOT ([`update`]); `None` when this boot did not come from one (no updates).
fn open_slots(slot: Option<&str>, os_key: Option<&str>, os_trial: Option<&str>) -> Option<update::Agent> {
    let firmware = if Path::new("/sys/firmware/efi").exists() {
        let flags = libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC;
        if let Err(e) = mount("efivarfs", "/sys/firmware/efi/efivars", "efivarfs", flags, "") {
            say!("update: efivarfs: {e}");
        }
        update::Firmware::Uefi
    } else {
        update::Firmware::Bios
    };
    let entry = update::efi_selected_entry();
    let slots = update::Slots::open(update::Esp::new("/boot", true), firmware, entry.as_deref(), slot);
    let (Some(st), Some(b)) = (slots.status(), slots.booted) else {
        say!("update: not booted from a slot of this image's loaders; updates are off");
        return None;
    };
    say!(
        "update: {:?} boot of slot {} ({}, {}{})",
        firmware,
        b.letter(),
        st.platform,
        k10_mesh::sha256::to_hex(&st.image[..4]),
        if st.committed { "" } else { ", trial" }
    );
    let mut trusted = release_keys();
    for k in os_key.unwrap_or("").split(',').map(str::trim).filter(|k| !k.is_empty()) {
        match k10_node::cluster::os::parse_hex32(k) {
            Some(key) => trusted.push(key),
            None => say!("update: os_key `{k}` is not a public key (64 hex digits)"),
        }
    }
    let trial = Duration::from_secs(os_trial.and_then(|v| v.parse().ok()).unwrap_or(600));
    Some(update::Agent::spawn(slots, trusted, DATA.into(), trial))
}

fn early_mounts() {
    let hard = libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC;
    for (src, dst, t, flags, data) in
        [("devtmpfs", "/dev", "devtmpfs", libc::MS_NOSUID | libc::MS_NOEXEC, "mode=0755"), ("proc", "/proc", "proc", hard, ""), ("sysfs", "/sys", "sysfs", hard, "")]
    {
        match mount(src, dst, t, flags, data) {
            Err(e) if e.raw_os_error() != Some(libc::EBUSY) => say!("mount {dst}: {e}"),
            _ => {}
        }
    }
    // Without /dev/console in the initramfs the kernel could not give us stdio: open it now.
    // SAFETY: fcntl probe and plain open/dup2 on the standard descriptors.
    unsafe {
        if libc::fcntl(2, libc::F_GETFD) < 0 {
            let fd = libc::open(c"/dev/console".as_ptr(), libc::O_RDWR | libc::O_NOCTTY);
            if fd >= 0 {
                for t in 0..3 {
                    libc::dup2(fd, t);
                }
                if fd > 2 {
                    libc::close(fd);
                }
            }
        }
    }
}

/// Loopback and the first Ethernet interface: static (`ip`), the kernel's `ip=`, or DHCP. Waits up to 30 s for a
/// first lease (the node advertises the interface's address); DHCP keeps trying in the background either way.
fn network(cfg: &Config, lease: &Arc<dhcp::Shared>) -> Option<String> {
    if cfg.get("net") == Some("none") {
        return None;
    }
    if let Err(e) = net::link_up("lo") {
        say!("net: lo: {e}");
    }
    let wait = Duration::from_secs(cfg.get("netwait").and_then(|v| v.parse().ok()).unwrap_or(10));
    let Some(ifc) = net::pick_iface(cfg.get("iface"), wait) else {
        say!("net: no Ethernet interface");
        return None;
    };
    if let Err(e) = net::link_up(&ifc) {
        say!("net: {ifc}: {e}");
    }
    let dns: Vec<std::net::Ipv4Addr> = cfg.get("dns").map(|d| d.split(',').filter_map(|s| s.trim().parse().ok()).collect()).unwrap_or_default();
    if !dns.is_empty() {
        if let Err(e) = net::write_resolv(&dns, None) {
            say!("net: /etc/resolv.conf: {e}");
        }
    }
    if let Some(spec) = cfg.get("ip") {
        match net::parse_static(spec).and_then(|(a, p, gw)| net::set_addr(&ifc, a, p, gw).map_err(|e| e.to_string())) {
            Ok(()) => say!("net: {ifc} static {spec}"),
            Err(e) => say!("net: {ifc}: {e}"),
        }
    } else if net::has_address(&ifc) {
        say!("net: {ifc} configured by the kernel (ip=)");
    } else {
        if !net::wait_carrier(&ifc, wait) {
            say!("net: {ifc}: no link after {} s", wait.as_secs());
        }
        match dhcp::spawn(&ifc, lease.clone(), dns.is_empty()) {
            Ok(()) => {
                if lease.wait_lease(Duration::from_secs(30)).is_none() {
                    say!("net: {ifc}: no DHCP lease yet, starting without an address (still trying)");
                }
            }
            Err(e) => say!("net: {ifc}: DHCP: {e}"),
        }
    }
    Some(ifc)
}

/// The node's default name is the hostname: `name` if configured, else `k10s-` and the interface's MAC tail.
fn set_hostname(cfg: &Config, iface: Option<&str>) {
    let name = match (cfg.get("name"), iface) {
        (Some(n), _) => n.to_string(),
        (None, Some(i)) => {
            let m = net::mac(i);
            format!("k10s-{:02x}{:02x}{:02x}", m[3], m[4], m[5])
        }
        (None, None) => "k10s".into(),
    };
    if let Err(e) = fs::write("/proc/sys/kernel/hostname", &name) {
        say!("hostname {name}: {e}");
    }
}

/// `/dev/NAME` of the partition whose GPT name is `label`, waiting up to `wait` for it to appear.
fn find_partition(label: &str, wait: Duration) -> Option<String> {
    let t = Instant::now();
    let want = format!("PARTNAME={label}");
    loop {
        for e in fs::read_dir("/sys/class/block").into_iter().flatten().flatten() {
            let ue = fs::read_to_string(e.path().join("uevent")).unwrap_or_default();
            if ue.lines().any(|l| l == want) {
                return ue.lines().find_map(|l| l.strip_prefix("DEVNAME=")).map(|n| format!("/dev/{n}"));
            }
        }
        if t.elapsed() >= wait {
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The data partition (`data=/dev/X[:FSTYPE]`, else GPT name K10S-DATA, ext4) on /data; without one the node's
/// state lives in RAM. K10S-DATA grows to the end of its disk unless `grow=0`.
fn mount_data(cfg: &Config) {
    let (spec, grow) = match cfg.get("data") {
        Some("none") => (None, false),
        Some(s) => (Some(s.to_string()), false),
        None => (find_partition("K10S-DATA", Duration::ZERO), cfg.get("grow").is_none() || cfg.flag("grow")),
    };
    let _ = fs::create_dir_all(DATA);
    let Some(spec) = spec else {
        say!("data: no data partition, state is volatile");
        return;
    };
    let (dev, fstype) = spec.split_once(':').unwrap_or((&spec, "ext4"));
    modules::load_by_name(&[fstype]);
    let t = Instant::now();
    while !Path::new(dev).exists() && t.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(10));
    }
    if grow {
        match grow::partition(dev) {
            Ok(Some(b)) => say!("data: {dev} grown to {} MiB (end of disk)", b >> 20),
            Ok(None) => {}
            Err(e) => say!("data: growing {dev}: {e}"),
        }
    }
    match mount(dev, DATA, fstype, libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC | libc::MS_NOATIME, "") {
        Ok(()) => say!("data: {dev} ({fstype}) on {DATA}"),
        Err(e) => return say!("data: mounting {dev} ({fstype}): {e}; state is volatile"),
    }
    if grow && fstype == "ext4" {
        match grow::ext4(dev, DATA) {
            Ok(Some(b)) => say!("data: ext4 grown to {} MiB", b >> 20),
            Ok(None) => {}
            Err(e) => say!("data: growing the filesystem on {dev}: {e}"),
        }
    }
}

/// A fresh data directory (root-owned mountpoint or RAM directory) goes to the node's user, which the node requires.
fn hand_data_to_node(cfg: &Config) {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let user = match k10_node::sandbox::User::resolve(cfg.get("user").unwrap_or(config::NODE_USER)) {
        Ok(u) => u,
        Err(e) => return say!("data: {e}"),
    };
    if fs::metadata(DATA).is_ok_and(|m| m.uid() == 0) && user.uid != 0 {
        if let Err(e) = std::os::unix::fs::chown(DATA, Some(user.uid), Some(user.gid)).and_then(|()| fs::set_permissions(DATA, fs::Permissions::from_mode(0o700))) {
            say!("data: handing {DATA} to {}: {e}", user.name);
        }
    }
}

/// `KEY_POWER` on any input device (the ACPI button driver and evdev) asks init to power off.
fn spawn_power_button() {
    for e in fs::read_dir("/dev/input").into_iter().flatten().flatten() {
        if !e.file_name().to_string_lossy().starts_with("event") {
            continue;
        }
        let Ok(mut f) = fs::File::open(e.path()) else { continue };
        let _ = std::thread::Builder::new().name("power-button".into()).spawn(move || {
            // struct input_event: struct timeval, u16 type, u16 code, i32 value.
            let o = 2 * std::mem::size_of::<libc::c_long>();
            let mut buf = vec![0u8; (o + 8) * 16];
            while let Ok(n) = f.read(&mut buf) {
                for ev in buf[..n].chunks_exact(o + 8) {
                    let (ty, code) = (u16::from_ne_bytes([ev[o], ev[o + 1]]), u16::from_ne_bytes([ev[o + 2], ev[o + 3]]));
                    let val = i32::from_ne_bytes([ev[o + 4], ev[o + 5], ev[o + 6], ev[o + 7]]);
                    if (ty, code, val) == (1, 116, 1) {
                        // SAFETY: signals init; SIGTERM is blocked and sigwait-ed there.
                        unsafe { libc::kill(1, libc::SIGTERM) };
                    }
                }
            }
        });
    }
}

/// When the node last answered its health check.
#[derive(Default)]
struct Health {
    /// Milliseconds since init started; 0 = never.
    last_ok_ms: AtomicU64,
    stopping: AtomicBool,
}

impl Health {
    /// Healthy recently, or still starting (up to 5 minutes for a first answer), or shutting down.
    fn fine(&self, uptime: Duration) -> bool {
        let last = self.last_ok_ms.load(Ordering::Relaxed);
        self.stopping.load(Ordering::Relaxed)
            || (last == 0 && uptime < Duration::from_secs(300))
            || (last > 0 && (uptime.as_millis() as u64).saturating_sub(last) < 60_000)
    }
}

static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();

fn uptime() -> Duration {
    START.get_or_init(Instant::now).elapsed()
}

fn healthy(addr: std::net::SocketAddr) -> bool {
    let Ok(mut c) = std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(2)) else { return false };
    let _ = c.set_read_timeout(Some(Duration::from_secs(2)));
    let mut b = [0u8; 12];
    c.write_all(b"GET /_k10s/health HTTP/1.1\r\nHost: init\r\nConnection: close\r\n\r\n").is_ok()
        && c.read_exact(&mut b).is_ok()
        && &b == b"HTTP/1.1 200"
}

fn spawn_health_probe(addr: std::net::SocketAddr, h: Arc<Health>) {
    uptime();
    let _ = std::thread::Builder::new().name("health".into()).spawn(move || {
        let mut failures = 0;
        loop {
            if healthy(addr) {
                if h.last_ok_ms.load(Ordering::Relaxed) == 0 {
                    say!("ready: the node answers on http://{addr} ({})", meminfo());
                } else if failures >= 3 {
                    say!("health: the node answers again");
                }
                h.last_ok_ms.store((uptime().as_millis() as u64).max(1), Ordering::Relaxed);
                failures = 0;
            } else {
                failures += 1;
                if failures == 3 && h.last_ok_ms.load(Ordering::Relaxed) > 0 {
                    say!("health: the node does not answer on http://{addr}");
                }
            }
            let ready = h.last_ok_ms.load(Ordering::Relaxed) > 0;
            std::thread::sleep(Duration::from_millis(if ready { 5000 } else { 100 }));
        }
    });
}

static WATCHDOG: Mutex<Option<fs::File>> = Mutex::new(None);

/// Pets `/dev/watchdog` every 5 s while the node is healthy: a hung node or kernel makes the hardware reset.
fn spawn_watchdog(h: Arc<Health>) {
    match fs::OpenOptions::new().write(true).open("/dev/watchdog") {
        Ok(f) => {
            say!("watchdog: armed, petted while the node is healthy");
            *WATCHDOG.lock().unwrap_or_else(|e| e.into_inner()) = Some(f);
            let _ = std::thread::Builder::new().name("watchdog".into()).spawn(move || loop {
                if h.fine(uptime()) {
                    if let Some(f) = WATCHDOG.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
                        let _ = f.write_all(b"\0");
                    }
                }
                std::thread::sleep(Duration::from_secs(5));
            });
        }
        Err(e) => say!("watchdog: /dev/watchdog: {e}"),
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Halt {
    PowerOff,
    Reboot,
}

/// The node, by `posix_spawn` of our own binary: std's spawn would link glibc symbols newer than the gnu build's floor
/// (2.34). The child starts with an empty signal mask and default dispositions; it reads its secrets, binds, then
/// sandboxes itself.
fn spawn_node(cfg: &Config, agent: Option<&update::Agent>) -> io::Result<libc::pid_t> {
    use std::ffi::CString;
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStringExt;
    let mut args = cfg.node_args(DATA, crate::NODE_BOOLEANS);
    // The update channel becomes the node's fd 3 (a descriptor already numbered 3 would keep close-on-exec).
    let channel = match agent.map(update::Agent::connect).transpose() {
        Ok(c) => c.map(|fd| if fd.as_raw_fd() == OS_AGENT_FD { fd.try_clone() } else { Ok(fd) }).transpose()?,
        Err(e) => {
            say!("update: {e}");
            None
        }
    };
    if channel.is_some() {
        args.extend(["--os-agent-fd".to_string(), OS_AGENT_FD.to_string()]);
    }
    say!("node: k10s {}", args.join(" "));
    let argv: Vec<CString> =
        std::iter::once("k10s").chain(args.iter().map(String::as_str)).map(CString::new).collect::<Result<_, _>>().map_err(io::Error::other)?;
    let env: Vec<CString> = std::env::vars_os()
        .map(|(k, v)| CString::new([k.into_vec(), b"=".to_vec(), v.into_vec()].concat()))
        .collect::<Result<_, _>>()
        .map_err(io::Error::other)?;
    let ptrs = |v: &[CString]| v.iter().map(|c| c.as_ptr() as *mut libc::c_char).chain([std::ptr::null_mut()]).collect::<Vec<_>>();
    let (argv_p, env_p) = (ptrs(&argv), ptrs(&env));
    let mut pid = 0;
    // SAFETY: initialized attributes and file actions, NULL-terminated argv/envp of live C strings.
    let r = unsafe {
        let mut attr: libc::posix_spawnattr_t = std::mem::zeroed();
        let mut fa: libc::posix_spawn_file_actions_t = std::mem::zeroed();
        libc::posix_spawnattr_init(&mut attr);
        libc::posix_spawn_file_actions_init(&mut fa);
        let mut empty: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut empty);
        libc::posix_spawnattr_setsigmask(&mut attr, &empty);
        libc::posix_spawnattr_setsigdefault(&mut attr, &signal_set());
        libc::posix_spawnattr_setflags(&mut attr, (libc::POSIX_SPAWN_SETSIGMASK | libc::POSIX_SPAWN_SETSIGDEF) as libc::c_short);
        libc::posix_spawn_file_actions_addopen(&mut fa, 0, c"/dev/null".as_ptr(), libc::O_RDONLY, 0);
        if let Some(c) = &channel {
            libc::posix_spawn_file_actions_adddup2(&mut fa, c.as_raw_fd(), OS_AGENT_FD);
        }
        let r = libc::posix_spawn(&mut pid, c"/proc/self/exe".as_ptr(), &fa, &attr, argv_p.as_ptr(), env_p.as_ptr());
        libc::posix_spawn_file_actions_destroy(&mut fa);
        libc::posix_spawnattr_destroy(&mut attr);
        r
    };
    if r != 0 {
        return Err(io::Error::from_raw_os_error(r));
    }
    Ok(pid)
}

fn sigwait_for(set: &libc::sigset_t, timeout: Duration) -> Option<libc::c_int> {
    let ts = libc::timespec { tv_sec: timeout.as_secs() as _, tv_nsec: timeout.subsec_nanos() as libc::c_long };
    // SAFETY: valid set and timeout; the siginfo is not needed.
    let s = unsafe { libc::sigtimedwait(set, std::ptr::null_mut(), &ts) };
    (s > 0).then_some(s)
}

/// Reaps every exited child; returns the node's wait status if it was among them.
fn reap(node: Option<libc::pid_t>) -> Option<libc::c_int> {
    let mut node_status = None;
    loop {
        let mut st = 0;
        // SAFETY: plain waitpid.
        let pid = unsafe { libc::waitpid(-1, &mut st, libc::WNOHANG) };
        if pid <= 0 {
            return node_status;
        }
        if Some(pid) == node {
            node_status = Some(st);
        }
    }
}

fn describe(st: libc::c_int) -> String {
    if libc::WIFEXITED(st) {
        format!("exit status {}", libc::WEXITSTATUS(st))
    } else {
        format!("signal {}", libc::WTERMSIG(st))
    }
}

/// Runs the node until init is asked to stop: restarts it when it exits (backoff 1 s doubling to 60 s, reset after a
/// minute of uptime), and on SIGHUP (address change) when it advertises the lease's address.
fn supervise(cfg: &Config, set: &libc::sigset_t, health: &Health, agent: Option<&update::Agent>) -> Halt {
    let mut node: Option<(libc::pid_t, Instant)> = None;
    let mut restart_at = Some(Instant::now());
    let mut backoff = Duration::from_secs(1);
    loop {
        if node.is_none() && restart_at.is_some_and(|t| t <= Instant::now()) {
            restart_at = None;
            match spawn_node(cfg, agent) {
                Ok(pid) => node = Some((pid, Instant::now())),
                Err(e) => {
                    say!("node: cannot start: {e}");
                    restart_at = Some(Instant::now() + backoff);
                    backoff = (backoff * 2).min(Duration::from_secs(60));
                }
            }
        }
        let wait = restart_at.map_or(Duration::from_secs(3600), |t| t.saturating_duration_since(Instant::now()).max(Duration::from_millis(1)));
        match sigwait_for(set, wait) {
            Some(libc::SIGCHLD) => {
                if let (Some(st), Some((_, started))) = (reap(node.map(|n| n.0)), node) {
                    node = None;
                    health.last_ok_ms.store(0, Ordering::Relaxed);
                    if started.elapsed() >= Duration::from_secs(60) {
                        backoff = Duration::from_secs(1);
                    }
                    if restart_at.is_none() {
                        say!("node: exited ({}), restarting in {} s", describe(st), backoff.as_secs());
                        restart_at = Some(Instant::now() + backoff);
                        backoff = (backoff * 2).min(Duration::from_secs(60));
                    }
                }
            }
            Some(libc::SIGHUP) if cfg.advertises_lease() => {
                if let Some((pid, _)) = node {
                    say!("node: restarting to advertise the new address");
                    restart_at = Some(Instant::now());
                    // SAFETY: plain kill of our child.
                    unsafe { libc::kill(pid, libc::SIGTERM) };
                }
            }
            Some(libc::SIGINT) => return stop(node, Halt::Reboot, set, health), // Ctrl-Alt-Del
            Some(SIG_UPDATE) => return stop(node, Halt::Reboot, set, health),
            Some(libc::SIGTERM) | Some(libc::SIGPWR) | Some(SIG_POWEROFF) => return stop(node, Halt::PowerOff, set, health),
            _ => {}
        }
    }
}

/// SIGTERM to the node (it leaves the cluster and drains), SIGKILL after [`STOP_TIMEOUT`].
fn stop(node: Option<(libc::pid_t, Instant)>, how: Halt, set: &libc::sigset_t, health: &Health) -> Halt {
    health.stopping.store(true, Ordering::Relaxed);
    let Some((pid, _)) = node else { return how };
    say!("shutdown: stopping the node");
    // SAFETY: plain kill of our child.
    unsafe { libc::kill(pid, libc::SIGTERM) };
    let deadline = Instant::now() + STOP_TIMEOUT;
    loop {
        if let Some(st) = reap(Some(pid)) {
            say!("shutdown: node stopped ({})", describe(st));
            return how;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            say!("shutdown: the node did not stop in {} s, killing it", STOP_TIMEOUT.as_secs());
            // SAFETY: plain kill and blocking wait for our child.
            unsafe {
                libc::kill(pid, libc::SIGKILL);
                libc::waitpid(pid, std::ptr::null_mut(), 0);
            }
            return how;
        }
        // Further power-off requests while stopping are ignored; SIGCHLD wakes us up.
        sigwait_for(set, left);
    }
}

fn meminfo() -> String {
    let mi = fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let kib = |k: &str| -> u64 { mi.lines().find(|l| l.starts_with(k)).and_then(|l| l.split_whitespace().nth(1)?.parse().ok()).unwrap_or(0) };
    format!("memory: {} MiB used of {} MiB", (kib("MemTotal:") - kib("MemAvailable:")) >> 10, kib("MemTotal:") >> 10)
}

fn shutdown(how: Halt) -> ! {
    // SAFETY: plain syscalls; the node has exited.
    unsafe {
        libc::sync();
        for m in [c"/data", c"/boot"] {
            if libc::umount(m.as_ptr()) != 0 && io::Error::last_os_error().raw_os_error() != Some(libc::EINVAL) {
                // Still busy: remount read-only so the journal is clean.
                libc::mount(std::ptr::null(), m.as_ptr(), std::ptr::null(), libc::MS_REMOUNT | libc::MS_RDONLY, std::ptr::null());
            }
        }
        libc::sync();
    }
    // Magic close: a driver without nowayout disarms; reboot(2) resets the machine either way.
    if let Some(mut f) = WATCHDOG.lock().unwrap_or_else(|e| e.into_inner()).take() {
        let _ = f.write_all(b"V");
    }
    say!("{}", if how == Halt::Reboot { "rebooting" } else { "powering off" });
    // SAFETY: reboot(2) as PID 1.
    unsafe { libc::reboot(if how == Halt::Reboot { libc::RB_AUTOBOOT } else { libc::RB_POWER_OFF }) };
    // reboot(2) failed: exiting PID 1 panics the kernel, which reboots with `panic=N`.
    std::process::exit(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invocations() {
        let v = |s: &[&str]| s.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(modprobe_request(&v(&["/init", "-q", "--", "crypto-sha256"])), Some("crypto-sha256"));
        assert_eq!(modprobe_request(&v(&["/usr/bin/k10s", "-q", "--", "x"])), None);
        assert_eq!(modprobe_request(&v(&["/init", "-q", "x"])), None);
        assert!(!is_init("/init")); // the test runner is not PID 1
    }

    #[test]
    fn health_windows() {
        let h = Health::default();
        assert!(h.fine(Duration::from_secs(10)) && !h.fine(Duration::from_secs(301)));
        h.last_ok_ms.store(100_000, Ordering::Relaxed);
        assert!(h.fine(Duration::from_secs(150)) && !h.fine(Duration::from_secs(161)));
        h.stopping.store(true, Ordering::Relaxed);
        assert!(h.fine(Duration::from_secs(1000)));
    }
}
