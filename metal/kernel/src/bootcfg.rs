//! Configuration and secrets from the boot loader (metal/README.md, "Booting"). k10boot (UEFI) and GRUB (BIOS) hand
//! over K10S-BOOT's k10s.conf and the key files it names as modules (k10-bootproto); QEMU's `-kernel` has fw_cfg
//! files instead (`-fw_cfg name=opt/k10s/cluster_key,file=FILE`).
//!
//! k10s.conf is shared with Tier 1, where its keys are `k10s node` flags. Those that mean the same here become boot
//! arguments, unless the command line has the argument; `k10s.KEY=VALUE` on the command line wins over the file, as
//! on Tier 1:
//!
//! | k10s.conf | argument |
//! |---|---|
//! | `join`, `backend`, `jit-indirect`, `timer-res-us`, `idle-spin-us`, `probe-ms`, `suspicion-mult`, `gossip-max-skew`, `ip` | `k10.join`, `k10.engine`, `k10.jit_indirect`, `k10.timer_res_us`, `k10.spin_us`, `k10.probe_ms`, `k10.suspicion_mult`, `k10.gossip_max_skew_s`, `k10.ip` (`A.B.C.D/P[,GATEWAY]`; DHCP without it) |
//! | `data`, `grow`, `os-commit-grace`, `os_trial`, `ntp`, `dns` | `k10.data`, `k10.grow`, `k10.os_commit_grace`, `k10.os_trial`, `k10.ntp` (SNTP servers or `none`), `k10.dns` (name servers) |
//! | `insecure-unauthenticated-cluster` | `k10.insecure_unauthenticated_cluster` |
//! | `watchdog` (`0`/`off`: off, anything else: on; Tier 2 arms it unless told not to) | `k10.watchdog=on\|off` |
//! | any of `gossip`, `rpc`, `join`, `advertise` (cluster mode, as on Tier 1) | `k10.cluster`, and `k10.gossip`, `k10.rpc`, `k10.http`: the port of `gossip`, `rpc`, `http` (default 7946, 7947, 80) at the address `advertise`, else the one configured unless unspecified, else 0.0.0.0: the interface's address (DHCP or `ip`; a changed lease reboots the node, which then advertises the new one) |
//!
//! The kernel listens on 7946, 7947 and 80 whatever the ports: these addresses are what it tells its peers.

use alloc::format;
use alloc::string::String;
use core::net::{IpAddr, Ipv4Addr, SocketAddr};
use core::sync::atomic::{AtomicU32, Ordering};
use k10_bootproto::conf;

use crate::kprintln;
use crate::pvh::BootInfo;

const MAPPED: [(&str, &str); 15] = [
    ("k10.join", "join"),
    ("k10.engine", "backend"),
    ("k10.jit_indirect", "jit-indirect"),
    ("k10.timer_res_us", "timer-res-us"),
    ("k10.spin_us", "idle-spin-us"),
    ("k10.probe_ms", "probe-ms"),
    ("k10.suspicion_mult", "suspicion-mult"),
    ("k10.gossip_max_skew_s", "gossip-max-skew"),
    ("k10.ip", "ip"),
    ("k10.data", "data"),
    ("k10.grow", "grow"),
    ("k10.os_commit_grace", "os-commit-grace"),
    ("k10.os_trial", "os_trial"),
    ("k10.ntp", "ntp"),
    ("k10.dns", "dns"),
];

/// Space-separated words in a fixed buffer (no heap yet when it is filled).
pub struct Args<'a> {
    buf: &'a mut [u8],
    len: usize,
}

impl<'a> Args<'a> {
    pub fn new(buf: &'a mut [u8]) -> Args<'a> {
        Args { buf, len: 0 }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    /// Appends `words`; false (and nothing appended) if they do not fit.
    pub fn push(&mut self, words: &str) -> bool {
        let sep = usize::from(self.len > 0 && !words.is_empty());
        if self.len + sep + words.len() > self.buf.len() {
            return false;
        }
        if sep == 1 {
            self.buf[self.len] = b' ';
        }
        self.buf[self.len + sep..self.len + sep + words.len()].copy_from_slice(words.as_bytes());
        self.len += sep + words.len();
        true
    }
}

/// Whether the command line sets the argument `arg`.
fn on_cmdline(cmdline: &str, arg: &str) -> bool {
    cmdline.split_ascii_whitespace().any(|w| w == arg || w.strip_prefix(arg).is_some_and(|r| r.starts_with('=')))
}

/// A k10s.conf value; `k10s.KEY=VALUE` of the command line wins.
fn value<'a>(text: &'a str, cmdline: &'a str, key: &str) -> Option<&'a str> {
    cmdline
        .split_ascii_whitespace()
        .filter_map(|w| w.strip_prefix("k10s.")?.strip_prefix(key)?.strip_prefix('='))
        .last()
        .or_else(|| conf::get(text, key))
}

/// Appends the arguments k10s.conf (`text`) implies to `out` (module docs).
pub fn derive(text: &str, cmdline: &str, out: &mut Args) {
    let mut put = |arg: &str, v: core::fmt::Arguments| {
        if on_cmdline(cmdline, arg) {
            return;
        }
        let mut word = [0u8; 256];
        let mut w = Args::new(&mut word);
        let ok = w.push(arg) && core::fmt::write(&mut w, v).is_ok();
        let n = w.len();
        let word = core::str::from_utf8(&word[..n]).unwrap_or("");
        if !ok || word.contains(char::is_whitespace) || !out.push(word) {
            kprintln!("k10-metal: k10s.conf: cannot use {word}");
        }
    };
    for (arg, key) in MAPPED {
        if let Some(v) = value(text, cmdline, key) {
            put(arg, format_args!("={v}"));
        }
    }
    if let Some(v) = value(text, cmdline, "watchdog") {
        put("k10.watchdog", format_args!("={}", if matches!(v, "0" | "false" | "no" | "off") { "off" } else { "on" }));
    }
    if value(text, cmdline, "insecure-unauthenticated-cluster").is_some_and(|v| !matches!(v, "0" | "false" | "no" | "off")) {
        put("k10.insecure_unauthenticated_cluster", format_args!(""));
    }
    if !["gossip", "rpc", "join", "advertise"].iter().any(|k| value(text, cmdline, k).is_some()) {
        return;
    }
    put("k10.cluster", format_args!(""));
    let advertise = value(text, cmdline, "advertise").and_then(|v| match v.parse::<Ipv4Addr>() {
        Ok(ip) => Some(IpAddr::V4(ip)),
        Err(_) => {
            kprintln!("k10-metal: k10s.conf advertise={v}: not an IPv4 address (no name resolution here): ignored");
            None
        }
    });
    // Unspecified: the cluster layer advertises the interface's address (cluster.rs).
    let own = IpAddr::V4(Ipv4Addr::UNSPECIFIED);
    for (arg, key, port) in [("k10.gossip", "gossip", crate::cluster::GOSSIP_PORT), ("k10.rpc", "rpc", crate::cluster::RPC_PORT), ("k10.http", "http", crate::net::HTTP_PORT)] {
        let given = value(text, cmdline, key).and_then(|v| v.parse::<SocketAddr>().ok());
        let ip = advertise.or(given.map(|a| a.ip()).filter(|ip| !ip.is_unspecified())).unwrap_or(own);
        put(arg, format_args!("={}", SocketAddr::new(ip, given.map_or(port, |a| a.port()))));
    }
}

impl core::fmt::Write for Args<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        if self.len + s.len() > self.buf.len() {
            return Err(core::fmt::Error);
        }
        self.buf[self.len..self.len + s.len()].copy_from_slice(s.as_bytes());
        self.len += s.len();
        Ok(())
    }
}

#[derive(Clone, Copy)]
pub enum Secret {
    Admin,
    Cluster,
    ClusterNext,
}

impl Secret {
    fn conf_key(self) -> &'static str {
        match self {
            Secret::Admin => "admin-token-file",
            Secret::Cluster => "cluster-key-file",
            Secret::ClusterNext => "cluster-next-key-file",
        }
    }

    fn fw_cfg(self) -> &'static str {
        match self {
            Secret::Admin => "opt/k10s/admin_key",
            Secret::Cluster => "opt/k10s/cluster_key",
            Secret::ClusterNext => "opt/k10s/cluster_next_key",
        }
    }

    /// Where the key is configured, for messages.
    pub fn source(self, bi: &BootInfo) -> String {
        if bi.from_loader() {
            format!("k10s.conf {}=", self.conf_key())
        } else {
            format!("fw_cfg {}", self.fw_cfg())
        }
    }
}

/// Modules already read (and wiped).
static CONSUMED: AtomicU32 = AtomicU32::new(0);

/// A key file, parsed: the module that k10s.conf names, wiped and released once read; without a boot loader, QEMU's
/// fw_cfg file. `Ok(None)`: not configured.
pub fn secret<T>(bi: &BootInfo, s: Secret, parse: impl FnOnce(&str) -> Result<T, &'static str>) -> Result<Option<T>, String> {
    if !bi.from_loader() {
        return crate::fw_cfg::secret(s.fw_cfg(), parse);
    }
    let key = s.conf_key();
    let Some(path) = bi.conf().and_then(|c| value(c, bi.cmdline(), key)) else { return Ok(None) };
    let Some((i, m)) = bi.module(path) else {
        return Err(format!("k10s.conf {key}={path}: not handed over by the boot loader (no such file on K10S-BOOT?)"));
    };
    if CONSUMED.fetch_or(1 << i, Ordering::Relaxed) & (1 << i) != 0 {
        return Err(format!("k10s.conf {key}={path}: the file already holds another key"));
    }
    // SAFETY: the module's memory, reserved until released below (once: CONSUMED).
    let raw = unsafe { core::slice::from_raw_parts_mut(m.paddr as *mut u8, m.size as usize) };
    let r = match core::str::from_utf8(raw) {
        Ok(t) => parse(t).map(Some).map_err(|e| format!("{path}: {e}")),
        Err(_) => Err(format!("{path}: not text")),
    };
    k10_mesh::crypto::zeroize(raw);
    crate::mem::frames::release_module(m.paddr, m.size);
    r
}

/// Wipes and releases the key files no configured key took (a standalone node handed a cluster key, GRUB's key files
/// of fixed names): no secret stays in memory past the boot.
pub fn wipe_unused_keys(bi: &BootInfo) {
    for (i, m) in bi.modules().iter().enumerate() {
        if !m.name().starts_with("/boot/") || CONSUMED.fetch_or(1 << i, Ordering::Relaxed) & (1 << i) != 0 {
            continue;
        }
        // SAFETY: the module's memory, reserved until released here (once: CONSUMED).
        k10_mesh::crypto::zeroize(unsafe { core::slice::from_raw_parts_mut(m.paddr as *mut u8, m.size as usize) });
        crate::mem::frames::release_module(m.paddr, m.size);
    }
}

/// The deployment bundle the loader handed over: module `k10s.bundle`, else QEMU's `-initrd` (unnamed).
pub fn bundle(bi: &BootInfo) -> Option<crate::pvh::Module> {
    bi.module(k10_bootproto::MOD_BUNDLE).map(|(_, m)| *m).or_else(|| bi.modules().iter().find(|m| m.name().is_empty()).copied())
}
