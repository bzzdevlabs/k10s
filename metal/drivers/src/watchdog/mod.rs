//! Hardware watchdog timers: reset the machine unless petted. The kernel arms one once it serves and pets it from
//! its main loop, so a hung kernel ends in a reset, and so in k10boot's boot counting and Tier 1 fallback.
//!
//! * [`wdat`]: the ACPI Watchdog Action Table (Microsoft's hardware watchdog specification): the firmware
//!   describes the watchdog as register actions, no AML. Preferred when present (it also covers chipsets whose
//!   watchdog the kernel has no driver for).
//! * [`itco`]: the Intel TCO timer of the ICH and PCH chipsets: through the LPC bridge (ICH6 to the 9-series PCH,
//!   QEMU's q35), or through the SMBus controller from the 100-series PCH on.
//! * [`esb`]: the Intel 6300ESB watchdog (QEMU's `-device i6300esb`).

pub mod esb;
pub mod itco;
pub mod wdat;

/// A PCI function's address.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Bdf {
    pub seg: u16,
    pub bus: u8,
    pub dev: u8,
    pub func: u8,
}

impl Bdf {
    pub const fn new(bus: u8, dev: u8, func: u8) -> Bdf {
        Bdf { seg: 0, bus, dev, func }
    }
}

/// Chipset access: port I/O, PCI configuration space of any function, physical memory registers.
pub trait Platform {
    fn inb(&self, port: u16) -> u8;
    fn inw(&self, port: u16) -> u16;
    fn inl(&self, port: u16) -> u32;
    fn outb(&self, port: u16, v: u8);
    fn outw(&self, port: u16, v: u16);
    fn outl(&self, port: u16, v: u32);
    /// An aligned dword of configuration space.
    fn cfg_read32(&self, f: Bdf, off: u16) -> u32;
    fn cfg_write32(&self, f: Bdf, off: u16, v: u32);
    fn cfg_write16(&self, f: Bdf, off: u16, v: u16);
    fn cfg_write8(&self, f: Bdf, off: u16, v: u8);
    /// Maps `[paddr, paddr + len)` for register access (uncached); false if it cannot be.
    fn map(&self, paddr: u64, len: u64) -> bool;
    fn mmio_read32(&self, paddr: u64) -> u32;
    fn mmio_write32(&self, paddr: u64, v: u32);
    fn mmio_read16(&self, paddr: u64) -> u16;
    fn mmio_write16(&self, paddr: u64, v: u16);
    fn mmio_read8(&self, paddr: u64) -> u8;
    fn mmio_write8(&self, paddr: u64, v: u8);
    fn delay_us(&self, us: u32);

    fn cfg_read16(&self, f: Bdf, off: u16) -> u16 {
        (self.cfg_read32(f, off & !3) >> ((off & 2) * 8)) as u16
    }
    fn cfg_read8(&self, f: Bdf, off: u16) -> u8 {
        (self.cfg_read32(f, off & !3) >> ((off & 3) * 8)) as u8
    }
}

pub trait Watchdog {
    /// e.g. "Intel TCO (ICH9)".
    fn name(&self) -> &'static str;
    /// Starts the countdown with a timeout of about `seconds`; returns the timeout actually set.
    fn arm(&mut self, p: &dyn Platform, seconds: u32) -> Result<u32, &'static str>;
    /// Restarts the countdown.
    fn pet(&mut self, p: &dyn Platform);
    fn disarm(&mut self, p: &dyn Platform);
    /// Whether this watchdog reset the machine before this boot (as far as the hardware tells).
    fn fired_before(&self) -> bool;
}
