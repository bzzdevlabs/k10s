//! ACPI static tables, read once at boot, without an AML interpreter (docs/research/native-hardware.md §3.4):
//! * RSDP (from the boot protocol, else the EBDA and BIOS area scan) → XSDT (RSDT for revision 0);
//! * MADT: the CPUs to start (local APIC and x2APIC entries, enabled ones only), I/O APICs, interrupt overrides;
//! * FADT: PM1 control (S5 power-off), the reset register, the PM timer, the hardware-reduced sleep registers;
//! * DSDT/SSDT: `\_S5` sleep type values, found by a byte-pattern scan (`Name (_S5, Package () {a, b, ...})`);
//! * MCFG: PCI Express ECAM windows; HPET: the timer block; SPCR: the console UART;
//! * DSDT/SSDT: virtio-mmio slots (`_HID "LNRO0005"` devices: the first Memory32Fixed and Extended Interrupt
//!   descriptors of their `_CRS`), also by pattern scan, for QEMU microvm and Firecracker.
//! Without ACPI, an Intel MultiProcessor table (QEMU `microvm,acpi=off`, old hypervisors) gives the CPUs and I/O
//! APICs. Tables with a bad checksum are used anyway (as Linux does) and reported with `k10.verbose`.

use alloc::vec::Vec;

use crate::arch::{inb, inl, inw, outb, outl, outw};
use crate::arch::paging;
use crate::kprintln;
use crate::sync::SetOnce;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cpu {
    pub apic_id: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct IoApic {
    pub addr: u64,
    /// [`GSI_AFTER_PREVIOUS`]: the GSI after the previous chip's last pin (MP tables give no base).
    pub gsi_base: u32,
}

pub const GSI_AFTER_PREVIOUS: u32 = u32::MAX;

/// An ISA interrupt routed to another GSI or with non-default polarity/trigger (MPS INTI flags).
#[derive(Clone, Copy, Debug)]
pub struct Override {
    pub irq: u8,
    pub gsi: u32,
    pub flags: u16,
}

#[derive(Clone, Copy, Debug)]
pub struct Ecam {
    pub base: u64,
    pub segment: u16,
    pub bus_start: u8,
    pub bus_end: u8,
}

/// Generic Address Structure: a register in memory, I/O or PCI configuration space.
#[derive(Clone, Copy, Debug, Default)]
pub struct Gas {
    pub space: u8,
    pub bits: u8,
    pub access: u8,
    pub addr: u64,
}

pub const SPACE_MEMORY: u8 = 0;
pub const SPACE_IO: u8 = 1;
pub const SPACE_PCI: u8 = 2;

impl Gas {
    fn parse(b: &[u8]) -> Gas {
        Gas { space: b[0], bits: b[1], access: b[3], addr: u64::from_le_bytes(b[4..12].try_into().unwrap()) }
    }
    fn io(port: u32, bytes: u8) -> Gas {
        Gas { space: SPACE_IO, bits: bytes * 8, access: 0, addr: port as u64 }
    }
    pub fn valid(&self) -> bool {
        self.addr != 0 && matches!(self.space, SPACE_MEMORY | SPACE_IO | SPACE_PCI)
    }
    /// Access width in bytes: the access size if given, else the register width.
    fn width(&self) -> usize {
        match self.access {
            1..=4 => 1 << (self.access - 1),
            _ => (self.bits as usize / 8).clamp(1, 8).next_power_of_two(),
        }
    }
    /// Bytes of the register: its width, at least one access (firmware may give a byte access size for a 16-bit
    /// register; the register is then accessed in several pieces, as ACPICA does).
    fn len(&self) -> usize {
        (self.bits as usize / 8).max(self.width()).min(8)
    }
    /// Memory-space registers must have been mapped ([`map_registers`]): these accesses take no lock, so the
    /// power-off path can use them after stopping the other CPUs.
    fn access(&self, w: usize, i: usize, v: Option<u64>) -> u64 {
        let a = self.addr + (i * w) as u64;
        match (self.space, v) {
            (SPACE_IO, None) => match w {
                1 => inb(a as u16) as u64,
                2 => inw(a as u16) as u64,
                _ => inl(a as u16) as u64,
            },
            (SPACE_IO, Some(v)) => {
                match w {
                    1 => outb(a as u16, v as u8),
                    2 => outw(a as u16, v as u16),
                    _ => outl(a as u16, v as u32),
                }
                0
            }
            // SAFETY: mapped uncached by `map_registers`.
            (SPACE_MEMORY, None) => unsafe {
                match w {
                    1 => core::ptr::read_volatile(a as *const u8) as u64,
                    2 => core::ptr::read_volatile(a as *const u16) as u64,
                    4 => core::ptr::read_volatile(a as *const u32) as u64,
                    _ => core::ptr::read_volatile(a as *const u64),
                }
            },
            (SPACE_MEMORY, Some(v)) => unsafe {
                match w {
                    1 => core::ptr::write_volatile(a as *mut u8, v as u8),
                    2 => core::ptr::write_volatile(a as *mut u16, v as u16),
                    4 => core::ptr::write_volatile(a as *mut u32, v as u32),
                    _ => core::ptr::write_volatile(a as *mut u64, v),
                }
                0
            },
            // Bus 0: device in bits 47:32, function in 31:16, register in 15:0 (writes only, byte by byte).
            (SPACE_PCI, Some(v)) => {
                for k in 0..w {
                    crate::pci::write8_bus0((self.addr >> 32) as u8, (self.addr >> 16) as u8, a as u16 + k as u16, (v >> (8 * k)) as u8);
                }
                0
            }
            _ => 0,
        }
    }
    pub fn read(&self) -> u64 {
        let w = self.width();
        (0..self.len() / w).fold(0, |v, i| v | self.access(w, i, None) << (8 * w * i))
    }
    pub fn write(&self, v: u64) {
        let w = self.width();
        for i in 0..self.len() / w {
            self.access(w, i, Some(v >> (8 * w * i)));
        }
    }
    /// Maps a memory-space register uncached (once, at boot).
    fn map(&self) {
        if self.valid() && self.space == SPACE_MEMORY {
            paging::map_mmio(self.addr, self.len() as u64);
        }
    }
}

/// FADT flags.
pub const FADT_TMR_VAL_EXT: u32 = 1 << 8;
pub const FADT_RESET_REG_SUP: u32 = 1 << 10;
pub const FADT_HW_REDUCED: u32 = 1 << 20;

#[derive(Clone, Copy, Debug, Default)]
pub struct Fadt {
    pub flags: u32,
    pub smi_cmd: u32,
    pub acpi_enable: u8,
    pub pm1a_evt: Gas,
    pub pm1b_evt: Gas,
    pub pm1_evt_len: u8,
    pub pm1a_cnt: Gas,
    pub pm1b_cnt: Gas,
    pub pm_tmr: Gas,
    pub gpe0: Gas,
    pub gpe0_len: u8,
    pub gpe1: Gas,
    pub gpe1_len: u8,
    pub reset: Gas,
    pub reset_value: u8,
    pub sleep_control: Gas,
    pub dsdt: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct Spcr {
    /// Interface type: 0 = 16550, 1 = 16450, 0x12 = 16550-compatible with the GAS width; others are not UARTs we
    /// drive (PL011, SBSA, ...).
    pub kind: u8,
    pub base: Gas,
    /// 0: keep the firmware's setting.
    pub baud: u32,
}

#[derive(Default)]
pub struct Tables {
    /// Where the CPU list came from: "acpi", "mptable" or "" (none).
    pub source: &'static str,
    pub oem: [u8; 6],
    pub revision: u8,
    pub signatures: Vec<[u8; 4]>,
    pub cpus: Vec<Cpu>,
    pub lapic: u64,
    pub ioapics: Vec<IoApic>,
    pub overrides: Vec<Override>,
    pub fadt: Option<Fadt>,
    pub ecam: Vec<Ecam>,
    pub hpet: Option<u64>,
    pub spcr: Option<Spcr>,
    /// `\_S5` SLP_TYPa / SLP_TYPb.
    pub s5: Option<(u8, u8)>,
    pub virtio_mmio: Vec<crate::dev::mmio::MmioSlot>,
    /// The Watchdog Action Table (watchdog.rs).
    pub wdat: Option<Vec<u8>>,
    pub bad_checksums: usize,
}

impl Tables {
    pub fn hw_reduced(&self) -> bool {
        self.fadt.is_some_and(|f| f.flags & FADT_HW_REDUCED != 0)
    }
    /// GSI and MPS flags of ISA interrupt `irq`.
    pub fn isa_irq(&self, irq: u8) -> (u32, u16) {
        self.overrides.iter().find(|o| o.irq == irq).map_or((irq as u32, 0), |o| (o.gsi, o.flags))
    }
}

static TABLES: SetOnce<Tables> = SetOnce::new();
/// The EBDA's physical address (BIOS data area word 0x40E), read before page 0 is unmapped.
static EBDA: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Reads what the tables search needs from page 0. Called before paging::init.
pub fn early() {
    // SAFETY: boot.rs identity-maps the first 4 GiB, page 0 included, until paging::init.
    let seg = unsafe { core::ptr::read_volatile(0x40E as *const u16) };
    let ebda = (seg as u64) << 4;
    if (0x8_0000..0xA_0000).contains(&ebda) {
        EBDA.store(ebda, core::sync::atomic::Ordering::Relaxed);
    }
}

fn ebda() -> Option<u64> {
    Some(EBDA.load(core::sync::atomic::Ordering::Relaxed)).filter(|&a| a != 0)
}

/// The EBDA's range (empty if there is none), which stays the firmware's.
pub fn ebda_range() -> (u64, u64) {
    ebda().map_or((0, 0), |a| (a, 0xA_0000))
}

pub fn get() -> Option<&'static Tables> {
    TABLES.get()
}

/// `len` bytes of physical memory (identity-mapped; mapped read-only first if it was not).
fn phys(addr: u64, len: usize) -> Option<&'static [u8]> {
    if addr == 0 || !paging::ensure_mapped(addr, len as u64) {
        return None;
    }
    // SAFETY: mapped above; firmware tables are never written while the kernel runs.
    Some(unsafe { core::slice::from_raw_parts(addr as *const u8, len) })
}

fn u16_at(b: &[u8], o: usize) -> u16 {
    b.get(o..o + 2).map_or(0, |s| u16::from_le_bytes(s.try_into().unwrap()))
}
fn u32_at(b: &[u8], o: usize) -> u32 {
    b.get(o..o + 4).map_or(0, |s| u32::from_le_bytes(s.try_into().unwrap()))
}
fn u64_at(b: &[u8], o: usize) -> u64 {
    b.get(o..o + 8).map_or(0, |s| u64::from_le_bytes(s.try_into().unwrap()))
}

fn checksum_ok(b: &[u8]) -> bool {
    b.iter().fold(0u8, |a, &x| a.wrapping_add(x)) == 0
}

/// A whole system description table (header included), if its header looks sane.
fn table(addr: u64, bad: &mut usize) -> Option<&'static [u8]> {
    let len = u32_at(phys(addr, 36)?, 4) as usize;
    if !(36..=16 << 20).contains(&len) {
        return None;
    }
    let t = phys(addr, len)?;
    if !checksum_ok(t) {
        *bad += 1;
        if crate::verbose() {
            kprintln!("acpi: {} at {:#x}: bad checksum (used anyway)", core::str::from_utf8(&t[..4]).unwrap_or("????"), addr);
        }
    }
    Some(t)
}

/// Whether `b` (36 bytes or more) starts with a valid RSDP.
fn rsdp_ok(b: &[u8]) -> bool {
    &b[..8] == b"RSD PTR " && checksum_ok(&b[..20]) && (b[15] < 2 || checksum_ok(&b[..(u32_at(b, 20) as usize).clamp(20, 36)]))
}

/// The first 16-byte-aligned address in `[start, end)` where `ok` accepts the bytes (the area mapped once).
fn scan(start: u64, end: u64, len: usize, ok: impl Fn(&[u8]) -> bool) -> Option<u64> {
    let area = phys(start, (end - start) as usize + len)?;
    (0..(end - start) as usize).step_by(16).find(|&o| ok(&area[o..o + len])).map(|o| start + o as u64)
}

fn find_rsdp(hint: u64) -> Option<u64> {
    if hint != 0 && phys(hint, 36).is_some_and(rsdp_ok) {
        return Some(hint);
    }
    // The EBDA's first KiB, then the BIOS area.
    ebda().and_then(|a| scan(a, a + 1024, 36, rsdp_ok)).or_else(|| scan(0xE_0000, 0x10_0000 - 36, 36, rsdp_ok))
}

/// Reads the tables (once, after paging is up). Returns whether ACPI or an MP table was found.
pub fn init(rsdp_hint: u64) -> bool {
    let mut t = Tables::default();
    let found = match find_rsdp(rsdp_hint) {
        Some(rsdp) => {
            parse(rsdp, &mut t);
            true
        }
        None => false,
    };
    if t.cpus.is_empty() {
        // No MADT: an MP table (QEMU microvm with acpi=off) for CPUs and I/O APICs.
        if mptable(&mut t) {
            t.source = "mptable";
        }
    } else {
        t.source = "acpi";
    }
    let any = found || !t.source.is_empty();
    TABLES.set(t);
    any
}

fn parse(rsdp: u64, t: &mut Tables) {
    let Some(r) = phys(rsdp, 36) else { return };
    t.oem.copy_from_slice(&r[9..15]);
    t.revision = r[15];
    let xsdt = if r[15] >= 2 { u64_at(r, 24) } else { 0 };
    let mut bad = 0;
    // The XSDT, else (revision 0, or an XSDT pointer that leads nowhere) the RSDT.
    let root = Some(xsdt).filter(|&x| x != 0).and_then(|x| table(x, &mut bad).filter(|t| &t[..4] == b"XSDT").map(|t| (t, 8)));
    let Some((rt, entry)) = root.or_else(|| table(u32_at(r, 16) as u64, &mut bad).filter(|t| &t[..4] == b"RSDT").map(|t| (t, 4))) else {
        return;
    };
    let mut ssdts = Vec::new();
    for o in (36..rt.len()).step_by(entry) {
        let a = if entry == 8 { u64_at(rt, o) } else { u32_at(rt, o) as u64 };
        let Some(b) = table(a, &mut bad) else { continue };
        let sig: [u8; 4] = b[..4].try_into().unwrap();
        t.signatures.push(sig);
        match &sig {
            b"APIC" => madt(b, t),
            b"FACP" => t.fadt = Some(fadt(b)),
            b"MCFG" => {
                for e in b.get(44..).unwrap_or(&[]).chunks_exact(16) {
                    t.ecam.push(Ecam { base: u64_at(e, 0), segment: u16_at(e, 8), bus_start: e[10], bus_end: e[11] });
                }
            }
            b"HPET" if b.len() >= 56 => {
                let g = Gas::parse(&b[40..52]);
                if g.space == SPACE_MEMORY && g.addr != 0 {
                    t.hpet = Some(g.addr);
                }
            }
            b"SPCR" if b.len() >= 59 => {
                let baud = match b[58] {
                    3 => 9600,
                    4 => 19200,
                    6 => 57600,
                    7 => 115200,
                    _ => 0,
                };
                t.spcr = Some(Spcr { kind: b[36], base: Gas::parse(&b[40..52]), baud });
            }
            b"SSDT" => ssdts.push(b),
            b"WDAT" => t.wdat = Some(b.to_vec()),
            _ => {}
        }
    }
    if let Some(f) = t.fadt {
        if let Some(d) = table(f.dsdt, &mut bad) {
            t.signatures.push(*b"DSDT");
            t.s5 = find_s5(&d[36..]);
            find_virtio_mmio(&d[36..], &mut t.virtio_mmio);
        }
    }
    if t.s5.is_none() {
        t.s5 = ssdts.iter().find_map(|b| find_s5(&b[36..]));
    }
    for b in &ssdts {
        find_virtio_mmio(&b[36..], &mut t.virtio_mmio);
    }
    t.bad_checksums = bad;
    if let Some(f) = &t.fadt {
        for g in [f.pm1a_evt, f.pm1b_evt, f.pm1a_cnt, f.pm1b_cnt, f.pm_tmr, f.gpe0, f.gpe1, f.reset, f.sleep_control] {
            g.map();
        }
        for (g, len) in [(f.gpe0, f.gpe0_len), (f.gpe1, f.gpe1_len)] {
            if g.valid() && g.space == SPACE_MEMORY {
                paging::map_mmio(g.addr, len as u64);
            }
        }
    }
}

fn madt(b: &[u8], t: &mut Tables) {
    t.lapic = u32_at(b, 36) as u64;
    let mut o = 44;
    while o + 2 <= b.len() {
        let (kind, len) = (b[o], b[o + 1] as usize);
        if len < 2 || o + len > b.len() {
            break;
        }
        let e = &b[o..o + len];
        // Flags bit 0: enabled; bit 1 (online capable) is a hot-pluggable CPU that is not there yet. Firmware may
        // list a CPU both as a local APIC and as an x2APIC.
        let cpu = |cpus: &mut Vec<Cpu>, id: u32, flags: u32| {
            if flags & 1 != 0 && !cpus.iter().any(|c| c.apic_id == id) {
                cpus.push(Cpu { apic_id: id });
            }
        };
        match kind {
            0 if len >= 8 && e[3] != 0xFF => cpu(&mut t.cpus, e[3] as u32, u32_at(e, 4)),
            9 if len >= 16 => cpu(&mut t.cpus, u32_at(e, 4), u32_at(e, 8)),
            1 if len >= 12 => t.ioapics.push(IoApic { addr: u32_at(e, 4) as u64, gsi_base: u32_at(e, 8) }),
            2 if len >= 10 => t.overrides.push(Override { irq: e[3], gsi: u32_at(e, 4), flags: u16_at(e, 8) }),
            5 if len >= 12 => t.lapic = u64_at(e, 4),
            _ => {}
        }
        o += len;
    }
}

fn fadt(b: &[u8]) -> Fadt {
    let has = |end: usize| b.len() >= end;
    // A 64-bit (X_) register if the table has it and it is set, else the legacy I/O port block.
    let reg = |x: usize, legacy: usize, len: u8| -> Gas {
        if has(x + 12) {
            let mut g = Gas::parse(&b[x..x + 12]);
            if g.addr != 0 {
                // Some firmware leaves the X_ register's width at 0 or below the legacy block length.
                if (g.bits as usize) < len as usize * 8 {
                    g.bits = len * 8;
                }
                return g;
            }
        }
        let port = u32_at(b, legacy);
        if port != 0 && len != 0 { Gas::io(port, len) } else { Gas::default() }
    };
    let flags = u32_at(b, 112);
    let pm1_cnt_len = b.get(89).copied().unwrap_or(2).min(2);
    let mut f = Fadt {
        flags,
        smi_cmd: u32_at(b, 48),
        acpi_enable: b.get(52).copied().unwrap_or(0),
        pm1_evt_len: b.get(88).copied().unwrap_or(0),
        pm1a_cnt: reg(172, 64, pm1_cnt_len),
        pm1b_cnt: reg(184, 68, pm1_cnt_len),
        pm_tmr: reg(208, 76, 4),
        gpe0_len: b.get(92).copied().unwrap_or(0),
        gpe1_len: b.get(93).copied().unwrap_or(0),
        dsdt: if has(148) && u64_at(b, 140) != 0 { u64_at(b, 140) } else { u32_at(b, 40) as u64 },
        ..Fadt::default()
    };
    let evt_half = (f.pm1_evt_len / 2).max(1);
    f.pm1a_evt = reg(148, 56, evt_half * 2);
    f.pm1b_evt = reg(160, 60, evt_half * 2);
    f.gpe0 = reg(220, 80, 1);
    f.gpe1 = reg(232, 84, 1);
    if has(129) {
        f.reset = Gas::parse(&b[116..128]);
        f.reset_value = b[128];
    }
    if has(256) {
        f.sleep_control = Gas::parse(&b[244..256]);
    }
    f
}

/// `Name (_S5, Package () { SLP_TYPa, SLP_TYPb, ... })` in AML: NameOp (0x08), optionally a root prefix, `_S5_`,
/// PackageOp (0x12), PkgLength, NumElements, then integer data objects.
fn find_s5(aml: &[u8]) -> Option<(u8, u8)> {
    let int = |b: &[u8], i: &mut usize| -> Option<u8> {
        let op = *b.get(*i)?;
        *i += 1;
        match op {
            0x00 => Some(0), // ZeroOp
            0x01 => Some(1), // OneOp
            0x0A => {
                *i += 1;
                b.get(*i - 1).copied()
            }
            0x0B | 0x0C => {
                let v = *b.get(*i)?;
                *i += if op == 0x0B { 2 } else { 4 };
                Some(v)
            }
            _ => None,
        }
    };
    let mut from = 0;
    while let Some(p) = aml[from..].windows(4).position(|w| w == b"_S5_") {
        let at = from + p;
        from = at + 4;
        let named = at >= 1 && aml[at - 1] == 0x08 || at >= 2 && aml[at - 1] == b'\\' && aml[at - 2] == 0x08;
        if !named || aml.get(at + 4) != Some(&0x12) {
            continue;
        }
        let mut i = at + 5;
        let Some(&pkg) = aml.get(i) else { continue };
        i += 1 + (pkg >> 6) as usize; // PkgLength: bits 7:6 = number of extra bytes
        i += 1; // NumElements
        let Some(a) = int(aml, &mut i) else { continue };
        let b = int(aml, &mut i).unwrap_or(0);
        return Some((a, b));
    }
    None
}

/// `Name (_HID, "LNRO0005")` (a string object: 0x0D, the id, NUL), then in the bytes up to the next one the
/// resource template of `_CRS`: Memory32Fixed (0x86, length 9: write flag, base, size) and Extended Interrupt
/// (0x89: length, flags, count, interrupt numbers).
fn find_virtio_mmio(aml: &[u8], out: &mut Vec<crate::dev::mmio::MmioSlot>) {
    const HID: &[u8] = b"\x0dLNRO0005\x00";
    let starts: Vec<usize> = aml.windows(HID.len()).enumerate().filter(|(_, w)| *w == HID).map(|(i, _)| i).collect();
    for (k, &at) in starts.iter().enumerate() {
        let end = starts.get(k + 1).copied().unwrap_or(aml.len()).min(at + 256);
        let dev = &aml[at..end];
        let mem = dev.windows(3).position(|w| w == [0x86, 0x09, 0x00]).map(|p| (u32_at(dev, p + 4) as u64, u32_at(dev, p + 8) as u64));
        let irq = dev.windows(2).position(|w| w[0] == 0x89 && w[1] >= 6).filter(|&p| dev.get(p + 4).is_some_and(|&n| n >= 1)).map(|p| u32_at(dev, p + 5));
        if let (Some((base, size)), Some(irq)) = (mem, irq) {
            if base != 0 && size >= 0x100 && !out.iter().any(|s| s.base == base) {
                out.push(crate::dev::mmio::MmioSlot { base, size, irq });
            }
        }
    }
}

/// Intel MultiProcessor Specification 1.4 table: processors and I/O APICs.
fn mptable(t: &mut Tables) -> bool {
    let valid = |b: &[u8]| &b[..4] == b"_MP_" && checksum_ok(&b[..16]);
    let fp = ebda()
        .and_then(|a| scan(a, a + 1024, 16, valid))
        .or_else(|| scan(0x9_FC00, 0xA_0000, 16, valid))
        .or_else(|| scan(0xF_0000, 0x10_0000 - 16, 16, valid));
    let Some(fp) = fp else { return false };
    let Some(cfg) = phys(fp, 16).map(|b| u32_at(b, 4) as u64) else { return false };
    let Some(h) = phys(cfg, 44) else { return false };
    if &h[..4] != b"PCMP" {
        return false;
    }
    let Some(b) = phys(cfg, u16_at(h, 4) as usize) else { return false };
    t.lapic = u32_at(b, 36) as u64;
    // Entries are walked to the table's length, not its entry count (which some generators leave at 0), as Linux does.
    let mut o = 44;
    while o < b.len() {
        let kind = b[o];
        match kind {
            0 if o + 20 <= b.len() => {
                if b[o + 3] & 1 != 0 {
                    t.cpus.push(Cpu { apic_id: b[o + 1] as u32 });
                }
                o += 20;
            }
            2 if o + 8 <= b.len() => {
                if b[o + 3] & 1 != 0 {
                    // GSIs follow each other from chip to chip; ioapic.rs counts the pins.
                    t.ioapics.push(IoApic { addr: u32_at(b, o + 4) as u64, gsi_base: GSI_AFTER_PREVIOUS });
                }
                o += 8;
            }
            1 | 3 | 4 => o += 8,
            _ => break,
        }
    }
    !t.cpus.is_empty()
}

/// One line for `k10.verbose` boots.
pub fn describe() {
    let Some(t) = get() else { return };
    if t.signatures.is_empty() {
        kprintln!("acpi: none; {} CPU(s) and {} I/O APIC(s) from the MP table, no power off", t.cpus.len(), t.ioapics.len());
        return;
    }
    let mut sigs = alloc::string::String::new();
    for s in &t.signatures {
        sigs.push_str(core::str::from_utf8(s).unwrap_or("????"));
        sigs.push(' ');
    }
    kprintln!(
        "acpi: {} rev {} [{}], tables {}; {} CPU(s) from {}, {} I/O APIC(s), ECAM {}, HPET {}, PM timer {}, S5 {:?}, reset {}{}",
        core::str::from_utf8(&t.oem).unwrap_or("?").trim_end(),
        t.revision,
        if t.hw_reduced() { "hardware-reduced" } else { "full" },
        sigs.trim_end(),
        t.cpus.len(),
        if t.source.is_empty() { "nowhere" } else { t.source },
        t.ioapics.len(),
        t.ecam.len(),
        if t.hpet.is_some() { "yes" } else { "no" },
        if t.fadt.is_some_and(|f| f.pm_tmr.valid()) { "yes" } else { "no" },
        t.s5,
        if t.fadt.is_some_and(|f| f.reset.valid()) { "register" } else { "legacy" },
        if t.bad_checksums > 0 { alloc::format!(", {} bad checksum(s)", t.bad_checksums) } else { alloc::string::String::new() }
    );
}
