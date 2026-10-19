//! Local APIC (xAPIC, memory-mapped at the address in IA32_APIC_BASE, identity-mapped uncached by boot.rs) and
//! the legacy 8259 PICs (remapped and fully masked: every interrupt we use is LAPIC timer or MSI-X).
//!
//! The timer is one-shot ("tickless"): the kernel arms it for the next instant it must regain control (end of an
//! invocation's time slice, next TCP timer). With TSC-deadline mode (KVM `-cpu host`) arming is a single
//! WRMSR of an absolute TSC value; otherwise the LAPIC one-shot count mode is used, calibrated against the TSC.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use super::idt::{VEC_SPURIOUS, VEC_TIMER};
use super::{cpuid, inb, outb, rdmsr, rdtsc, wrmsr};

const IA32_APIC_BASE: u32 = 0x1B;
const IA32_TSC_DEADLINE: u32 = 0x6E0;

const REG_ID: usize = 0x20;
const REG_TPR: usize = 0x80;
const REG_EOI: usize = 0xB0;
const REG_SVR: usize = 0xF0;
const REG_LVT_TIMER: usize = 0x320;
const REG_TIMER_INIT: usize = 0x380;
const REG_TIMER_CUR: usize = 0x390;
const REG_TIMER_DIV: usize = 0x3E0;
const REG_ICR_LO: usize = 0x300;
const REG_ICR_HI: usize = 0x310;
const ICR_PENDING: u32 = 1 << 12;

static BASE: AtomicU64 = AtomicU64::new(0xFEE0_0000);
static TSC_DEADLINE: AtomicBool = AtomicBool::new(false);
/// LAPIC timer ticks per 2^32 TSC ticks (count mode only).
static LAPIC_PER_TSC_FP32: AtomicU64 = AtomicU64::new(0);

fn read(reg: usize) -> u32 {
    unsafe { core::ptr::read_volatile((BASE.load(Ordering::Relaxed) as usize + reg) as *const u32) }
}
fn write(reg: usize, v: u32) {
    unsafe { core::ptr::write_volatile((BASE.load(Ordering::Relaxed) as usize + reg) as *mut u32, v) }
}

/// Remaps the 8259 PICs to vectors 0x30..0x3F (so a stray legacy IRQ is never mistaken for an exception) and masks
/// every line.
fn disable_pic() {
    outb(0x20, 0x11);
    outb(0xA0, 0x11);
    outb(0x21, 0x30);
    outb(0xA1, 0x38);
    outb(0x21, 0x04);
    outb(0xA1, 0x02);
    outb(0x21, 0x01);
    outb(0xA1, 0x01);
    outb(0x21, 0xFF);
    outb(0xA1, 0xFF);
    let _ = inb(0x21);
}

pub fn init() {
    disable_pic();
    init_local();
}

/// Enables this CPU's local APIC and configures its timer (every CPU; the boot CPU also masks the 8259s and
/// calibrates).
pub fn init_local() {
    let base = rdmsr(IA32_APIC_BASE);
    // Global enable (bit 11), keep xAPIC mode (bit 10 clear).
    wrmsr(IA32_APIC_BASE, (base | (1 << 11)) & !(1 << 10));
    BASE.store(base & 0xF_FFFF_F000, Ordering::Relaxed);
    write(REG_TPR, 0);
    write(REG_SVR, 0x100 | VEC_SPURIOUS as u32);
    let deadline = cpuid(1, 0).2 & (1 << 24) != 0;
    TSC_DEADLINE.store(deadline, Ordering::Relaxed);
    if deadline {
        write(REG_LVT_TIMER, (0b10 << 17) | VEC_TIMER as u32);
    } else {
        write(REG_TIMER_DIV, 0b1011); // divide by 1
        write(REG_LVT_TIMER, VEC_TIMER as u32); // one-shot
    }
}

/// Calibrates the LAPIC count-mode timer against the TSC (only needed without TSC-deadline mode).
pub fn calibrate(tsc_hz: u64) {
    if TSC_DEADLINE.load(Ordering::Relaxed) {
        return;
    }
    write(REG_LVT_TIMER, 1 << 16 | VEC_TIMER as u32); // masked during calibration
    write(REG_TIMER_INIT, u32::MAX);
    let t0 = rdtsc();
    let window = tsc_hz / 1000; // 1 ms
    while rdtsc() - t0 < window {
        core::hint::spin_loop();
    }
    let ticks = u32::MAX - read(REG_TIMER_CUR);
    let dt = rdtsc() - t0;
    write(REG_TIMER_INIT, 0);
    LAPIC_PER_TSC_FP32.store(((ticks as u128) << 32).checked_div(dt as u128).unwrap_or(1 << 32) as u64, Ordering::Relaxed);
    write(REG_LVT_TIMER, VEC_TIMER as u32);
}

pub fn uses_tsc_deadline() -> bool {
    TSC_DEADLINE.load(Ordering::Relaxed)
}

/// Arms the one-shot timer to fire at TSC value `deadline` (replacing any earlier arming).
pub fn arm_timer(deadline: u64) {
    if TSC_DEADLINE.load(Ordering::Relaxed) {
        wrmsr(IA32_TSC_DEADLINE, deadline.max(1));
    } else {
        let now = rdtsc();
        let dt = deadline.saturating_sub(now).max(1);
        let ticks = ((dt as u128 * LAPIC_PER_TSC_FP32.load(Ordering::Relaxed) as u128) >> 32).clamp(1, u32::MAX as u128);
        write(REG_TIMER_INIT, ticks as u32);
    }
}

pub fn eoi() {
    write(REG_EOI, 0);
}

fn icr_wait() {
    while read(REG_ICR_LO) & ICR_PENDING != 0 {
        core::hint::spin_loop();
    }
}

/// Fixed-delivery IPI with `vector` to the CPU whose local APIC id is `apic_id`.
pub fn send_ipi(apic_id: u32, vector: u8) {
    // Interrupts off: the two ICR writes must not be split by a handler that sends an IPI too.
    let was = super::interrupts_enabled();
    super::interrupts_disable();
    icr_wait();
    write(REG_ICR_HI, apic_id << 24);
    write(REG_ICR_LO, vector as u32);
    if was {
        super::interrupts_enable();
    }
}

/// INIT, then two STARTUP IPIs with `vector` (page `vector * 4096`), to every other CPU (MP spec / SDM 8.4.4.1).
pub fn start_all_aps(vector: u8, delay_us: &dyn Fn(u64)) {
    icr_wait();
    write(REG_ICR_HI, 0);
    write(REG_ICR_LO, 0x000C_4500); // all excluding self, level assert, INIT
    icr_wait();
    delay_us(1000);
    for _ in 0..2 {
        write(REG_ICR_HI, 0);
        write(REG_ICR_LO, 0x000C_4600 | vector as u32); // all excluding self, STARTUP
        icr_wait();
        delay_us(200);
    }
}

pub fn id() -> u32 {
    read(REG_ID) >> 24
}

/// MSI address/data that deliver `vector` to this CPU (fixed delivery, edge, physical destination).
pub fn msi(vector: u8) -> (u64, u32) {
    (0xFEE0_0000 | ((id() as u64) << 12), vector as u32)
}
