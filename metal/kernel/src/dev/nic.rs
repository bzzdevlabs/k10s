//! The Ethernet controllers of real machines (metal/drivers: Intel e1000e, igb/igc, Realtek r8169-class) behind
//! [`NetDevice`]: their register window (a memory BAR, mapped uncached), DMA memory ([`DmaRegion`]), delays, and
//! the interrupt — MSI-X entry 0 where the driver uses it (igb/igc, Realtek chips that have MSI-X), else one MSI
//! message, else polling — routed to `VEC_NET` on CPU 0 like virtio-net's.

use alloc::boxed::Box;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use k10_drivers::ids::{self, Family};
use k10_drivers::{e1000e, igc, ring, rtl8169, DmaMem, Hw, IrqMode, Link, Nic, Regs};

use super::{DmaRegion, NetCounters, NetDevice, NetRx, NetTx};
use crate::kprintln;
use crate::mem::frames::Tag;
use crate::pci;

/// A memory BAR, mapped uncached.
#[derive(Clone, Copy)]
pub struct Mmio {
    base: u64,
}

impl Regs for Mmio {
    fn read32(&self, off: u32) -> u32 {
        // SAFETY: inside the BAR mapped by `probe`.
        unsafe { core::ptr::read_volatile((self.base + off as u64) as *const u32) }
    }
    fn write32(&self, off: u32, v: u32) {
        unsafe { core::ptr::write_volatile((self.base + off as u64) as *mut u32, v) }
    }
    fn read16(&self, off: u32) -> u16 {
        unsafe { core::ptr::read_volatile((self.base + off as u64) as *const u16) }
    }
    fn write16(&self, off: u32, v: u16) {
        unsafe { core::ptr::write_volatile((self.base + off as u64) as *mut u16, v) }
    }
    fn read8(&self, off: u32) -> u8 {
        unsafe { core::ptr::read_volatile((self.base + off as u64) as *const u8) }
    }
    fn write8(&self, off: u32, v: u8) {
        unsafe { core::ptr::write_volatile((self.base + off as u64) as *mut u8, v) }
    }
}

pub struct KHw {
    mmio: Mmio,
    dev: pci::Device,
}

impl Regs for KHw {
    fn read32(&self, off: u32) -> u32 {
        self.mmio.read32(off)
    }
    fn write32(&self, off: u32, v: u32) {
        self.mmio.write32(off, v)
    }
    fn read16(&self, off: u32) -> u16 {
        self.mmio.read16(off)
    }
    fn write16(&self, off: u32, v: u16) {
        self.mmio.write16(off, v)
    }
    fn read8(&self, off: u32) -> u8 {
        self.mmio.read8(off)
    }
    fn write8(&self, off: u32, v: u8) {
        self.mmio.write8(off, v)
    }
}

pub fn delay_us(us: u32) {
    let end = crate::time::now_ns() + us as u64 * 1000;
    while crate::time::now_ns() < end {
        core::hint::spin_loop();
    }
}

impl Hw for KHw {
    type Mem = DmaRegion;
    fn config_read32(&self, off: u16) -> u32 {
        self.dev.read32(off)
    }
    fn config_write16(&self, off: u16, v: u16) {
        self.dev.write16(off, v)
    }
    fn bus_master(&self) {
        self.dev.enable(true);
    }
    fn delay_us(&self, us: u32) {
        delay_us(us)
    }
    fn dma(&self, bytes: usize) -> Option<DmaRegion> {
        DmaRegion::alloc(bytes, Tag::Net)
    }
    fn random(&self) -> u64 {
        crate::arch::rdrand().unwrap_or_else(crate::arch::rdtsc)
    }
    fn log(&self, args: core::fmt::Arguments) {
        kprintln!("{args}");
    }
}

impl DmaMem for DmaRegion {
    fn paddr(&self) -> u64 {
        DmaRegion::paddr(self)
    }
    fn ptr(&self) -> *mut u8 {
        DmaRegion::ptr(self)
    }
    fn len(&self) -> usize {
        DmaRegion::len(self)
    }
}

impl NetRx for ring::Rx<DmaRegion> {
    fn pop(&mut self) -> Option<(u16, usize)> {
        ring::Rx::pop(self)
    }
    fn frame(&self, id: u16, len: usize) -> &[u8] {
        ring::Rx::frame(self, id, len)
    }
    fn recycle(&mut self, id: u16) {
        ring::Rx::recycle(self, id)
    }
    fn pending(&self) -> bool {
        ring::Rx::pending(self)
    }
}

impl NetTx for ring::Tx<DmaRegion> {
    fn ready(&mut self) -> bool {
        ring::Tx::ready(self)
    }
    fn send(&mut self, len: usize, fill: &mut dyn FnMut(&mut [u8])) {
        ring::Tx::send(self, len, fill)
    }
}

/// A driver of metal/drivers as a [`NetDevice`].
struct Adapter<D> {
    d: D,
    irq: IrqMode,
}

impl<D: Nic<Mem = DmaRegion>> NetDevice for Adapter<D> {
    fn name(&self) -> &'static str {
        self.d.name()
    }
    fn mac(&self) -> [u8; 6] {
        self.d.mac()
    }
    fn queue(&mut self, _q: usize) -> (&mut dyn NetRx, &mut dyn NetTx) {
        let (rx, tx) = self.d.rings();
        (rx, tx)
    }
    fn flush(&mut self) {
        self.d.flush()
    }
    fn rx_interrupts(&mut self, on: bool) {
        self.d.rx_interrupts(on)
    }
    fn has_interrupts(&self) -> bool {
        self.irq != IrqMode::Polled
    }
    fn counters(&self) -> NetCounters {
        let c = self.d.counters();
        NetCounters {
            rx_packets: c.rx_packets,
            rx_bytes: c.rx_bytes,
            tx_packets: c.tx_packets,
            tx_bytes: c.tx_bytes,
            kicks: c.doorbells,
            tx_full: c.tx_full,
        }
    }
    fn footprint(&self) -> usize {
        self.d.footprint()
    }
    fn link(&mut self) -> Option<Link> {
        Some(self.d.link())
    }
}

/// What the interrupt hooks need to reach the device without the driver object: its registers and kind.
static IRQ_BASE: AtomicU64 = AtomicU64::new(0);
/// 1 e1000e, 2 igb, 3 igc, 4 RTL8168, 5 RTL8125 (0: none).
static IRQ_KIND: AtomicU32 = AtomicU32::new(0);
static IRQ_XID: AtomicU32 = AtomicU32::new(0);

fn hook_mmio() -> Mmio {
    Mmio { base: IRQ_BASE.load(Ordering::Relaxed) }
}

fn rtl_chip() -> rtl8169::Chip {
    rtl8169::Chip { xid: IRQ_XID.load(Ordering::Relaxed) as u16, r8125: IRQ_KIND.load(Ordering::Relaxed) == 5 }
}

fn hook_rx(on: bool) {
    let m = hook_mmio();
    match IRQ_KIND.load(Ordering::Relaxed) {
        1 => e1000e::set_rx_irq(&m, on),
        2 | 3 => igc::set_rx_irq(&m, on),
        4 | 5 => rtl8169::set_rx_irq(&m, rtl_chip(), on),
        _ => {}
    }
}

fn hook_ack() {
    let m = hook_mmio();
    match IRQ_KIND.load(Ordering::Relaxed) {
        1 => {
            e1000e::ack(&m);
        }
        2 => {
            igc::ack(&m, Family::I82576);
        }
        3 => {
            igc::ack(&m, Family::I225);
        }
        4 | 5 => {
            rtl8169::ack(&m, rtl_chip());
        }
        _ => {}
    }
}

/// Brings up `d` if one of these drivers supports it: `None` if not, or if bring-up failed (reported).
pub fn probe(d: &pci::Device, nbufs: u16) -> Option<Box<dyn NetDevice>> {
    let id = ids::nic(d.vendor, d.device)?;
    let model = id.model;
    // Intel: BAR 0; Realtek: the first memory BAR (BAR 2 on the 8168 and 8125, BAR 0 is I/O).
    d.assign_bars();
    let bar = (0..6).find(|&i| {
        let (io, base) = d.bar(i);
        !io && base != 0 && (id.vendor == ids::INTEL || i > 0)
    })?;
    let (_, base) = d.bar(bar);
    let size = d.bar_size(bar).max(0x1000);
    if !crate::arch::paging::map_mmio(base, size) {
        kprintln!("{model}: BAR {bar} at {base:#x} cannot be mapped");
        return None;
    }
    // Memory decoding first; bus mastering once the driver has reset the device (Hw::bus_master).
    d.enable(false);
    let hw = KHw { mmio: Mmio { base }, dev: *d };
    // Interrupt: igb/igc take MSI-X only, e1000e MSI only, Realtek either.
    let msix = matches!(id.family, Family::I82576 | Family::I350 | Family::I210 | Family::I225 | Family::Rtl8168 | Family::Rtl8125)
        .then(|| d.msix())
        .flatten();
    let irq = match msix {
        Some(m) if m.route(0, crate::arch::idt::VEC_NET) => {
            d.msix_enable(&m);
            IrqMode::MsiX
        }
        _ if !matches!(id.family, Family::I82576 | Family::I350 | Family::I210 | Family::I225) && d.msi_route(crate::arch::idt::VEC_NET) => IrqMode::Msi,
        _ => IrqMode::Polled,
    };
    let (kind, dev): (u32, Result<Box<dyn NetDevice>, &str>) = match id.family {
        Family::I82574 | Family::PchLpt | Family::PchSpt | Family::PchCnp => {
            (1, e1000e::E1000e::new(hw, d.device, nbufs, irq).map(|n| Box::new(Adapter { d: n, irq }) as Box<dyn NetDevice>))
        }
        Family::I82576 | Family::I350 | Family::I210 | Family::I225 => {
            let irq = if irq == IrqMode::MsiX { irq } else { IrqMode::Polled };
            let k = if id.family == Family::I225 { 3 } else { 2 };
            (k, igc::Igc::new(hw, d.device, nbufs, irq).map(|n| Box::new(Adapter { d: n, irq }) as Box<dyn NetDevice>))
        }
        Family::Rtl8168 | Family::Rtl8125 => {
            let k = if id.family == Family::Rtl8125 { 5 } else { 4 };
            (k, rtl8169::Rtl8169::new(hw, d.device, nbufs, irq).map(|n| {
                IRQ_XID.store(n.chip().xid as u32, Ordering::Relaxed);
                Box::new(Adapter { d: n, irq }) as Box<dyn NetDevice>
            }))
        }
    };
    match dev {
        Ok(n) => {
            if n.has_interrupts() {
                IRQ_BASE.store(base, Ordering::Relaxed);
                IRQ_KIND.store(kind, Ordering::Release);
                super::set_net_hooks(hook_rx, Some(hook_ack));
            }
            Some(n)
        }
        Err(e) => {
            kprintln!("{model} at {:02x}:{:02x}.{}: {e}", d.bus, d.dev, d.func);
            None
        }
    }
}
