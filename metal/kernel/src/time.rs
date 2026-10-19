//! Time: the TSC is the clock. Its frequency comes from (in order) KVM's paravirtual clock (exact, no waiting),
//! CPUID leaf 0x15, or a 10 ms calibration against the PIT. Wall-clock time at boot comes from kvmclock's wall
//! clock (no port I/O), else the CMOS RTC; afterwards it advances with the monotonic clock.
//!
//! Also home of the preemption request flag set by the timer interrupt (see `runtime::sched`).

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::arch::{self, cpuid, inb, outb, rdtsc, wrmsr};

static TSC_HZ: AtomicU64 = AtomicU64::new(0);
/// Nanoseconds per TSC tick as a 32.32 fixed-point number.
static NS_PER_TSC_FP32: AtomicU64 = AtomicU64::new(0);
static TSC_BOOT: AtomicU64 = AtomicU64::new(0);
static BOOT_UNIX_MS: AtomicU64 = AtomicU64::new(0);
/// kvmclock system time at `kmain` entry: kvmclock counts from 0 when KVM creates the VM, so this is the time QEMU's
/// remaining setup and the firmware took before the kernel got control (0 when unknown).
static VM_TO_KMAIN_NS: AtomicU64 = AtomicU64::new(0);

pub fn vm_to_kmain_ns() -> u64 {
    VM_TO_KMAIN_NS.load(Ordering::Relaxed)
}

/// TSC value at which the running invocation must be preempted (u64::MAX = none).
pub static PREEMPT_AT: AtomicU64 = AtomicU64::new(u64::MAX);
/// Set by the timer interrupt when `PREEMPT_AT` has passed; polled at TIR safepoints.
pub static PREEMPT: AtomicBool = AtomicBool::new(false);
pub static TIMER_IRQS: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug)]
pub enum Source {
    KvmClock,
    Cpuid15,
    Pit,
}

/// `tsc_boot` is the TSC value read as early as possible in `kmain` (uptime origin).
pub fn init(tsc_boot: u64) -> Source {
    TSC_BOOT.store(tsc_boot, Ordering::Relaxed);
    let (hz, wall, src) = if let Some((hz, wall)) = kvmclock(tsc_boot) {
        (hz, Some(wall), Source::KvmClock)
    } else if let Some(hz) = cpuid15_hz() {
        (hz, None, Source::Cpuid15)
    } else {
        (pit_calibrate(), None, Source::Pit)
    };
    TSC_HZ.store(hz, Ordering::Relaxed);
    NS_PER_TSC_FP32.store(((1_000_000_000u128 << 32) / hz as u128) as u64, Ordering::Relaxed);
    let boot_ms = match wall {
        Some(ns) => ns / 1_000_000,
        None => rtc_unix_seconds() * 1000,
    };
    BOOT_UNIX_MS.store(boot_ms, Ordering::Relaxed);
    src
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

/// Wall-clock Unix milliseconds.
pub fn unix_ms() -> u64 {
    BOOT_UNIX_MS.load(Ordering::Relaxed) + now_ms()
}

/// Timer interrupt: request preemption if the running invocation's slice/budget deadline has passed.
pub fn on_timer_irq() {
    TIMER_IRQS.fetch_add(1, Ordering::Relaxed);
    if rdtsc() >= PREEMPT_AT.load(Ordering::Relaxed) {
        PREEMPT.store(true, Ordering::Relaxed);
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
    wrmsr(MSR_KVM_STEAL_TIME, &raw mut STEAL as u64 | 1);
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
    const MSR_KVM_WALL_CLOCK_NEW: u32 = 0x4b56_4d00;
    const MSR_KVM_SYSTEM_TIME_NEW: u32 = 0x4b56_4d01;
    if cpuid(0x4000_0001, 0).0 & KVM_FEATURE_CLOCKSOURCE2 == 0 {
        return None;
    }
    wrmsr(MSR_KVM_SYSTEM_TIME_NEW, &raw mut PVCLOCK as u64 | 1);
    let (mul, shift, tsc_ts, sys_ns) = loop {
        // The hypervisor makes `version` odd while updating.
        // SAFETY: plain volatile reads of a static that only the hypervisor writes.
        let (v, mul, shift, tsc_ts, sys, v2) = unsafe {
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
            break (mul, shift, tsc_ts, sys);
        }
        core::hint::spin_loop();
    };
    wrmsr(MSR_KVM_SYSTEM_TIME_NEW, 0);
    // ns = ((tsc << shift) * mul) >> 32  ⇒  hz = 1e9 * 2^32 / (mul * 2^shift)
    let num: u128 = 1_000_000_000u128 << 32;
    let hz = if shift >= 0 { num / ((mul as u128) << shift) } else { (num << (-shift)) / mul as u128 } as u64;

    // Wall clock: Unix time at system_time = 0, plus system_time at `tsc_boot`.
    wrmsr(MSR_KVM_WALL_CLOCK_NEW, &raw mut PVWALL as u64);
    let (sec, nsec) = unsafe { (core::ptr::read_volatile(&raw const PVWALL.sec), core::ptr::read_volatile(&raw const PVWALL.nsec)) };
    let to_ns = |ticks: u64| ((ticks as u128 * 1_000_000_000) / hz as u128) as u64;
    let sys_at_boot = if tsc_boot >= tsc_ts { sys_ns + to_ns(tsc_boot - tsc_ts) } else { sys_ns.saturating_sub(to_ns(tsc_ts - tsc_boot)) };
    let wall = sec as u64 * 1_000_000_000 + nsec as u64 + sys_at_boot;
    VM_TO_KMAIN_NS.store(sys_at_boot, Ordering::Relaxed);
    Some((hz, wall))
}

fn cpuid15_hz() -> Option<u64> {
    if cpuid(0, 0).0 < 0x15 {
        return None;
    }
    let (den, num, crystal, _) = cpuid(0x15, 0);
    if den == 0 || num == 0 || crystal == 0 {
        return None;
    }
    Some(crystal as u64 * num as u64 / den as u64)
}

/// Counts TSC ticks during 10 ms of PIT channel 2 (gate via port 0x61, no interrupt needed).
fn pit_calibrate() -> u64 {
    const PIT_HZ: u64 = 1_193_182;
    const LATCH: u64 = PIT_HZ / 100; // 10 ms
    outb(0x61, (inb(0x61) & !0x02) | 0x01);
    outb(0x43, 0b1011_0000); // channel 2, lo/hi byte, mode 0, binary
    outb(0x42, LATCH as u8);
    outb(0x42, (LATCH >> 8) as u8);
    let t0 = rdtsc();
    while inb(0x61) & 0x20 == 0 {
        arch::pause();
    }
    let t1 = rdtsc();
    (t1 - t0) * PIT_HZ / LATCH
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
