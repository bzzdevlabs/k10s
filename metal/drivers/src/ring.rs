//! Descriptor rings with one fixed buffer per descriptor, as the virtio driver keeps them: receive buffer `i` is
//! descriptor `i` for the ring's lifetime, likewise for transmit, so nothing is allocated per packet and a frame is
//! handed out by buffer id. Every format here is 16 bytes per descriptor.
//!
//! Ownership: Intel rings give the device the descriptors between its head and the tail register the driver
//! writes; one receive descriptor is always held back (tail == head means "none for the device"), the most
//! recently returned one, and buffers come back in the order they were handed out (an early return waits for the
//! ones before it). Realtek rings have no tail register: an OWN bit per descriptor, set last.

use alloc::vec::Vec;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{fence, Ordering};

use crate::{DmaMem, Hw};

pub const BUF: usize = 2048;
/// Largest Ethernet frame (without FCS) sent or accepted.
pub const MAX_FRAME: usize = 1514;
/// Smallest frame on the wire without FCS; shorter ones are padded (by the MAC, or here for Realtek).
pub const MIN_FRAME: usize = 60;
const DESC: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    /// Intel legacy descriptors (e1000, e1000e): status byte 12 (DD bit 0, EOP bit 1), errors byte 13, length at 8.
    IntelLegacy,
    /// Intel advanced one-buffer descriptors (igb, igc): status/error dword at 8 (DD bit 0, EOP bit 1), length at
    /// 12; the device overwrites the buffer address with write-back fields, so it is written again on reuse.
    IntelAdvanced,
    /// Realtek (r8169, r8125): OWN/EOR/FS/LS and the length in the first dword, the buffer address at 8.
    Realtek,
}

const RTL_OWN: u32 = 1 << 31;
const RTL_EOR: u32 = 1 << 30;
const RTL_FS: u32 = 1 << 29;
const RTL_LS: u32 = 1 << 28;
/// Receive error summary (RxRES).
const RTL_RX_RES: u32 = 1 << 21;
/// Legacy receive errors that drop a frame: CE, SE, SEQ, CXE, RXE (IPE/TCPE are checksum results: smoltcp
/// checks checksums itself).
const LEGACY_RX_ERRORS: u8 = 0x97;
/// Advanced receive errors: CE, SE, SEQ, CXE, RXE.
const ADV_RX_ERRORS: u32 = 0x9700_0000;
const LEGACY_TX_EOP: u8 = 1 << 0;
const LEGACY_TX_IFCS: u8 = 1 << 1;
const LEGACY_TX_RS: u8 = 1 << 3;
const ADV_TX_DTYP_DATA: u32 = 0x3 << 20;
const ADV_TX_EOP: u32 = 1 << 24;
const ADV_TX_IFCS: u32 = 1 << 25;
const ADV_TX_RS: u32 = 1 << 27;
const ADV_TX_DEXT: u32 = 1 << 29;
const ADV_TX_PAYLEN_SHIFT: u32 = 14;

struct Ring<M> {
    ring: M,
    bufs: M,
    n: u16,
}

impl<M: DmaMem> Ring<M> {
    fn alloc(hw: &impl Hw<Mem = M>, n: u16) -> Option<Ring<M>> {
        Some(Ring { ring: hw.dma(n as usize * DESC)?, bufs: hw.dma(n as usize * BUF)?, n })
    }
    fn desc(&self, i: u16) -> *mut u8 {
        // SAFETY: i < n, inside the ring.
        unsafe { self.ring.ptr().add(i as usize * DESC) }
    }
    fn buf_paddr(&self, i: u16) -> u64 {
        self.bufs.paddr() + i as u64 * BUF as u64
    }
    fn buf(&self, i: u16) -> *mut u8 {
        unsafe { self.bufs.ptr().add(i as usize * BUF) }
    }
    fn r32(&self, i: u16, off: usize) -> u32 {
        unsafe { read_volatile(self.desc(i).add(off) as *const u32) }
    }
    fn w32(&self, i: u16, off: usize, v: u32) {
        unsafe { write_volatile(self.desc(i).add(off) as *mut u32, v) }
    }
    fn w64(&self, i: u16, off: usize, v: u64) {
        self.w32(i, off, v as u32);
        self.w32(i, off + 4, (v >> 32) as u32);
    }
}

/// The receive half of a queue pair.
pub struct Rx<M> {
    fmt: Format,
    r: Ring<M>,
    /// Next descriptor the device completes.
    next: u16,
    /// Intel: the tail register's value, i.e. the descriptor held back (the last one returned).
    tail: u16,
    /// Returned ahead of their turn (Intel).
    returned: Vec<bool>,
    /// Handed out and not returned yet: not a new frame even though its descriptor reads as completed.
    out: Vec<bool>,
    tail_dirty: bool,
    pub packets: u64,
    pub bytes: u64,
    pub errors: u64,
}

/// What a completed receive descriptor says.
enum Done {
    Frame(usize),
    Bad,
}

impl<M: DmaMem> Rx<M> {
    /// `n` descriptors (a multiple of 8: Intel rings are 128-byte multiples), every one armed; the first tail is
    /// [`Rx::tail`].
    pub fn new(hw: &impl Hw<Mem = M>, fmt: Format, n: u16) -> Option<Rx<M>> {
        let n = (n.max(8) / 8) * 8;
        let r = Ring::alloc(hw, n)?;
        let rx = Rx {
            fmt,
            r,
            next: 0,
            tail: n - 1,
            returned: alloc::vec![false; n as usize],
            out: alloc::vec![false; n as usize],
            tail_dirty: false,
            packets: 0,
            bytes: 0,
            errors: 0,
        };
        for i in 0..n {
            rx.arm(i);
        }
        fence(Ordering::SeqCst);
        Some(rx)
    }
    pub fn ring_paddr(&self) -> u64 {
        self.r.ring.paddr()
    }
    pub fn len(&self) -> u16 {
        self.r.n
    }
    pub fn ring_bytes(&self) -> u32 {
        self.r.n as u32 * DESC as u32
    }
    /// The tail register value to write (Intel).
    pub fn tail(&self) -> u16 {
        self.tail
    }
    /// Whether the tail moved since the last call (the caller then writes it).
    pub fn take_tail(&mut self) -> Option<u16> {
        core::mem::take(&mut self.tail_dirty).then_some(self.tail)
    }
    pub fn footprint(&self) -> usize {
        self.r.ring.len() + self.r.bufs.len()
    }

    fn arm(&self, i: u16) {
        let addr = self.r.buf_paddr(i);
        match self.fmt {
            Format::IntelLegacy | Format::IntelAdvanced => {
                self.r.w64(i, 0, addr);
                self.r.w64(i, 8, 0);
            }
            Format::Realtek => {
                self.r.w64(i, 8, addr);
                self.r.w32(i, 4, 0);
                fence(Ordering::Release);
                let eor = if i == self.r.n - 1 { RTL_EOR } else { 0 };
                self.r.w32(i, 0, RTL_OWN | eor | BUF as u32);
            }
        }
    }

    fn done(&self, i: u16) -> Option<Done> {
        if self.out[i as usize] {
            return None;
        }
        let d = match self.fmt {
            Format::IntelLegacy => {
                let w = self.r.r32(i, 12);
                if w & 1 == 0 {
                    return None;
                }
                fence(Ordering::Acquire);
                let len = self.r.r32(i, 8) as u16 as usize;
                let (status, errors) = (w as u8, (w >> 8) as u8);
                if status & 2 == 0 || errors & LEGACY_RX_ERRORS != 0 { Done::Bad } else { Done::Frame(len) }
            }
            Format::IntelAdvanced => {
                let s = self.r.r32(i, 8);
                if s & 1 == 0 {
                    return None;
                }
                fence(Ordering::Acquire);
                let len = self.r.r32(i, 12) as u16 as usize;
                if s & 2 == 0 || s & ADV_RX_ERRORS != 0 { Done::Bad } else { Done::Frame(len) }
            }
            Format::Realtek => {
                let s = self.r.r32(i, 0);
                if s & RTL_OWN != 0 {
                    return None;
                }
                fence(Ordering::Acquire);
                // The length counts the frame check sequence.
                let len = ((s & 0x3FFF) as usize).saturating_sub(4);
                if s & (RTL_FS | RTL_LS) != RTL_FS | RTL_LS || s & RTL_RX_RES != 0 { Done::Bad } else { Done::Frame(len) }
            }
        };
        Some(d)
    }

    /// Next received frame: (buffer id, length). Bad frames are counted and their buffers returned.
    pub fn pop(&mut self) -> Option<(u16, usize)> {
        loop {
            let d = self.done(self.next)?;
            let id = self.next;
            self.next = (self.next + 1) % self.r.n;
            self.out[id as usize] = true;
            match d {
                Done::Frame(len) if (14..=BUF).contains(&len) => {
                    self.packets += 1;
                    self.bytes += len as u64;
                    return Some((id, len));
                }
                _ => {
                    self.errors += 1;
                    self.recycle(id);
                }
            }
        }
    }

    pub fn frame(&self, id: u16, len: usize) -> &[u8] {
        // SAFETY: id < n, len <= BUF (checked by pop); the device does not own a popped buffer.
        unsafe { core::slice::from_raw_parts(self.r.buf(id), len.min(BUF)) }
    }

    /// Gives buffer `id` back to the device.
    pub fn recycle(&mut self, id: u16) {
        if id >= self.r.n || !core::mem::take(&mut self.out[id as usize]) {
            return;
        }
        if self.fmt == Format::Realtek {
            self.arm(id);
            return;
        }
        self.returned[id as usize] = true;
        loop {
            let want = (self.tail + 1) % self.r.n;
            if !core::mem::take(&mut self.returned[want as usize]) {
                break;
            }
            self.arm(want);
            self.tail = want;
            self.tail_dirty = true;
        }
    }

    pub fn pending(&self) -> bool {
        self.done(self.next).is_some()
    }
}

/// The transmit half of a queue pair.
pub struct Tx<M> {
    fmt: Format,
    r: Ring<M>,
    /// Next descriptor to fill.
    next: u16,
    /// Oldest descriptor the device may still own.
    clean: u16,
    inflight: u16,
    dirty: bool,
    pub packets: u64,
    pub bytes: u64,
    pub full: u64,
}

impl<M: DmaMem> Tx<M> {
    pub fn new(hw: &impl Hw<Mem = M>, fmt: Format, n: u16) -> Option<Tx<M>> {
        let n = (n.max(8) / 8) * 8;
        let r = Ring::alloc(hw, n)?;
        if fmt == Format::Realtek {
            r.w32(n - 1, 0, RTL_EOR);
        }
        Some(Tx { fmt, r, next: 0, clean: 0, inflight: 0, dirty: false, packets: 0, bytes: 0, full: 0 })
    }
    pub fn ring_paddr(&self) -> u64 {
        self.r.ring.paddr()
    }
    pub fn len(&self) -> u16 {
        self.r.n
    }
    pub fn ring_bytes(&self) -> u32 {
        self.r.n as u32 * DESC as u32
    }
    /// The tail register value (Intel): the next descriptor to fill.
    pub fn tail(&self) -> u16 {
        self.next
    }
    /// Whether frames were queued since the last call (the caller then rings the doorbell).
    pub fn take_dirty(&mut self) -> bool {
        core::mem::take(&mut self.dirty)
    }
    pub fn footprint(&self) -> usize {
        self.r.ring.len() + self.r.bufs.len()
    }
    pub fn inflight(&self) -> u16 {
        self.inflight
    }

    fn done(&self, i: u16) -> bool {
        let d = match self.fmt {
            Format::IntelLegacy | Format::IntelAdvanced => self.r.r32(i, 12) & 1 != 0,
            Format::Realtek => self.r.r32(i, 0) & RTL_OWN == 0,
        };
        if d {
            fence(Ordering::Acquire);
        }
        d
    }

    /// Takes back the descriptors the device has sent.
    pub fn reap(&mut self) {
        while self.inflight > 0 && self.done(self.clean) {
            self.clean = (self.clean + 1) % self.r.n;
            self.inflight -= 1;
        }
    }

    /// True if a frame can be queued now (reaping completions if needed).
    pub fn ready(&mut self) -> bool {
        if self.inflight + 1 >= self.r.n {
            self.reap();
        }
        if self.inflight + 1 >= self.r.n {
            self.full += 1;
            return false;
        }
        true
    }

    /// Builds a frame of `len` bytes with `fill` in the next buffer and queues it (the doorbell is the caller's).
    pub fn send(&mut self, len: usize, fill: &mut dyn FnMut(&mut [u8])) {
        let len = len.min(MAX_FRAME);
        if self.inflight + 1 >= self.r.n {
            // Callers ask `ready` first; build into scratch and drop rather than overwrite a queued frame.
            let mut scratch = [0u8; MAX_FRAME];
            return fill(&mut scratch[..len]);
        }
        let i = self.next;
        let b = self.r.buf(i);
        // SAFETY: buffer i is not owned by the device (inflight < n - 1).
        let wire = unsafe {
            fill(core::slice::from_raw_parts_mut(b, len));
            if self.fmt == Format::Realtek && len < MIN_FRAME {
                core::ptr::write_bytes(b.add(len), 0, MIN_FRAME - len);
                MIN_FRAME
            } else {
                len
            }
        };
        self.write(i, wire);
        self.next = (i + 1) % self.r.n;
        self.inflight += 1;
        self.dirty = true;
        self.packets += 1;
        self.bytes += len as u64;
    }

    fn write(&self, i: u16, len: usize) {
        let addr = self.r.buf_paddr(i);
        match self.fmt {
            Format::IntelLegacy => {
                self.r.w64(i, 0, addr);
                let cmd = (LEGACY_TX_EOP | LEGACY_TX_IFCS | LEGACY_TX_RS) as u32;
                self.r.w32(i, 8, len as u32 | cmd << 24);
                self.r.w32(i, 12, 0);
            }
            Format::IntelAdvanced => {
                self.r.w64(i, 0, addr);
                self.r.w32(i, 8, len as u32 | ADV_TX_DTYP_DATA | ADV_TX_DEXT | ADV_TX_EOP | ADV_TX_IFCS | ADV_TX_RS);
                self.r.w32(i, 12, (len as u32) << ADV_TX_PAYLEN_SHIFT);
            }
            Format::Realtek => {
                self.r.w64(i, 8, addr);
                self.r.w32(i, 4, 0);
                fence(Ordering::Release);
                let eor = if i == self.r.n - 1 { RTL_EOR } else { 0 };
                self.r.w32(i, 0, RTL_OWN | eor | RTL_FS | RTL_LS | len as u32);
            }
        }
    }

    /// Queues one frame of `len` zero bytes without the RS/EOP bits Linux leaves out too (I219 descriptor-ring
    /// flush, e1000e.rs). Only on an idle ring.
    pub(crate) fn queue_flush_frame(&mut self, len: u32) {
        let i = self.next;
        self.r.w64(i, 0, self.r.buf_paddr(i));
        self.r.w32(i, 8, len | (LEGACY_TX_IFCS as u32) << 24);
        self.r.w32(i, 12, 0);
        self.next = (i + 1) % self.r.n;
        self.clean = self.next;
    }
}
