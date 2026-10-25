//! PCI: configuration space through ECAM (the ACPI MCFG windows: every segment, the 4 KiB extended space) or else
//! the legacy port mechanism (0xCF8 address / 0xCFC data: segment 0, 256 bytes), one bus enumeration at boot,
//! BARs (I/O, 32- and 64-bit memory; drivers map the ranges they use uncached), MSI-X, and the bridges above a
//! device (enabled with it, so its DMA and MMIO are forwarded).
//!
//! Enumeration follows PCI-PCI bridges from each segment's first bus (every device firmware set up is reachable
//! that way). Other root buses (multi-socket servers have one per socket or root complex) are found by probing
//! every remaining bus of the ECAM windows; that brute-force pass runs on bare metal, where ECAM reads are cheap,
//! and under a hypervisor only with `k10.pci=full` (each read of an absent function is a VM exit there, and
//! hypervisors describe a single root bus per segment).

use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::arch::{inl, outl};
use crate::sync::{IrqLock, SetOnce};

/// Next free (I/O port, 32-bit MMIO) address for BARs the firmware did not assign.
static NEXT_BAR: IrqLock<(u64, u64)> = IrqLock::new((0xC000, 0xE000_0000));
/// Ends of those pools: the I/O space, and the I/O APIC / HPET / local APIC range.
const IO_POOL_END: u64 = 0x1_0000;
const MEM_POOL_END: u64 = 0xFEC0_0000;
/// The port mechanism's address/data pair must not be interleaved between CPUs.
static PORTS: IrqLock<()> = IrqLock::new(());
static ECAM: SetOnce<Vec<crate::acpi::Ecam>> = SetOnce::new();
static DEVICES: SetOnce<Vec<Device>> = SetOnce::new();
static FULL_SCAN: AtomicBool = AtomicBool::new(false);

pub const CMD_IO: u16 = 1;
pub const CMD_MEM: u16 = 1 << 1;
pub const CMD_MASTER: u16 = 1 << 2;
pub const CMD_INTX_OFF: u16 = 1 << 10;
const CAP_MSI: u8 = 0x05;
const CAP_MSIX: u8 = 0x11;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bdf {
    pub seg: u16,
    pub bus: u8,
    pub dev: u8,
    pub func: u8,
}

#[derive(Clone, Copy, Debug)]
pub struct Device {
    pub seg: u16,
    pub bus: u8,
    pub dev: u8,
    pub func: u8,
    pub vendor: u16,
    pub device: u16,
    pub class: u8,
    pub subclass: u8,
    /// The bridge whose secondary bus this device is on (`None`: a root bus).
    pub upstream: Option<Bdf>,
}

/// Sets the ECAM windows (from MCFG), mapping them uncached, and the scan mode. Called once before [`enumerate`].
pub fn init(windows: &[crate::acpi::Ecam], full_scan: bool) {
    let mut v = Vec::new();
    for w in windows {
        if w.bus_end < w.bus_start {
            continue;
        }
        let len = ((w.bus_end - w.bus_start) as u64 + 1) << 20;
        // ECAM covers buses from 0: the window's base is where bus 0 would be.
        let first = w.base + ((w.bus_start as u64) << 20);
        if crate::arch::paging::map_mmio(first, len) {
            v.push(*w);
        }
    }
    ECAM.set(v);
    FULL_SCAN.store(full_scan, Ordering::Relaxed);
}

pub fn uses_ecam() -> bool {
    ECAM.get().is_some_and(|v| !v.is_empty())
}

fn ecam_addr(b: Bdf, off: u16) -> Option<u64> {
    let w = ECAM.get()?.iter().find(|w| w.segment == b.seg && (w.bus_start..=w.bus_end).contains(&b.bus))?;
    Some(w.base + ((b.bus as u64) << 20 | (b.dev as u64) << 15 | (b.func as u64) << 12 | (off & 0xFFC) as u64))
}

fn port_addr(b: Bdf, off: u16) -> u32 {
    0x8000_0000 | (b.bus as u32) << 16 | (b.dev as u32) << 11 | (b.func as u32) << 8 | (off as u32 & 0xFC)
}

pub fn read32(b: Bdf, off: u16) -> u32 {
    if let Some(a) = ecam_addr(b, off) {
        // SAFETY: inside a mapped ECAM window.
        return unsafe { core::ptr::read_volatile(a as *const u32) };
    }
    if b.seg != 0 || off >= 256 {
        return u32::MAX;
    }
    let _g = PORTS.lock();
    outl(0xCF8, port_addr(b, off));
    inl(0xCFC)
}

pub fn write32(b: Bdf, off: u16, v: u32) {
    if let Some(a) = ecam_addr(b, off) {
        unsafe { core::ptr::write_volatile(a as *mut u32, v) };
        return;
    }
    if b.seg != 0 || off >= 256 {
        return;
    }
    let _g = PORTS.lock();
    outl(0xCF8, port_addr(b, off));
    outl(0xCFC, v)
}

/// A 16- or 8-bit write, without a read-modify-write of the dword (which would write back, and so clear, the
/// write-one-to-clear status bits next to the command register).
fn write_narrow(b: Bdf, off: u16, v: u32, bytes: u8) {
    if let Some(a) = ecam_addr(b, off) {
        let a = a + (off & 3) as u64;
        unsafe {
            if bytes == 2 {
                core::ptr::write_volatile(a as *mut u16, v as u16)
            } else {
                core::ptr::write_volatile(a as *mut u8, v as u8)
            }
        }
        return;
    }
    if b.seg != 0 || off >= 256 {
        return;
    }
    let _g = PORTS.lock();
    outl(0xCF8, port_addr(b, off));
    if bytes == 2 {
        crate::arch::outw(0xCFC + (off & 2), v as u16)
    } else {
        crate::arch::outb(0xCFC + (off & 3), v as u8)
    }
}

/// A 16-bit configuration write (no read-modify-write of the dword).
pub fn write16(b: Bdf, off: u16, v: u16) {
    write_narrow(b, off & !1, v as u32, 2)
}

/// A byte configuration write.
pub fn write8(b: Bdf, off: u16, v: u8) {
    write_narrow(b, off, v as u32, 1)
}

/// A byte write on segment 0, bus 0 (an ACPI register in PCI configuration space, e.g. a reset register).
pub fn write8_bus0(dev: u8, func: u8, off: u16, v: u8) {
    write_narrow(Bdf { seg: 0, bus: 0, dev, func }, off, v as u32, 1);
}

impl Device {
    pub fn bdf(&self) -> Bdf {
        Bdf { seg: self.seg, bus: self.bus, dev: self.dev, func: self.func }
    }
    pub fn read32(&self, off: u16) -> u32 {
        read32(self.bdf(), off)
    }
    pub fn write32(&self, off: u16, v: u32) {
        write32(self.bdf(), off, v)
    }
    pub fn read16(&self, off: u16) -> u16 {
        (self.read32(off & !3) >> ((off & 2) * 8)) as u16
    }
    pub fn write16(&self, off: u16, v: u16) {
        write_narrow(self.bdf(), off & !1, v as u32, 2)
    }
    pub fn read8(&self, off: u16) -> u8 {
        (self.read32(off & !3) >> ((off & 3) * 8)) as u8
    }

    /// Enables I/O and memory space (and bus mastering if `master`; drivers turn it on once the device is reset, so
    /// no DMA a firmware driver left running reaches memory the kernel uses) and disables legacy INTx (we use MSI-X
    /// or polling), on the device and on every bridge above it.
    pub fn enable(&self, master: bool) {
        let cmd = self.read16(0x04);
        self.write16(0x04, cmd | CMD_IO | CMD_MEM | if master { CMD_MASTER } else { 0 } | CMD_INTX_OFF);
        let mut up = self.upstream;
        while let Some(b) = up {
            let Some(d) = devices().iter().find(|d| d.bdf() == b) else { break };
            let cmd = d.read16(0x04);
            if cmd & (CMD_IO | CMD_MEM | CMD_MASTER) != CMD_IO | CMD_MEM | CMD_MASTER {
                d.write16(0x04, cmd | CMD_IO | CMD_MEM | CMD_MASTER);
            }
            up = d.upstream;
        }
    }

    /// BAR `i`: `(is_io, base address)` (a 64-bit BAR takes slots `i` and `i + 1`).
    pub fn bar(&self, i: u8) -> (bool, u64) {
        let off = 0x10 + 4 * i as u16;
        let lo = self.read32(off);
        if lo & 1 != 0 {
            return (true, (lo & !0x3) as u64);
        }
        let mut base = (lo & !0xF) as u64;
        if (lo >> 1) & 0b11 == 0b10 {
            base |= (self.read32(off + 4) as u64) << 32;
        }
        (false, base)
    }

    /// Size of BAR `i` in bytes (0: not implemented), found by writing all ones with decoding off.
    pub fn bar_size(&self, i: u8) -> u64 {
        let off = 0x10 + 4 * i as u16;
        let cmd = self.read16(0x04);
        self.write16(0x04, cmd & !(CMD_IO | CMD_MEM));
        let lo = self.read32(off);
        self.write32(off, u32::MAX);
        let mlo = self.read32(off);
        self.write32(off, lo);
        let size = if lo & 1 != 0 {
            (!(mlo & !0x3) as u64 + 1) & 0xFFFF
        } else if (lo >> 1) & 0b11 == 0b10 {
            let hi = self.read32(off + 4);
            self.write32(off + 4, u32::MAX);
            let mhi = self.read32(off + 4);
            self.write32(off + 4, hi);
            let mask = (mhi as u64) << 32 | (mlo & !0xF) as u64;
            (!mask).wrapping_add(1)
        } else {
            (!(mlo & !0xF) as u64 + 1) & 0xFFFF_FFFF
        };
        self.write16(0x04, cmd);
        if mlo == 0 { 0 } else { size }
    }

    /// Gives every unprogrammed BAR an address (minimal firmware such as qboot leaves them at 0): I/O BARs from
    /// 0xC000 up, memory BARs (64-bit ones too, with a zero high half) from 0xE000_0000 up (inside the uncached
    /// PCI hole below 4 GiB), each aligned to its size. BARs firmware already placed are left alone.
    pub fn assign_bars(&self) {
        let mut i = 0u8;
        while i < 6 {
            let off = 0x10 + 4 * i as u16;
            let orig = self.read32(off);
            let is_io = orig & 1 != 0;
            let is64 = !is_io && (orig >> 1) & 0b11 == 0b10;
            let (_, base) = self.bar(i);
            let size = self.bar_size(i);
            let mut next = NEXT_BAR.lock();
            let (pool, end) = if is_io { (&mut next.0, IO_POOL_END) } else { (&mut next.1, MEM_POOL_END) };
            let addr = (*pool).div_ceil(size.max(1)) * size;
            if size != 0 && size <= 1 << 28 && base == 0 && addr + size <= end {
                *pool = addr + size;
                let cmd = self.read16(0x04);
                self.write16(0x04, cmd & !(CMD_IO | CMD_MEM));
                self.write32(off, addr as u32 | (orig & if is_io { 0x3 } else { 0xF }));
                if is64 {
                    self.write32(off + 4, 0);
                }
                self.write16(0x04, cmd);
            }
            drop(next);
            i += if is64 { 2 } else { 1 };
        }
    }

    /// The capabilities with id `cap`: (offset, the capability's first dword). One configuration read per
    /// capability (each is a VM exit or two under a hypervisor).
    pub fn capabilities(&self, cap: u8) -> impl Iterator<Item = (u16, u32)> + '_ {
        let mut p = if self.read16(0x06) & (1 << 4) != 0 { (self.read8(0x34) & !3) as u16 } else { 0 };
        let mut guard = 48;
        core::iter::from_fn(move || {
            while p != 0 && guard > 0 {
                guard -= 1;
                let here = p;
                let head = self.read32(here);
                p = ((head >> 8) & 0xFC) as u16;
                if head as u8 == cap {
                    return Some((here, head));
                }
            }
            None
        })
    }

    /// The MSI-X table (mapped uncached), or `None` without MSI-X.
    pub fn msix(&self) -> Option<MsiX> {
        let (cap, head) = self.capabilities(CAP_MSIX).next()?;
        let size = ((head >> 16) as u16 & 0x7FF) + 1;
        let t = self.read32(cap + 4);
        let (io, base) = self.bar((t & 7) as u8);
        let table = base + (t & !7) as u64;
        (!io && base != 0 && crate::arch::paging::map_mmio(table, size as u64 * 16)).then_some(MsiX { table, size, cap })
    }

    /// Points the MSI capability at `vector` on this CPU (one message, unmasked) and enables it. False without MSI,
    /// or if this CPU's APIC id does not fit an MSI address.
    pub fn msi_route(&self, vector: u8) -> bool {
        let Some((cap, head)) = self.capabilities(CAP_MSI).next() else { return false };
        let Some((addr, data)) = crate::arch::apic::msi(vector) else { return false };
        let ctl = (head >> 16) as u16;
        self.write16(cap + 2, ctl & !1);
        self.write32(cap + 4, addr as u32);
        // 64-bit capable: the upper address dword, then the data; per-vector masking: the mask dword after the data.
        let data_off = if ctl & (1 << 7) != 0 {
            self.write32(cap + 8, (addr >> 32) as u32);
            cap + 12
        } else {
            cap + 8
        };
        self.write16(data_off, data as u16);
        if ctl & (1 << 8) != 0 {
            self.write32(data_off + 4, 0);
        }
        self.write16(cap + 2, (ctl & !(0x7 << 4)) | 1);
        true
    }

    /// Turns MSI-X on (and the function mask off): entries left masked stay silent.
    pub fn msix_enable(&self, m: &MsiX) {
        let ctl = self.read16(m.cap + 2);
        self.write16(m.cap + 2, (ctl | (1 << 15)) & !(1 << 14));
    }
}

pub struct MsiX {
    table: u64,
    pub size: u16,
    cap: u16,
}

impl MsiX {
    /// Points table entry `entry` at `vector` on this CPU and unmasks it. False if the entry does not exist or this
    /// CPU's APIC id does not fit an MSI address.
    pub fn route(&self, entry: u16, vector: u8) -> bool {
        let Some((addr, data)) = crate::arch::apic::msi(vector) else { return false };
        if entry >= self.size {
            return false;
        }
        let e = (self.table + 16 * entry as u64) as *mut u32;
        // SAFETY: inside the MSI-X table, mapped uncached by `Device::msix`.
        unsafe {
            core::ptr::write_volatile(e.add(3), 1); // masked while it changes
            core::ptr::write_volatile(e, addr as u32);
            core::ptr::write_volatile(e.add(1), (addr >> 32) as u32);
            core::ptr::write_volatile(e.add(2), data);
            core::ptr::write_volatile(e.add(3), 0);
        }
        true
    }
}

fn probe(b: Bdf, upstream: Option<Bdf>) -> Option<(Device, u8)> {
    let id = read32(b, 0);
    if id & 0xFFFF == 0xFFFF || id == 0 {
        return None;
    }
    let class = read32(b, 0x08);
    let header = (read32(b, 0x0C) >> 16) as u8;
    let d = Device {
        seg: b.seg,
        bus: b.bus,
        dev: b.dev,
        func: b.func,
        vendor: id as u16,
        device: (id >> 16) as u16,
        class: (class >> 24) as u8,
        subclass: (class >> 16) as u8,
        upstream,
    };
    Some((d, header))
}

/// Scans `bus` of `seg` and, depth first, the buses behind its bridges.
fn scan_bus(seg: u16, bus: u8, upstream: Option<Bdf>, seen: &mut [u64; 4], out: &mut Vec<Device>) {
    if seen[bus as usize / 64] & (1 << (bus % 64)) != 0 {
        return;
    }
    seen[bus as usize / 64] |= 1 << (bus % 64);
    for dev in 0..32 {
        let Some((d0, h0)) = probe(Bdf { seg, bus, dev, func: 0 }, upstream) else { continue };
        let funcs = if h0 & 0x80 != 0 { 8 } else { 1 };
        for func in 0..funcs {
            let Some((d, h)) = (if func == 0 { Some((d0, h0)) } else { probe(Bdf { seg, bus, dev, func }, upstream) }) else { continue };
            out.push(d);
            if h & 0x7F == 1 {
                let secondary = (d.read32(0x18) >> 8) as u8;
                if secondary > bus {
                    scan_bus(seg, secondary, Some(d.bdf()), seen, out);
                }
            }
        }
    }
}

/// Every PCI function, scanned once (see the module docs).
pub fn devices() -> &'static [Device] {
    if let Some(v) = DEVICES.get() {
        return v;
    }
    let mut out = Vec::new();
    let windows: Vec<(u16, u8, u8)> = match ECAM.get() {
        Some(w) if !w.is_empty() => w.iter().map(|w| (w.segment, w.bus_start, w.bus_end)).collect(),
        _ => alloc::vec![(0, 0, 255)],
    };
    let hypervisor = crate::arch::cpuid(1, 0).2 & (1 << 31) != 0;
    let full = FULL_SCAN.load(Ordering::Relaxed) || !hypervisor;
    for &(seg, first, last) in &windows {
        let mut seen = [0u64; 4];
        scan_bus(seg, first, None, &mut seen, &mut out);
        if full && uses_ecam() {
            for bus in first..=last {
                if seen[bus as usize / 64] & (1 << (bus % 64)) == 0 && (0..32).any(|dev| probe(Bdf { seg, bus, dev, func: 0 }, None).is_some()) {
                    scan_bus(seg, bus, None, &mut seen, &mut out);
                }
            }
        }
    }
    DEVICES.set(out);
    DEVICES.get().unwrap()
}
