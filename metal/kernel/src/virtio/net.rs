//! virtio-net on any virtio transport, behind [`NetDevice`].
//!
//! * Queue 0 = RX, queue 1 = TX. Features: only MAC (plus ANY_LAYOUT on legacy devices, VERSION_1 on modern ones)
//!   — no checksum offload, no GSO, no mergeable buffers, so every frame fits one 2 KiB buffer behind the
//!   `virtio_net_hdr` (10 bytes legacy, 12 bytes with VERSION_1).
//! * Buffers are fixed: RX buffer `i` is descriptor `i` forever, likewise for TX; nothing is allocated per packet.
//!   A legacy device without ANY_LAYOUT gets the header in a descriptor of its own: buffer `i` is then the chain
//!   `2i` (header) → `2i + 1` (frame). Buffer ids the device hands back are checked before use.
//! * Interrupts: RX only, through MSI-X entry 0 on PCI or the slot's I/O APIC line on MMIO (acknowledged at the
//!   device in the handler and before halting), to `VEC_NET` on CPU 0; suppressed (VRING_AVAIL_F_NO_INTERRUPT)
//!   while the kernel is busy and re-enabled just before it halts. TX completions are reaped lazily, without
//!   interrupts. Notifications to the device ("kicks", one VM exit each) are skipped while the device says it is
//!   already processing (VRING_USED_F_NO_NOTIFY) and batched by the caller (`flush`).

use core::ptr::write_volatile;
use core::sync::atomic::{fence, AtomicPtr, AtomicU64, Ordering};

use super::{Desc, Queue, Transport, DESC_F_NEXT, DESC_F_WRITE};
use crate::dev::{DmaRegion, NetCounters, NetDevice, NetRx, NetTx};
use crate::kprintln;
use crate::mem::frames::Tag;

const F_MAC: u64 = 1 << 5;
const F_ANY_LAYOUT: u64 = 1 << 27;

pub const BUF_LEN: usize = 2048;
/// Largest Ethernet frame (without FCS) we send or accept.
pub const MAX_FRAME: usize = 1514;

/// The RX queue's `avail.flags`, so the scheduler can turn RX interrupts on while a CPU-bound invocation runs
/// (an arriving packet then preempts it) without access to the driver object.
static RX_AVAIL_FLAGS: AtomicPtr<u16> = AtomicPtr::new(core::ptr::null_mut());
/// The MMIO device whose interrupt line must be acknowledged (0: MSI-X, nothing to do).
static MMIO_ACK: AtomicU64 = AtomicU64::new(0);

fn set_rx_interrupts(on: bool) {
    let p = RX_AVAIL_FLAGS.load(Ordering::Relaxed);
    if !p.is_null() {
        unsafe { write_volatile(p, if on { 0 } else { 1 }) };
        fence(Ordering::SeqCst);
    }
}

fn ack_irq() {
    let base = MMIO_ACK.load(Ordering::Relaxed);
    if base != 0 {
        super::mmio::ack_at(base);
    }
}

pub struct Rx {
    q: Queue,
    bufs: DmaRegion,
    nbufs: u16,
    hdr: usize,
    /// Header and frame in two chained descriptors (legacy device without ANY_LAYOUT).
    chain: bool,
    kick_pending: bool,
    packets: u64,
    bytes: u64,
}

pub struct Tx {
    q: Queue,
    bufs: DmaRegion,
    nbufs: u16,
    hdr: usize,
    chain: bool,
    free: alloc::vec::Vec<u16>,
    /// Buffers the device holds (a completion for any other is ignored).
    queued: alloc::vec::Vec<bool>,
    kick_pending: bool,
    packets: u64,
    bytes: u64,
    full: u64,
}

pub struct VirtioNet {
    /// Kept for the device's lifetime (its registers).
    _t: Transport,
    name: &'static str,
    mac: [u8; 6],
    rx: Rx,
    tx: Tx,
    irq: bool,
    kicks: u64,
}

fn buf(region: &DmaRegion, id: u16) -> *mut u8 {
    (region.paddr() + id as u64 * BUF_LEN as u64) as *mut u8
}

/// The head descriptor of buffer `id`, and the buffer of head descriptor `head` (`None`: not one of ours).
fn head(chain: bool, id: u16) -> u16 {
    if chain { 2 * id } else { id }
}
fn buffer(chain: bool, head: u16, nbufs: u16) -> Option<u16> {
    let id = if chain { (head % 2 == 0).then_some(head / 2)? } else { head };
    (id < nbufs).then_some(id)
}

impl NetRx for Rx {
    fn pop(&mut self) -> Option<(u16, usize)> {
        loop {
            let (h, len) = self.q.pop_used()?;
            let Some(id) = buffer(self.chain, h, self.nbufs) else { continue };
            let len = (len as usize).saturating_sub(self.hdr).min(BUF_LEN - self.hdr);
            self.packets += 1;
            self.bytes += len as u64;
            return Some((id, len));
        }
    }
    fn frame(&self, id: u16, len: usize) -> &[u8] {
        unsafe { core::slice::from_raw_parts(buf(&self.bufs, id).add(self.hdr), len) }
    }
    fn recycle(&mut self, id: u16) {
        self.q.push(head(self.chain, id));
        self.kick_pending = true;
    }
    fn pending(&self) -> bool {
        self.q.has_used()
    }
}

impl Tx {
    fn reap(&mut self) {
        while let Some((h, _)) = self.q.pop_used() {
            if let Some(id) = buffer(self.chain, h, self.nbufs) {
                if core::mem::take(&mut self.queued[id as usize]) {
                    self.free.push(id);
                }
            }
        }
    }
}

impl NetTx for Tx {
    fn ready(&mut self) -> bool {
        if self.free.is_empty() {
            self.reap();
        }
        if self.free.is_empty() {
            self.full += 1;
            return false;
        }
        true
    }
    fn send(&mut self, len: usize, fill: &mut dyn FnMut(&mut [u8])) {
        let len = len.min(MAX_FRAME);
        let Some(id) = self.free.pop() else {
            // smoltcp only builds frames from tokens handed out after `ready()`, so this is unreachable in
            // practice; build into a scratch buffer and drop the frame rather than corrupt a queued one.
            let mut scratch = [0u8; MAX_FRAME];
            return fill(&mut scratch[..len]);
        };
        let b = buf(&self.bufs, id);
        unsafe {
            core::ptr::write_bytes(b, 0, self.hdr);
            fill(core::slice::from_raw_parts_mut(b.add(self.hdr), len));
        }
        if self.chain {
            self.q.set_desc(2 * id + 1, Desc { addr: b as u64 + self.hdr as u64, len: len as u32, flags: 0, next: 0 });
        } else {
            self.q.set_desc(id, Desc { addr: b as u64, len: (self.hdr + len) as u32, flags: 0, next: 0 });
        }
        self.queued[id as usize] = true;
        self.q.push(head(self.chain, id));
        self.kick_pending = true;
        self.packets += 1;
        self.bytes += len as u64;
    }
}

impl VirtioNet {
    /// Brings up a virtio-net PCI function with `nbufs` RX and TX buffers.
    pub fn probe_pci(d: &crate::pci::Device, nbufs: u16) -> Option<VirtioNet> {
        let t = super::pci::transport(d)?;
        let name = if t.modern() { "virtio-net (pci)" } else { "virtio-net (pci legacy)" };
        // RX interrupts: MSI-X entry 0. On legacy devices MSI-X must be on before the queues get their vectors.
        let features = t.negotiate(F_MAC | F_ANY_LAYOUT)?;
        let msix = d.msix();
        let irq = msix.as_ref().is_some_and(|m| m.route(0, crate::arch::idt::VEC_NET));
        if let Some(m) = &msix {
            d.msix_enable(m);
            if let Transport::PciLegacy(l) = &t {
                l.msix_on();
            }
        }
        Self::init(t, name, features, nbufs, irq.then_some(0), irq, 0)
    }

    /// Brings up a virtio-net MMIO device whose interrupt line is `gsi`.
    pub fn probe_mmio(m: super::mmio::Mmio, gsi: u32, nbufs: u16) -> Option<VirtioNet> {
        let t = Transport::Mmio(m);
        let name = if t.modern() { "virtio-net (mmio)" } else { "virtio-net (mmio legacy)" };
        let features = t.negotiate(F_MAC | F_ANY_LAYOUT)?;
        // An ISA line (Firecracker's 5..) may be moved to another GSI or made active-low by an ACPI override. The
        // line is programmed edge-triggered whatever the device says (microvm declares it level): the driver
        // acknowledges the device in the handler and before it halts, so no edge is lost.
        let (gsi, flags) = match crate::acpi::get() {
            Some(a) if gsi < 16 => a.isa_irq(gsi as u8),
            _ => (gsi, 0),
        };
        let irq = crate::arch::ioapic::route(gsi, crate::arch::idt::VEC_NET, crate::arch::apic::id(), flags & !0b1100);
        Self::init(t, name, features, nbufs, None, irq, if irq { m.base() } else { 0 })
    }

    fn init(t: Transport, name: &'static str, features: u64, nbufs: u16, vector: Option<u16>, irq: bool, mmio_ack: u64) -> Option<VirtioNet> {
        let hdr = if t.modern() { 12 } else { 10 };
        let chain = !t.modern() && features & F_ANY_LAYOUT == 0;
        if chain && crate::verbose() {
            kprintln!("{name}: no ANY_LAYOUT: each header in a descriptor of its own");
        }
        let per = if chain { 2 } else { 1 };
        // Any failure resets the device, so it stops using the rings before their memory is freed.
        let fail = |why: &str| -> Option<VirtioNet> {
            kprintln!("{name}: {why}");
            t.set_status(0);
            None
        };
        let (Some(mut rxq), Some(mut txq)) = (Queue::new(&t, 0, nbufs * per, vector, Tag::Net), Queue::new(&t, 1, nbufs * per, None, Tag::Net)) else {
            return fail("queues unavailable");
        };
        let (nrx, ntx) = (nbufs.min(rxq.size / per), nbufs.min(txq.size / per));
        let (Some(rx_bufs), Some(tx_bufs)) = (DmaRegion::alloc(nrx as usize * BUF_LEN, Tag::Net), DmaRegion::alloc(ntx as usize * BUF_LEN, Tag::Net)) else {
            drop((rxq, txq));
            return fail("no memory for the buffers");
        };
        let mut mac = [0u8; 6];
        if features & F_MAC != 0 {
            for (i, m) in mac.iter_mut().enumerate() {
                *m = t.config8(i as u16);
            }
        } else {
            // No address from the device: a random locally administered unicast one.
            let r = crate::arch::rdrand().unwrap_or_else(crate::arch::rdtsc).to_le_bytes();
            mac.copy_from_slice(&r[..6]);
            mac[0] = (mac[0] & 0xFE) | 0x02;
        }
        // Post every RX buffer; with chained headers, fix the TX header descriptors too.
        for i in 0..nrx {
            let b = buf(&rx_bufs, i) as u64;
            if chain {
                rxq.set_desc(2 * i, Desc { addr: b, len: hdr as u32, flags: DESC_F_WRITE | DESC_F_NEXT, next: 2 * i + 1 });
                rxq.set_desc(2 * i + 1, Desc { addr: b + hdr as u64, len: (BUF_LEN - hdr) as u32, flags: DESC_F_WRITE, next: 0 });
            } else {
                rxq.set_desc(i, Desc { addr: b, len: BUF_LEN as u32, flags: DESC_F_WRITE, next: 0 });
            }
            rxq.push(head(chain, i));
        }
        if chain {
            for i in 0..ntx {
                txq.set_desc(2 * i, Desc { addr: buf(&tx_bufs, i) as u64, len: hdr as u32, flags: DESC_F_NEXT, next: 2 * i + 1 });
            }
        }
        rxq.set_no_interrupt(true);
        txq.set_no_interrupt(true);
        t.ready();
        rxq.kick();
        if irq {
            RX_AVAIL_FLAGS.store(rxq.avail_flags_ptr(), Ordering::Relaxed);
            MMIO_ACK.store(mmio_ack, Ordering::Relaxed);
            crate::dev::set_net_hooks(set_rx_interrupts, (mmio_ack != 0).then_some(ack_irq as fn()));
        }
        Some(VirtioNet {
            _t: t,
            name,
            mac,
            rx: Rx { q: rxq, bufs: rx_bufs, nbufs: nrx, hdr, chain, kick_pending: false, packets: 0, bytes: 0 },
            tx: Tx { q: txq, bufs: tx_bufs, nbufs: ntx, hdr, chain, free: (0..ntx).rev().collect(), queued: alloc::vec![false; ntx as usize], kick_pending: false, packets: 0, bytes: 0, full: 0 },
            irq,
            kicks: 0,
        })
    }
}

impl NetDevice for VirtioNet {
    fn name(&self) -> &'static str {
        self.name
    }
    fn mac(&self) -> [u8; 6] {
        self.mac
    }
    fn queue(&mut self, _q: usize) -> (&mut dyn NetRx, &mut dyn NetTx) {
        (&mut self.rx, &mut self.tx)
    }
    fn flush(&mut self) {
        if core::mem::take(&mut self.tx.kick_pending) && self.tx.q.kick() {
            self.kicks += 1;
        }
        if core::mem::take(&mut self.rx.kick_pending) && self.rx.q.kick() {
            self.kicks += 1;
        }
    }
    fn rx_interrupts(&mut self, on: bool) {
        self.rx.q.set_no_interrupt(!on);
        fence(Ordering::SeqCst);
        if on {
            // An MMIO line that stayed raised since the last frame must drop, or no edge announces the next one.
            ack_irq();
        }
    }
    fn has_interrupts(&self) -> bool {
        self.irq
    }
    fn counters(&self) -> NetCounters {
        NetCounters {
            rx_packets: self.rx.packets,
            rx_bytes: self.rx.bytes,
            tx_packets: self.tx.packets,
            tx_bytes: self.tx.bytes,
            kicks: self.kicks,
            tx_full: self.tx.full,
        }
    }
    fn footprint(&self) -> usize {
        self.rx.q.footprint() + self.tx.q.footprint() + self.rx.bufs.len() + self.tx.bufs.len()
    }
}
