//! virtio devices (virtio 1.x and legacy 0.9.5) over three transports, with split virtqueues:
//! * PCI modern (virtio 1.x: `1af4:1041` net, `1af4:1042` block, and transitional devices that offer the modern
//!   capabilities): common/notify/device configuration in memory BARs (64-bit ones above 4 GiB included), 64-bit
//!   queue addresses, FEATURES_OK, VERSION_1 (12-byte net header);
//! * PCI legacy (`disable-modern=on`, GCP gen 1/2): registers in I/O BAR0, one page-aligned area per queue;
//! * MMIO (QEMU microvm, Firecracker), version 2 (modern) or 1 (legacy, microvm's default).
//! Each queue is one zeroed [`DmaRegion`]: descriptors, available ring, then the used ring at the next 4 KiB
//! boundary (the layout legacy devices require, valid for modern ones too). Rings use avail/used flags for
//! notification suppression (no EVENT_IDX).

pub mod blk;
pub mod mmio;
pub mod net;
pub mod pci;

use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{fence, Ordering};

use crate::dev::DmaRegion;
use crate::mem::frames::{Tag, FRAME};

pub const VENDOR: u16 = 0x1AF4;
pub const TYPE_NET: u32 = 1;
pub const TYPE_BLOCK: u32 = 2;

pub const STATUS_ACK: u8 = 1;
pub const STATUS_DRIVER: u8 = 2;
pub const STATUS_DRIVER_OK: u8 = 4;
pub const STATUS_FEATURES_OK: u8 = 8;
pub const STATUS_FAILED: u8 = 0x80;

pub const F_VERSION_1: u64 = 1 << 32;

pub const DESC_F_NEXT: u16 = 1;
pub const DESC_F_WRITE: u16 = 2;
const AVAIL_F_NO_INTERRUPT: u16 = 1;
const USED_F_NO_NOTIFY: u16 = 1;
pub const NO_VECTOR: u16 = 0xFFFF;

pub enum Transport {
    PciLegacy(pci::Legacy),
    PciModern(pci::Modern),
    Mmio(mmio::Mmio),
}

/// How the driver tells the device a queue has new buffers.
#[derive(Clone, Copy)]
pub enum Notifier {
    Port(u16),
    Mmio16(u64),
    Mmio32(u64),
}

impl Notifier {
    fn notify(&self, q: u16) {
        match *self {
            Notifier::Port(p) => crate::arch::outw(p, q),
            Notifier::Mmio16(a) => unsafe { write_volatile(a as *mut u16, q) },
            Notifier::Mmio32(a) => unsafe { write_volatile(a as *mut u32, q as u32) },
        }
    }
}

impl Transport {
    pub fn modern(&self) -> bool {
        match self {
            Transport::PciLegacy(_) => false,
            Transport::PciModern(_) => true,
            Transport::Mmio(m) => m.version() >= 2,
        }
    }
    fn status(&self) -> u8 {
        match self {
            Transport::PciLegacy(t) => t.status(),
            Transport::PciModern(t) => t.status(),
            Transport::Mmio(t) => t.status(),
        }
    }
    pub fn set_status(&self, s: u8) {
        match self {
            Transport::PciLegacy(t) => t.set_status(s),
            Transport::PciModern(t) => t.set_status(s),
            Transport::Mmio(t) => t.set_status(s),
        }
    }
    fn device_features(&self) -> u64 {
        match self {
            Transport::PciLegacy(t) => t.features(),
            Transport::PciModern(t) => t.features(),
            Transport::Mmio(t) => t.features(),
        }
    }
    fn set_driver_features(&self, f: u64) {
        match self {
            Transport::PciLegacy(t) => t.set_features(f),
            Transport::PciModern(t) => t.set_features(f),
            Transport::Mmio(t) => t.set_features(f),
        }
    }
    /// Largest size of queue `q` (0: no such queue).
    fn queue_max(&self, q: u16) -> u16 {
        match self {
            Transport::PciLegacy(t) => t.queue_max(q),
            Transport::PciModern(t) => t.queue_max(q),
            Transport::Mmio(t) => t.queue_max(q),
        }
    }
    /// Whether the transport fixes the queue size (legacy PCI).
    fn fixed_queue_size(&self) -> bool {
        matches!(self, Transport::PciLegacy(_))
    }
    fn queue_setup(&self, q: u16, size: u16, desc: u64, avail: u64, used: u64, vector: Option<u16>) -> Option<Notifier> {
        match self {
            Transport::PciLegacy(t) => t.queue_setup(q, desc, vector),
            Transport::PciModern(t) => t.queue_setup(q, size, desc, avail, used, vector),
            Transport::Mmio(t) => t.queue_setup(q, size, desc, avail, used),
        }
    }
    /// Device-specific configuration byte `off`.
    pub fn config8(&self, off: u16) -> u8 {
        match self {
            Transport::PciLegacy(t) => t.config8(off),
            Transport::PciModern(t) => t.config8(off),
            Transport::Mmio(t) => t.config8(off),
        }
    }
    /// Device configuration field of `N` (1, 2, 4 or 8) bytes, read with accesses of its width (64-bit fields as two
    /// 32-bit halves), as virtio 1.x requires; re-read until the configuration generation is stable.
    pub fn config<const N: usize>(&self, off: u16) -> u64 {
        let w = N.min(4);
        loop {
            let g = self.generation();
            let mut v = 0u64;
            for i in 0..N / w {
                let o = off + (i * w) as u16;
                let part = match self {
                    Transport::PciLegacy(t) => (0..w as u16).fold(0, |a, k| a | (t.config8(o + k) as u64) << (8 * k)),
                    Transport::PciModern(t) => t.config_wide(o, w),
                    Transport::Mmio(t) => t.config_wide(o, w),
                };
                v |= part << (8 * w * i);
            }
            if self.generation() == g {
                return v;
            }
        }
    }
    fn generation(&self) -> u32 {
        match self {
            Transport::PciLegacy(_) => 0,
            Transport::PciModern(t) => t.generation(),
            Transport::Mmio(t) => t.generation(),
        }
    }

    /// Reset, ACKNOWLEDGE, DRIVER, then the feature handshake: `wanted` & offered (+ VERSION_1 on modern
    /// transports, which require it). Returns the negotiated features, or `None` (device marked FAILED) if a
    /// modern device refused them or lacks VERSION_1.
    pub fn negotiate(&self, wanted: u64) -> Option<u64> {
        self.set_status(0);
        self.set_status(STATUS_ACK);
        self.set_status(STATUS_ACK | STATUS_DRIVER);
        let offered = self.device_features();
        if !self.modern() {
            let f = offered & wanted & 0xFFFF_FFFF;
            self.set_driver_features(f);
            return Some(f);
        }
        if offered & F_VERSION_1 == 0 {
            self.set_status(STATUS_FAILED);
            return None;
        }
        let f = offered & (wanted | F_VERSION_1);
        self.set_driver_features(f);
        self.set_status(STATUS_ACK | STATUS_DRIVER | STATUS_FEATURES_OK);
        if self.status() & STATUS_FEATURES_OK == 0 {
            self.set_status(STATUS_FAILED);
            return None;
        }
        Some(f)
    }

    /// DRIVER_OK: the device may start using the queues.
    pub fn ready(&self) {
        let s = self.status();
        self.set_status(s | STATUS_DRIVER_OK);
    }
}

#[repr(C)]
pub struct Desc {
    pub addr: u64,
    pub len: u32,
    pub flags: u16,
    pub next: u16,
}

/// One split virtqueue.
pub struct Queue {
    pub index: u16,
    pub size: u16,
    desc: *mut Desc,
    avail: *mut u16, // flags, idx, ring[size], used_event
    used: *mut u16,  // flags, idx, {id: u32, len: u32}[size], avail_event
    /// Our copy of avail.idx.
    avail_idx: u16,
    /// Next used entry to consume.
    last_used: u16,
    notifier: Notifier,
    mem: DmaRegion,
}

// SAFETY: the rings are exclusively owned DMA memory.
unsafe impl Send for Queue {}

impl Queue {
    /// Sets up queue `index` with `want` entries (fewer if the device's maximum is smaller; exactly the maximum on
    /// transports that fix it), interrupts through MSI-X table entry `vector` on PCI (`None`: none), its memory
    /// accounted under `tag`.
    pub fn new(t: &Transport, index: u16, want: u16, vector: Option<u16>, tag: Tag) -> Option<Queue> {
        let max = t.queue_max(index);
        if max == 0 {
            return None;
        }
        let size = if t.fixed_queue_size() { max } else { want.max(1).next_power_of_two().min(max) };
        let n = size as usize;
        let page = FRAME as usize;
        let used_off = (16 * n + 6 + 2 * n).div_ceil(page) * page;
        let mem = DmaRegion::alloc(used_off + (6 + 8 * n).div_ceil(page) * page, tag)?;
        let base = mem.paddr();
        let notifier = t.queue_setup(index, size, base, base + 16 * n as u64, base + used_off as u64, vector)?;
        Some(Queue {
            index,
            size,
            desc: base as *mut Desc,
            avail: (base + 16 * n as u64) as *mut u16,
            used: (base + used_off as u64) as *mut u16,
            avail_idx: 0,
            last_used: 0,
            notifier,
            mem,
        })
    }
    pub fn footprint(&self) -> usize {
        self.mem.len()
    }
    pub fn set_desc(&mut self, i: u16, d: Desc) {
        unsafe { write_volatile(self.desc.add(i as usize), d) };
    }
    fn used_idx(&self) -> u16 {
        unsafe { read_volatile(self.used.add(1)) }
    }
    fn used_elem(&self, i: u16) -> (u32, u32) {
        unsafe {
            let e = (self.used as *mut u8).add(4 + 8 * (i % self.size) as usize) as *const u32;
            (read_volatile(e), read_volatile(e.add(1)))
        }
    }
    /// Suppresses (true) or allows device interrupts for this queue.
    pub fn set_no_interrupt(&self, off: bool) {
        unsafe { write_volatile(self.avail, if off { AVAIL_F_NO_INTERRUPT } else { 0 }) }
    }
    /// The avail flags word (for the receive-interrupt switch used outside the driver).
    pub fn avail_flags_ptr(&self) -> *mut u16 {
        self.avail
    }
    /// Makes descriptor chain `head` available to the device (publication of `avail.idx` happens here).
    pub fn push(&mut self, head: u16) {
        unsafe { write_volatile(self.avail.add(2 + (self.avail_idx % self.size) as usize), head) };
        self.avail_idx = self.avail_idx.wrapping_add(1);
        fence(Ordering::Release);
        unsafe { write_volatile(self.avail.add(1), self.avail_idx) };
    }
    /// Next completed chain, if any: `(head id, written length)`.
    pub fn pop_used(&mut self) -> Option<(u16, u32)> {
        if self.last_used == self.used_idx() {
            return None;
        }
        fence(Ordering::Acquire);
        let (id, len) = self.used_elem(self.last_used);
        self.last_used = self.last_used.wrapping_add(1);
        Some((id as u16, len))
    }
    pub fn has_used(&self) -> bool {
        self.last_used != self.used_idx()
    }
    /// Notifies the device unless it said it does not need it. Returns whether a notification was sent.
    pub fn kick(&self) -> bool {
        fence(Ordering::SeqCst);
        if unsafe { read_volatile(self.used) } & USED_F_NO_NOTIFY != 0 {
            return false;
        }
        self.notifier.notify(self.index);
        true
    }
}
