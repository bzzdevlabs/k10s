//! Per-CPU state. Each CPU's `IA32_GS_BASE` points at its [`PerCpu`], whose first word points at itself, so
//! [`this`] is one `mov rax, gs:[0]` (no system call or APIC read). The kernel never runs user code, so GS is never
//! swapped.
//!
//! What lives here is what the timer interrupt and the scheduler of *this* CPU share: the preemption request (flag,
//! deadline, JIT preemption word), the idle flag other CPUs check before sending a wake-up IPI, and load counters
//! the dispatcher (CPU 0) reads.

use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use crate::arch::wrmsr;

pub const MAX_CPUS: usize = 8;
const IA32_GS_BASE: u32 = 0xC000_0101;

#[repr(C, align(64))]
pub struct PerCpu {
    /// Points at this structure (read through `gs:[0]`).
    self_ptr: AtomicU64,
    pub id: u32,
    pub apic_id: AtomicU32,
    pub online: AtomicBool,
    /// Halted (or about to halt) with nothing to do: a producer must send a wake-up IPI.
    pub idle: AtomicBool,
    /// TSC value at which the running invocation must be preempted (u64::MAX = none).
    pub preempt_at: AtomicU64,
    /// Set when `preempt_at` has passed (or on request); polled at TIR safepoints.
    pub preempt: AtomicBool,
    /// `VmCtx::stack_limit` of the innermost JIT invocation running on this CPU (null: none).
    pub preempt_word: AtomicPtr<AtomicUsize>,
    /// Tasks queued for or running on this CPU (dispatch decisions).
    pub load: AtomicU32,
    /// Page-permission generation this CPU's TLB reflects (see `arch::paging`).
    pub tlb_gen: AtomicU64,
    pub timer_irqs: AtomicU64,
    pub wakeups: AtomicU64,
    pub tasks_run: AtomicU64,
    /// TSC ticks halted (completed halts), and the TSC when the current halt began (0: not halted).
    pub halted_tsc: AtomicU64,
    pub halt_start: AtomicU64,
}

impl PerCpu {
    /// TSC ticks spent halted so far, including a halt in progress.
    pub fn halted(&self) -> u64 {
        let s = self.halt_start.load(Ordering::Relaxed);
        self.halted_tsc.load(Ordering::Relaxed) + if s != 0 { crate::arch::rdtsc().saturating_sub(s) } else { 0 }
    }
}

impl PerCpu {
    const fn new(id: u32) -> PerCpu {
        PerCpu {
            self_ptr: AtomicU64::new(0),
            id,
            apic_id: AtomicU32::new(0),
            online: AtomicBool::new(false),
            idle: AtomicBool::new(false),
            preempt_at: AtomicU64::new(u64::MAX),
            preempt: AtomicBool::new(false),
            preempt_word: AtomicPtr::new(core::ptr::null_mut()),
            load: AtomicU32::new(0),
            tlb_gen: AtomicU64::new(0),
            timer_irqs: AtomicU64::new(0),
            wakeups: AtomicU64::new(0),
            tasks_run: AtomicU64::new(0),
            halted_tsc: AtomicU64::new(0),
            halt_start: AtomicU64::new(0),
        }
    }
}

static CPUS: [PerCpu; MAX_CPUS] = [
    PerCpu::new(0),
    PerCpu::new(1),
    PerCpu::new(2),
    PerCpu::new(3),
    PerCpu::new(4),
    PerCpu::new(5),
    PerCpu::new(6),
    PerCpu::new(7),
];

/// Points this CPU's GS base at `CPUS[id]` (first thing a CPU does, before any lock or console use).
pub fn init_cpu(id: usize) {
    let p = &CPUS[id];
    p.self_ptr.store(p as *const PerCpu as u64, Ordering::Relaxed);
    wrmsr(IA32_GS_BASE, p as *const PerCpu as u64);
}

/// This CPU's state.
#[inline(always)]
pub fn this() -> &'static PerCpu {
    let p: u64;
    // SAFETY: GS base points at this CPU's `PerCpu` (init_cpu), whose first word is its own address.
    unsafe {
        core::arch::asm!("mov {}, gs:[0]", out(reg) p, options(nostack, preserves_flags, readonly));
        &*(p as *const PerCpu)
    }
}

/// This CPU's index (0 = the boot CPU, which owns the NIC).
#[inline(always)]
pub fn id() -> usize {
    this().id as usize
}

pub fn cpu(i: usize) -> &'static PerCpu {
    &CPUS[i]
}

/// CPUs that have come online (the boot CPU plus started application processors).
pub fn online() -> usize {
    CPUS.iter().filter(|c| c.online.load(Ordering::Acquire)).count()
}
