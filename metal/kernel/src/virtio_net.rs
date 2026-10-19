//! virtio-net, legacy (virtio 0.9.5) PCI transport: QEMU `-device virtio-net-pci,disable-modern=on`.
//!
//! * Registers are in I/O space (BAR0). Queue 0 = RX, queue 1 = TX. Each split virtqueue is one physically
//!   contiguous area (descriptors, available ring, then the used ring at the next 4 KiB boundary) whose page
//!   number is written to QUEUE_PFN.
//! * Features: only MAC (and ANY_LAYOUT if offered) — no checksum offload, no GSO, no mergeable buffers, so every
//!   frame fits one 2 KiB buffer behind a 10-byte `virtio_net_hdr`.
//! * Buffers are fixed: RX buffer `i` is descriptor `i` forever, likewise for TX; nothing is allocated per packet.
//! * Interrupts: MSI-X vector 0 → `VEC_NET` for RX, suppressed (VRING_AVAIL_F_NO_INTERRUPT) while the kernel is
//!   busy and re-enabled just before it halts. TX completions are reaped lazily, without interrupts.
//!   Notifications to the device ("kicks", one VM exit each) are skipped while the device says it is already
//!   processing (VRING_USED_F_NO_NOTIFY) and batched by the caller (`flush`).

use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{fence, AtomicPtr, Ordering};

use crate::arch::{inb, inl, inw, outb, outl, outw};
use crate::kprintln;
use crate::mem::frames::{self, Tag, FRAME};
use crate::pci;

const REG_HOST_FEATURES: u16 = 0x00;
const REG_GUEST_FEATURES: u16 = 0x04;
const REG_QUEUE_PFN: u16 = 0x08;
const REG_QUEUE_SIZE: u16 = 0x0C;
const REG_QUEUE_SEL: u16 = 0x0E;
const REG_QUEUE_NOTIFY: u16 = 0x10;
const REG_STATUS: u16 = 0x12;
// Present only while MSI-X is enabled; the device-specific config follows them (0x18 instead of 0x14).
const REG_MSIX_CONFIG: u16 = 0x14;
const REG_MSIX_QUEUE: u16 = 0x16;

const STATUS_ACK: u8 = 1;
const STATUS_DRIVER: u8 = 2;
const STATUS_DRIVER_OK: u8 = 4;
const STATUS_FAILED: u8 = 0x80;

const F_MAC: u32 = 1 << 5;
const F_ANY_LAYOUT: u32 = 1 << 27;

const DESC_F_WRITE: u16 = 2;
const AVAIL_F_NO_INTERRUPT: u16 = 1;
const USED_F_NO_NOTIFY: u16 = 1;
const NO_VECTOR: u16 = 0xFFFF;

/// The RX queue's `avail.flags`, so the scheduler can turn RX interrupts on while a CPU-bound invocation runs
/// (an arriving packet then preempts it) without access to the driver object.
static RX_AVAIL_FLAGS: AtomicPtr<u16> = AtomicPtr::new(core::ptr::null_mut());

/// Enables or suppresses RX interrupts (see [`VirtioNet::rx_interrupts`]); no-op before the NIC is up.
pub fn set_rx_interrupts(on: bool) {
    let p = RX_AVAIL_FLAGS.load(Ordering::Relaxed);
    if !p.is_null() {
        unsafe { write_volatile(p, if on { 0 } else { AVAIL_F_NO_INTERRUPT }) };
        fence(Ordering::SeqCst);
    }
}

/// Legacy `virtio_net_hdr` without MRG_RXBUF.
pub const HDR_LEN: usize = 10;
pub const BUF_LEN: usize = 2048;
/// Largest Ethernet frame (without FCS) we send or accept.
pub const MAX_FRAME: usize = 1514;

#[repr(C)]
struct Desc {
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
}

/// One split virtqueue in legacy layout.
pub struct Queue {
    size: u16,
    desc: *mut Desc,
    avail: *mut u16, // flags, idx, ring[size], used_event
    used: *mut u16,  // flags, idx, {id: u32, len: u32}[size], avail_event
    /// Our copy of avail.idx.
    avail_idx: u16,
    /// Next used entry to consume.
    last_used: u16,
    bufs: u64,
    nbufs: u16,
    area: (u64, usize),
}

impl Queue {
    fn avail_ring(&self, i: u16) -> *mut u16 {
        unsafe { self.avail.add(2 + (i % self.size) as usize) }
    }
    fn used_idx(&self) -> u16 {
        unsafe { read_volatile(self.used.add(1)) }
    }
    fn used_flags(&self) -> u16 {
        unsafe { read_volatile(self.used) }
    }
    fn used_elem(&self, i: u16) -> (u32, u32) {
        unsafe {
            let e = (self.used as *mut u8).add(4 + 8 * (i % self.size) as usize) as *const u32;
            (read_volatile(e), read_volatile(e.add(1)))
        }
    }
    fn set_avail_flags(&self, f: u16) {
        unsafe { write_volatile(self.avail, f) }
    }
    pub fn buf(&self, id: u16) -> *mut u8 {
        (self.bufs + id as u64 * BUF_LEN as u64) as *mut u8
    }
    /// Makes descriptor `id` available to the device (publication of `avail.idx` happens here).
    fn push(&mut self, id: u16) {
        unsafe { write_volatile(self.avail_ring(self.avail_idx), id) };
        self.avail_idx = self.avail_idx.wrapping_add(1);
        fence(Ordering::Release);
        unsafe { write_volatile(self.avail.add(1), self.avail_idx) };
    }
    /// Next completed descriptor, if any: `(id, written length)`.
    fn pop_used(&mut self) -> Option<(u16, u32)> {
        if self.last_used == self.used_idx() {
            return None;
        }
        fence(Ordering::Acquire);
        let (id, len) = self.used_elem(self.last_used);
        self.last_used = self.last_used.wrapping_add(1);
        Some((id as u16, len))
    }
    fn has_used(&self) -> bool {
        self.last_used != self.used_idx()
    }
}

/// Receive half: queue 0 with its posted buffers.
pub struct RxSide {
    q: Queue,
    kick_pending: bool,
    pub packets: u64,
    pub bytes: u64,
}

/// Transmit half: queue 1 and its free descriptor list.
pub struct TxSide {
    q: Queue,
    free: alloc::vec::Vec<u16>,
    kick_pending: bool,
    pub packets: u64,
    pub bytes: u64,
    pub full: u64,
}

pub struct VirtioNet {
    io: u16,
    pub mac: [u8; 6],
    pub rx: RxSide,
    pub tx: TxSide,
    pub msix: bool,
    pub kicks: u64,
}

impl RxSide {
    /// Next received frame: `(descriptor id, frame length)`.
    pub fn pop(&mut self) -> Option<(u16, usize)> {
        let (id, len) = self.q.pop_used()?;
        let len = (len as usize).saturating_sub(HDR_LEN).min(BUF_LEN - HDR_LEN);
        self.packets += 1;
        self.bytes += len as u64;
        Some((id, len))
    }
    pub fn frame(&self, id: u16, len: usize) -> &[u8] {
        unsafe { core::slice::from_raw_parts(self.q.buf(id).add(HDR_LEN), len) }
    }
    /// Returns a buffer to the device (the kick is deferred to `VirtioNet::flush`).
    pub fn recycle(&mut self, id: u16) {
        self.q.push(id);
        self.kick_pending = true;
    }
    pub fn pending(&self) -> bool {
        self.q.has_used()
    }
}

impl TxSide {
    fn reap(&mut self) {
        while let Some((id, _)) = self.q.pop_used() {
            self.free.push(id);
        }
    }
    /// True if a frame can be queued now (reaping completions if needed).
    pub fn ready(&mut self) -> bool {
        if self.free.is_empty() {
            self.reap();
        }
        if self.free.is_empty() {
            self.full += 1;
            return false;
        }
        true
    }
    /// Builds a frame of `len` bytes with `f` in a free buffer and queues it (the kick is deferred to `flush`).
    pub fn send<R>(&mut self, len: usize, f: impl FnOnce(&mut [u8]) -> R) -> R {
        let len = len.min(MAX_FRAME);
        let id = match self.free.pop() {
            Some(id) => id,
            None => {
                // smoltcp only builds frames from tokens handed out after `ready()`, so this is unreachable in
                // practice; build into a scratch buffer and drop the frame rather than corrupt a queued one.
                let mut scratch = [0u8; MAX_FRAME];
                return f(&mut scratch[..len]);
            }
        };
        let buf = self.q.buf(id);
        let r = unsafe {
            core::ptr::write_bytes(buf, 0, HDR_LEN);
            f(core::slice::from_raw_parts_mut(buf.add(HDR_LEN), len))
        };
        unsafe {
            write_volatile(
                self.q.desc.add(id as usize),
                Desc { addr: buf as u64, len: (HDR_LEN + len) as u32, flags: 0, next: 0 },
            );
        }
        self.q.push(id);
        self.kick_pending = true;
        self.packets += 1;
        self.bytes += len as u64;
        r
    }
}

impl VirtioNet {
    /// Finds the first legacy virtio-net function (1AF4:1000) and brings it up with `nbufs` RX and TX buffers.
    pub fn probe(nbufs: u16) -> Option<VirtioNet> {
        let d = &pci::find(0x1AF4, 0x1000)?;
        if d.bar(0).1 == 0 {
            d.assign_bars();
        }
        let (is_io, base) = d.bar(0);
        if !is_io {
            kprintln!("virtio-net: BAR0 is not I/O space (modern-only device? use disable-modern=on)");
            return None;
        }
        d.enable();
        let io = base as u16;
        let w8 = |o: u16, v: u8| outb(io + o, v);

        w8(REG_STATUS, 0); // reset
        w8(REG_STATUS, STATUS_ACK);
        w8(REG_STATUS, STATUS_ACK | STATUS_DRIVER);
        let host = inl(io + REG_HOST_FEATURES);
        let guest = host & (F_MAC | F_ANY_LAYOUT);
        outl(io + REG_GUEST_FEATURES, guest);

        // MSI-X: one table entry (vector 0) for RX; it must be enabled before queues get their vectors, and it
        // shifts the device-specific config from 0x14 to 0x18.
        let msix = enable_msix(d);
        if msix {
            outw(io + REG_MSIX_CONFIG, NO_VECTOR);
        }

        let rx = setup_queue(io, 0, nbufs, msix, Some(0));
        let tx = setup_queue(io, 1, nbufs, msix, None);
        let (Some(mut rx), Some(tx)) = (rx, tx) else {
            w8(REG_STATUS, STATUS_FAILED);
            return None;
        };

        let cfg = if msix { 0x18 } else { 0x14 };
        let mut mac = [0u8; 6];
        for (i, m) in mac.iter_mut().enumerate() {
            *m = inb(io + cfg + i as u16);
        }
        if guest & F_MAC == 0 {
            mac = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
        }

        // Post every RX buffer.
        for i in 0..rx.nbufs {
            unsafe {
                write_volatile(
                    rx.desc.add(i as usize),
                    Desc { addr: rx.buf(i) as u64, len: BUF_LEN as u32, flags: DESC_F_WRITE, next: 0 },
                );
            }
            rx.push(i);
        }
        rx.set_avail_flags(AVAIL_F_NO_INTERRUPT);
        tx.set_avail_flags(AVAIL_F_NO_INTERRUPT);
        w8(REG_STATUS, STATUS_ACK | STATUS_DRIVER | STATUS_DRIVER_OK);
        outw(io + REG_QUEUE_NOTIFY, 0);

        let free = (0..tx.nbufs).rev().collect();
        if msix {
            RX_AVAIL_FLAGS.store(rx.avail, Ordering::Relaxed);
        }
        Some(VirtioNet {
            io,
            mac,
            rx: RxSide { q: rx, kick_pending: false, packets: 0, bytes: 0 },
            tx: TxSide { q: tx, free, kick_pending: false, packets: 0, bytes: 0, full: 0 },
            msix,
            kicks: 0,
        })
    }

    /// Sends the notifications owed to the device, unless it said it does not need them.
    pub fn flush(&mut self) {
        fence(Ordering::SeqCst);
        if core::mem::take(&mut self.tx.kick_pending) && self.tx.q.used_flags() & USED_F_NO_NOTIFY == 0 {
            outw(self.io + REG_QUEUE_NOTIFY, 1);
            self.kicks += 1;
        }
        if core::mem::take(&mut self.rx.kick_pending) && self.rx.q.used_flags() & USED_F_NO_NOTIFY == 0 {
            outw(self.io + REG_QUEUE_NOTIFY, 0);
            self.kicks += 1;
        }
    }

    /// Enables (true) or suppresses (false) RX interrupts. After enabling, the caller must re-check
    /// `rx.pending()` before halting: a frame may have arrived while interrupts were suppressed.
    pub fn rx_interrupts(&mut self, on: bool) {
        self.rx.q.set_avail_flags(if on { 0 } else { AVAIL_F_NO_INTERRUPT });
        fence(Ordering::SeqCst);
    }

    /// Bytes of RAM used by the rings and buffers.
    pub fn footprint(&self) -> usize {
        self.rx.q.area.1 + self.tx.q.area.1
    }
}

// --- smoltcp glue -------------------------------------------------------------------------------------------

pub struct RxToken<'a> {
    rx: &'a mut RxSide,
    id: u16,
    len: usize,
}

pub struct TxToken<'a> {
    tx: &'a mut TxSide,
}

impl smoltcp::phy::RxToken for RxToken<'_> {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        let r = f(self.rx.frame(self.id, self.len));
        self.rx.recycle(self.id);
        r
    }
}

impl smoltcp::phy::TxToken for TxToken<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        self.tx.send(len, f)
    }
}

impl smoltcp::phy::Device for VirtioNet {
    type RxToken<'a> = RxToken<'a>;
    type TxToken<'a> = TxToken<'a>;

    fn receive(&mut self, _: smoltcp::time::Instant) -> Option<(RxToken<'_>, TxToken<'_>)> {
        // A received packet may need an immediate reply (ACK/RST), so only hand one out when TX has room.
        if !self.tx.ready() {
            return None;
        }
        let (id, len) = self.rx.pop()?;
        Some((RxToken { rx: &mut self.rx, id, len }, TxToken { tx: &mut self.tx }))
    }

    fn transmit(&mut self, _: smoltcp::time::Instant) -> Option<TxToken<'_>> {
        self.tx.ready().then(|| TxToken { tx: &mut self.tx })
    }

    fn capabilities(&self) -> smoltcp::phy::DeviceCapabilities {
        let mut caps = smoltcp::phy::DeviceCapabilities::default();
        caps.medium = smoltcp::phy::Medium::Ethernet;
        caps.max_transmission_unit = MAX_FRAME;
        caps
    }
}

/// Sets up queue `index`. With MSI-X enabled, `vector` is its MSI-X table entry (None = no interrupts).
fn setup_queue(io: u16, index: u16, nbufs: u16, msix: bool, vector: Option<u16>) -> Option<Queue> {
    outw(io + REG_QUEUE_SEL, index);
    if msix {
        let v = vector.unwrap_or(NO_VECTOR);
        outw(io + REG_MSIX_QUEUE, v);
        if inw(io + REG_MSIX_QUEUE) != v {
            kprintln!("virtio-net: queue {index}: MSI-X vector rejected");
        }
    }
    let size = inw(io + REG_QUEUE_SIZE);
    if size == 0 || inl(io + REG_QUEUE_PFN) != 0 {
        kprintln!("virtio-net: queue {index} unavailable (size {size})");
        return None;
    }
    let n = size as usize;
    let ring_bytes = (16 * n + 6 + 2 * n).div_ceil(FRAME as usize) * FRAME as usize + (6 + 8 * n).div_ceil(FRAME as usize) * FRAME as usize;
    let nbufs = nbufs.min(size);
    let buf_bytes = nbufs as usize * BUF_LEN;
    let total = ring_bytes + buf_bytes.div_ceil(FRAME as usize) * FRAME as usize;
    let nframes = total / FRAME as usize;
    let area = frames::alloc(nframes, 1, Tag::Net)?;
    unsafe { core::ptr::write_bytes(area as *mut u8, 0, ring_bytes) };
    let desc = area as *mut Desc;
    let avail = (area + 16 * n as u64) as *mut u16;
    let used_off = (16 * n + 6 + 2 * n).div_ceil(FRAME as usize) * FRAME as usize;
    let used = (area + used_off as u64) as *mut u16;
    outl(io + REG_QUEUE_PFN, (area / FRAME) as u32);
    Some(Queue {
        size,
        desc,
        avail,
        used,
        avail_idx: 0,
        last_used: 0,
        bufs: area + ring_bytes as u64,
        nbufs,
        area: (area, total),
    })
}

/// Enables MSI-X with table entry 0 → `VEC_NET` on this CPU. Returns false if the function has no MSI-X.
fn enable_msix(d: &pci::Device) -> bool {
    let Some(cap) = d.find_capability(0x11) else { return false };
    let table = d.read32(cap + 4);
    let bir = (table & 7) as u8;
    let (is_io, bar) = d.bar(bir);
    if is_io || bar == 0 {
        return false;
    }
    let entry = (bar + (table & !7) as u64) as *mut u32;
    let (addr, data) = crate::arch::apic::msi(crate::arch::idt::VEC_NET);
    unsafe {
        write_volatile(entry, addr as u32);
        write_volatile(entry.add(1), (addr >> 32) as u32);
        write_volatile(entry.add(2), data);
        write_volatile(entry.add(3), 0); // unmasked
    }
    // Message control: enable (bit 15), clear function mask (bit 14).
    let ctl = d.read16(cap + 2);
    d.write16(cap + 2, (ctl | (1 << 15)) & !(1 << 14));
    true
}
