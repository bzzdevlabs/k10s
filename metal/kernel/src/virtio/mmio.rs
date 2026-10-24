//! virtio-mmio transport (virtio 1.x §4.2): version 2 (modern, Firecracker) and version 1 (legacy, QEMU microvm's
//! default).

use core::ptr::{read_volatile, write_volatile};

use super::Notifier;

const MAGIC: u64 = 0x000;
const VERSION: u64 = 0x004;
const DEVICE_ID: u64 = 0x008;
const DEVICE_FEATURES: u64 = 0x010;
const DEVICE_FEATURES_SEL: u64 = 0x014;
const DRIVER_FEATURES: u64 = 0x020;
const DRIVER_FEATURES_SEL: u64 = 0x024;
const GUEST_PAGE_SIZE: u64 = 0x028; // v1
const QUEUE_SEL: u64 = 0x030;
const QUEUE_NUM_MAX: u64 = 0x034;
const QUEUE_NUM: u64 = 0x038;
const QUEUE_ALIGN: u64 = 0x03C; // v1
const QUEUE_PFN: u64 = 0x040; // v1
const QUEUE_READY: u64 = 0x044; // v2
const QUEUE_NOTIFY: u64 = 0x050;
const INTERRUPT_STATUS: u64 = 0x060;
const INTERRUPT_ACK: u64 = 0x064;
const STATUS: u64 = 0x070;
const QUEUE_DESC_LOW: u64 = 0x080;
const QUEUE_DRIVER_LOW: u64 = 0x090;
const QUEUE_DEVICE_LOW: u64 = 0x0A0;
const CONFIG_GENERATION: u64 = 0x0FC;
const CONFIG: u64 = 0x100;

#[derive(Clone, Copy)]
pub struct Mmio {
    base: u64,
    version: u32,
}

impl Mmio {
    /// The slot at `base` (mapped uncached) if it holds a virtio device of a supported version.
    pub fn new(base: u64, size: u64) -> Option<Mmio> {
        if !crate::arch::paging::map_mmio(base, size.max(0x200)) {
            return None;
        }
        let m = Mmio { base, version: 0 };
        let version = m.r32(VERSION);
        (m.r32(MAGIC) == 0x7472_6976 && matches!(version, 1 | 2)).then_some(Mmio { base, version })
    }
    fn r32(&self, o: u64) -> u32 {
        unsafe { read_volatile((self.base + o) as *const u32) }
    }
    fn w32(&self, o: u64, v: u32) {
        unsafe { write_volatile((self.base + o) as *mut u32, v) }
    }
    pub fn version(&self) -> u32 {
        self.version
    }
    /// 0: an empty slot.
    pub fn device_type(&self) -> u32 {
        self.r32(DEVICE_ID)
    }
    pub fn status(&self) -> u8 {
        self.r32(STATUS) as u8
    }
    pub fn set_status(&self, s: u8) {
        self.w32(STATUS, s as u32);
        if s == 0 {
            // A reset completes when the status reads back 0.
            for _ in 0..1_000_000 {
                if self.r32(STATUS) == 0 {
                    break;
                }
                core::hint::spin_loop();
            }
        }
    }
    pub fn features(&self) -> u64 {
        self.w32(DEVICE_FEATURES_SEL, 0);
        let lo = self.r32(DEVICE_FEATURES) as u64;
        if self.version < 2 {
            return lo;
        }
        self.w32(DEVICE_FEATURES_SEL, 1);
        lo | (self.r32(DEVICE_FEATURES) as u64) << 32
    }
    pub fn set_features(&self, f: u64) {
        self.w32(DRIVER_FEATURES_SEL, 0);
        self.w32(DRIVER_FEATURES, f as u32);
        self.w32(DRIVER_FEATURES_SEL, 1);
        self.w32(DRIVER_FEATURES, (f >> 32) as u32);
        if self.version < 2 {
            self.w32(GUEST_PAGE_SIZE, 4096);
        }
    }
    pub fn generation(&self) -> u32 {
        if self.version < 2 { 0 } else { self.r32(CONFIG_GENERATION) }
    }
    pub fn queue_max(&self, q: u16) -> u16 {
        self.w32(QUEUE_SEL, q as u32);
        let in_use = if self.version < 2 { self.r32(QUEUE_PFN) != 0 } else { self.r32(QUEUE_READY) != 0 };
        if in_use { 0 } else { self.r32(QUEUE_NUM_MAX).min(u16::MAX as u32) as u16 }
    }
    pub fn queue_setup(&self, q: u16, size: u16, desc: u64, avail: u64, used: u64) -> Option<Notifier> {
        self.w32(QUEUE_SEL, q as u32);
        self.w32(QUEUE_NUM, size as u32);
        if self.version < 2 {
            self.w32(QUEUE_ALIGN, 4096);
            self.w32(QUEUE_PFN, (desc >> 12) as u32);
        } else {
            for (reg, a) in [(QUEUE_DESC_LOW, desc), (QUEUE_DRIVER_LOW, avail), (QUEUE_DEVICE_LOW, used)] {
                self.w32(reg, a as u32);
                self.w32(reg + 4, (a >> 32) as u32);
            }
            self.w32(QUEUE_READY, 1);
        }
        Some(Notifier::Mmio32(self.base + QUEUE_NOTIFY))
    }
    pub fn config8(&self, off: u16) -> u8 {
        self.config_wide(off, 1) as u8
    }
    /// A configuration field read with one access of `w` bytes.
    pub fn config_wide(&self, off: u16, w: usize) -> u64 {
        let a = self.base + CONFIG + off as u64;
        unsafe {
            match w {
                1 => read_volatile(a as *const u8) as u64,
                2 => read_volatile(a as *const u16) as u64,
                _ => read_volatile(a as *const u32) as u64,
            }
        }
    }
    /// Acknowledges every pending interrupt cause (the line drops until the device raises it again).
    pub fn ack_interrupt(&self) {
        let s = self.r32(INTERRUPT_STATUS);
        if s != 0 {
            self.w32(INTERRUPT_ACK, s);
        }
    }
    pub fn base(&self) -> u64 {
        self.base
    }
}

/// Acknowledges the interrupt of the device at `base` (from the interrupt handler, through the NIC hook).
pub fn ack_at(base: u64) {
    Mmio { base, version: 2 }.ack_interrupt()
}
