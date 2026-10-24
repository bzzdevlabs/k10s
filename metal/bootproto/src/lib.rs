//! What the boot paths of k10-metal agree on (metal/README.md, "Booting"): k10boot, the UEFI loader (metal/boot), the
//! kernel's Multiboot2 shim behind GRUB (BIOS), and the kernel. The kernel's one entry interface stays the PVH
//! `hvm_start_info`; whatever a loader hands over beyond QEMU's `-kernel`/`-initrd` travels as PVH modules named by
//! their `cmdline_paddr` string:
//!
//! | module | contents |
//! |---|---|
//! | `k10s.bundle` (or unnamed: QEMU's `-initrd`) | the deployment bundle |
//! | `k10s.conf` | the node configuration of K10S-BOOT, shared with Tier 1 ([`conf`]) |
//! | `/boot/NAME` | a file of K10S-BOOT that k10s.conf names (`admin-token-file=/boot/admin.key`), by its Tier 1 path |
//! | `k10s.framebuffer` | the UEFI GOP mode ([`Framebuffer`]) |
//! | `k10s.efi` | UEFI runtime services, to clear the boot counter ([`EfiHandoff`], [`counter`]) |
//!
//! Nothing here allocates: the kernel reads these before it has a heap.

#![no_std]

#[cfg(test)]
extern crate std;

pub mod devices;

pub const MOD_BUNDLE: &str = "k10s.bundle";
pub const MOD_CONF: &str = "k10s.conf";
pub const MOD_FRAMEBUFFER: &str = "k10s.framebuffer";
pub const MOD_EFI: &str = "k10s.efi";

fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

fn u64_at(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

/// `k10s.conf` (docs/OS.md, "Configuration"), as Tier 1 reads it: `KEY=VALUE` lines, `#` comments, an optional
/// `k10s.` prefix, values optionally in double quotes; a later line wins.
pub mod conf {
    /// The keys whose value is a file of K10S-BOOT (`/boot/…`) that a loader hands over as a module of that name.
    pub const FILE_KEYS: [&str; 3] = ["admin-token-file", "cluster-key-file", "cluster-next-key-file"];

    pub fn entries(text: &str) -> impl Iterator<Item = (&str, &str)> {
        text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).map(|line| {
            let (k, v) = line.split_once('=').unwrap_or((line, ""));
            let k = k.trim();
            (k.strip_prefix("k10s.").unwrap_or(k), unquote(v.trim()))
        })
    }

    pub fn get<'a>(text: &'a str, key: &str) -> Option<&'a str> {
        entries(text).filter(|&(k, _)| k == key).last().map(|(_, v)| v)
    }

    /// A boolean key: present and not `0`, `false`, `no` or `off`.
    pub fn flag(text: &str, key: &str) -> bool {
        get(text, key).is_some_and(|v| !matches!(v, "0" | "false" | "no" | "off"))
    }

    pub fn unquote(v: &str) -> &str {
        v.strip_prefix('"').and_then(|v| v.strip_suffix('"')).unwrap_or(v)
    }

    /// The path on K10S-BOOT (`/` separated, no leading slash) of a Tier 1 path `/boot/NAME`; `None` for any other
    /// path or one that leaves the partition.
    pub fn boot_file(path: &str) -> Option<&str> {
        let rel = path.strip_prefix("/boot/")?;
        let ok = !rel.is_empty() && rel.split('/').all(|c| !c.is_empty() && c != "." && c != "..") && !rel.contains('\\');
        ok.then_some(rel)
    }
}

/// `/k10s/tier` on K10S-BOOT: a GRUB environment block, so that GRUB (`load_env --file`) reads the same file on BIOS.
/// `tier=auto|linux|metal`, and `metal_cmdline=` the native kernel's command line.
pub mod tier {
    pub const HEADER: &[u8] = b"# GRUB Environment Block\n";

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub enum Tier {
        /// The native kernel if it drives this machine's network adapter ([`crate::devices`]), else Tier 1.
        Auto,
        /// Tier 1: Linux (systemd-boot, the A/B UKIs).
        Linux,
        /// The native kernel.
        Metal,
    }

    impl Tier {
        pub fn parse(s: &str) -> Option<Tier> {
            match s {
                "auto" => Some(Tier::Auto),
                "linux" => Some(Tier::Linux),
                "metal" => Some(Tier::Metal),
                _ => None,
            }
        }

        pub fn name(self) -> &'static str {
            match self {
                Tier::Auto => "auto",
                Tier::Linux => "linux",
                Tier::Metal => "metal",
            }
        }
    }

    /// The value of `key`, parsed exactly as GRUB 2.06 does (grub-core/lib/envblk.c): after the header, `#` lines are
    /// skipped, a name runs to the next `=` (even across lines: a stray line spoils the next variable), a value to
    /// the next newline (a value without one is dropped), `\` escapes the next byte. `None` if the file is not an
    /// environment block, the key is absent or its value is not UTF-8 (or holds an escape).
    pub fn get<'a>(file: &'a [u8], key: &str) -> Option<&'a str> {
        let mut p = file.strip_prefix(HEADER)?;
        let mut found = None;
        while !p.is_empty() {
            if p[0] == b'#' {
                p = p.iter().position(|&c| c == b'\n').map_or(&[][..], |i| &p[i + 1..]);
                continue;
            }
            let Some(eq) = p.iter().position(|&c| c == b'=') else { break };
            let (name, rest) = (&p[..eq], &p[eq + 1..]);
            let mut i = 0;
            while i < rest.len() && rest[i] != b'\n' {
                i += if rest[i] == b'\\' { 2 } else { 1 };
            }
            if i >= rest.len() {
                break;
            }
            if name == key.as_bytes() {
                let v = &rest[..i];
                found = if v.contains(&b'\\') { None } else { core::str::from_utf8(v).ok() };
            }
            p = &rest[i + 1..];
        }
        found
    }
}

/// The UEFI GOP mode a loader leaves the screen in (module [`MOD_FRAMEBUFFER`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Framebuffer {
    pub base: u64,
    pub size: u64,
    pub width: u32,
    pub height: u32,
    /// Pixels per scan line.
    pub stride: u32,
    pub format: PixelFormat,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PixelFormat {
    /// 32 bits per pixel: red, green, blue, reserved bytes (PixelRedGreenBlueReserved8BitPerColor).
    Rgbx,
    /// 32 bits per pixel: blue, green, red, reserved bytes (PixelBlueGreenRedReserved8BitPerColor).
    Bgrx,
    /// Pixels as the red, green, blue and reserved bit masks say (PixelBitMask).
    Bitmask { red: u32, green: u32, blue: u32, reserved: u32 },
}

const FB_MAGIC: &[u8; 8] = b"K10S-FB1";

impl Framebuffer {
    pub const LEN: usize = 56;

    pub fn encode(&self) -> [u8; Self::LEN] {
        let mut b = [0u8; Self::LEN];
        let (kind, masks) = match self.format {
            PixelFormat::Rgbx => (0u32, [0; 4]),
            PixelFormat::Bgrx => (1, [0; 4]),
            PixelFormat::Bitmask { red, green, blue, reserved } => (2, [red, green, blue, reserved]),
        };
        b[..8].copy_from_slice(FB_MAGIC);
        b[8..16].copy_from_slice(&self.base.to_le_bytes());
        b[16..24].copy_from_slice(&self.size.to_le_bytes());
        for (i, v) in [self.width, self.height, self.stride, kind].into_iter().chain(masks).enumerate() {
            b[24 + 4 * i..28 + 4 * i].copy_from_slice(&v.to_le_bytes());
        }
        b
    }

    pub fn decode(b: &[u8]) -> Option<Framebuffer> {
        if b.len() < Self::LEN || &b[..8] != FB_MAGIC {
            return None;
        }
        let w = |i: usize| u32_at(b, 24 + 4 * i);
        let format = match w(3) {
            0 => PixelFormat::Rgbx,
            1 => PixelFormat::Bgrx,
            2 => PixelFormat::Bitmask { red: w(4), green: w(5), blue: w(6), reserved: w(7) },
            _ => return None,
        };
        Some(Framebuffer { base: u64_at(b, 8), size: u64_at(b, 16), width: w(0), height: w(1), stride: w(2), format })
    }
}

/// A firmware memory region that UEFI runtime services use (`EFI_MEMORY_RUNTIME`): the kernel maps these for its
/// runtime calls.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct RuntimeRegion {
    /// `EFI_MEMORY_TYPE`.
    pub kind: u32,
    pub phys: u64,
    pub pages: u64,
    /// `EFI_MEMORY_*` attributes.
    pub attr: u64,
}

/// Module [`MOD_EFI`]: where the kernel finds UEFI runtime services (the loader never calls SetVirtualAddressMap, so
/// they run at their physical addresses).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct EfiHandoff {
    pub system_table: u64,
    pub runtime_services: u64,
    /// The loader counted this boot in the [`counter`] variable: the kernel clears it once healthy.
    pub counter_armed: bool,
    pub regions: [RuntimeRegion; EfiHandoff::MAX_REGIONS],
    pub nregions: usize,
}

const EFI_MAGIC: &[u8; 8] = b"K10S-EF1";
const EFI_HEADER: usize = 32;
const EFI_REGION: usize = 32;

impl EfiHandoff {
    pub const MAX_REGIONS: usize = 128;
    /// Bytes [`EfiHandoff::encode`] needs at most.
    pub const MAX_LEN: usize = EFI_HEADER + EFI_REGION * Self::MAX_REGIONS;

    /// Writes the record into `out` (at least [`EfiHandoff::MAX_LEN`] bytes); returns its length. Regions past
    /// [`EfiHandoff::MAX_REGIONS`] are dropped.
    pub fn encode(out: &mut [u8], system_table: u64, runtime_services: u64, armed: bool, regions: impl Iterator<Item = RuntimeRegion>) -> usize {
        out[..8].copy_from_slice(EFI_MAGIC);
        out[8..16].copy_from_slice(&system_table.to_le_bytes());
        out[16..24].copy_from_slice(&runtime_services.to_le_bytes());
        out[24..28].copy_from_slice(&(armed as u32).to_le_bytes());
        let mut n = 0;
        for r in regions.take(Self::MAX_REGIONS) {
            let o = EFI_HEADER + EFI_REGION * n;
            out[o..o + 4].copy_from_slice(&r.kind.to_le_bytes());
            out[o + 4..o + 8].fill(0);
            out[o + 8..o + 16].copy_from_slice(&r.phys.to_le_bytes());
            out[o + 16..o + 24].copy_from_slice(&r.pages.to_le_bytes());
            out[o + 24..o + 32].copy_from_slice(&r.attr.to_le_bytes());
            n += 1;
        }
        out[28..32].copy_from_slice(&(n as u32).to_le_bytes());
        EFI_HEADER + EFI_REGION * n
    }

    pub fn decode(b: &[u8]) -> Option<EfiHandoff> {
        if b.len() < EFI_HEADER || &b[..8] != EFI_MAGIC {
            return None;
        }
        let n = u32_at(b, 28) as usize;
        if n > Self::MAX_REGIONS || b.len() < EFI_HEADER + EFI_REGION * n {
            return None;
        }
        let mut h = EfiHandoff {
            system_table: u64_at(b, 8),
            runtime_services: u64_at(b, 16),
            counter_armed: u32_at(b, 24) != 0,
            regions: [RuntimeRegion::default(); Self::MAX_REGIONS],
            nregions: n,
        };
        for (i, r) in h.regions[..n].iter_mut().enumerate() {
            let o = EFI_HEADER + EFI_REGION * i;
            *r = RuntimeRegion { kind: u32_at(b, o), phys: u64_at(b, o + 8), pages: u64_at(b, o + 16), attr: u64_at(b, o + 24) };
        }
        Some(h)
    }

    pub fn regions(&self) -> &[RuntimeRegion] {
        &self.regions[..self.nregions]
    }
}

/// The native kernel's boot counter (metal/README.md, "Tiers"), a non-volatile UEFI variable. k10boot counts each
/// native boot in it before booting the kernel; the kernel deletes it through runtime services once it is healthy.
/// Two counted boots in a row that never got healthy pin the machine to Tier 1, until another kernel is installed
/// or `/k10s/tier` is written again (its modification time is part of the record).
pub mod counter {
    /// `K10sNativeBoot-6b10c5a0-0000-4000-8000-0000000000e2`.
    pub const NAME: &str = "K10sNativeBoot";
    pub const GUID: [u8; 16] = [0xa0, 0xc5, 0x10, 0x6b, 0, 0, 0, 0x40, 0x80, 0, 0, 0, 0, 0, 0, 0xe2];
    /// EFI_VARIABLE_NON_VOLATILE | BOOTSERVICE_ACCESS | RUNTIME_ACCESS.
    pub const ATTRIBUTES: u32 = 7;
    /// Unhealthy native boots in a row before Tier 1 is pinned.
    pub const MAX_FAILURES: u8 = 2;
    pub const LEN: usize = 50;
    const VERSION: u8 = 1;

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub struct State {
        /// Native boots counted since the kernel was last healthy.
        pub failures: u8,
        /// SHA-256 of the signed kernel those boots ran.
        pub kernel: [u8; 32],
        /// `/k10s/tier`'s modification time (EFI_TIME) then.
        pub tier_stamp: [u8; 16],
    }

    impl State {
        pub fn encode(&self) -> [u8; LEN] {
            let mut b = [0u8; LEN];
            b[0] = VERSION;
            b[1] = self.failures;
            b[2..34].copy_from_slice(&self.kernel);
            b[34..50].copy_from_slice(&self.tier_stamp);
            b
        }

        /// `None` for anything but a record of this version (treated as no record).
        pub fn decode(b: &[u8]) -> Option<State> {
            (b.len() == LEN && b[0] == VERSION).then(|| State {
                failures: b[1],
                kernel: b[2..34].try_into().unwrap(),
                tier_stamp: b[34..50].try_into().unwrap(),
            })
        }
    }

    /// Before a native boot of `kernel`: `Ok(state)` to store, the boot counted; `Err(failures)` when the last
    /// boots of this kernel and tier file never got healthy: boot Tier 1.
    pub fn count(prev: Option<State>, kernel: [u8; 32], tier_stamp: [u8; 16]) -> Result<State, u8> {
        let failures = match prev {
            Some(p) if p.kernel == kernel && p.tier_stamp == tier_stamp => p.failures,
            _ => 0,
        };
        if failures >= MAX_FAILURES {
            return Err(failures);
        }
        Ok(State { failures: failures + 1, kernel, tier_stamp })
    }
}

/// The E820 memory map of `hvm_start_info` (version 1).
pub mod e820 {
    pub const RAM: u32 = 1;
    pub const RESERVED: u32 = 2;
    pub const ACPI: u32 = 3;
    pub const NVS: u32 = 4;
    pub const UNUSABLE: u32 = 5;
    pub const PMEM: u32 = 7;

    /// `struct hvm_memmap_table_entry` (also the layout of a Multiboot2 memory map entry).
    #[repr(C)]
    #[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
    pub struct Entry {
        pub addr: u64,
        pub size: u64,
        pub kind: u32,
        pub reserved: u32,
    }

    /// The E820 type of a UEFI memory descriptor, as Linux's EFI stub maps them: what is free once boot services
    /// exit is RAM; runtime services, MMIO and the rest are reserved.
    pub fn from_efi(kind: u32, attr: u64) -> u32 {
        const EFI_MEMORY_SP: u64 = 1 << 18;
        match kind {
            // Loader code/data, boot services code/data, conventional memory.
            1..=4 | 7 if attr & EFI_MEMORY_SP == 0 => RAM,
            8 => UNUSABLE,
            9 => ACPI,
            10 => NVS,
            14 => PMEM,
            _ => RESERVED,
        }
    }

    /// Sorts `map` by address and merges adjacent or overlapping entries of the same type; returns the new length.
    pub fn normalize(map: &mut [Entry]) -> usize {
        for i in 1..map.len() {
            let mut j = i;
            while j > 0 && map[j - 1].addr > map[j].addr {
                map.swap(j - 1, j);
                j -= 1;
            }
        }
        let mut n = 0;
        for i in 0..map.len() {
            let e = map[i];
            if e.size == 0 {
                continue;
            }
            if n > 0 && map[n - 1].kind == e.kind && map[n - 1].addr + map[n - 1].size >= e.addr {
                let end = (map[n - 1].addr + map[n - 1].size).max(e.addr + e.size);
                map[n - 1].size = end - map[n - 1].addr;
            } else {
                map[n] = Entry { reserved: 0, ..e };
                n += 1;
            }
        }
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec::Vec;

    #[test]
    fn conf_like_tier1() {
        let text = "# node\nk10s.join = 10.0.0.1:7946\nname=\"edge one\"\nlog-stderr\njoin=10.0.0.2:7946\npin=off\n";
        assert_eq!(conf::get(text, "join"), Some("10.0.0.2:7946"));
        assert_eq!(conf::get(text, "name"), Some("edge one"));
        assert!(conf::flag(text, "log-stderr") && !conf::flag(text, "pin") && !conf::flag(text, "absent"));
        assert_eq!(conf::boot_file("/boot/admin.key"), Some("admin.key"));
        assert_eq!(conf::boot_file("/boot/keys/c.key"), Some("keys/c.key"));
        for bad in ["/etc/admin.key", "/boot/", "/boot/../x", "/boot//x", "/boot/a\\b", "boot/x"] {
            assert_eq!(conf::boot_file(bad), None, "{bad}");
        }
    }

    #[test]
    fn tier_file_as_grub_reads_it() {
        let f = b"# GRUB Environment Block\n# comment\ntier=metal\nmetal_cmdline=k10.verbose k10.smp=2\n####";
        assert_eq!(tier::get(f, "tier").and_then(tier::Tier::parse), Some(tier::Tier::Metal));
        assert_eq!(tier::get(f, "metal_cmdline"), Some("k10.verbose k10.smp=2"));
        // No header: not an environment block (GRUB's load_env refuses it too).
        assert_eq!(tier::get(b"metal\n", "tier"), None);
        // A value needs its newline; a stray line spoils the next name; the last assignment wins.
        assert_eq!(tier::get(b"# GRUB Environment Block\ntier=metal", "tier"), None);
        assert_eq!(tier::get(b"# GRUB Environment Block\n\ntier=metal\n", "tier"), None);
        assert_eq!(tier::get(b"# GRUB Environment Block\nstray\ntier=metal\n", "tier"), None);
        assert_eq!(tier::get(b"# GRUB Environment Block\ntier=auto\ntier=linux\n", "tier"), Some("linux"));
        assert_eq!(tier::get(b"# GRUB Environment Block\ntier=metal\nbroken", "tier"), Some("metal"));
        assert_eq!(tier::get(b"# GRUB Environment Block\ntier=me\\\ntal\n", "tier"), None);
        for n in 0..f.len() {
            let _ = tier::get(&f[..n], "tier");
        }
    }

    #[test]
    fn records_round_trip() {
        for format in [PixelFormat::Rgbx, PixelFormat::Bgrx, PixelFormat::Bitmask { red: 0xf800, green: 0x7e0, blue: 0x1f, reserved: 0 }] {
            let fb = Framebuffer { base: 0x8000_0000, size: 3 << 20, width: 1024, height: 768, stride: 1024, format };
            assert_eq!(Framebuffer::decode(&fb.encode()), Some(fb));
        }
        assert_eq!(Framebuffer::decode(&[0; 56]), None);

        let regions: Vec<RuntimeRegion> =
            (0..3).map(|i| RuntimeRegion { kind: 5 + (i % 2), phys: 0x7f00_0000 + i as u64 * 0x10000, pages: 16, attr: 1 << 63 | 8 }).collect();
        let mut buf = [0u8; EfiHandoff::MAX_LEN];
        let n = EfiHandoff::encode(&mut buf, 0x7fa0_0000, 0x7fa1_0000, true, regions.iter().copied());
        let h = EfiHandoff::decode(&buf[..n]).unwrap();
        assert_eq!((h.system_table, h.runtime_services, h.counter_armed), (0x7fa0_0000, 0x7fa1_0000, true));
        assert_eq!(h.regions(), &regions[..]);
        assert!(EfiHandoff::decode(&buf[..n - 1]).is_none());
        let n = EfiHandoff::encode(&mut buf, 1, 2, false, (0..500).map(|_| RuntimeRegion::default()));
        assert_eq!((n, EfiHandoff::decode(&buf[..n]).unwrap().nregions), (EfiHandoff::MAX_LEN, EfiHandoff::MAX_REGIONS));
    }

    #[test]
    fn boot_counter() {
        let (k1, k2, t1, t2) = ([1; 32], [2; 32], [1; 16], [2; 16]);
        let s1 = counter::count(None, k1, t1).unwrap();
        assert_eq!(s1.failures, 1);
        assert_eq!(counter::State::decode(&s1.encode()), Some(s1));
        let s2 = counter::count(Some(s1), k1, t1).unwrap();
        assert_eq!(s2.failures, 2);
        assert_eq!(counter::count(Some(s2), k1, t1), Err(2));
        // Another kernel, or the tier file written again: counted from zero.
        assert_eq!(counter::count(Some(s2), k2, t1).unwrap().failures, 1);
        assert_eq!(counter::count(Some(s2), k1, t2).unwrap().failures, 1);
        assert_eq!(counter::State::decode(&[9; counter::LEN]), None);
    }

    #[test]
    fn e820_from_uefi() {
        use e820::*;
        assert_eq!([7, 1, 2, 3, 4, 0, 5, 6, 9, 10, 11, 8, 14].map(|k| from_efi(k, 0xf)), [1, 1, 1, 1, 1, 2, 2, 2, 3, 4, 2, 5, 7]);
        assert_eq!(from_efi(7, 1 << 18), RESERVED);
        let e = |addr: u64, size: u64, kind: u32| Entry { addr, size, kind, reserved: 0 };
        let mut m = [e(0x100000, 0x100000, RAM), e(0, 0xa0000, RAM), e(0x200000, 0x1000, RAM), e(0xa0000, 0x60000, RESERVED),
                     e(0x300000, 0, RAM), e(0x201000, 0x1000, ACPI), e(0x202000, 0x10000, RAM), e(0x208000, 0x10000, RAM)];
        let n = normalize(&mut m);
        assert_eq!(&m[..n], &[e(0, 0xa0000, RAM), e(0xa0000, 0x60000, RESERVED), e(0x100000, 0x101000, RAM), e(0x201000, 0x1000, ACPI),
                              e(0x202000, 0x16000, RAM)]);
    }

    #[test]
    fn device_table() {
        assert_eq!(devices::find(0x1af4, 0x1000).map(|d| d.class), Some(devices::Class::Network));
        assert_eq!(devices::find(0x1af4, 0x1041).map(|d| d.class), Some(devices::Class::Network));
        assert!(devices::find(0x8086, 0x10d3).is_none());
    }
}
