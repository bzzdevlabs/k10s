//! Kernel modules without udev or kmod: modalias coldplug, uevent hotplug, the kernel's usermode modprobe helper.
//!
//! Coldplug matches every `/sys/devices/**/modalias` against `modules.alias` and loads the module and its
//! dependencies (`modules.dep`, `modules.softdep` "pre:") with `finit_module`, repeating until a scan finds nothing
//! new (a bus driver creates new devices). Hotplug does the same for `ACTION=add` uevents.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::{say, Fd};

const MODULE_INIT_COMPRESSED_FILE: libc::c_uint = 4;

pub struct Modules {
    dir: PathBuf,
    /// (pattern, module name) in file order.
    aliases: Vec<(String, String)>,
    /// Module name → (path relative to `dir`, dependency names).
    deps: HashMap<String, (String, Vec<String>)>,
    /// Module name → modules to load first (`modules.softdep` "pre:"), e.g. ext4 → crc32c.
    softdeps: HashMap<String, Vec<String>>,
    loaded: HashSet<String>,
    failed: HashSet<String>,
    debug: bool,
}

/// `kernel/fs/ext4/ext4.ko.xz` → `ext4`; `-` and `_` are equivalent in module names.
fn mod_name(path: &str) -> String {
    let file = path.rsplit('/').next().unwrap_or(path);
    file.split(".ko").next().unwrap_or(file).replace('-', "_")
}

pub fn kernel_release() -> String {
    fs::read_to_string("/proc/sys/kernel/osrelease").map(|s| s.trim().to_string()).unwrap_or_default()
}

impl Modules {
    /// The running kernel's module tree, or `None` when it has none (every driver built in).
    pub fn open(debug: bool) -> Option<Modules> {
        Modules::open_at(&Path::new("/lib/modules").join(kernel_release()), Path::new("/sys/module"), debug)
    }

    fn open_at(dir: &Path, sys_module: &Path, debug: bool) -> Option<Modules> {
        let alias = fs::read_to_string(dir.join("modules.alias")).ok()?;
        let aliases = alias
            .lines()
            .filter_map(|l| l.strip_prefix("alias "))
            .filter_map(|l| l.split_once(' '))
            .map(|(p, m)| (p.to_string(), m.trim().replace('-', "_")))
            .collect();
        let mut deps = HashMap::new();
        for l in fs::read_to_string(dir.join("modules.dep")).unwrap_or_default().lines() {
            if let Some((path, ds)) = l.split_once(':') {
                deps.insert(mod_name(path), (path.to_string(), ds.split_whitespace().map(mod_name).collect()));
            }
        }
        let mut softdeps = HashMap::new();
        for l in fs::read_to_string(dir.join("modules.softdep")).unwrap_or_default().lines() {
            let mut w = l.split_whitespace();
            let (Some("softdep"), Some(m)) = (w.next(), w.next()) else { continue };
            let mut pre = Vec::new();
            let mut in_pre = false;
            for t in w {
                match t {
                    "pre:" => in_pre = true,
                    "post:" => in_pre = false,
                    t if in_pre => pre.push(t.replace('-', "_")),
                    _ => {}
                }
            }
            softdeps.insert(m.replace('-', "_"), pre);
        }
        // Loaded or built in: /sys/module/<name> exists.
        let loaded = fs::read_dir(sys_module)
            .map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
            .unwrap_or_default();
        Some(Modules { dir: dir.to_path_buf(), aliases, deps, softdeps, loaded, failed: HashSet::new(), debug })
    }

    fn matching(&self, alias: &str) -> Vec<String> {
        let mut v: Vec<String> = Vec::new();
        for (p, m) in &self.aliases {
            if glob(p.as_bytes(), alias.as_bytes()) && !v.contains(m) {
                v.push(m.clone());
            }
        }
        v
    }

    /// Loads drivers for every device in /sys until a scan finds nothing new. Returns the number loaded.
    pub fn coldplug(&mut self) -> usize {
        let mut seen = HashSet::new();
        let mut n = 0;
        for _round in 0..8 {
            let mut found = Vec::new();
            walk_modaliases(Path::new("/sys/devices"), 0, &mut found);
            let fresh: Vec<String> = found.into_iter().filter(|a| seen.insert(a.clone())).collect();
            if fresh.is_empty() {
                break;
            }
            let before = n;
            for a in &fresh {
                for m in self.matching(a) {
                    if self.debug {
                        say!("modules: {a} -> {m}");
                    }
                    n += self.load(&m, 0);
                }
            }
            // Drivers just loaded may still be probing: give their child devices a moment to appear.
            std::thread::sleep(Duration::from_millis(if n == before { 5 } else { 20 }));
        }
        n
    }

    /// Loads a module by name or alias (`crc32c`, `fs-vfat`, `crypto-sha256`), like `modprobe NAME`.
    pub fn load_any(&mut self, name: &str) -> usize {
        let m = name.replace('-', "_");
        if self.deps.contains_key(&m) || self.loaded.contains(&m) {
            return self.load(&m, 0);
        }
        self.matching(name).iter().map(|h| self.load(h, 0)).sum()
    }

    pub fn is_loaded(&self, name: &str) -> bool {
        self.loaded.contains(&name.replace('-', "_"))
    }

    /// Loads module `m` after its soft and hard dependencies. Returns how many modules were inserted.
    fn load(&mut self, m: &str, depth: u32) -> usize {
        if self.loaded.contains(m) || self.failed.contains(m) || depth > 32 {
            return 0;
        }
        let Some((path, deps)) = self.deps.get(m).cloned() else {
            self.failed.insert(m.to_string()); // not shipped in this image
            return 0;
        };
        // Marked first: a softdep cycle ends here.
        self.failed.insert(m.to_string());
        let mut n = 0;
        for pre in self.softdeps.get(m).cloned().unwrap_or_default() {
            n += self.load_any(&pre);
        }
        for d in &deps {
            n += self.load(d, depth + 1);
        }
        self.failed.remove(m);
        let full = self.dir.join(&path);
        let flags = if path.ends_with(".ko") { 0 } else { MODULE_INIT_COMPRESSED_FILE };
        let r = fs::File::open(&full).and_then(|f| {
            // SAFETY: valid descriptor and empty parameter string.
            match unsafe { libc::syscall(libc::SYS_finit_module, f.as_raw_fd(), c"".as_ptr(), flags) } {
                0 => Ok(1),
                _ => match io::Error::last_os_error() {
                    e if e.raw_os_error() == Some(libc::EEXIST) => Ok(0), // loaded meanwhile (coldplug and hotplug race)
                    e => Err(e),
                },
            }
        });
        match r {
            Ok(k) => {
                if self.debug && k > 0 {
                    say!("modules: loaded {path}");
                }
                self.loaded.insert(m.to_string());
                n + k
            }
            Err(e) => {
                say!("modules: {path}: {e}");
                self.failed.insert(m.to_string());
                n
            }
        }
    }
}

/// Listens to kernel uevents and loads the driver of every device added later (USB NICs and storage, late PCIe).
/// Started before the coldplug scan, so no device added in between is missed.
pub fn spawn_hotplug(debug: bool) {
    // SAFETY: plain socket call.
    let fd = Fd(unsafe { libc::socket(libc::AF_NETLINK, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, libc::NETLINK_KOBJECT_UEVENT) });
    // SAFETY: all-zero sockaddr_nl is valid.
    let mut sa: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
    sa.nl_family = libc::AF_NETLINK as libc::sa_family_t;
    sa.nl_groups = 1;
    // SAFETY: valid, correctly sized sockaddr_nl.
    if fd.0 < 0 || unsafe { libc::bind(fd.0, &sa as *const _ as *const libc::sockaddr, std::mem::size_of_val(&sa) as _) } != 0 {
        say!("hotplug: netlink: {}", io::Error::last_os_error());
        return;
    }
    let spawned = std::thread::Builder::new().name("hotplug".into()).spawn(move || {
        let fd = fd; // the whole socket moves here, not just its number
        let Some(mut m) = Modules::open(debug) else { return };
        let mut buf = vec![0u8; 16 << 10];
        loop {
            // SAFETY: valid buffer.
            let n = unsafe { libc::recv(fd.0, buf.as_mut_ptr().cast(), buf.len(), 0) };
            if n < 0 {
                match io::Error::last_os_error().raw_os_error() {
                    Some(libc::EINTR) | Some(libc::ENOBUFS) => continue, // ENOBUFS: a burst overran the socket
                    _ => {
                        say!("hotplug: recv: {}", io::Error::last_os_error());
                        return;
                    }
                }
            }
            let msg = &buf[..n as usize];
            let add = msg.split(|&b| b == 0).any(|f| f == b"ACTION=add");
            let alias = msg.split(|&b| b == 0).find_map(|f| f.strip_prefix(b"MODALIAS="));
            if let (true, Some(a)) = (add, alias) {
                let a = String::from_utf8_lossy(a).into_owned();
                for h in m.matching(&a) {
                    if m.load(&h, 0) > 0 {
                        say!("hotplug: {a} -> {h}");
                    }
                }
            }
        }
    });
    if let Err(e) = spawned {
        say!("hotplug: thread: {e}");
    }
}

/// Deletes the module files whose module is not loaded, freeing their RAM (the initramfs is RAM). Returns bytes freed.
pub fn prune() -> u64 {
    fn walk(dir: &Path, freed: &mut u64) {
        let Ok(rd) = fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                walk(&p, freed);
            } else if p.to_string_lossy().contains(".ko") && !Path::new("/sys/module").join(mod_name(&p.to_string_lossy())).exists() {
                *freed += e.metadata().map(|m| m.len()).unwrap_or(0);
                let _ = fs::remove_file(&p);
            }
        }
    }
    let mut freed = 0;
    walk(&Path::new("/lib/modules").join(kernel_release()), &mut freed);
    freed
}

/// Filesystem and codepage drivers are not modalias-driven: loads them by name (no-op when built in).
pub fn load_by_name(names: &[&str]) {
    if let Some(mut m) = Modules::open(false) {
        for n in names {
            m.load_any(n);
        }
    }
}

/// The kernel's usermode modprobe helper (`/proc/sys/kernel/modprobe` is `/init`): `/init -q -- NAME`.
pub fn modprobe(name: &str) -> std::process::ExitCode {
    let Some(mut m) = Modules::open(false) else { return std::process::ExitCode::FAILURE };
    if m.load_any(name) > 0 || m.is_loaded(name) {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::FAILURE
    }
}

fn walk_modaliases(dir: &Path, depth: u32, out: &mut Vec<String>) {
    if depth > 16 {
        return;
    }
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_dir() {
            walk_modaliases(&e.path(), depth + 1, out);
        } else if e.file_name() == "modalias" {
            if let Ok(s) = fs::read_to_string(e.path()) {
                let s = s.trim();
                if !s.is_empty() {
                    out.push(s.to_string());
                }
            }
        }
    }
}

/// The fnmatch(3) subset of modules.alias: `*`, `?`, `[...]` with ranges and `!`/`^` negation.
fn glob(p: &[u8], s: &[u8]) -> bool {
    let (mut pi, mut si) = (0, 0);
    let (mut star_p, mut star_s) = (usize::MAX, 0);
    while si < s.len() {
        if pi < p.len() {
            match p[pi] {
                b'*' => {
                    star_p = pi;
                    star_s = si;
                    pi += 1;
                    continue;
                }
                b'?' => {
                    pi += 1;
                    si += 1;
                    continue;
                }
                b'[' => {
                    if let Some((true, len)) = class(&p[pi..], s[si]) {
                        pi += len;
                        si += 1;
                        continue;
                    }
                }
                c if c == s[si] => {
                    pi += 1;
                    si += 1;
                    continue;
                }
                _ => {}
            }
        }
        if star_p == usize::MAX {
            return false;
        }
        pi = star_p + 1;
        star_s += 1;
        si = star_s;
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

/// Matches `c` against the class at the start of `p` (`[...]`): (matched, class length), `None` if unterminated.
fn class(p: &[u8], c: u8) -> Option<(bool, usize)> {
    let neg = matches!(p.get(1), Some(b'!') | Some(b'^'));
    let start = if neg { 2 } else { 1 };
    let mut i = start;
    let mut ok = false;
    while i < p.len() && (p[i] != b']' || i == start) {
        if i + 2 < p.len() && p[i + 1] == b'-' && p[i + 2] != b']' {
            ok |= (p[i]..=p[i + 2]).contains(&c);
            i += 3;
        } else {
            ok |= p[i] == c;
            i += 1;
        }
    }
    (i < p.len()).then_some((ok != neg, i + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_like_kmod() {
        assert!(glob(b"pci:v00001AF4d00001000sv*sd*bc*sc*i*", b"pci:v00001AF4d00001000sv00001AF4sd00000001bc02sc00i00"));
        assert!(glob(b"virtio:d00000001v*", b"virtio:d00000001v00001AF4"));
        assert!(!glob(b"virtio:d00000002v*", b"virtio:d00000001v00001AF4"));
        assert!(glob(b"acpi*:LNXPWRBN:*", b"acpi:LNXPWRBN:"));
        assert!(glob(b"usb:v*p*d*dc*dsc*dp*ic03isc01ip0[12]in*", b"usb:v0627p0001d0000dc00dsc00dp00ic03isc01ip02in00"));
        assert!(!glob(b"usb:v*p*d*dc*dsc*dp*ic03isc01ip0[12]in*", b"usb:v0627p0001d0000dc00dsc00dp00ic03isc01ip03in00"));
        assert!(glob(b"a[!x]c", b"abc") && !glob(b"a[!b]c", b"abc") && !glob(b"a[bc", b"abc"));
        assert!(glob(b"*", b"") && glob(b"a*b*c", b"axxbyyc") && !glob(b"a*b*c", b"axxbyy"));
    }

    #[test]
    fn module_tree() {
        let t = std::env::temp_dir().join(format!("k10s-pid1-mods-{}", std::process::id()));
        let sys = t.join("sys-module");
        fs::create_dir_all(sys.join("virtio")).unwrap();
        fs::write(t.join("modules.alias"), "alias virtio:d00000001v* virtio_net\nalias fs-vfat vfat\nalias crypto-crc32c crc32c_generic\n").unwrap();
        fs::write(
            t.join("modules.dep"),
            "kernel/drivers/net/virtio_net.ko.xz: kernel/net/core/failover.ko.xz kernel/drivers/net/net_failover.ko.xz\n\
             kernel/drivers/net/net_failover.ko.xz: kernel/net/core/failover.ko.xz\nkernel/net/core/failover.ko.xz:\n\
             kernel/fs/fat/vfat.ko: kernel/fs/fat/fat.ko\nkernel/fs/ext4/ext4.ko: kernel/fs/jbd2/jbd2.ko\n",
        )
        .unwrap();
        fs::write(t.join("modules.softdep"), "# comment\nsoftdep ext4 pre: crc32c post: x\n").unwrap();
        let m = Modules::open_at(&t, &sys, false).unwrap();
        assert_eq!(m.matching("virtio:d00000001v00001AF4"), ["virtio_net"]);
        assert_eq!(m.deps["virtio_net"].1, ["failover", "net_failover"]);
        assert_eq!(m.softdeps["ext4"], ["crc32c"]);
        assert!(m.is_loaded("virtio") && !m.is_loaded("vfat"));
        assert_eq!(mod_name("kernel/fs/nls/nls_iso8859-1.ko.zst"), "nls_iso8859_1");
        fs::remove_dir_all(&t).unwrap();
    }
}
