//! The Intel 6300ESB watchdog (PCI 8086:25AB): two 20-bit down-counters clocked at about 1 kHz (PCI clock / 2^15);
//! timer 1 runs out, then timer 2, then the WDT_OUTPUT pin resets the machine. Configuration space holds the
//! mode and the enable/lock bits; the memory BAR the preload values and the reload register, each write of
//! which must follow the two-step unlock sequence.

use super::{Bdf, Platform, Watchdog};

pub const VENDOR: u16 = 0x8086;
pub const DEVICE: u16 = 0x25AB;
/// Configuration: interrupt type (bits 0-1), clock select (bit 2), output disable (bit 5).
pub const CONFIG: u16 = 0x60;
/// Lock register: lock (bit 0), enable (bit 1), free-running mode (bit 2).
pub const LOCK: u16 = 0x68;
pub const LOCK_LOCKED: u8 = 1 << 0;
pub const LOCK_ENABLE: u8 = 1 << 1;
/// Interrupt on timer 1 disabled, 1 kHz clock, reset output enabled.
pub const CONFIG_WATCHDOG: u16 = 0x0003;
pub const TIMER1: u64 = 0x00;
pub const TIMER2: u64 = 0x04;
pub const RELOAD: u64 = 0x0C;
pub const RELOAD_KEEPALIVE: u16 = 1 << 8;
pub const RELOAD_TIMEOUT: u16 = 1 << 9;
pub const UNLOCK1: u16 = 0x80;
pub const UNLOCK2: u16 = 0x86;

pub struct Esb {
    f: Bdf,
    base: u64,
    fired: bool,
}

impl Esb {
    fn unlock(&self, p: &dyn Platform) {
        p.mmio_write16(self.base + RELOAD, UNLOCK1);
        p.mmio_write16(self.base + RELOAD, UNLOCK2);
    }

    /// Takes over the watchdog at function `f` whose memory BAR 0 is at `bar0` (mapped by the caller): watchdog
    /// mode, stopped (unless the firmware locked it running), the timeout status read and cleared.
    pub fn new(p: &dyn Platform, f: Bdf, bar0: u64) -> Result<Esb, &'static str> {
        if bar0 == 0 {
            return Err("no memory BAR");
        }
        let e = Esb { f, base: bar0, fired: false };
        p.cfg_write16(f, CONFIG, CONFIG_WATCHDOG);
        if p.cfg_read8(f, LOCK) & LOCK_LOCKED == 0 {
            p.cfg_write8(f, LOCK, 0);
        }
        e.unlock(p);
        let fired = p.mmio_read16(bar0 + RELOAD) & RELOAD_TIMEOUT != 0;
        e.unlock(p);
        p.mmio_write16(bar0 + RELOAD, RELOAD_TIMEOUT | RELOAD_KEEPALIVE);
        Ok(Esb { fired, ..e })
    }
}

impl Watchdog for Esb {
    fn name(&self) -> &'static str {
        "i6300ESB"
    }
    fn arm(&mut self, p: &dyn Platform, seconds: u32) -> Result<u32, &'static str> {
        // Each timer counts `seconds << 9` ticks of ~1 ms: both together about `seconds`.
        let s = seconds.clamp(1, 2046);
        let ticks = s << 9;
        self.unlock(p);
        p.mmio_write32(self.base + TIMER1, ticks);
        self.unlock(p);
        p.mmio_write32(self.base + TIMER2, ticks);
        self.unlock(p);
        p.mmio_write16(self.base + RELOAD, RELOAD_KEEPALIVE);
        let lock = p.cfg_read8(self.f, LOCK);
        if lock & LOCK_LOCKED != 0 && lock & LOCK_ENABLE == 0 {
            return Err("locked disabled by the firmware");
        }
        p.cfg_write8(self.f, LOCK, (lock & LOCK_LOCKED) | LOCK_ENABLE);
        Ok(s)
    }
    fn pet(&mut self, p: &dyn Platform) {
        self.unlock(p);
        p.mmio_write16(self.base + RELOAD, RELOAD_KEEPALIVE);
    }
    fn disarm(&mut self, p: &dyn Platform) {
        if p.cfg_read8(self.f, LOCK) & LOCK_LOCKED == 0 {
            p.cfg_write8(self.f, LOCK, 0);
        }
    }
    fn fired_before(&self) -> bool {
        self.fired
    }
}
