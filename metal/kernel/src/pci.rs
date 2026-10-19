//! PCI configuration space through the legacy port-I/O mechanism (0xCF8 address / 0xCFC data), bus enumeration
//! (bus 0 plus buses behind PCI-PCI bridges), BARs and capability lists.

use alloc::vec::Vec;

use crate::arch::{inl, outl};
use crate::sync::IrqLock;

/// Next free (I/O port, 32-bit MMIO) address for BARs the firmware did not assign.
static NEXT_BAR: IrqLock<(u32, u32)> = IrqLock::new((0xC000, 0xE000_0000));

#[derive(Clone, Copy, Debug)]
pub struct Device {
    pub bus: u8,
    pub dev: u8,
    pub func: u8,
    pub vendor: u16,
    pub device: u16,
    pub class: u8,
    pub subclass: u8,
}

fn addr(bus: u8, dev: u8, func: u8, off: u8) -> u32 {
    0x8000_0000 | (bus as u32) << 16 | (dev as u32) << 11 | (func as u32) << 8 | (off as u32 & 0xFC)
}

pub fn read32(bus: u8, dev: u8, func: u8, off: u8) -> u32 {
    outl(0xCF8, addr(bus, dev, func, off));
    inl(0xCFC)
}

pub fn write32(bus: u8, dev: u8, func: u8, off: u8, v: u32) {
    outl(0xCF8, addr(bus, dev, func, off));
    outl(0xCFC, v)
}

impl Device {
    pub fn read32(&self, off: u8) -> u32 {
        read32(self.bus, self.dev, self.func, off)
    }
    pub fn write32(&self, off: u8, v: u32) {
        write32(self.bus, self.dev, self.func, off, v)
    }
    pub fn read16(&self, off: u8) -> u16 {
        (self.read32(off & !3) >> ((off & 2) * 8)) as u16
    }
    pub fn write16(&self, off: u8, v: u16) {
        let old = self.read32(off & !3);
        let sh = (off & 2) * 8;
        self.write32(off & !3, (old & !(0xFFFF << sh)) | (v as u32) << sh);
    }
    pub fn read8(&self, off: u8) -> u8 {
        (self.read32(off & !3) >> ((off & 3) * 8)) as u8
    }

    /// Enables I/O space, memory space and bus mastering; disables legacy INTx (we use MSI-X or polling).
    pub fn enable(&self) {
        let cmd = self.read16(0x04);
        self.write16(0x04, cmd | 0b111 | (1 << 10));
    }

    /// BAR `i`: `(is_io, base address)`.
    pub fn bar(&self, i: u8) -> (bool, u64) {
        let off = 0x10 + 4 * i;
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

    /// Gives every unprogrammed BAR an address (minimal firmware such as qboot leaves them at 0): I/O BARs from
    /// 0xC000 up, 32-bit memory BARs from 0xE000_0000 up (inside the uncached PCI hole mapped by boot.rs), each
    /// aligned to its size. BARs firmware already placed are left alone.
    pub fn assign_bars(&self) {
        let cmd = self.read16(0x04);
        self.write16(0x04, cmd & !0b11); // decode off while sizing
        let mut i = 0u8;
        while i < 6 {
            let off = 0x10 + 4 * i;
            let orig = self.read32(off);
            let is_io = orig & 1 != 0;
            let is64 = !is_io && (orig >> 1) & 0b11 == 0b10;
            self.write32(off, 0xFFFF_FFFF);
            let mask = self.read32(off);
            self.write32(off, orig);
            let size = if is_io { (!(mask & !0x3)).wrapping_add(1) & 0xFFFF } else { (!(mask & !0xF)).wrapping_add(1) };
            let base = if is_io { orig & !0x3 } else { orig & !0xF };
            if mask != 0 && mask != 0xFFFF_FFFF && size != 0 && base == 0 {
                let mut next = NEXT_BAR.lock();
                let pool = if is_io { &mut next.0 } else { &mut next.1 };
                let addr = (*pool).div_ceil(size) * size;
                *pool = addr + size;
                self.write32(off, addr | (orig & if is_io { 0x3 } else { 0xF }));
                if is64 {
                    self.write32(off + 4, 0);
                }
            }
            i += if is64 { 2 } else { 1 };
        }
        self.write16(0x04, cmd);
    }

    /// Offsets of capabilities with id `cap`.
    pub fn find_capability(&self, cap: u8) -> Option<u8> {
        if self.read16(0x06) & (1 << 4) == 0 {
            return None;
        }
        let mut p = self.read8(0x34) & !3;
        let mut guard = 48;
        while p != 0 && guard > 0 {
            if self.read8(p) == cap {
                return Some(p);
            }
            p = self.read8(p + 1) & !3;
            guard -= 1;
        }
        None
    }
}

fn probe(bus: u8, dev: u8, func: u8) -> Option<Device> {
    let id = read32(bus, dev, func, 0);
    if id & 0xFFFF == 0xFFFF {
        return None;
    }
    let class = read32(bus, dev, func, 0x08);
    Some(Device {
        bus,
        dev,
        func,
        vendor: id as u16,
        device: (id >> 16) as u16,
        class: (class >> 24) as u8,
        subclass: (class >> 16) as u8,
    })
}

/// First function on bus 0 with this vendor/device id (stops scanning there: each config read is two VM exits).
pub fn find(vendor: u16, device: u16) -> Option<Device> {
    for dev in 0..32 {
        let id = read32(0, dev, 0, 0);
        if id & 0xFFFF == 0xFFFF {
            continue;
        }
        let multi = read32(0, dev, 0, 0x0C) >> 16 & 0x80 != 0;
        for func in 0..if multi { 8 } else { 1 } {
            if let Some(d) = probe(0, dev, func) {
                if d.vendor == vendor && d.device == device {
                    return Some(d);
                }
            }
        }
    }
    None
}

/// Enumerates bus 0 and, recursively, the secondary buses of PCI-PCI bridges (a full 256-bus brute-force scan
/// would cost ~130k port accesses = VM exits).
pub fn enumerate() -> Vec<Device> {
    let mut out = Vec::new();
    let mut buses = alloc::vec![0u8];
    let mut seen = [false; 256];
    while let Some(bus) = buses.pop() {
        if core::mem::replace(&mut seen[bus as usize], true) {
            continue;
        }
        for dev in 0..32 {
            let Some(d0) = probe(bus, dev, 0) else { continue };
            let multi = read32(bus, dev, 0, 0x0C) >> 16 & 0x80 != 0;
            for func in 0..if multi { 8 } else { 1 } {
                let Some(d) = (if func == 0 { Some(d0) } else { probe(bus, dev, func) }) else { continue };
                if d.class == 0x06 && d.subclass == 0x04 {
                    buses.push((read32(bus, dev, func, 0x18) >> 8) as u8);
                }
                out.push(d);
            }
        }
    }
    out
}
