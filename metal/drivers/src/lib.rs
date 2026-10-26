//! The hardware side of the native kernel's drivers for real machines: Ethernet controllers (Intel e1000e and the
//! igb/igc family, Realtek r8169-class), watchdog timers (ACPI WDAT, Intel TCO, i6300ESB) and control-method power
//! buttons (found in the ACPI tables' AML).
//!
//! Drivers are written against small traits — [`Hw`] for a PCI function's registers, configuration space, delays
//! and DMA memory, [`watchdog::Platform`] for port I/O and chipset registers — that the kernel implements
//! (metal/kernel/src/dev/nic.rs, watchdog.rs) and that the host tests implement with register models built from
//! the datasheets and from the behavior Linux's drivers document (tests/). A NIC is brought up by its constructor
//! (reset, PHY, rings) and then driven through [`Nic`]: fixed-buffer descriptor rings ([`ring`]), doorbells
//! batched by [`Nic::flush`], one receive interrupt source switched on only while the kernel is idle.

#![no_std]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod e1000e;
pub mod igc;
pub mod ids;
pub mod power_button;
pub mod ring;
pub mod rtl8169;
pub mod watchdog;

/// Memory the device may read and write: physically contiguous, page-aligned, zeroed when allocated, returned to
/// its allocator on drop.
pub trait DmaMem {
    /// The address the device uses (physical); [`DmaMem::ptr`] is where the CPU reaches the same bytes.
    fn paddr(&self) -> u64;
    fn ptr(&self) -> *mut u8;
    fn len(&self) -> usize;
}

/// Register access to one memory BAR of a PCI function.
pub trait Regs {
    fn read32(&self, off: u32) -> u32;
    fn write32(&self, off: u32, v: u32);
    fn read16(&self, off: u32) -> u16;
    fn write16(&self, off: u32, v: u16);
    fn read8(&self, off: u32) -> u8;
    fn write8(&self, off: u32, v: u8);
}

/// What a NIC driver needs besides its registers.
pub trait Hw: Regs {
    type Mem: DmaMem;
    /// The function's PCI configuration space: an aligned dword read, a 16-bit write.
    fn config_read32(&self, off: u16) -> u32;
    fn config_write16(&self, off: u16, v: u16);
    /// Lets the device master the bus (DMA): drivers call it once they have reset the device, so nothing a
    /// firmware driver left running reaches memory the kernel now uses.
    fn bus_master(&self);
    fn delay_us(&self, us: u32);
    /// Zeroed DMA memory of at least `bytes`.
    fn dma(&self, bytes: usize) -> Option<Self::Mem>;
    /// Random bits (a locally administered MAC address when the NVM holds none).
    fn random(&self) -> u64;
    fn log(&self, args: core::fmt::Arguments);
}

/// Polls `done` every `step_us` for up to `total_us`; true once it held.
pub fn wait(hw: &impl Hw, total_us: u32, step_us: u32, mut done: impl FnMut() -> bool) -> bool {
    let mut t = 0;
    loop {
        if done() {
            return true;
        }
        if t >= total_us {
            return false;
        }
        hw.delay_us(step_us);
        t += step_us;
    }
}

/// The link as the MAC sees it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Link {
    pub up: bool,
    /// Megabits per second (0 when down or unknown).
    pub mbps: u32,
    pub full_duplex: bool,
}

/// Counters of a NIC's queue pair.
#[derive(Clone, Copy, Debug, Default)]
pub struct Counters {
    pub rx_packets: u64,
    pub rx_bytes: u64,
    /// Frames the device marked bad, or that did not fit one buffer: dropped.
    pub rx_errors: u64,
    pub tx_packets: u64,
    pub tx_bytes: u64,
    /// Transmit attempts that found the ring full.
    pub tx_full: u64,
    /// Doorbell (tail / poll register) writes.
    pub doorbells: u64,
}

/// How the device signals received frames (and link changes) to the kernel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IrqMode {
    /// No interrupt: the kernel polls the rings.
    Polled,
    /// One MSI message.
    Msi,
    /// MSI-X table entry 0.
    MsiX,
}

/// A NIC brought up by its driver: one receive/transmit queue pair.
pub trait Nic {
    type Mem: DmaMem;
    /// Driver and chip, e.g. "e1000e (82574L)".
    fn name(&self) -> &'static str;
    fn mac(&self) -> [u8; 6];
    fn rings(&mut self) -> (&mut ring::Rx<Self::Mem>, &mut ring::Tx<Self::Mem>);
    fn rings_ref(&self) -> (&ring::Rx<Self::Mem>, &ring::Tx<Self::Mem>);
    /// Writes the doorbells owed to the device (new receive buffers, frames to send).
    fn flush(&mut self);
    /// Enables or masks the receive interrupt.
    fn rx_interrupts(&mut self, on: bool);
    /// Acknowledges the interrupt at the device (called from the interrupt handler).
    fn ack(&mut self);
    fn link(&mut self) -> Link;
    fn doorbells(&self) -> u64;
    fn counters(&self) -> Counters {
        let (rx, tx) = self.rings_ref();
        Counters {
            rx_packets: rx.packets,
            rx_bytes: rx.bytes,
            rx_errors: rx.errors,
            tx_packets: tx.packets,
            tx_bytes: tx.bytes,
            tx_full: tx.full,
            doorbells: self.doorbells(),
        }
    }
    /// Bytes of RAM its rings and buffers use.
    fn footprint(&self) -> usize {
        let (rx, tx) = self.rings_ref();
        rx.footprint() + tx.footprint()
    }
}

/// A unicast address the device may use: not all zeros, not multicast.
pub fn mac_valid(m: &[u8; 6]) -> bool {
    m.iter().any(|&b| b != 0) && m[0] & 1 == 0 && *m != [0xFF; 6]
}

/// A random locally administered unicast address.
pub fn random_mac(hw: &impl Hw) -> [u8; 6] {
    let r = hw.random().to_le_bytes();
    let mut m = [0u8; 6];
    m.copy_from_slice(&r[..6]);
    m[0] = (m[0] & 0xFE) | 0x02;
    m
}

/// Bytes `0..6` of a receive-address register pair (Intel RAL/RAH, Realtek IDR0..5): low dword then high half.
pub fn mac_from(lo: u32, hi: u32) -> [u8; 6] {
    let mut m = [0u8; 6];
    m[..4].copy_from_slice(&lo.to_le_bytes());
    m[4..].copy_from_slice(&hi.to_le_bytes()[..2]);
    m
}

/// MII (IEEE 802.3 clause 22) registers and bits every PHY here has.
pub mod mii {
    pub const BMCR: u8 = 0;
    pub const BMSR: u8 = 1;
    pub const PHYID1: u8 = 2;
    pub const PHYID2: u8 = 3;
    pub const BMCR_RESET: u16 = 1 << 15;
    pub const BMCR_ANENABLE: u16 = 1 << 12;
    pub const BMCR_PDOWN: u16 = 1 << 11;
    pub const BMCR_ISOLATE: u16 = 1 << 10;
    pub const BMCR_ANRESTART: u16 = 1 << 9;

    /// The control register value that powers the PHY up and restarts auto-negotiation, from its current value.
    pub fn autoneg(bmcr: u16) -> u16 {
        (bmcr & !(BMCR_PDOWN | BMCR_ISOLATE | BMCR_RESET)) | BMCR_ANENABLE | BMCR_ANRESTART
    }
}
