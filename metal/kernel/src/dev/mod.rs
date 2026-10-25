//! Device interfaces. Drivers (virtio, NVMe, AHCI; Intel/Realtek NICs next) implement [`NetDevice`] or
//! [`BlockDevice`]; the network stack and storage only see these traits, so a driver could also be a TIR module
//! behind a host interface later (docs/research/native-hardware.md §5.2).
//!
//! DMA memory is a [`DmaRegion`]: physically contiguous, page-aligned frames, like a TIR instance slot. Devices are
//! handed physical addresses inside regions only (the identity map makes them valid pointers too); without an
//! IOMMU nothing stops a device from reaching elsewhere, which is why first-party drivers stay in the kernel.
//! Queues are numbered: a NIC has receive/transmit queue pairs, a disk request queues (one each today; network I/O
//! runs on CPU 0).

pub mod ahci;
pub mod mmio;
pub mod nic;
pub mod nvme;

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};

pub use k10_drivers::Link;

use crate::mem::frames::{self, Tag, FRAME};
use crate::sync::IrqLock;

/// Physically contiguous, zeroed, page-aligned memory for DMA, returned to the frame allocator on drop.
pub struct DmaRegion {
    paddr: u64,
    frames: usize,
    tag: Tag,
}

// SAFETY: exclusively owned frames.
unsafe impl Send for DmaRegion {}

impl DmaRegion {
    pub fn alloc(bytes: usize, tag: Tag) -> Option<DmaRegion> {
        let frames = bytes.div_ceil(FRAME as usize).max(1);
        let paddr = frames::alloc(frames, 1, tag)?;
        // SAFETY: freshly allocated frames, identity-mapped.
        unsafe { core::ptr::write_bytes(paddr as *mut u8, 0, frames * FRAME as usize) };
        Some(DmaRegion { paddr, frames, tag })
    }
    /// Physical (= virtual) address of byte 0.
    pub fn paddr(&self) -> u64 {
        self.paddr
    }
    pub fn ptr(&self) -> *mut u8 {
        self.paddr as *mut u8
    }
    pub fn len(&self) -> usize {
        self.frames * FRAME as usize
    }
    pub fn as_slice(&self) -> &[u8] {
        unsafe { core::slice::from_raw_parts(self.ptr(), self.len()) }
    }
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { core::slice::from_raw_parts_mut(self.ptr(), self.len()) }
    }
}

impl Drop for DmaRegion {
    fn drop(&mut self) {
        frames::free(self.paddr, self.frames, self.tag);
    }
}

/// Counters a NIC driver keeps (for `/_k10s/status`).
#[derive(Clone, Copy, Debug, Default)]
pub struct NetCounters {
    pub rx_packets: u64,
    pub rx_bytes: u64,
    pub tx_packets: u64,
    pub tx_bytes: u64,
    /// Notifications to the device (VM exits under a hypervisor).
    pub kicks: u64,
    /// Transmit attempts that found the queue full.
    pub tx_full: u64,
}

/// The receive half of a queue pair: buffers the device filled, handed out by id and returned after use.
pub trait NetRx {
    /// Next received frame: (buffer id, frame length without the device's header).
    fn pop(&mut self) -> Option<(u16, usize)>;
    fn frame(&self, id: u16, len: usize) -> &[u8];
    /// Gives the buffer back to the device (the notification is deferred to [`NetDevice::flush`]).
    fn recycle(&mut self, id: u16);
    fn pending(&self) -> bool;
}

/// The transmit half of a queue pair.
pub trait NetTx {
    /// True if a frame can be queued now (reaping completions if needed).
    fn ready(&mut self) -> bool;
    /// Builds a frame of `len` bytes with `fill` in a free buffer and queues it (notification deferred to `flush`).
    fn send(&mut self, len: usize, fill: &mut dyn FnMut(&mut [u8]));
}

/// An Ethernet NIC.
pub trait NetDevice {
    /// Driver and transport, e.g. "virtio-net (pci)".
    fn name(&self) -> &'static str;
    fn mac(&self) -> [u8; 6];
    /// Largest IP packet (Ethernet payload) it carries.
    fn mtu(&self) -> usize {
        1500
    }
    fn queue_pairs(&self) -> usize {
        1
    }
    /// Queue pair `q`'s receive and transmit halves.
    fn queue(&mut self, q: usize) -> (&mut dyn NetRx, &mut dyn NetTx);
    /// Sends the notifications owed to the device.
    fn flush(&mut self);
    /// Enables (true) or suppresses (false) receive interrupts. After enabling, the caller re-checks
    /// [`NetRx::pending`] before halting: a frame may have arrived meanwhile.
    fn rx_interrupts(&mut self, on: bool);
    /// Whether received frames raise an interrupt at all (else the idle loop polls).
    fn has_interrupts(&self) -> bool;
    fn counters(&self) -> NetCounters;
    /// Bytes of RAM its rings and buffers use.
    fn footprint(&self) -> usize;
    /// The link as the MAC sees it (`None`: no link status, e.g. virtio-net).
    fn link(&mut self) -> Option<Link> {
        None
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockOp {
    Read,
    Write,
    /// Makes every completed write durable (a no-op on devices without a volatile cache).
    Flush,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockError {
    /// The device reported an I/O error.
    Io,
    Unsupported,
    ReadOnly,
    /// Outside the device, or not a whole number of blocks in one request.
    Range,
    /// The queue is full: poll, then submit again.
    Busy,
    /// No completion within the synchronous helpers' deadline.
    Timeout,
}

/// One request: `blocks` logical blocks at `lba` to/from `paddr` (inside a [`DmaRegion`]); `tag` comes back with
/// its completion.
#[derive(Clone, Copy, Debug)]
pub struct BlockRequest {
    pub op: BlockOp,
    pub lba: u64,
    pub blocks: u32,
    pub paddr: u64,
    pub tag: u32,
}

/// A disk: asynchronous requests on numbered queues, completions polled.
pub trait BlockDevice: Send {
    fn name(&self) -> &'static str;
    /// Logical block size in bytes (512 or 4096).
    fn block_size(&self) -> u32;
    fn blocks(&self) -> u64;
    fn read_only(&self) -> bool;
    fn queues(&self) -> usize {
        1
    }
    /// Largest request in blocks.
    fn max_blocks(&self) -> u32;
    fn submit(&mut self, q: usize, r: BlockRequest) -> Result<(), BlockError>;
    /// Next completion on queue `q`: (tag, result).
    fn poll(&mut self, q: usize) -> Option<(u32, Result<(), BlockError>)>;
    /// Whether completions raise an interrupt (on CPU 0, vector `VEC_DISK`): else waiters poll.
    fn has_interrupts(&self) -> bool {
        false
    }
    /// Before power off or reboot, with nothing in flight: tells the device (NVMe's normal shutdown).
    fn shutdown(&mut self) {}
}

/// Runs one request on queue 0 and waits for it (up to 10 s; nothing else may be in flight on that queue).
pub fn block_sync(d: &mut dyn BlockDevice, r: BlockRequest) -> Result<(), BlockError> {
    d.submit(0, r)?;
    let deadline = crate::time::now_ns() + 10_000_000_000;
    loop {
        if let Some((tag, res)) = d.poll(0) {
            if tag == r.tag {
                return res;
            }
        }
        if crate::time::now_ns() > deadline {
            return Err(BlockError::Timeout);
        }
        core::hint::spin_loop();
    }
}

/// Every disk found at boot, for storage (M2).
pub static BLOCK: IrqLock<Vec<Box<dyn BlockDevice>>> = IrqLock::new(Vec::new());

/// The NIC driver's receive-interrupt switch, callable from the scheduler and interrupt handlers (the NIC itself is
/// owned by CPU 0's network stack): a `fn(bool)`, 0 = none.
static RX_IRQ_HOOK: AtomicUsize = AtomicUsize::new(0);
/// What the NIC's interrupt handler must do at the device (acknowledge a level-style interrupt line): a `fn()`.
static IRQ_ACK_HOOK: AtomicUsize = AtomicUsize::new(0);

pub fn set_net_hooks(rx_irq: fn(bool), ack: Option<fn()>) {
    RX_IRQ_HOOK.store(rx_irq as usize, Ordering::Release);
    IRQ_ACK_HOOK.store(ack.map_or(0, |f| f as usize), Ordering::Release);
}

/// Enables or suppresses NIC receive interrupts (no-op before the NIC is up).
pub fn set_rx_interrupts(on: bool) {
    let f = RX_IRQ_HOOK.load(Ordering::Acquire);
    if f != 0 {
        // SAFETY: stored from a `fn(bool)` by `set_net_hooks`.
        unsafe { core::mem::transmute::<usize, fn(bool)>(f)(on) }
    }
}

/// The NIC interrupt's device-side acknowledgement.
pub fn net_irq_ack() {
    let f = IRQ_ACK_HOOK.load(Ordering::Acquire);
    if f != 0 {
        // SAFETY: stored from a `fn()` by `set_net_hooks`.
        unsafe { core::mem::transmute::<usize, fn()>(f)() }
    }
}

/// Finds the devices (PCI, then virtio-mmio slots from the command line and the ACPI tables): the first NIC is
/// returned, disks are registered in [`BLOCK`].
pub fn probe(cmdline: &str, nic_bufs: u16) -> Option<Box<dyn NetDevice>> {
    let mut nic: Option<Box<dyn NetDevice>> = None;
    for d in crate::pci::devices() {
        // Storage controllers by class: NVMe (01:08:02) and AHCI (01:06:01).
        match (d.class, d.subclass, d.read8(0x09)) {
            (0x01, 0x08, 0x02) => {
                if let Some(n) = nvme::Nvme::probe(d) {
                    BLOCK.lock().push(Box::new(n));
                }
                continue;
            }
            (0x01, 0x06, 0x01) => {
                for disk in ahci::probe(d) {
                    BLOCK.lock().push(Box::new(disk));
                }
                continue;
            }
            _ => {}
        }
        if d.vendor != crate::virtio::VENDOR {
            if nic.is_none() {
                nic = nic::probe(d, nic_bufs);
            }
            continue;
        }
        match crate::virtio::pci::device_type(d) {
            Some(crate::virtio::TYPE_NET) if nic.is_none() => {
                if let Some(n) = crate::virtio::net::VirtioNet::probe_pci(d, nic_bufs) {
                    nic = Some(Box::new(n));
                }
            }
            Some(crate::virtio::TYPE_BLOCK) => {
                if let Some(b) = crate::virtio::blk::VirtioBlk::probe_pci(d) {
                    BLOCK.lock().push(Box::new(b));
                }
            }
            _ => {}
        }
    }
    let mut slots = mmio::from_cmdline(cmdline);
    for s in crate::acpi::get().map_or(&[][..], |t| &t.virtio_mmio) {
        if !slots.iter().any(|c| c.base == s.base) {
            slots.push(*s);
        }
    }
    for m in slots {
        let Some(t) = crate::virtio::mmio::Mmio::new(m.base, m.size) else { continue };
        match t.device_type() {
            crate::virtio::TYPE_NET if nic.is_none() => {
                if let Some(n) = crate::virtio::net::VirtioNet::probe_mmio(t, m.irq, nic_bufs) {
                    nic = Some(Box::new(n));
                }
            }
            crate::virtio::TYPE_BLOCK => {
                if let Some(b) = crate::virtio::blk::VirtioBlk::probe_mmio(t) {
                    BLOCK.lock().push(Box::new(b));
                }
            }
            _ => {}
        }
    }
    nic
}
