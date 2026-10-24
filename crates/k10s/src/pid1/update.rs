//! A/B OS updates (docs/OS.md, "Updates"). Each slot of K10S-BOOT holds one signed image: the UKI itself
//! (`/EFI/Linux/k10s-X.efi`, booted by systemd-boot on UEFI) and its kernel, initramfs and command line extracted for
//! GRUB (`/k10s/X/`, BIOS). An update writes the other slot completely, then arms a trial boot of it in both loaders:
//!
//! * UEFI: the file is named `k10s-X+3.efi` and `loader.conf` says `default k10s-X+[1-9]*`. systemd-boot counts
//!   the tries down in the name (`k10s-X+2-1.efi`, …); at `+0` the pattern stops matching and it boots the other,
//!   committed slot (entries without tries left sort last).
//! * BIOS: `grubenv` says `k10s_try=X3`; `grub.cfg` counts it down (`X2`, `X1`, `X0`) and boots `k10s_slot` once
//!   the tries are used up.
//!
//! The trial is committed (counter dropped from the name, `default k10s-X.efi`, `k10s_slot=X`) once the node
//! rejoined the cluster and serves its placement; a trial that crashes, hangs (watchdog) or does not get there within
//! `os_trial` seconds falls back, and init records the image as failed. Every step is a complete file written to a
//! temporary name, synced and renamed, so a power cut at any point leaves a bootable good image.
//!
//! The Raspberry Pi firmware boots a whole FAT partition instead: partition 2 (slot A) or 3 (slot B), as
//! `autoboot.txt` on K10S-BOOT (partition 1) says. A Pi image's slot is a signed FAT image (firmware, devicetrees,
//! kernel, initramfs, `config.txt`) written raw to its partition. An update writes the other partition, records the
//! trial in `k10s/trying` (image and tries left) and reboots with the firmware's one-shot tryboot flag, which boots
//! the `[tryboot]` partition once; any later boot goes back to the committed one. A commit rewrites `autoboot.txt`; a
//! committed boot that finds `k10s/trying` reboots into the trial again until its tries are used up, then records the
//! image as failed.

use super::say;
use k10_mesh::osimage;
use k10_mesh::sha256::{sha256, to_hex};
use k10_node::cluster::os::{parse_hex32, OsStatus};
use std::fs;
use std::io::{self, Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// The k10s release key(s): images they signed install on every node.
const RELEASE_KEYS: &str = include_str!("../../../../os/release-key.pub");
/// Boots of a trial slot before the loaders fall back.
const TRIES: u32 = 3;
/// Failed images remembered on K10S-BOOT (newest last).
const MAX_FAILED: usize = 16;
const GRUBENV_HEADER: &str = "# GRUB Environment Block\n";
const GRUBENV_LEN: usize = 1024;
/// systemd-boot's loader.conf with `default` filled in (os/mkimage.py writes the same with `k10s-a.efi`).
const LOADER_CONF: &str = "timeout 0\ndefault {}\neditor no\nauto-entries no\nauto-firmware no\nconsole-mode keep\n";

/// Hex public keys, one per line (`#` comments).
pub fn parse_keys(text: &str) -> Vec<[u8; 32]> {
    text.lines().map(|l| l.split('#').next().unwrap_or("").trim()).filter(|l| !l.is_empty()).filter_map(parse_hex32).collect()
}

pub fn release_keys() -> Vec<[u8; 32]> {
    parse_keys(RELEASE_KEYS)
}

/// The image's A/B boot record on a Pi (K10S-BOOT): `[all]` is the committed partition, `[tryboot]` the other.
const AUTOBOOT: &str = "autoboot.txt";
/// A Pi trial in progress: `IMAGE TRIES_LEFT`.
const TRYING: &str = "k10s/trying";

/// Init reboots with `LINUX_REBOOT_CMD_RESTART2 "0 tryboot"` (a Pi trial boot) instead of a plain reboot.
pub static TRYBOOT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Firmware {
    Uefi,
    Bios,
    /// Raspberry Pi firmware: A/B partitions picked by `autoboot.txt`, trials by its tryboot flag.
    Pi,
}

/// What the Pi firmware tells about this boot, and the slot partitions.
pub struct PiBoot {
    /// Block devices of slots A and B (partitions 2 and 3 of the boot disk).
    pub parts: [PathBuf; 2],
    /// `/chosen/bootloader/partition` of the firmware's devicetree.
    pub partition: Option<u32>,
    /// `/chosen/bootloader/tryboot`.
    pub tryboot: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Slot {
    A,
    B,
}

impl Slot {
    pub fn letter(self) -> char {
        match self {
            Slot::A => 'a',
            Slot::B => 'b',
        }
    }

    pub fn other(self) -> Slot {
        match self {
            Slot::A => Slot::B,
            Slot::B => Slot::A,
        }
    }

    fn parse(c: char) -> Option<Slot> {
        match c.to_ascii_lowercase() {
            'a' => Some(Slot::A),
            'b' => Some(Slot::B),
            _ => None,
        }
    }

    /// The Pi partition holding the slot.
    fn partition(self) -> u32 {
        match self {
            Slot::A => 2,
            Slot::B => 3,
        }
    }

    fn of_partition(n: u32) -> Option<Slot> {
        [Slot::A, Slot::B].into_iter().find(|s| s.partition() == n)
    }

    fn index(self) -> usize {
        self as usize
    }
}

/// `autoboot.txt` with `committed` as the default partition and the other one for tryboot.
fn autoboot(committed: Slot) -> String {
    format!("[all]\ntryboot_a_b=1\nboot_partition={}\n[tryboot]\nboot_partition={}\n", committed.partition(), committed.other().partition())
}

/// The `[all]` partition of `autoboot.txt`.
fn autoboot_committed(text: &str) -> Option<Slot> {
    let mut all = false;
    for l in text.lines().map(str::trim) {
        if l.starts_with('[') {
            all = l == "[all]";
        } else if let Some(n) = l.strip_prefix("boot_partition=").filter(|_| all) {
            return n.trim().parse().ok().and_then(Slot::of_partition);
        }
    }
    None
}

/// The signed image in a Pi slot partition: a FAT filesystem (its size from the boot sector) followed by the
/// signature trailer.
fn read_slot(dev: &Path) -> Option<Vec<u8>> {
    use std::os::unix::fs::FileExt;
    let f = fs::File::open(dev).ok()?;
    let mut bs = [0u8; 512];
    f.read_exact_at(&mut bs, 0).ok()?;
    let sectors = match u16::from_le_bytes([bs[19], bs[20]]) {
        0 => u32::from_le_bytes([bs[32], bs[33], bs[34], bs[35]]) as usize,
        n => n as usize,
    };
    let payload = u16::from_le_bytes([bs[11], bs[12]]) as usize * sectors;
    if bs[510..512] != [0x55, 0xAA] || payload == 0 || payload >= osimage::MAX_IMAGE_BYTES {
        return None;
    }
    let mut tail = [0u8; 1024];
    let n = f.read_at(&mut tail, payload as u64).ok()?;
    // The trailer ends with its length (header and signature) and "K10S-END".
    let end = (12..=n).find(|&e| &tail[e - 8..e] == b"K10S-END" && u32::from_le_bytes(tail[e - 12..e - 8].try_into().unwrap()) as usize == e - 12)?;
    let mut blob = vec![0u8; payload + end];
    f.read_exact_at(&mut blob, 0).ok()?;
    osimage::meta(&blob).ok().map(|_| blob)
}

/// Writes `blob` to a Pi slot partition and reads it back from the device (the page cache dropped).
fn write_slot(dev: &Path, blob: &[u8]) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::FileExt;
    let f = fs::OpenOptions::new().read(true).write(true).open(dev)?;
    f.write_all_at(blob, 0)?;
    f.sync_all()?;
    // SAFETY: advice on our own descriptor.
    unsafe { libc::posix_fadvise(f.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
    let mut back = vec![0u8; blob.len()];
    f.read_exact_at(&mut back, 0)?;
    if sha256(&back) != sha256(blob) {
        return Err(io::Error::other("the slot reads back different data"));
    }
    Ok(())
}

/// The ESP's files, written only through these operations. Tests cut the power after any of them.
pub struct Esp {
    root: PathBuf,
    /// Remount the real ESP read-write around writes.
    remount: bool,
    #[cfg(test)]
    pub budget: std::cell::Cell<Option<usize>>,
}

impl Esp {
    pub fn new(root: impl Into<PathBuf>, remount: bool) -> Esp {
        Esp {
            root: root.into(),
            remount,
            #[cfg(test)]
            budget: std::cell::Cell::new(None),
        }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    /// One durable step; in tests, the power fails once the budget is spent.
    fn step(&self) -> io::Result<()> {
        #[cfg(test)]
        match self.budget.get() {
            Some(0) => return Err(io::Error::other("power cut")),
            Some(n) => self.budget.set(Some(n - 1)),
            None => {}
        }
        Ok(())
    }

    fn read(&self, rel: &str) -> Option<Vec<u8>> {
        fs::read(self.path(rel)).ok()
    }

    fn sync_dir(&self, rel: &str) -> io::Result<()> {
        fs::File::open(self.path(rel))?.sync_all()
    }

    fn parent(rel: &str) -> &str {
        rel.rsplit_once('/').map_or("", |(d, _)| d)
    }

    /// `rel` replaced by `data`: written to `rel.tmp`, synced, renamed (see [`Esp::rename`]).
    fn write(&self, rel: &str, data: &[u8]) -> io::Result<()> {
        self.step()?;
        let tmp = format!("{rel}.tmp");
        fs::create_dir_all(self.path(Self::parent(rel)))?;
        self.sync_dir(Self::parent(Self::parent(rel)))?;
        let mut f = fs::File::create(self.path(&tmp))?;
        f.write_all(data)?;
        f.sync_all()?;
        self.rename(&tmp, rel)
    }

    /// vfat creates the new name's directory entry empty (no cluster, size 0) and fills it when the file's inode
    /// is written back: the renamed file is synced, then the directory.
    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        self.step()?;
        fs::rename(self.path(from), self.path(to))?;
        fs::File::open(self.path(to))?.sync_all()?;
        self.sync_dir(Self::parent(to))
    }

    fn remove(&self, rel: &str) -> io::Result<()> {
        self.step()?;
        match fs::remove_file(self.path(rel)) {
            Ok(()) => self.sync_dir(Self::parent(rel)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// GRUB's environment block is rewritten in place (GRUB's `save_env` writes the same sectors); every variable
    /// fits the first sector, which disks write atomically.
    fn write_grubenv(&self, vars: &[(&str, &str)]) -> io::Result<()> {
        self.step()?;
        let mut text = GRUBENV_HEADER.to_string();
        for (k, v) in vars {
            text.push_str(&format!("{k}={v}\n"));
        }
        let mut block = text.into_bytes();
        debug_assert!(block.len() <= 512);
        block.resize(GRUBENV_LEN, b'#');
        let p = self.path("boot/grub/grubenv");
        fs::create_dir_all(self.path("boot/grub"))?;
        let mut f = fs::OpenOptions::new().write(true).create(true).truncate(false).open(&p)?;
        f.write_all(&block)?;
        f.set_len(GRUBENV_LEN as u64)?;
        f.sync_all()
    }

    fn grubenv(&self, key: &str) -> Option<String> {
        let b = self.read("boot/grub/grubenv")?;
        let t = String::from_utf8_lossy(&b).into_owned();
        t.lines().find_map(|l| l.strip_prefix(key)?.strip_prefix('=').map(str::to_string)).filter(|v| !v.is_empty())
    }

    fn set_default(&self, pattern: &str) -> io::Result<()> {
        self.write("loader/loader.conf", LOADER_CONF.replace("{}", pattern).as_bytes())
    }

    /// The UKI file name of `slot` in `EFI/Linux`, with its boot counter if it has one.
    fn uki(&self, slot: Slot) -> Option<String> {
        let prefix = format!("k10s-{}", slot.letter());
        let mut names: Vec<String> = fs::read_dir(self.path("EFI/Linux"))
            .ok()?
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| {
                let l = n.to_ascii_lowercase();
                l.ends_with(".efi") && l.strip_prefix(&prefix).is_some_and(|r| r.starts_with('.') || r.starts_with('+'))
            })
            .collect();
        names.sort();
        names.pop()
    }

    fn begin(&self) -> io::Result<()> {
        if self.remount {
            remount(false)?;
        }
        Ok(())
    }

    fn end(&self) {
        if self.remount {
            if let Err(e) = remount(true) {
                say!("update: remounting K10S-BOOT read-only: {e}");
            }
        }
    }
}

fn remount(ro: bool) -> io::Result<()> {
    let flags = libc::MS_REMOUNT | libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC | if ro { libc::MS_RDONLY } else { 0 };
    // SAFETY: valid C strings; remount of our own mount point.
    if unsafe { libc::mount(std::ptr::null(), c"/boot".as_ptr(), std::ptr::null(), flags, std::ptr::null()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Tries left of a counted UKI name (`k10s-b+2-1.efi` → 2).
fn tries_left(name: &str) -> Option<u32> {
    let stem = name.strip_suffix(".efi")?;
    let (_, counter) = stem.rsplit_once('+')?;
    counter.split('-').next()?.parse().ok()
}

/// The kernel, initramfs and command line of a UKI (PE32+ sections `.linux`, `.initrd`, `.cmdline`).
pub fn uki_sections(pe: &[u8]) -> Result<(&[u8], &[u8], String), String> {
    let u16_at = |o: usize| pe.get(o..o + 2).map(|b| u16::from_le_bytes([b[0], b[1]]));
    let u32_at = |o: usize| pe.get(o..o + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    let bad = || "not a UKI".to_string();
    let lfanew = u32_at(0x3c).ok_or_else(bad)? as usize;
    if pe.get(..2) != Some(b"MZ") || pe.get(lfanew..lfanew + 4) != Some(b"PE\0\0") {
        return Err(bad());
    }
    let nsec = u16_at(lfanew + 6).ok_or_else(bad)? as usize;
    let table = lfanew + 24 + u16_at(lfanew + 20).ok_or_else(bad)? as usize;
    let mut found: [Option<&[u8]>; 3] = [None; 3];
    for i in 0..nsec {
        let h = table + 40 * i;
        let name = pe.get(h..h + 8).ok_or_else(bad)?;
        let (vsize, raw, ptr) = (u32_at(h + 8).ok_or_else(bad)?, u32_at(h + 16).ok_or_else(bad)?, u32_at(h + 20).ok_or_else(bad)?);
        let data = pe.get(ptr as usize..ptr as usize + vsize.min(raw) as usize).ok_or_else(bad)?;
        for (j, want) in [b".linux\0\0", b".initrd\0", b".cmdline"].iter().enumerate() {
            if name == *want {
                found[j] = Some(data);
            }
        }
    }
    match found {
        [Some(k), Some(i), Some(c)] => Ok((k, i, String::from_utf8_lossy(c).trim_matches(|c: char| c == '\0' || c.is_whitespace()).to_string())),
        _ => Err("the UKI lacks a .linux, .initrd or .cmdline section".into()),
    }
}

/// The slots, as found at boot and as changed by updates.
pub struct Slots {
    esp: Esp,
    firmware: Firmware,
    /// The slot this boot came from (unknown when not booted through the image's loaders: no updates then).
    pub booted: Option<Slot>,
    /// This boot is a trial of `booted`.
    pub trial: bool,
    /// SHA-256 of the booted slot's image, and its `ARCH-PROFILE`.
    pub image: Option<[u8; 32]>,
    pub platform: Option<String>,
    pub failed: Vec<[u8; 32]>,
    /// Pi: the slot partitions.
    pi: Option<[PathBuf; 2]>,
    /// Pi: a trial was interrupted (power cut, crash); this boot reboots into it again.
    pub retry: bool,
}

impl Slots {
    /// Reads the boot state. `efi_entry` is systemd-boot's `LoaderEntrySelected`, `cmdline_slot` the `slot` key
    /// GRUB passes. A trial of the other slot that ended here is recorded as failed and its files removed;
    /// leftovers of an update cut short before its trial are removed too.
    pub fn open(esp: Esp, firmware: Firmware, efi_entry: Option<&str>, cmdline_slot: Option<&str>) -> Slots {
        let failed = esp
            .read("k10s/failed")
            .map(|b| String::from_utf8_lossy(&b).lines().filter_map(|l| parse_hex32(l.trim())).collect())
            .unwrap_or_default();
        let (booted, trial) = match firmware {
            Firmware::Uefi => {
                let e = efi_entry.map(str::to_ascii_lowercase);
                let slot = e.as_deref().and_then(|e| e.strip_prefix("k10s-")).and_then(|r| r.chars().next()).and_then(Slot::parse);
                (slot, e.is_some_and(|e| e.contains('+')))
            }
            Firmware::Bios => {
                let slot = cmdline_slot.and_then(|s| s.chars().next()).and_then(Slot::parse);
                let t = esp.grubenv("k10s_try");
                (slot, slot.is_some_and(|s| t.is_some_and(|t| t.starts_with(s.letter()))))
            }
            Firmware::Pi => (None, false),
        };
        let mut s = Slots { esp, firmware, booted, trial, image: None, platform: None, failed, pi: None, retry: false };
        if let Some(name) = booted.and_then(|b| s.esp.uki(b)) {
            if let Some(blob) = s.esp.read(&format!("EFI/Linux/{name}")) {
                s.image = Some(sha256(&blob));
                s.platform = osimage::meta(&blob).ok().map(|i| format!("{}-{}", i.meta.arch, i.meta.profile));
            }
        }
        if let (Some(b), false) = (booted, trial) {
            if s.leftovers(b) {
                if let Err(e) = s.esp.begin().and_then(|()| s.settle(b)) {
                    say!("update: cleaning up slot {}: {e}", b.other().letter());
                }
                s.esp.end();
            }
        }
        s
    }

    /// Reads the boot state on a Pi. A committed boot that finds an interrupted trial reboots into it again while it
    /// has tries left, else records its image as failed; `autoboot.txt` is made to point at the booted slot.
    pub fn open_pi(esp: Esp, pi: PiBoot) -> Slots {
        let mut s = Slots::open(esp, Firmware::Pi, None, None);
        let committed = s.esp.read(AUTOBOOT).and_then(|b| autoboot_committed(&String::from_utf8_lossy(&b)));
        s.booted = pi.partition.and_then(Slot::of_partition);
        s.trial = pi.tryboot && s.booted.is_some() && s.booted != committed;
        if let Some(blob) = s.booted.and_then(|b| read_slot(&pi.parts[b.index()])) {
            s.image = Some(sha256(&blob));
            s.platform = osimage::meta(&blob).ok().map(|i| format!("{}-{}", i.meta.arch, i.meta.profile));
        }
        s.pi = Some(pi.parts);
        let Some(b) = s.booted.filter(|_| !s.trial) else { return s };
        let trying = s.esp.read(TRYING).map(|t| String::from_utf8_lossy(&t).into_owned());
        let trying = trying.as_deref().and_then(|t| t.split_once(' ')).and_then(|(h, n)| Some((parse_hex32(h)?, n.trim().parse::<u32>().ok()?)));
        if trying.is_none() && committed == Some(b) {
            return s;
        }
        let r = s.esp.begin().and_then(|()| {
            match trying {
                Some((h, _)) if s.image == Some(h) => {}
                Some((h, left)) if left > 0 => {
                    say!("update: the trial of slot {} ({}) was interrupted; booting it again", b.other().letter(), short(&h));
                    s.esp.write(TRYING, format!("{} {}\n", to_hex(&h), left - 1).as_bytes())?;
                    s.retry = true;
                }
                Some((h, _)) => {
                    say!("update: slot {} ({}) did not come up in {TRIES} boots; back on slot {}", b.other().letter(), short(&h), b.letter());
                    s.record_failed(h)?;
                }
                None => {}
            }
            if !s.retry {
                s.esp.remove(TRYING)?;
            }
            if committed != Some(b) {
                s.esp.write(AUTOBOOT, autoboot(b).as_bytes())?;
            }
            Ok(())
        });
        s.esp.end();
        if let Err(e) = r {
            say!("update: K10S-BOOT: {e}");
        }
        s
    }

    /// Anything of an update of the other slot: its counted UKI, a GRUB trial, or a loader default not on `b`.
    fn leftovers(&self, b: Slot) -> bool {
        let t = b.other();
        self.esp.uki(t).is_some_and(|n| n.contains('+'))
            || self.esp.grubenv("k10s_try").is_some()
            || self.esp.grubenv("k10s_slot").is_some_and(|s| !s.starts_with(b.letter()))
            || self.esp.read("loader/loader.conf").is_some_and(|c| !String::from_utf8_lossy(&c).contains(&format!("default k10s-{}.efi", b.letter())))
    }

    /// Booted the committed slot `b` with update leftovers: a trial of the other slot that used up its tries is
    /// a failed image; either way the other slot is cleared and both loaders point at `b`.
    fn settle(&mut self, b: Slot) -> io::Result<()> {
        let t = b.other();
        let tried_out = match self.firmware {
            Firmware::Uefi => self.esp.uki(t).is_some_and(|n| tries_left(&n) == Some(0)),
            Firmware::Bios => self.esp.grubenv("k10s_try").is_some_and(|v| v == format!("{}0", t.letter())),
            Firmware::Pi => false,
        };
        if tried_out {
            if let Some(h) = self.esp.uki(t).and_then(|n| self.esp.read(&format!("EFI/Linux/{n}"))).map(|blob| sha256(&blob)) {
                say!("update: slot {} ({}) did not come up in {TRIES} boots; back on slot {}", t.letter(), short(&h), b.letter());
                self.record_failed(h)?;
            }
        }
        self.clear(t)?;
        if let Some(n) = self.esp.uki(b).filter(|n| n.contains('+')) {
            self.esp.rename(&format!("EFI/Linux/{n}"), &format!("EFI/Linux/k10s-{}.efi", b.letter()))?;
        }
        self.esp.write_grubenv(&[("k10s_slot", &b.letter().to_string())])?;
        self.esp.set_default(&format!("k10s-{}.efi", b.letter()))
    }

    fn record_failed(&mut self, h: [u8; 32]) -> io::Result<()> {
        self.failed.retain(|f| *f != h);
        self.failed.push(h);
        let skip = self.failed.len().saturating_sub(MAX_FAILED);
        self.failed.drain(..skip);
        let text: String = self.failed.iter().map(|f| format!("{}\n", to_hex(f))).collect();
        self.esp.write("k10s/failed", text.as_bytes())
    }

    fn clear(&self, t: Slot) -> io::Result<()> {
        while let Some(n) = self.esp.uki(t) {
            self.esp.remove(&format!("EFI/Linux/{n}"))?;
        }
        for f in ["vmlinuz", "initrd", "boot.cfg"] {
            self.esp.remove(&format!("k10s/{}/{f}", t.letter()))?;
        }
        Ok(())
    }

    /// The report for the node; `None` when this boot cannot be identified (no updates then).
    pub fn status(&self) -> Option<OsStatus> {
        Some(OsStatus {
            platform: self.platform.clone()?,
            image: self.image?,
            committed: !self.trial,
            updatable: self.booted.is_some(),
            failed: self.failed.last().copied(),
        })
    }

    /// Whether a request for `h` is acceptable now at all (refusals here say nothing about the image).
    pub fn can_install(&self, h: &[u8; 32]) -> Result<Slot, String> {
        let b = self.booted.ok_or("not booted from a slot")?;
        if self.trial || self.retry {
            return Err("this boot is a trial: it commits or falls back first".into());
        }
        if self.image == Some(*h) {
            return Err("already running this image".into());
        }
        if self.failed.contains(h) {
            return Err("this image failed here before".into());
        }
        Ok(b)
    }

    /// Records an image that cannot be installed here as failed (the rollout stops on it).
    pub fn refuse(&mut self, h: [u8; 32]) -> io::Result<()> {
        self.esp.begin()?;
        let r = self.record_failed(h);
        self.esp.end();
        r
    }

    /// Verifies `blob` (hash `h`, signed by one of `trusted`, for this platform), writes it to the other slot and
    /// arms its trial boot. The caller reboots on success.
    pub fn install(&mut self, h: &[u8; 32], blob: &[u8], trusted: &[[u8; 32]]) -> Result<Slot, String> {
        let b = self.can_install(h)?;
        if sha256(blob) != *h {
            return Err("the blob does not match its hash".into());
        }
        let info = osimage::verify(blob, trusted).map_err(|e| e.to_string())?;
        let platform = format!("{}-{}", info.meta.arch, info.meta.profile);
        if Some(&platform) != self.platform.as_ref() {
            return Err(format!("image for {platform}, this node runs {}", self.platform.as_deref().unwrap_or("?")));
        }
        let t = b.other();
        if let Some(parts) = &self.pi {
            say!("update: verified {} ({platform} {}, signed by {}); writing slot {}", short(h), info.meta.version, short(&info.key), t.letter());
            self.esp.begin().map_err(|e| format!("remounting K10S-BOOT: {e}"))?;
            let r = (|| -> io::Result<()> {
                // The firmware boots the other partition only through tryboot, so a partial write is never booted.
                self.esp.step()?;
                write_slot(&parts[t.index()], blob)?;
                say!("update: slot {} written; arming its trial boot", t.letter());
                self.esp.write(TRYING, format!("{} {}\n", to_hex(h), TRIES - 1).as_bytes())?;
                if self.esp.read(AUTOBOOT).as_deref() != Some(autoboot(b).as_bytes()) {
                    self.esp.write(AUTOBOOT, autoboot(b).as_bytes())?;
                }
                Ok(())
            })();
            self.esp.end();
            r.map_err(|e| format!("writing slot {}: {e}", t.letter()))?;
            return Ok(t);
        }
        let (kernel, initrd, cmdline) = uki_sections(&blob[..info.payload_len])?;
        say!("update: verified {} ({platform} {}, signed by {}); writing slot {}", short(h), info.meta.version, short(&info.key), t.letter());
        self.esp.begin().map_err(|e| format!("remounting K10S-BOOT: {e}"))?;
        let r = (|| -> io::Result<()> {
            self.clear(t)?;
            // GRUB's copy for BIOS boots (x86 only).
            if cfg!(target_arch = "x86_64") {
                let d = format!("k10s/{}", t.letter());
                self.esp.write(&format!("{d}/vmlinuz"), kernel)?;
                self.esp.write(&format!("{d}/initrd"), initrd)?;
                let cfg = format!("linux /{d}/vmlinuz {cmdline} k10s.slot={}\ninitrd /{d}/initrd\n", t.letter());
                self.esp.write(&format!("{d}/boot.cfg"), cfg.as_bytes())?;
            }
            let name = format!("k10s-{}", t.letter());
            self.esp.write(&format!("EFI/Linux/{name}.new"), blob)?;
            self.esp.rename(&format!("EFI/Linux/{name}.new"), &format!("EFI/Linux/{name}+{TRIES}.efi"))?;
            say!("update: slot {} written; arming its trial boot", t.letter());
            self.esp.write_grubenv(&[("k10s_slot", &b.letter().to_string()), ("k10s_try", &format!("{}{TRIES}", t.letter()))])?;
            self.esp.set_default(&format!("k10s-{}+[1-9]*", t.letter()))
        })();
        self.esp.end();
        r.map_err(|e| format!("writing slot {}: {e}", t.letter()))?;
        Ok(t)
    }

    /// The trial boot is good: it becomes the committed slot.
    pub fn commit(&mut self) -> io::Result<()> {
        let (Some(b), true) = (self.booted, self.trial) else { return Ok(()) };
        self.esp.begin()?;
        let r = (|| -> io::Result<()> {
            if self.pi.is_some() {
                self.esp.write(AUTOBOOT, autoboot(b).as_bytes())?;
                return self.esp.remove(TRYING);
            }
            let l = b.letter();
            self.esp.write_grubenv(&[("k10s_slot", &l.to_string())])?;
            // While the counted file is renamed, `k10s-X*` matches it under either name.
            self.esp.set_default(&format!("k10s-{l}*"))?;
            if let Some(n) = self.esp.uki(b).filter(|n| n.contains('+')) {
                self.esp.rename(&format!("EFI/Linux/{n}"), &format!("EFI/Linux/k10s-{l}.efi"))?;
            }
            self.esp.set_default(&format!("k10s-{l}.efi"))
        })();
        self.esp.end();
        r?;
        self.trial = false;
        Ok(())
    }

    /// The trial did not get good in time: record it as failed and point both loaders back at the committed slot.
    /// The caller reboots.
    pub fn abandon(&mut self) -> io::Result<()> {
        let (Some(b), true) = (self.booted, self.trial) else { return Ok(()) };
        let s = b.other();
        self.esp.begin()?;
        let r = (|| -> io::Result<()> {
            if let Some(h) = self.image {
                self.record_failed(h)?;
            }
            if self.pi.is_some() {
                return self.esp.remove(TRYING);
            }
            self.esp.write_grubenv(&[("k10s_slot", &s.letter().to_string())])?;
            self.esp.set_default(&format!("k10s-{}.efi", s.letter()))
        })();
        self.esp.end();
        r
    }
}

fn short(h: &[u8; 32]) -> String {
    to_hex(&h[..4])
}

/// Init's end of the update channel ([`k10_node::cluster::os`]): one socket pair per node start, the slots, the
/// deadline of a trial boot. Asks init's main loop for a reboot (SIGUSR1) once a slot is armed or abandoned.
pub struct Agent {
    tx: mpsc::Sender<UnixStream>,
}

impl Agent {
    pub fn spawn(mut slots: Slots, trusted: Vec<[u8; 32]>, data: PathBuf, trial: Duration) -> Agent {
        let (tx, rx) = mpsc::channel::<UnixStream>();
        let deadline = slots.trial.then(|| Instant::now() + trial);
        let tryboot = slots.firmware == Firmware::Pi;
        if let (Some(b), true) = (slots.booted, slots.trial) {
            say!("update: trial boot of slot {}; it commits once the node rejoined and serves (within {} s)", b.letter(), trial.as_secs());
        }
        let _ = std::thread::Builder::new().name("update".into()).spawn(move || {
            let mut conn: Option<(UnixStream, Vec<u8>)> = None;
            if slots.retry {
                request_reboot(true);
            }
            loop {
                while let Ok(s) = rx.try_recv() {
                    conn = Some((s, Vec::new()));
                    send_status(&mut conn, &slots);
                }
                if slots.trial && deadline.is_some_and(|d| Instant::now() >= d) {
                    say!("update: the trial boot did not commit within {} s; falling back", trial.as_secs());
                    if let Err(e) = slots.abandon() {
                        say!("update: {e}");
                    }
                    request_reboot(false);
                }
                let Some((s, buf)) = conn.as_mut() else {
                    // The node waits for its first status line: hand it over as soon as the node starts.
                    if let Ok(s) = rx.recv_timeout(Duration::from_millis(500)) {
                        conn = Some((s, Vec::new()));
                        send_status(&mut conn, &slots);
                    }
                    continue;
                };
                let _ = s.set_read_timeout(Some(Duration::from_millis(500)));
                let mut chunk = [0u8; 512];
                match s.read(&mut chunk) {
                    Ok(0) => conn = None,
                    Ok(n) if buf.len() + n <= 4096 => buf.extend_from_slice(&chunk[..n]),
                    Ok(_) => conn = None,
                    Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted) => {}
                    Err(_) => conn = None,
                }
                let Some((_, buf)) = conn.as_mut() else { continue };
                let lines: Vec<String> = std::iter::from_fn(|| {
                    let i = buf.iter().position(|&b| b == b'\n')?;
                    let l: Vec<u8> = buf.drain(..=i).collect();
                    Some(String::from_utf8_lossy(&l[..i]).into_owned())
                })
                .collect();
                for l in lines {
                    let (verb, arg) = l.split_once(' ').unwrap_or((&l, ""));
                    let Some(h) = parse_hex32(arg) else { continue };
                    match verb {
                        "update" => match slots.can_install(&h) {
                            Err(e) => say!("update: not installing {}: {e}", short(&h)),
                            Ok(_) => {
                                let r = read_blob(&data, &h).map_err(|e| format!("reading the blob: {e}")).and_then(|b| slots.install(&h, &b, &trusted));
                                match r {
                                    Ok(t) => {
                                        say!("update: slot {} armed; rebooting into it", t.letter());
                                        request_reboot(tryboot);
                                    }
                                    Err(e) => {
                                        say!("update: refusing {}: {e}", short(&h));
                                        if let Err(e) = slots.refuse(h) {
                                            say!("update: recording {}: {e}", short(&h));
                                        }
                                        send_line(&mut conn, &format!("error refusing {}: {e}\n", to_hex(&h)));
                                        send_status(&mut conn, &slots);
                                    }
                                }
                            }
                        },
                        "ready" if slots.trial && slots.image == Some(h) => match slots.commit() {
                            Ok(()) => {
                                say!("update: committed slot {} ({})", slots.booted.map_or('?', Slot::letter), short(&h));
                                send_status(&mut conn, &slots);
                            }
                            Err(e) => say!("update: committing: {e}"),
                        },
                        _ => {}
                    }
                }
            }
        });
        Agent { tx }
    }

    /// A socket pair for a node start: this end goes to the agent, the returned one becomes the node's fd 3.
    pub fn connect(&self) -> io::Result<OwnedFd> {
        let (mine, theirs) = UnixStream::pair()?;
        self.tx.send(mine).map_err(|_| io::Error::other("the update agent stopped"))?;
        Ok(theirs.into())
    }
}

fn send_line(conn: &mut Option<(UnixStream, Vec<u8>)>, line: &str) {
    if let Some((s, _)) = conn.as_mut() {
        if s.write_all(line.as_bytes()).is_err() {
            *conn = None;
        }
    }
}

fn send_status(conn: &mut Option<(UnixStream, Vec<u8>)>, slots: &Slots) {
    if let Some(st) = slots.status() {
        send_line(conn, &st.line());
    }
}

/// SIGUSR1 to init: its main loop stops the node (it leaves the cluster) and reboots, with the Pi's tryboot flag when
/// `tryboot`. The agent then parks.
fn request_reboot(tryboot: bool) -> ! {
    TRYBOOT.store(tryboot, std::sync::atomic::Ordering::SeqCst);
    // SAFETY: a process-directed signal to ourselves; every thread blocks it and the main loop waits for it.
    unsafe { libc::kill(libc::getpid(), libc::SIGUSR1) };
    loop {
        std::thread::park();
    }
}

/// The node's request `update IMAGE`: the blob it stored (`DATA/blobs/IMAGE`), read without following links and
/// bounded in size (the node is less trusted than init).
pub fn read_blob(data: &Path, h: &[u8; 32]) -> io::Result<Vec<u8>> {
    use std::os::unix::fs::OpenOptionsExt;
    let p = data.join("blobs").join(to_hex(h));
    let mut f = fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(&p)?;
    let m = f.metadata()?;
    if !m.is_file() || m.len() > osimage::MAX_IMAGE_BYTES as u64 {
        return Err(io::Error::other("not a regular file of at most 64 MiB"));
    }
    let mut v = Vec::with_capacity(m.len() as usize);
    Read::take(&mut f, osimage::MAX_IMAGE_BYTES as u64 + 1).read_to_end(&mut v)?;
    Ok(v)
}

/// systemd-boot's `LoaderEntrySelected` from efivarfs (4 attribute bytes, then UTF-16LE).
pub fn efi_selected_entry() -> Option<String> {
    let b = fs::read("/sys/firmware/efi/efivars/LoaderEntrySelected-4a67b082-0a4c-41cf-b6c7-440b29bb8c4f").ok()?;
    let units: Vec<u16> = b.get(4..)?.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&u| u != 0).collect();
    String::from_utf16(&units).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use k10_mesh::crypto::ed25519;

    const SEED: [u8; 32] = [11; 32];

    /// A minimal PE32+ with the three UKI sections (what os/mkimage.py builds, without the stub's code).
    fn fake_uki(tag: &str) -> Vec<u8> {
        let secs: [(&[u8; 8], Vec<u8>); 3] =
            [(b".cmdline", format!("console=ttyS0 {tag}\0").into_bytes()), (b".initrd\0", vec![7; 300]), (b".linux\0\0", tag.as_bytes().repeat(50))];
        let mut pe = vec![0u8; 0x200];
        pe[..2].copy_from_slice(b"MZ");
        pe[0x3c..0x40].copy_from_slice(&0x40u32.to_le_bytes());
        pe[0x40..0x44].copy_from_slice(b"PE\0\0");
        pe[0x46..0x48].copy_from_slice(&3u16.to_le_bytes());
        pe[0x54..0x56].copy_from_slice(&0u16.to_le_bytes());
        let mut off = 0x200;
        for (i, (name, data)) in secs.iter().enumerate() {
            let h = 0x40 + 24 + 40 * i;
            pe[h..h + 8].copy_from_slice(*name);
            pe[h + 8..h + 12].copy_from_slice(&(data.len() as u32).to_le_bytes());
            pe[h + 16..h + 20].copy_from_slice(&(data.len().div_ceil(512) as u32 * 512).to_le_bytes());
            pe[h + 20..h + 24].copy_from_slice(&(off as u32).to_le_bytes());
            off += data.len().div_ceil(512) * 512;
        }
        for (_, data) in &secs {
            let mut d = data.clone();
            d.resize(d.len().div_ceil(512) * 512, 0);
            pe.extend_from_slice(&d);
        }
        pe
    }

    fn image(tag: &str) -> (Vec<u8>, [u8; 32]) {
        let meta = osimage::ImageMeta { arch: "x86_64".into(), profile: "vm".into(), version: tag.into() };
        let blob = osimage::sign(&fake_uki(tag), &meta, &SEED).unwrap();
        let h = sha256(&blob);
        (blob, h)
    }

    /// A fresh ESP as os/mkimage.py writes it: slot A committed.
    fn fresh(dir: &Path, a: &[u8]) -> Esp {
        let _ = fs::remove_dir_all(dir);
        let esp = Esp::new(dir, false);
        esp.write("EFI/Linux/k10s-a.efi", a).unwrap();
        esp.set_default("k10s-a.efi").unwrap();
        esp.write_grubenv(&[("k10s_slot", "a")]).unwrap();
        esp
    }

    fn fnmatch(pat: &str, s: &str) -> bool {
        match (pat.chars().next(), s.chars().next()) {
            (None, None) => true,
            (Some('*'), _) => fnmatch(&pat[1..], s) || (!s.is_empty() && fnmatch(pat, &s[s.chars().next().unwrap().len_utf8()..])),
            (Some('['), Some(c)) => {
                let end = pat.find(']').unwrap();
                let set = &pat[1..end];
                let ok = set.as_bytes().windows(3).any(|w| w[1] == b'-' && (w[0] as char..=w[2] as char).contains(&c)) || set.contains(c);
                ok && fnmatch(&pat[end + 1..], &s[c.len_utf8()..])
            }
            (Some(p), Some(c)) => p == c && fnmatch(&pat[1..], &s[c.len_utf8()..]),
            _ => false,
        }
    }

    /// systemd-boot 252: entries sorted (no tries left last, then by id descending), the first matching
    /// `default`, else the first; a counted entry is renamed before it boots. Returns the booted file's id.
    fn uefi_boot(dir: &Path) -> Option<String> {
        let conf = fs::read_to_string(dir.join("loader/loader.conf")).ok()?;
        let pat = conf.lines().find_map(|l| l.strip_prefix("default ")).unwrap().to_string();
        let mut entries: Vec<String> = fs::read_dir(dir.join("EFI/Linux")).ok()?.flatten().map(|e| e.file_name().into_string().unwrap()).filter(|n| n.ends_with(".efi")).collect();
        entries.sort_by(|a, b| (tries_left(a) == Some(0)).cmp(&(tries_left(b) == Some(0))).then(b.cmp(a)));
        let id = entries.iter().find(|e| fnmatch(&pat, e)).or(entries.first())?.clone();
        if let Some(left) = tries_left(&id).filter(|&l| l > 0) {
            let stem = id.rsplit_once('+').unwrap().0;
            let done: u32 = id.strip_suffix(".efi").unwrap().rsplit_once('+').unwrap().1.split('-').nth(1).map_or(0, |d| d.parse().unwrap());
            fs::rename(dir.join("EFI/Linux").join(&id), dir.join("EFI/Linux").join(format!("{stem}+{}-{}.efi", left - 1, done + 1))).unwrap();
        }
        Some(id)
    }

    /// os/mkimage.py's grub.cfg: returns the slot GRUB boots.
    fn bios_boot(esp: &Esp) -> char {
        let mut slot = esp.grubenv("k10s_slot").unwrap_or_default();
        let try_ = esp.grubenv("k10s_try").unwrap_or_default();
        let mut vars = vec![("k10s_slot".to_string(), slot.clone())];
        for s in ["a", "b"] {
            for n in (1..=TRIES).rev() {
                if try_ == format!("{s}{n}") {
                    vars.push(("k10s_try".into(), format!("{s}{}", n - 1)));
                    slot = s.into();
                }
            }
        }
        if vars.len() == 1 && !try_.is_empty() {
            vars.push(("k10s_try".into(), try_));
        }
        let v: Vec<(&str, &str)> = vars.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        esp.write_grubenv(&v).unwrap();
        if slot == "b" { 'b' } else { 'a' }
    }

    /// One boot on `fw` up to init's slot inspection.
    fn boot(dir: &Path, fw: Firmware) -> Slots {
        let esp = Esp::new(dir, false);
        let s = match fw {
            Firmware::Uefi => {
                let id = uefi_boot(dir).expect("UEFI finds an entry");
                Slots::open(esp, fw, Some(&id), None)
            }
            Firmware::Bios => {
                let c = bios_boot(&esp);
                assert!(dir.join(format!("k10s/{c}/boot.cfg")).exists() || c == 'a', "GRUB boots a slot that has files");
                Slots::open(esp, fw, None, Some(&c.to_string()))
            }
            Firmware::Pi => unreachable!("pi_boot"),
        };
        s
    }

    /// The loaders of the UEFI/BIOS images (BIOS: x86 only).
    const LOADERS: &[Firmware] = if cfg!(target_arch = "x86_64") { &[Firmware::Uefi, Firmware::Bios] } else { &[Firmware::Uefi] };

    fn trusted() -> Vec<[u8; 32]> {
        vec![ed25519::public_key(&SEED)]
    }

    fn tmp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("k10s-update-{}-{name}", std::process::id()))
    }

    #[test]
    fn sections_and_keys() {
        let (blob, _) = image("v1");
        let info = osimage::inspect(&blob).unwrap();
        let (k, i, c) = uki_sections(&blob[..info.payload_len]).unwrap();
        assert_eq!((k.len(), i.len(), c.as_str()), (100, 300, "console=ttyS0 v1"));
        assert!(uki_sections(b"MZ").is_err());
        assert_eq!(parse_keys(&format!("# release\n{}  # x\n\nzz\n", to_hex(&[5; 32]))), vec![[5; 32]]);
        assert_eq!((tries_left("k10s-b+2-1.efi"), tries_left("k10s-b+0-3.efi"), tries_left("k10s-b.efi")), (Some(2), Some(0), None));
        assert!(fnmatch("k10s-b+[1-9]*", "k10s-b+3.efi") && !fnmatch("k10s-b+[1-9]*", "k10s-b+0-3.efi") && fnmatch("k10s-a.efi", "k10s-a.efi"));
    }

    /// Cuts the power after every durable step of an update (and of its commit), on both firmware types: every
    /// boot comes up on a good image, and repeating the update afterwards ends on the new image.
    #[test]
    fn power_cut_at_every_step() {
        let (a, ha) = image("v1");
        let (b, hb) = image("v2");
        for &fw in LOADERS {
            for cut in 0.. {
                let dir = tmp(&format!("cut-{fw:?}-{cut}"));
                fresh(&dir, &a);
                let mut s = boot(&dir, fw);
                assert_eq!((s.booted, s.trial, s.image), (Some(Slot::A), false, Some(ha)));
                s.esp.budget.set(Some(cut));
                let installed = s.install(&hb, &b, &trusted()).is_ok();
                // Power cut (or the reboot after a complete install): the next boot is a good image.
                let mut s = boot(&dir, fw);
                assert!(s.image == Some(ha) || s.image == Some(hb), "{fw:?} cut {cut}: booted {:?}", s.image);
                assert!(s.failed.is_empty(), "{fw:?} cut {cut}: nothing failed");
                if s.image == Some(ha) {
                    assert!(!installed && !s.trial);
                    s.install(&hb, &b, &trusted()).unwrap();
                    s = boot(&dir, fw);
                }
                assert_eq!((s.image, s.trial), (Some(hb), true), "{fw:?} cut {cut}");
                // Cut the commit at the same step, then boot until committed.
                s.esp.budget.set(Some(cut));
                let committed = s.commit().is_ok();
                let mut s = boot(&dir, fw);
                assert_eq!(s.image, Some(hb), "{fw:?} cut {cut}: the commit never falls back");
                if s.trial {
                    assert!(!committed);
                    s.commit().unwrap();
                    s = boot(&dir, fw);
                }
                assert_eq!((s.image, s.trial, s.booted), (Some(hb), false, Some(Slot::B)));
                let _ = fs::remove_dir_all(&dir);
                if installed && committed {
                    break;
                }
            }
        }
    }

    /// A bad image (never reaches init, or never commits): the loaders fall back after the tries, init records it,
    /// refuses it afterwards, and the next good image installs.
    #[test]
    fn bad_images_fall_back() {
        let (a, ha) = image("v1");
        let (bad, hbad) = image("bad");
        let (c, hc) = image("v3");
        for &fw in LOADERS {
            let dir = tmp(&format!("bad-{fw:?}"));
            fresh(&dir, &a);
            let mut s = boot(&dir, fw);
            s.install(&hbad, &bad, &trusted()).unwrap();
            // The bad image crashes before init: the loader just boots again.
            for _ in 0..TRIES {
                match fw {
                    Firmware::Uefi => assert!(uefi_boot(&dir).unwrap().starts_with("k10s-b+")),
                    Firmware::Bios => assert_eq!(bios_boot(&Esp::new(&dir, false)), 'b'),
                    Firmware::Pi => unreachable!(),
                }
            }
            let mut s = boot(&dir, fw);
            assert_eq!((s.image, s.trial, s.failed.clone()), (Some(ha), false, vec![hbad]), "{fw:?}");
            assert!(s.esp.uki(Slot::B).is_none() && !dir.join("k10s/b/vmlinuz").exists());
            assert_eq!(s.status().unwrap().failed, Some(hbad));
            assert!(s.install(&hbad, &bad, &trusted()).unwrap_err().contains("failed here before"));
            // A trial that never commits is abandoned by init (timeout).
            s.install(&hc, &c, &trusted()).unwrap();
            let mut s = boot(&dir, fw);
            assert_eq!((s.image, s.trial), (Some(hc), true));
            s.abandon().unwrap();
            let s = boot(&dir, fw);
            assert_eq!((s.image, s.trial, s.failed.clone()), (Some(ha), false, vec![hbad, hc]), "{fw:?}");
            assert!(s.esp.uki(Slot::B).is_none());
            let _ = fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn refusals() {
        let (a, _) = image("v1");
        let dir = tmp("refuse");
        fresh(&dir, &a);
        let mut s = boot(&dir, Firmware::Uefi);
        let (b, hb) = image("v2");
        assert!(s.install(&hb, &b, &[[1; 32]]).unwrap_err().contains("untrusted"));
        assert!(s.install(&[0; 32], &b, &trusted()).unwrap_err().contains("hash"));
        let metal = osimage::sign(&fake_uki("m"), &osimage::ImageMeta { arch: "x86_64".into(), profile: "metal".into(), version: "m".into() }, &SEED).unwrap();
        assert!(s.install(&sha256(&metal), &metal, &trusted()).unwrap_err().contains("x86_64-metal"));
        let unsigned = fake_uki("u");
        assert!(s.install(&sha256(&unsigned), &unsigned, &trusted()).is_err());
        // Booted without the image's loader: identified, but not updatable.
        let s = Slots::open(Esp::new(&dir, false), Firmware::Uefi, None, None);
        assert!(s.status().is_none() && s.booted.is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    /// A Pi slot image: a FAT boot sector giving its size, signed for aarch64-pi.
    fn pi_image(tag: &str) -> (Vec<u8>, [u8; 32]) {
        let mut fat = vec![0u8; 64 * 512];
        fat[11..13].copy_from_slice(&512u16.to_le_bytes());
        fat[19..21].copy_from_slice(&64u16.to_le_bytes());
        fat[510..512].copy_from_slice(&[0x55, 0xAA]);
        fat[512..512 + tag.len()].copy_from_slice(tag.as_bytes());
        let meta = osimage::ImageMeta { arch: "aarch64".into(), profile: "pi".into(), version: tag.into() };
        let blob = osimage::sign(&fat, &meta, &SEED).unwrap();
        let h = sha256(&blob);
        (blob, h)
    }

    /// A Pi disk as os/mkimage.py writes it: K10S-BOOT (a directory here) with autoboot.txt on slot A, the slot
    /// partitions as files (A holds `a` and whatever followed it, B is blank).
    fn pi_fresh(dir: &Path, a: &[u8]) {
        let _ = fs::remove_dir_all(dir);
        let esp = Esp::new(dir, false);
        esp.write(AUTOBOOT, autoboot(Slot::A).as_bytes()).unwrap();
        fs::write(dir.join("p2"), [a, &[0x5a; 4096]].concat()).unwrap();
        fs::write(dir.join("p3"), vec![0u8; 128 << 10]).unwrap();
    }

    /// One Pi boot: the firmware takes autoboot.txt's [tryboot] partition when the one-shot flag is set, else [all].
    fn pi_boot(dir: &Path, tryboot: bool) -> Slots {
        let text = fs::read_to_string(dir.join(AUTOBOOT)).unwrap();
        let section = if tryboot { "[tryboot]" } else { "[all]" };
        let part = text.split(section).nth(1).unwrap().lines().find_map(|l| l.strip_prefix("boot_partition=")).unwrap().parse().unwrap();
        Slots::open_pi(Esp::new(dir, false), PiBoot { parts: [dir.join("p2"), dir.join("p3")], partition: Some(part), tryboot })
    }

    #[test]
    fn autoboot_files() {
        assert_eq!(autoboot_committed(&autoboot(Slot::B)), Some(Slot::B));
        assert_eq!(autoboot_committed("[tryboot]\nboot_partition=2\n[all]\nboot_partition=3\n"), Some(Slot::B));
        assert_eq!(autoboot_committed("[all]\nboot_partition=7\n"), None);
        assert_eq!(autoboot_committed(""), None);
    }

    /// Pi: install into slot B, tryboot it, commit; then cut the power after every durable step of the update and
    /// of its commit: every boot comes up on a good image, an interrupted trial is retried, and it ends committed.
    #[test]
    fn pi_update_and_power_cuts() {
        let (a, ha) = pi_image("v1");
        let (b, hb) = pi_image("v2");
        for cut in 0.. {
            let dir = tmp(&format!("pi-cut-{cut}"));
            pi_fresh(&dir, &a);
            let mut s = pi_boot(&dir, false);
            assert_eq!((s.booted, s.trial, s.image, s.platform.as_deref()), (Some(Slot::A), false, Some(ha), Some("aarch64-pi")));
            s.esp.budget.set(Some(cut));
            let installed = s.install(&hb, &b, &trusted()).is_ok();
            // A complete install reboots with tryboot; a power cut loses the flag.
            let mut s = pi_boot(&dir, installed);
            if !installed {
                assert_eq!((s.image, s.trial), (Some(ha), false), "cut {cut}");
                if s.retry {
                    s = pi_boot(&dir, true);
                } else {
                    s.install(&hb, &b, &trusted()).unwrap();
                    s = pi_boot(&dir, true);
                }
            }
            assert_eq!((s.booted, s.image, s.trial), (Some(Slot::B), Some(hb), true), "cut {cut}");
            assert!(s.can_install(&ha).is_err());
            s.esp.budget.set(Some(cut));
            let committed = s.commit().is_ok();
            let mut s = pi_boot(&dir, false);
            if s.image == Some(ha) {
                assert!(!committed && s.retry, "cut {cut}: an interrupted commit retries the trial");
                s = pi_boot(&dir, true);
                assert!(s.trial);
                s.commit().unwrap();
                s = pi_boot(&dir, false);
            }
            assert_eq!((s.booted, s.image, s.trial, s.retry), (Some(Slot::B), Some(hb), false, false), "cut {cut}");
            assert!(s.failed.is_empty() && !dir.join(TRYING).exists());
            assert_eq!(fs::read_to_string(dir.join(AUTOBOOT)).unwrap(), autoboot(Slot::B));
            let _ = fs::remove_dir_all(&dir);
            if installed && committed {
                break;
            }
        }
    }

    /// Pi: an image that never reaches init is tried TRIES times (one tryboot per committed boot), then recorded;
    /// one that never commits is abandoned by init. Both are refused afterwards.
    #[test]
    fn pi_bad_images_fall_back() {
        let (a, ha) = pi_image("v1");
        let (bad, hbad) = pi_image("bad");
        let (c, hc) = pi_image("v3");
        let dir = tmp("pi-bad");
        pi_fresh(&dir, &a);
        let mut s = pi_boot(&dir, false);
        s.install(&hbad, &bad, &trusted()).unwrap();
        let mut tries = 1;
        loop {
            // The trial crashes before init: the next boot is the committed slot.
            let s = pi_boot(&dir, false);
            assert_eq!(s.image, Some(ha));
            if !s.retry {
                break;
            }
            tries += 1;
        }
        assert_eq!(tries, TRIES);
        let mut s = pi_boot(&dir, false);
        assert_eq!((s.image, s.retry, s.failed.clone()), (Some(ha), false, vec![hbad]));
        assert!(s.install(&hbad, &bad, &trusted()).unwrap_err().contains("failed here before"));
        s.install(&hc, &c, &trusted()).unwrap();
        let mut s = pi_boot(&dir, true);
        assert_eq!((s.image, s.trial), (Some(hc), true));
        s.abandon().unwrap();
        let s = pi_boot(&dir, false);
        assert_eq!((s.image, s.trial, s.retry, s.failed.clone()), (Some(ha), false, false, vec![hbad, hc]));
        let x86 = image("x").0;
        let mut s = s;
        assert!(s.install(&sha256(&x86), &x86, &trusted()).unwrap_err().contains("x86_64-vm"));
        let _ = fs::remove_dir_all(&dir);
    }
}
