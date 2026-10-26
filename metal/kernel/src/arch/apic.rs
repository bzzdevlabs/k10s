//! Local APIC, in x2APIC mode (MSR interface, 32-bit APIC ids) when the CPU has it, else xAPIC (memory-mapped at
//! the address in IA32_APIC_BASE, uncached in the direct map), and the legacy 8259 PICs (remapped and fully
//! masked: every interrupt we use is the LAPIC timer, an IPI, MSI-X or an I/O APIC line).
//!
//! Firmware may hand over in x2APIC mode (it must when APIC ids exceed 254); leaving it is an illegal transition,
//! so the kernel keeps it. Otherwise the boot CPU picks x2APIC under a hypervisor when CPUID offers it (MSR accesses
//! are cheaper exits than MMIO), or when the MADT lists APIC ids above 254; on bare metal it stays in xAPIC mode,
//! as Linux does without interrupt remapping (`k10.apic=xapic` keeps xAPIC everywhere it can). The application
//! processors follow the boot CPU.
//!
//! The timer is one-shot ("tickless"): the kernel arms it for the next instant it must regain control (end of an
//! invocation's time slice, next TCP timer). With TSC-deadline mode (KVM `-cpu host`) arming is a single
//! WRMSR of an absolute TSC value; otherwise the LAPIC one-shot count mode is used, calibrated against the TSC.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use super::idt::{VEC_SPURIOUS, VEC_TIMER};
use super::{cpuid, inb, outb, rdmsr, rdtsc, wrmsr};
use crate::mem::{PhysAddr, VirtAddr};

const IA32_APIC_BASE: u32 = 0x1B;
const IA32_TSC_DEADLINE: u32 = 0x6E0;
const APIC_EN: u64 = 1 << 11;
const APIC_EXTD: u64 = 1 << 10;
/// x2APIC registers are MSRs 0x800 + (xAPIC offset >> 4).
const X2APIC_MSR: u32 = 0x800;

const REG_ID: usize = 0x20;
const REG_TPR: usize = 0x80;
const REG_EOI: usize = 0xB0;
const REG_SVR: usize = 0xF0;
const REG_ICR_LO: usize = 0x300;
const REG_ICR_HI: usize = 0x310;
const REG_LVT_TIMER: usize = 0x320;
const REG_TIMER_INIT: usize = 0x380;
const REG_TIMER_CUR: usize = 0x390;
const REG_TIMER_DIV: usize = 0x3E0;
const ICR_PENDING: u32 = 1 << 12;
const ICR_NMI: u32 = 0b100 << 8;
const ICR_INIT: u32 = 0b101 << 8;
const ICR_STARTUP: u32 = 0b110 << 8;
const ICR_ASSERT: u32 = 1 << 14;
const ICR_LEVEL: u32 = 1 << 15;

/// The xAPIC registers in the direct map.
static BASE: AtomicU64 = AtomicU64::new(PhysAddr::new(0xFEE0_0000).virt().get());
static X2APIC: AtomicBool = AtomicBool::new(false);
static TSC_DEADLINE: AtomicBool = AtomicBool::new(false);
/// LAPIC timer ticks per 2^32 TSC ticks (count mode only).
static LAPIC_PER_TSC_FP32: AtomicU64 = AtomicU64::new(0);

fn read(reg: usize) -> u32 {
    if X2APIC.load(Ordering::Relaxed) {
        rdmsr(X2APIC_MSR + (reg >> 4) as u32) as u32
    } else {
        unsafe { core::ptr::read_volatile(VirtAddr::new(BASE.load(Ordering::Relaxed) + reg as u64).ptr::<u32>()) }
    }
}
fn write(reg: usize, v: u32) {
    if X2APIC.load(Ordering::Relaxed) {
        wrmsr(X2APIC_MSR + (reg >> 4) as u32, v as u64)
    } else {
        unsafe { core::ptr::write_volatile(VirtAddr::new(BASE.load(Ordering::Relaxed) + reg as u64).ptr::<u32>(), v) }
    }
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

pub fn has_x2apic() -> bool {
    cpuid(1, 0).2 & (1 << 21) != 0
}

/// Boot CPU: masks the 8259s (harmless where there are none: unused ports), picks the APIC mode (`wide_ids`: the
/// firmware lists APIC ids above 254), enables its local APIC.
pub fn init(prefer_xapic: bool, wide_ids: bool) {
    disable_pic();
    let base = rdmsr(IA32_APIC_BASE);
    let hypervisor = cpuid(1, 0).2 & (1 << 31) != 0;
    let x2 = base & APIC_EXTD != 0 || has_x2apic() && (wide_ids || hypervisor && !prefer_xapic);
    X2APIC.store(x2, Ordering::Relaxed);
    if !X2APIC.load(Ordering::Relaxed) {
        let regs = PhysAddr::new(base & 0xF_FFFF_F000);
        BASE.store(super::paging::map_mmio(regs, 4096).unwrap_or(regs.virt()).get(), Ordering::Relaxed);
    }
    init_local();
}

pub fn x2apic() -> bool {
    X2APIC.load(Ordering::Relaxed)
}

/// Enables this CPU's local APIC in the boot CPU's mode and configures its timer (every CPU).
pub fn init_local() {
    let base = rdmsr(IA32_APIC_BASE);
    if X2APIC.load(Ordering::Relaxed) {
        if base & APIC_EXTD == 0 {
            // xAPIC (or disabled) → enabled → x2APIC: the architectural order.
            wrmsr(IA32_APIC_BASE, base | APIC_EN);
            wrmsr(IA32_APIC_BASE, base | APIC_EN | APIC_EXTD);
        }
    } else {
        wrmsr(IA32_APIC_BASE, (base | APIC_EN) & !APIC_EXTD);
    }
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

/// Sends an IPI: `low` is the ICR's low word (vector, delivery mode, level, shorthand).
fn icr(dest: u32, low: u32) {
    // Interrupts off: in xAPIC mode the two ICR writes must not be split by a handler that sends an IPI too.
    let was = super::interrupts_enabled();
    super::interrupts_disable();
    if X2APIC.load(Ordering::Relaxed) {
        wrmsr(X2APIC_MSR + (REG_ICR_LO >> 4) as u32, (dest as u64) << 32 | low as u64);
    } else {
        while read(REG_ICR_LO) & ICR_PENDING != 0 {
            core::hint::spin_loop();
        }
        write(REG_ICR_HI, dest << 24);
        write(REG_ICR_LO, low);
        while read(REG_ICR_LO) & ICR_PENDING != 0 {
            core::hint::spin_loop();
        }
    }
    if was {
        super::interrupts_enable();
    }
}

/// Fixed-delivery IPI with `vector` to the CPU whose local APIC id is `apic_id`.
pub fn send_ipi(apic_id: u32, vector: u8) {
    icr(apic_id, vector as u32);
}

/// INIT (level assert) to one CPU: puts it in wait-for-SIPI (SDM 8.4.4.1).
pub fn send_init(apic_id: u32) {
    icr(apic_id, ICR_INIT | ICR_ASSERT | ICR_LEVEL);
}

/// STARTUP to one CPU: it starts in real mode at page `vector * 4096`.
pub fn send_startup(apic_id: u32, vector: u8) {
    icr(apic_id, ICR_STARTUP | vector as u32);
}

/// An NMI to one CPU (stopping the machine; CPUs the kernel did not start are left alone).
pub fn send_nmi(apic_id: u32) {
    icr(apic_id, ICR_NMI | ICR_ASSERT);
}

pub fn id() -> u32 {
    if X2APIC.load(Ordering::Relaxed) {
        read(REG_ID)
    } else {
        read(REG_ID) >> 24
    }
}

/// MSI address/data that deliver `vector` to this CPU (fixed delivery, edge, physical destination); `None` when the
/// APIC id does not fit the 8-bit MSI destination (needs interrupt remapping).
pub fn msi(vector: u8) -> Option<(u64, u32)> {
    let id = id();
    (id < 256).then(|| (0xFEE0_0000 | ((id as u64) << 12), vector as u32))
}
