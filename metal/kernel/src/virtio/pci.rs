//! virtio PCI transports: modern (virtio 1.x capabilities in memory BARs) and legacy (I/O BAR0).

use core::cell::Cell;
use core::ptr::{read_volatile, write_volatile};

use super::{Notifier, Transport, NO_VECTOR};
use crate::arch::{inb, inl, inw, outb, outl, outw};
use crate::pci::Device;

// Legacy registers (I/O space).
const L_HOST_FEATURES: u16 = 0x00;
const L_GUEST_FEATURES: u16 = 0x04;
const L_QUEUE_PFN: u16 = 0x08;
const L_QUEUE_SIZE: u16 = 0x0C;
const L_QUEUE_SEL: u16 = 0x0E;
const L_QUEUE_NOTIFY: u16 = 0x10;
const L_STATUS: u16 = 0x12;
// Present only while MSI-X is enabled; the device-specific config follows them (0x18 instead of 0x14).
const L_MSIX_CONFIG: u16 = 0x14;
const L_MSIX_QUEUE: u16 = 0x16;

// Modern common configuration (virtio 1.x §4.1.4.3).
const C_DEVICE_FEATURE_SELECT: u64 = 0x00;
const C_DEVICE_FEATURE: u64 = 0x04;
const C_DRIVER_FEATURE_SELECT: u64 = 0x08;
const C_DRIVER_FEATURE: u64 = 0x0C;
const C_CONFIG_MSIX_VECTOR: u64 = 0x10;
const C_DEVICE_STATUS: u64 = 0x14;
const C_CONFIG_GENERATION: u64 = 0x15;
const C_QUEUE_SELECT: u64 = 0x16;
const C_QUEUE_SIZE: u64 = 0x18;
const C_QUEUE_MSIX_VECTOR: u64 = 0x1A;
const C_QUEUE_ENABLE: u64 = 0x1C;
const C_QUEUE_NOTIFY_OFF: u64 = 0x1E;
const C_QUEUE_DESC: u64 = 0x20;
const C_QUEUE_DRIVER: u64 = 0x28;
const C_QUEUE_DEVICE: u64 = 0x30;

const CAP_VENDOR: u8 = 0x09;
const CFG_COMMON: u8 = 1;
const CFG_NOTIFY: u8 = 2;
const CFG_DEVICE: u8 = 4;

/// The virtio device type of a virtio PCI function: modern ids are 0x1040 + type, transitional ones (0x1000..0x103F)
/// carry it in the subsystem id.
pub fn device_type(d: &Device) -> Option<u32> {
    match d.device {
        0x1040..=0x107F => Some((d.device - 0x1040) as u32),
        0x1000..=0x103F => Some(d.read16(0x2E) as u32),
        _ => None,
    }
}

/// A virtio PCI function's transport: modern when it has the virtio 1.x capabilities, else legacy (I/O BAR0).
pub fn transport(d: &Device) -> Option<Transport> {
    if d.bar(0).1 == 0 {
        d.assign_bars();
    }
    d.enable(false);
    let t = match Modern::new(d) {
        Some(m) => Transport::PciModern(m),
        None => {
            let (is_io, base) = d.bar(0);
            if !is_io || base == 0 || !(0x1000..=0x103F).contains(&d.device) {
                return None;
            }
            Transport::PciLegacy(Legacy { io: base as u16, msix: Cell::new(false) })
        }
    };
    // Reset (stopping whatever a firmware driver left running), then allow DMA.
    t.set_status(0);
    d.enable(true);
    Some(t)
}

pub struct Legacy {
    io: u16,
    /// MSI-X is enabled: the vector registers exist and the device configuration moves to 0x18.
    msix: Cell<bool>,
}

impl Legacy {
    pub fn status(&self) -> u8 {
        inb(self.io + L_STATUS)
    }
    pub fn set_status(&self, s: u8) {
        outb(self.io + L_STATUS, s)
    }
    pub fn features(&self) -> u64 {
        inl(self.io + L_HOST_FEATURES) as u64
    }
    pub fn set_features(&self, f: u64) {
        outl(self.io + L_GUEST_FEATURES, f as u32)
    }
    /// Must follow enabling MSI-X at the function, before the queues get their vectors.
    pub fn msix_on(&self) {
        self.msix.set(true);
        outw(self.io + L_MSIX_CONFIG, NO_VECTOR);
    }
    pub fn queue_max(&self, q: u16) -> u16 {
        outw(self.io + L_QUEUE_SEL, q);
        if inl(self.io + L_QUEUE_PFN) != 0 {
            return 0; // already in use
        }
        inw(self.io + L_QUEUE_SIZE)
    }
    pub fn queue_setup(&self, q: u16, desc: u64, vector: Option<u16>) -> Option<Notifier> {
        outw(self.io + L_QUEUE_SEL, q);
        if self.msix.get() {
            let v = vector.unwrap_or(NO_VECTOR);
            outw(self.io + L_MSIX_QUEUE, v);
            if inw(self.io + L_MSIX_QUEUE) != v {
                crate::kprintln!("virtio: queue {q}: MSI-X vector rejected");
            }
        }
        outl(self.io + L_QUEUE_PFN, (desc >> 12) as u32);
        Some(Notifier::Port(self.io + L_QUEUE_NOTIFY))
    }
    pub fn config8(&self, off: u16) -> u8 {
        inb(self.io + if self.msix.get() { 0x18 } else { 0x14 } + off)
    }
}

pub struct Modern {
    common: u64,
    notify: u64,
    notify_mult: u32,
    device: u64,
}

impl Modern {
    fn new(d: &Device) -> Option<Modern> {
        let (mut common, mut notify, mut device, mut mult) = (0, 0, 0, 0);
        // Each BAR is sized and mapped once (config accesses are VM exits).
        let mut bars: [Option<Option<u64>>; 6] = [None; 6];
        for cap in d.capabilities(CAP_VENDOR) {
            let kind = d.read8(cap + 3);
            let bar = d.read8(cap + 4);
            let off = d.read32(cap + 8) as u64;
            if bar > 5 || !matches!(kind, CFG_COMMON | CFG_NOTIFY | CFG_DEVICE) {
                continue;
            }
            let Some(base) = *bars[bar as usize].get_or_insert_with(|| d.map_bar(bar)) else { continue };
            match kind {
                CFG_COMMON if common == 0 => common = base + off,
                CFG_NOTIFY if notify == 0 => {
                    notify = base + off;
                    mult = d.read32(cap + 16);
                }
                CFG_DEVICE if device == 0 => device = base + off,
                _ => {}
            }
        }
        (common != 0 && notify != 0).then_some(Modern { common, notify, notify_mult: mult, device })
    }
    fn r8(&self, o: u64) -> u8 {
        unsafe { read_volatile((self.common + o) as *const u8) }
    }
    fn r16(&self, o: u64) -> u16 {
        unsafe { read_volatile((self.common + o) as *const u16) }
    }
    fn r32(&self, o: u64) -> u32 {
        unsafe { read_volatile((self.common + o) as *const u32) }
    }
    fn w8(&self, o: u64, v: u8) {
        unsafe { write_volatile((self.common + o) as *mut u8, v) }
    }
    fn w16(&self, o: u64, v: u16) {
        unsafe { write_volatile((self.common + o) as *mut u16, v) }
    }
    fn w32(&self, o: u64, v: u32) {
        unsafe { write_volatile((self.common + o) as *mut u32, v) }
    }
    fn w64(&self, o: u64, v: u64) {
        self.w32(o, v as u32);
        self.w32(o + 4, (v >> 32) as u32);
    }
    pub fn status(&self) -> u8 {
        self.r8(C_DEVICE_STATUS)
    }
    pub fn set_status(&self, s: u8) {
        self.w8(C_DEVICE_STATUS, s);
        if s == 0 {
            // A reset completes when the status reads back 0 (a device that never does fails negotiation later).
            for _ in 0..1_000_000 {
                if self.r8(C_DEVICE_STATUS) == 0 {
                    break;
                }
                core::hint::spin_loop();
            }
            self.w16(C_CONFIG_MSIX_VECTOR, NO_VECTOR);
        }
    }
    pub fn features(&self) -> u64 {
        self.w32(C_DEVICE_FEATURE_SELECT, 0);
        let lo = self.r32(C_DEVICE_FEATURE) as u64;
        self.w32(C_DEVICE_FEATURE_SELECT, 1);
        lo | (self.r32(C_DEVICE_FEATURE) as u64) << 32
    }
    pub fn set_features(&self, f: u64) {
        self.w32(C_DRIVER_FEATURE_SELECT, 0);
        self.w32(C_DRIVER_FEATURE, f as u32);
        self.w32(C_DRIVER_FEATURE_SELECT, 1);
        self.w32(C_DRIVER_FEATURE, (f >> 32) as u32);
    }
    pub fn generation(&self) -> u32 {
        self.r8(C_CONFIG_GENERATION) as u32
    }
    pub fn queue_max(&self, q: u16) -> u16 {
        self.w16(C_QUEUE_SELECT, q);
        if self.r16(C_QUEUE_ENABLE) != 0 {
            return 0;
        }
        self.r16(C_QUEUE_SIZE)
    }
    pub fn queue_setup(&self, q: u16, size: u16, desc: u64, avail: u64, used: u64, vector: Option<u16>) -> Option<Notifier> {
        self.w16(C_QUEUE_SELECT, q);
        self.w16(C_QUEUE_SIZE, size);
        let v = vector.unwrap_or(NO_VECTOR);
        self.w16(C_QUEUE_MSIX_VECTOR, v);
        if self.r16(C_QUEUE_MSIX_VECTOR) != v {
            crate::kprintln!("virtio: queue {q}: MSI-X vector rejected");
        }
        self.w64(C_QUEUE_DESC, desc);
        self.w64(C_QUEUE_DRIVER, avail);
        self.w64(C_QUEUE_DEVICE, used);
        let off = self.r16(C_QUEUE_NOTIFY_OFF) as u64;
        self.w16(C_QUEUE_ENABLE, 1);
        Some(Notifier::Mmio16(self.notify + off * self.notify_mult as u64))
    }
    pub fn config8(&self, off: u16) -> u8 {
        self.config_wide(off, 1) as u8
    }
    /// A configuration field read with one access of `w` bytes.
    pub fn config_wide(&self, off: u16, w: usize) -> u64 {
        if self.device == 0 {
            return 0;
        }
        let a = self.device + off as u64;
        unsafe {
            match w {
                1 => read_volatile(a as *const u8) as u64,
                2 => read_volatile(a as *const u16) as u64,
                _ => read_volatile(a as *const u32) as u64,
            }
        }
    }
}
