//! Time: the TSC is the clock. Its frequency comes from (in order) KVM's paravirtual clock (exact, no waiting),
//! CPUID leaf 0x15 when it states the crystal frequency, a 20 ms calibration against the HPET or else the ACPI PM
//! timer (both from the ACPI tables), CPUID 0x15 with leaf 0x16's nominal base frequency, and last a 20 ms
//! calibration against the PIT (which modern boards may gate off). Wall-clock time at boot comes from kvmclock's wall
//! clock (no port I/O), else the CMOS RTC; afterwards it advances with the monotonic clock and, with kvmclock, is
//! re-read from it every [`RESYNC_NS`] ([`resync_wall_if_due`]), so host clock adjustments and drift do not build
//! up in gossip and admin-request timestamps. Without kvmclock the SNTP client (sntp.rs) steps and slews it.
//! Resyncs and corrections move the wall clock only: `now_ns` stays monotonic.
//!
//! Also home of the preemption request flag set by the timer interrupt (see `runtime::sched`).

use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use crate::arch::{self, cpuid, inb, outb, rdtsc, wrmsr};
use crate::mem::{PhysAddr, VirtAddr};
use crate::sync::{Global, SetOnce};
use k10_host::clock::{EdgeCache, GuestClock};

static TSC_HZ: AtomicU64 = AtomicU64::new(0);
/// Nanoseconds per TSC tick as a 32.32 fixed-point number.
static NS_PER_TSC_FP32: AtomicU64 = AtomicU64::new(0);
static TSC_BOOT: AtomicU64 = AtomicU64::new(0);
/// Unix time in ns at `now_ns() == 0`.
static WALL_AT_BOOT_NS: AtomicU64 = AtomicU64::new(0);
static KVMCLOCK: AtomicBool = AtomicBool::new(false);
static NEXT_RESYNC_NS: AtomicU64 = AtomicU64::new(0);
/// Interval between wall-clock resyncs from kvmclock.
pub const RESYNC_NS: u64 = 10_000_000_000;
/// kvmclock system time at `kmain` entry: kvmclock counts from 0 when KVM creates the VM, so this is the time QEMU's
/// remaining setup and the firmware took before the kernel got control (0 when unknown).
static VM_TO_KMAIN_NS: AtomicU64 = AtomicU64::new(0);

pub fn vm_to_kmain_ns() -> u64 {
    VM_TO_KMAIN_NS.load(Ordering::Relaxed)
}

// Preemption state is per CPU (`percpu::PerCpu`: `preempt_at`, `preempt`, `preempt_word`); these act on the
// calling CPU.

pub static TIMER_IRQS: AtomicU64 = AtomicU64::new(0);

/// Clears this CPU's preemption request; returns whether one was pending.
#[inline]
pub fn take_preempt() -> bool {
    crate::percpu::this().preempt.swap(false, Ordering::Relaxed)
}

/// Starts a new slice on this CPU: no request pending, preemption due at TSC `at` (u64::MAX: none). The caller
/// arms the timer.
#[inline]
pub fn set_preempt_at(at: u64) {
    let c = crate::percpu::this();
    c.preempt.store(false, Ordering::Relaxed);
    c.preempt_at.store(at, Ordering::Relaxed);
}

/// TSC value at which this CPU's running invocation must be preempted.
#[inline]
pub fn preempt_at() -> u64 {
    crate::percpu::this().preempt_at.load(Ordering::Relaxed)
}

/// Moves this CPU's preemption deadline earlier (keeps a pending request).
#[inline]
pub fn tighten_preempt_at(at: u64) {
    crate::percpu::this().preempt_at.store(at, Ordering::Relaxed);
}

/// Requests preemption of this CPU's running invocation at its next safepoint (interpreter: the flag; JIT: the flag
/// and the running context's stack limit, so that the next JIT safepoint calls the interrupt hook,
/// docs/JIT-ABI.md). Callable from interrupt handlers.
#[inline]
pub fn request_preempt() {
    let c = crate::percpu::this();
    c.preempt.store(true, Ordering::Relaxed);
    let w = c.preempt_word.load(Ordering::Relaxed);
    if !w.is_null() {
        // SAFETY: a published word stays valid until it is unpublished (runtime::jit), on this CPU.
        unsafe { (*w).store(usize::MAX, Ordering::Relaxed) };
    }
}

/// Publishes `w` as this CPU's running JIT context's preemption word (null: none) and returns the previous one. A
/// request that is already pending is forwarded to the new word.
#[inline]
pub fn set_preempt_word(w: *mut AtomicUsize) -> *mut AtomicUsize {
    let c = crate::percpu::this();
    let old = c.preempt_word.swap(w, Ordering::Relaxed);
    if !w.is_null() && c.preempt.load(Ordering::Relaxed) {
        // SAFETY: the caller publishes a live word.
        unsafe { (*w).store(usize::MAX, Ordering::Relaxed) };
    }
    old
}

#[derive(Clone, Copy, Debug)]
pub enum Source {
    KvmClock,
    Cpuid15,
    Hpet,
    PmTimer,
    Cpuid16,
    Pit,
}

/// `tsc_boot` is the TSC value read as early as possible in `kmain` (uptime origin). Needs the ACPI tables.
pub fn init(tsc_boot: u64) -> Source {
    TSC_BOOT.store(tsc_boot, Ordering::Relaxed);
    let tables = crate::acpi::get();
    let (hz, wall, src) = if let Some((hz, wall)) = kvmclock(tsc_boot) {
        (hz, Some(wall), Source::KvmClock)
    } else if let Some(hz) = cpuid15_hz(false) {
        (hz, None, Source::Cpuid15)
    } else if let Some(hz) = tables.and_then(|t| t.hpet).and_then(hpet_hz) {
        (hz, None, Source::Hpet)
    } else if let Some(hz) = tables.and_then(|t| t.fadt).and_then(pmtimer_hz) {
        (hz, None, Source::PmTimer)
    } else if let Some(hz) = cpuid15_hz(true) {
        (hz, None, Source::Cpuid16)
    } else {
        (pit_calibrate(), None, Source::Pit)
    };
    // Without an invariant TSC (CPUID 0x8000_0007 EDX bit 8) the clock rate follows P-states on old CPUs.
    let hypervisor = cpuid(1, 0).2 & (1 << 31) != 0;
    if !hypervisor && cpuid(0x8000_0000, 0).0 >= 0x8000_0007 && cpuid(0x8000_0007, 0).3 & (1 << 8) == 0 {
        crate::kprintln!("time: WARNING: the TSC is not invariant; timekeeping may drift");
    }
    TSC_HZ.store(hz, Ordering::Relaxed);
    NS_PER_TSC_FP32.store(((1_000_000_000u128 << 32) / hz as u128) as u64, Ordering::Relaxed);
    WALL_AT_BOOT_NS.store(wall.unwrap_or_else(|| rtc_unix_seconds() * 1_000_000_000), Ordering::Relaxed);
    KVMCLOCK.store(wall.is_some(), Ordering::Relaxed);
    NEXT_RESYNC_NS.store(if wall.is_some() { RESYNC_NS } else { u64::MAX }, Ordering::Relaxed);
    src
}

/// Re-reads the wall clock from kvmclock if [`RESYNC_NS`] has passed (CPU 0's main loop).
pub fn resync_wall_if_due() {
    if now_ns() >= NEXT_RESYNC_NS.load(Ordering::Relaxed) {
        resync_wall();
    }
}

/// Re-reads the wall clock from kvmclock now; false without kvmclock. CPU 0 only (its kvmclock area).
pub fn resync_wall() -> bool {
    if !KVMCLOCK.load(Ordering::Relaxed) {
        return false;
    }
    let r = pvclock_read();
    let base = kvm_wall_base_ns();
    let tsc = rdtsc();
    let mono = tsc_to_ns(tsc.wrapping_sub(TSC_BOOT.load(Ordering::Relaxed)));
    WALL_AT_BOOT_NS.store((base + r.system_ns(tsc)).saturating_sub(mono), Ordering::Relaxed);
    NEXT_RESYNC_NS.store(mono + RESYNC_NS, Ordering::Relaxed);
    true
}

/// Whether the wall clock follows kvmclock (else the SNTP client keeps it right).
pub fn has_kvmclock() -> bool {
    KVMCLOCK.load(Ordering::Relaxed)
}

/// Moves the wall clock by `delta_ns` (SNTP: a step, or a slew's increment). CPU 0 only.
pub fn adjust_wall(delta_ns: i64) {
    let w = WALL_AT_BOOT_NS.load(Ordering::Relaxed);
    WALL_AT_BOOT_NS.store(w.saturating_add_signed(delta_ns), Ordering::Relaxed);
}

pub fn tsc_hz() -> u64 {
    TSC_HZ.load(Ordering::Relaxed)
}

/// TSC ticks → nanoseconds.
#[inline]
pub fn tsc_to_ns(ticks: u64) -> u64 {
    ((ticks as u128 * NS_PER_TSC_FP32.load(Ordering::Relaxed) as u128) >> 32) as u64
}

/// Nanoseconds → TSC ticks.
#[inline]
pub fn ns_to_tsc(ns: u64) -> u64 {
    (ns as u128 * TSC_HZ.load(Ordering::Relaxed) as u128 / 1_000_000_000) as u64
}

/// Monotonic nanoseconds since boot.
#[inline]
pub fn now_ns() -> u64 {
    tsc_to_ns(rdtsc().wrapping_sub(TSC_BOOT.load(Ordering::Relaxed)))
}

pub fn now_ms() -> u64 {
    now_ns() / 1_000_000
}

/// Wall-clock Unix nanoseconds.
pub fn unix_ns() -> u64 {
    WALL_AT_BOOT_NS.load(Ordering::Relaxed).saturating_add(now_ns())
}

/// Wall-clock Unix milliseconds.
pub fn unix_ms() -> u64 {
    unix_ns() / 1_000_000
}

/// The guest clock (`now_ns`, `time_ms`), set at boot.
static GUEST_CLOCK: SetOnce<GuestClock> = SetOnce::new();
/// Each CPU's current guest-clock intervals.
static EDGES: [Global<EdgeCache>; crate::percpu::MAX_CPUS] = [const { Global::new(EdgeCache::EMPTY) }; crate::percpu::MAX_CPUS];

/// Installs the guest clock (once, at boot).
pub fn set_guest_clock(clock: GuestClock) {
    GUEST_CLOCK.set(clock);
}

fn guest(read: impl FnOnce(&GuestClock, &mut EdgeCache) -> u64) -> u64 {
    let clock = GUEST_CLOCK.get().expect("guest clock set at boot");
    read(clock, &mut EDGES[crate::percpu::id()].borrow_mut())
}

/// `now_ns` for guests.
pub fn guest_now_ns() -> u64 {
    guest(|c, e| c.now_ns(now_ns(), e))
}

/// `time_ms` for guests.
pub fn guest_unix_ms() -> u64 {
    guest(|c, e| c.unix_ms(unix_ns(), e))
}

/// Timer interrupt: request preemption if this CPU's running invocation's slice/budget deadline has passed.
pub fn on_timer_irq() {
    TIMER_IRQS.fetch_add(1, Ordering::Relaxed);
    let c = crate::percpu::this();
    c.timer_irqs.fetch_add(1, Ordering::Relaxed);
    if rdtsc() >= c.preempt_at.load(Ordering::Relaxed) {
        request_preempt();
    }
}

// --- KVM steal time -------------------------------------------------------------------------------------------

/// `struct kvm_steal_time` (64 bytes): nanoseconds this vCPU was runnable but not running (host contention).
#[repr(C, align(64))]
struct StealTime {
    steal: u64,
    version: u32,
    flags: u32,
    preempted: u8,
    pad: [u8; 47],
}
static mut STEAL: StealTime = StealTime { steal: 0, version: 0, flags: 0, preempted: 0, pad: [0; 47] };
static STEAL_ON: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Registers the steal-time area with KVM (if offered).
pub fn init_steal_time() {
    const KVM_FEATURE_STEAL_TIME: u32 = 1 << 5;
    const MSR_KVM_STEAL_TIME: u32 = 0x4b56_4d03;
    let (_, b, _, _) = cpuid(0x4000_0000, 0);
    if b != u32::from_le_bytes(*b"KVMK") || cpuid(0x4000_0001, 0).0 & KVM_FEATURE_STEAL_TIME == 0 {
        return;
    }
    wrmsr(MSR_KVM_STEAL_TIME, VirtAddr::of(&raw const STEAL).phys().get() | 1);
    STEAL_ON.store(true, Ordering::Relaxed);
}

/// Total steal time in ns (0 without KVM steal-time support).
pub fn steal_ns() -> u64 {
    if !STEAL_ON.load(Ordering::Relaxed) {
        return 0;
    }
    loop {
        // SAFETY: volatile reads of an area only the hypervisor writes (version odd while updating).
        let (v, st, v2) = unsafe {
            use core::ptr::read_volatile as rd;
            (rd(&raw const STEAL.version), rd(&raw const STEAL.steal), rd(&raw const STEAL.version))
        };
        if v == v2 && v & 1 == 0 {
            return st;
        }
        core::hint::spin_loop();
    }
}

// --- TSC frequency sources ------------------------------------------------------------------------------------

/// pvclock_vcpu_time_info (KVM ABI).
#[repr(C, align(64))]
struct PvClock {
    version: u32,
    pad0: u32,
    tsc_timestamp: u64,
    system_time: u64,
    tsc_to_system_mul: u32,
    tsc_shift: i8,
    flags: u8,
    pad: [u8; 2],
}
static mut PVCLOCK: PvClock =
    PvClock { version: 0, pad0: 0, tsc_timestamp: 0, system_time: 0, tsc_to_system_mul: 0, tsc_shift: 0, flags: 0, pad: [0; 2] };

#[repr(C, align(16))]
struct PvWallClock {
    version: u32,
    sec: u32,
    nsec: u32,
}
static mut PVWALL: PvWallClock = PvWallClock { version: 0, sec: 0, nsec: 0 };

const MSR_KVM_WALL_CLOCK_NEW: u32 = 0x4b56_4d00;
const MSR_KVM_SYSTEM_TIME_NEW: u32 = 0x4b56_4d01;

/// One consistent kvmclock reading: system time (ns since the VM was created) at a TSC value, and the TSC scale.
struct PvReading {
    tsc: u64,
    system_ns: u64,
    mul: u32,
    shift: i8,
}

impl PvReading {
    /// Nanoseconds for `ticks` TSC ticks (the pvclock scaling).
    fn scale(&self, ticks: u64) -> u64 {
        let t = if self.shift >= 0 { (ticks as u128) << self.shift } else { ticks as u128 >> -self.shift };
        ((t * self.mul as u128) >> 32) as u64
    }

    /// System time at TSC value `tsc`.
    fn system_ns(&self, tsc: u64) -> u64 {
        if tsc >= self.tsc {
            self.system_ns + self.scale(tsc - self.tsc)
        } else {
            self.system_ns.saturating_sub(self.scale(self.tsc - tsc))
        }
    }
}

/// Reads this CPU's kvmclock (registered for the read only).
fn pvclock_read() -> PvReading {
    wrmsr(MSR_KVM_SYSTEM_TIME_NEW, VirtAddr::of(&raw const PVCLOCK).phys().get() | 1);
    let r = loop {
        // The hypervisor makes `version` odd while updating.
        // SAFETY: plain volatile reads of a static that only the hypervisor writes.
        let (v, mul, shift, tsc, sys, v2) = unsafe {
            use core::ptr::read_volatile as rd;
            (
                rd(&raw const PVCLOCK.version),
                rd(&raw const PVCLOCK.tsc_to_system_mul),
                rd(&raw const PVCLOCK.tsc_shift),
                rd(&raw const PVCLOCK.tsc_timestamp),
                rd(&raw const PVCLOCK.system_time),
                rd(&raw const PVCLOCK.version),
            )
        };
        if v == v2 && v & 1 == 0 && mul != 0 {
            break PvReading { tsc, system_ns: sys, mul, shift };
        }
        core::hint::spin_loop();
    };
    wrmsr(MSR_KVM_SYSTEM_TIME_NEW, 0);
    r
}

/// Unix time (ns) at kvmclock system time 0, as the host's clock says now.
fn kvm_wall_base_ns() -> u64 {
    wrmsr(MSR_KVM_WALL_CLOCK_NEW, VirtAddr::of(&raw const PVWALL).phys().get());
    loop {
        // SAFETY: as in `pvclock_read`.
        let (v, sec, nsec, v2) = unsafe {
            use core::ptr::read_volatile as rd;
            (rd(&raw const PVWALL.version), rd(&raw const PVWALL.sec), rd(&raw const PVWALL.nsec), rd(&raw const PVWALL.version))
        };
        if v == v2 && v & 1 == 0 {
            return sec as u64 * 1_000_000_000 + nsec as u64;
        }
        core::hint::spin_loop();
    }
}

/// KVM paravirtual clock: exact TSC frequency, and the Unix time (ns) at TSC value `tsc_boot`.
fn kvmclock(tsc_boot: u64) -> Option<(u64, u64)> {
    let (_, b, c, d) = cpuid(0x4000_0000, 0);
    let mut sig = [0u8; 12];
    sig[0..4].copy_from_slice(&b.to_le_bytes());
    sig[4..8].copy_from_slice(&c.to_le_bytes());
    sig[8..12].copy_from_slice(&d.to_le_bytes());
    if &sig != b"KVMKVMKVM\0\0\0" {
        return None;
    }
    const KVM_FEATURE_CLOCKSOURCE2: u32 = 1 << 3;
    if cpuid(0x4000_0001, 0).0 & KVM_FEATURE_CLOCKSOURCE2 == 0 {
        return None;
    }
    let r = pvclock_read();
    // ns = ((tsc << shift) * mul) >> 32  ⇒  hz = 1e9 * 2^32 / (mul * 2^shift)
    let num: u128 = 1_000_000_000u128 << 32;
    let hz = if r.shift >= 0 { num / ((r.mul as u128) << r.shift) } else { (num << (-r.shift)) / r.mul as u128 } as u64;
    let sys_at_boot = r.system_ns(tsc_boot);
    VM_TO_KMAIN_NS.store(sys_at_boot, Ordering::Relaxed);
    Some((hz, kvm_wall_base_ns() + sys_at_boot))
}

/// TSC frequency from CPUID 0x15 (TSC/crystal ratio and crystal Hz). When the crystal is not stated, `nominal`
/// takes leaf 0x16's base frequency (MHz) as the TSC frequency, as Linux does.
fn cpuid15_hz(nominal: bool) -> Option<u64> {
    let max = cpuid(0, 0).0;
    if max < 0x15 {
        return None;
    }
    let (den, num, crystal, _) = cpuid(0x15, 0);
    if den == 0 || num == 0 {
        return None;
    }
    if crystal != 0 {
        return Some(crystal as u64 * num as u64 / den as u64);
    }
    let base_mhz = if nominal && max >= 0x16 { cpuid(0x16, 0).0 & 0xFFFF } else { 0 };
    (base_mhz != 0).then_some(base_mhz as u64 * 1_000_000)
}

/// TSC ticks per second against a counter of `hz` read by `read` (`mask`: its width), over `ms` milliseconds.
/// Each counter read is bracketed by TSC reads and timed at their midpoint.
fn calibrate(read: &dyn Fn() -> u64, mask: u64, hz: u64, ms: u64) -> Option<u64> {
    let sample = || {
        let a = rdtsc();
        let c = read();
        (a / 2 + rdtsc() / 2, c)
    };
    let (t0, c0) = sample();
    let want = hz * ms / 1000;
    loop {
        let (t1, c1) = sample();
        let dc = c1.wrapping_sub(c0) & mask;
        if dc >= want {
            return Some(((t1 - t0) as u128 * hz as u128 / dc as u128) as u64);
        }
        // A counter that does not move (absent, or disabled by the firmware): give up after 2^31 TSC ticks
        // (0.2–2 s whatever the TSC's rate).
        if t1 - t0 > 1 << 31 {
            return None;
        }
    }
}

/// HPET main counter: period (femtoseconds) in the capabilities register, enabled if the firmware left it off.
fn hpet_hz(base: u64) -> Option<u64> {
    let regs = crate::arch::paging::map_mmio(PhysAddr::new(base), 1024)?;
    let reg = |o: u64| (regs + o).ptr::<u64>();
    // SAFETY: the HPET register block (ACPI HPET table), mapped uncached above.
    let cap = unsafe { core::ptr::read_volatile(reg(0)) };
    let period_fs = cap >> 32;
    if period_fs == 0 || period_fs > 100_000_000 {
        return None;
    }
    unsafe {
        let conf = core::ptr::read_volatile(reg(0x10));
        if conf & 1 == 0 {
            core::ptr::write_volatile(reg(0x10), conf | 1);
        }
    }
    let mask = if cap & (1 << 13) != 0 { u64::MAX } else { u32::MAX as u64 };
    calibrate(&|| unsafe { core::ptr::read_volatile(reg(0xF0)) }, mask, 1_000_000_000_000_000 / period_fs, 20)
}

/// ACPI PM timer: 3.579545 MHz, 24 bits (32 with TMR_VAL_EXT).
fn pmtimer_hz(f: crate::acpi::Fadt) -> Option<u64> {
    if !f.pm_tmr.valid() {
        return None;
    }
    let mask = if f.flags & crate::acpi::FADT_TMR_VAL_EXT != 0 { u32::MAX as u64 } else { 0xFF_FFFF };
    let tmr = crate::acpi::Gas { bits: 32, access: 3, ..f.pm_tmr };
    calibrate(&|| tmr.read(), mask, 3_579_545, 20)
}

/// TSC ticks during 20 ms of the PIT's channel 0 (rate generator, read back with latch commands: no IRQ, no port
/// 0x61 gate, which microVMs lack). Without a PIT (it never moves) the TSC is assumed to run at 2 GHz, loudly.
fn pit_calibrate() -> u64 {
    const PIT_HZ: u64 = 1_193_182;
    let want = PIT_HZ / 50;
    outb(0x43, 0b0011_0100); // channel 0, lo/hi byte, mode 2, binary
    outb(0x40, 0xFF);
    outb(0x40, 0xFF);
    let read = || {
        outb(0x43, 0); // latch channel 0
        let lo = inb(0x40);
        u16::from_le_bytes([lo, inb(0x40)]) as u64
    };
    // The new count loads on the next PIT clock edge: wait until the counter shows it (it then counts down from
    // 0xFFFF), so the old mode's value is not taken for elapsed time.
    let mut last = read();
    let start = rdtsc();
    while last < 0xF000 && rdtsc() - start < 1 << 31 {
        last = read();
    }
    let t0 = rdtsc();
    let (mut elapsed, mut still) = (0, 0u32);
    while elapsed < want {
        let c = read();
        // Counts down from 0xFFFF and reloads; a wrap is never missed (it takes 55 ms).
        let d = if c <= last { last - c } else { last + 0xFFFF - c };
        still = if d == 0 { still + 1 } else { 0 };
        if still > 1_000_000 {
            crate::kprintln!("time: WARNING: no HPET, PM timer or PIT to calibrate the TSC against; assuming 2 GHz");
            return 2_000_000_000;
        }
        elapsed += d;
        last = c;
    }
    ((rdtsc() - t0) as u128 * PIT_HZ as u128 / elapsed as u128) as u64
}

// --- CMOS real-time clock ---------------------------------------------------------------------------------------

fn cmos(reg: u8) -> u8 {
    outb(0x70, reg);
    inb(0x71)
}

fn rtc_unix_seconds() -> u64 {
    // Wait for the end of an update cycle, then read twice until stable.
    let read = || {
        while cmos(0x0A) & 0x80 != 0 {
            arch::pause();
        }
        [cmos(0x00), cmos(0x02), cmos(0x04), cmos(0x07), cmos(0x08), cmos(0x09), cmos(0x32)]
    };
    let mut r = read();
    loop {
        let r2 = read();
        if r2 == r {
            break;
        }
        r = r2;
    }
    let b = cmos(0x0B);
    let bcd = |v: u8| if b & 0x04 == 0 { (v & 0x0F) + (v >> 4) * 10 } else { v };
    let sec = bcd(r[0]) as u64;
    let min = bcd(r[1]) as u64;
    let mut hour = bcd(r[2] & 0x7F) as u64;
    if b & 0x02 == 0 && r[2] & 0x80 != 0 {
        hour = (hour + 12) % 24;
    }
    let day = bcd(r[3]) as u64;
    let month = bcd(r[4]) as u64;
    let century = if r[6] != 0 && r[6] != 0xFF { bcd(r[6]) as u64 } else { 20 };
    let year = century * 100 + bcd(r[5]) as u64;
    days_from_civil(year as i64, month as u32, day as u32) as u64 * 86400 + hour * 3600 + min * 60 + sec
}

/// Days since 1970-01-01 (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}
