//! The hardware watchdog (drivers in metal/drivers: ACPI WDAT, Intel TCO, i6300ESB, in that order of preference):
//! found at boot, armed when the main loop starts (`k10.watchdog=SECONDS`, default 60; `off`), petted by CPU 0's
//! main loop at most once a second, disarmed before a requested power off or reboot and before a halt
//! (`k10.panic=0`). A kernel that stops running its main loop — hung, or stuck with interrupts off — is reset,
//! and an unhealthy boot counts against k10boot's native-kernel boots (fallback to Tier 1).
//!
//! `k10.selftest=watchdog` arms it and hangs on purpose (interrupts off), for the tests.

use alloc::boxed::Box;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use k10_drivers::watchdog::{esb, itco, wdat, Bdf, Platform, Watchdog};

use crate::arch;
use crate::kprintln;
use crate::mem::PhysAddr;
use crate::sync::IrqLock;

pub const DEFAULT_TIMEOUT_S: u32 = 60;
const PET_EVERY_NS: u64 = 1_000_000_000;

struct Plat;

fn bdf(f: Bdf) -> crate::pci::Bdf {
    crate::pci::Bdf { seg: f.seg, bus: f.bus, dev: f.dev, func: f.func }
}

impl Platform for Plat {
    fn inb(&self, port: u16) -> u8 {
        arch::inb(port)
    }
    fn inw(&self, port: u16) -> u16 {
        arch::inw(port)
    }
    fn inl(&self, port: u16) -> u32 {
        arch::inl(port)
    }
    fn outb(&self, port: u16, v: u8) {
        arch::outb(port, v)
    }
    fn outw(&self, port: u16, v: u16) {
        arch::outw(port, v)
    }
    fn outl(&self, port: u16, v: u32) {
        arch::outl(port, v)
    }
    fn cfg_read32(&self, f: Bdf, off: u16) -> u32 {
        crate::pci::read32(bdf(f), off)
    }
    fn cfg_write32(&self, f: Bdf, off: u16, v: u32) {
        crate::pci::write32(bdf(f), off, v)
    }
    fn cfg_write16(&self, f: Bdf, off: u16, v: u16) {
        crate::pci::write16(bdf(f), off, v)
    }
    fn cfg_write8(&self, f: Bdf, off: u16, v: u8) {
        crate::pci::write8(bdf(f), off, v)
    }
    fn map(&self, paddr: u64, len: u64) -> bool {
        arch::paging::map_mmio(PhysAddr::new(paddr), len).is_some()
    }
    // The drivers name registers by physical address; they are reached through the direct map.
    fn mmio_read32(&self, a: u64) -> u32 {
        // SAFETY: mapped through `map` by the driver before use.
        unsafe { core::ptr::read_volatile(PhysAddr::new(a).ptr::<u32>()) }
    }
    fn mmio_write32(&self, a: u64, v: u32) {
        unsafe { core::ptr::write_volatile(PhysAddr::new(a).ptr::<u32>(), v) }
    }
    fn mmio_read16(&self, a: u64) -> u16 {
        unsafe { core::ptr::read_volatile(PhysAddr::new(a).ptr::<u16>()) }
    }
    fn mmio_write16(&self, a: u64, v: u16) {
        unsafe { core::ptr::write_volatile(PhysAddr::new(a).ptr::<u16>(), v) }
    }
    fn mmio_read8(&self, a: u64) -> u8 {
        unsafe { core::ptr::read_volatile(PhysAddr::new(a).ptr::<u8>()) }
    }
    fn mmio_write8(&self, a: u64, v: u8) {
        unsafe { core::ptr::write_volatile(PhysAddr::new(a).ptr::<u8>(), v) }
    }
    fn delay_us(&self, us: u32) {
        crate::dev::nic::delay_us(us)
    }
}

static WD: IrqLock<Option<Box<dyn Watchdog + Send>>> = IrqLock::new(None);
static ARMED: AtomicBool = AtomicBool::new(false);
static LAST_PET: AtomicU64 = AtomicU64::new(0);
static TIMEOUT_S: AtomicU64 = AtomicU64::new(DEFAULT_TIMEOUT_S as u64);

fn find(wdat_table: Option<&[u8]>) -> Option<Box<dyn Watchdog + Send>> {
    let p = Plat;
    if let Some(t) = wdat_table {
        match wdat::Wdat::parse(t).and_then(|w| {
            if w.memory().all(|(a, n)| p.map(a, n)) { w.init(&p) } else { Err("WDAT registers not mappable") }
        }) {
            Ok(w) => return Some(Box::new(w)),
            Err(e) => kprintln!("watchdog: ACPI WDAT not used: {e}"),
        }
    }
    let devs = crate::pci::devices();
    for d in devs.iter().filter(|d| d.vendor == 0x8086) {
        if let Some(v) = itco::generation(d.device, d.class, d.subclass) {
            match itco::Tco::probe(&p, Bdf { seg: d.seg, bus: d.bus, dev: d.dev, func: d.func }, v) {
                Ok(t) => return Some(Box::new(t)),
                Err(e) => kprintln!("watchdog: Intel TCO at {:02x}:{:02x}.{} not used: {e}", d.bus, d.dev, d.func),
            }
        }
    }
    for d in devs.iter().filter(|d| (d.vendor, d.device) == (esb::VENDOR, esb::DEVICE)) {
        d.assign_bars();
        d.enable(false);
        let (io, base) = d.bar(0);
        if io || base == 0 || !p.map(base, 16) {
            continue;
        }
        match esb::Esb::new(&p, Bdf { seg: d.seg, bus: d.bus, dev: d.dev, func: d.func }, base) {
            Ok(e) => return Some(Box::new(e)),
            Err(e) => kprintln!("watchdog: i6300ESB not used: {e}"),
        }
    }
    None
}

/// Finds the watchdog (`k10.watchdog=off`: none). `wdat`: the ACPI WDAT table, if the firmware has one.
pub fn init(arg: Option<&str>, wdat: Option<&[u8]>) -> Result<(), &'static str> {
    let timeout = match arg {
        Some("off" | "0") => return Ok(()),
        None | Some("on") => DEFAULT_TIMEOUT_S,
        Some(v) => v.parse::<u32>().ok().filter(|&s| s <= 3600).ok_or("want a timeout in seconds (1..3600), on or off")?,
    };
    TIMEOUT_S.store(timeout as u64, Ordering::Relaxed);
    let Some(w) = find(wdat) else {
        if crate::verbose() {
            kprintln!("watchdog: none found");
        }
        return Ok(());
    };
    kprintln!("watchdog: {}{}", w.name(), if w.fired_before() { "; it reset the machine before this boot" } else { "" });
    *WD.lock() = Some(w);
    Ok(())
}

/// Starts the countdown (the main loop is about to run).
pub fn arm() {
    let mut g = WD.lock();
    let Some(w) = g.as_mut() else { return };
    match w.arm(&Plat, TIMEOUT_S.load(Ordering::Relaxed) as u32) {
        Ok(s) => {
            ARMED.store(true, Ordering::Release);
            LAST_PET.store(crate::time::now_ns(), Ordering::Relaxed);
            kprintln!("watchdog: armed, {s} s");
        }
        Err(e) => kprintln!("watchdog: {} cannot be armed: {e}", w.name()),
    }
}

/// Restarts the countdown if a second has passed since the last time (CPU 0's main loop).
pub fn pet() {
    if !ARMED.load(Ordering::Relaxed) {
        return;
    }
    let now = crate::time::now_ns();
    if now < LAST_PET.load(Ordering::Relaxed) + PET_EVERY_NS {
        return;
    }
    LAST_PET.store(now, Ordering::Relaxed);
    if let Some(w) = WD.lock().as_mut() {
        w.pet(&Plat);
    }
}

/// Stops the countdown (power off, reboot, halt). Takes no lock that a stopped CPU may hold: called with the
/// other CPUs stopped, from the CPU stopping the machine.
pub fn disarm() {
    if !ARMED.swap(false, Ordering::AcqRel) {
        return;
    }
    // SAFETY: the other CPUs are stopped or not touching the watchdog (only CPU 0's main loop pets it).
    unsafe { WD.force_unlock() };
    if let Some(w) = WD.lock().as_mut() {
        w.disarm(&Plat);
    }
}

/// `k10.selftest=watchdog`: arms the watchdog with `k10.watchdog`'s timeout and stops responding.
pub fn selftest_hang() -> ! {
    if WD.lock().is_none() {
        crate::fatal(format_args!("watchdog selftest: no watchdog found"));
    }
    arm();
    kprintln!("watchdog selftest: hanging with interrupts off; the watchdog must reset the machine");
    arch::interrupts_disable();
    loop {
        core::hint::spin_loop();
    }
}
