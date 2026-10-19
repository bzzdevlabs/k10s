//! Coroutines: every in-flight invocation runs on its own small stack, so the scheduler (on the boot stack) can
//! suspend it at a TIR safepoint — time-slice preemption — or while it waits for a busy service, and run network
//! I/O and other invocations meanwhile. A switch saves the six callee-saved registers and `rsp`, nothing else:
//! a switch only happens at a call boundary, and the kernel is compiled soft-float (no vector state to save).
//!
//! Stacks are carved from physical frames and recycled through a pool: 32 KiB with the interpreter, 96 KiB with
//! the JIT (whose frames live on this stack; see [`set_stack_size`]). There are no guard pages, so each stack has
//! a canary at its bottom that is checked at every switch back to the scheduler. Native use is bounded: by the
//! nested-call limit (runtime::MAX_CALL_DEPTH) for host code, and for JIT frames by the JIT's own stack-limit
//! check, which keeps `runtime::jit::HOST_RESERVE` bytes free below the deepest JIT frame.
//!
//! A switch also saves the running JIT context's preemption word (`percpu::PerCpu::preempt_word`) in the suspended frame
//! and restores it on resume, so the timer always preempts the context that is actually running.

use alloc::vec::Vec;
use core::arch::global_asm;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::mem::frames::{self, Tag, FRAME};
use crate::percpu::{self, MAX_CPUS};
use crate::sync::IrqLock;

/// Interpreter: the measured high-water mark with the test services (incl. nested calls) is under 3 KiB; 32 KiB
/// leaves room for MAX_CALL_DEPTH nested invocations, formatting and interrupt frames.
pub const STACK_SIZE_INTERP: usize = 32 * 1024;
/// JIT: 64 KiB for JIT frames (a small function's frame is 32 bytes: ~2000 frames, docs/TIR.md §6 asks for
/// ≥ 1000) + the 32 KiB host reserve.
pub const STACK_SIZE_JIT: usize = 96 * 1024;
static STACK_SIZE: AtomicUsize = AtomicUsize::new(STACK_SIZE_INTERP);

/// Sets the size of coroutine stacks allocated from now on (boot, before any coroutine exists).
pub fn set_stack_size(bytes: usize) {
    trim_pool();
    STACK_SIZE.store(bytes, Ordering::Relaxed);
}

pub fn stack_size() -> usize {
    STACK_SIZE.load(Ordering::Relaxed)
}

/// Bytes at the bottom of every stack reserved for the canary.
pub const CANARY_BYTES: usize = CANARY_WORDS * 8;
const CANARY: u64 = 0x6b31_3073_5f53_544b; // "KTS_s01k"
const CANARY_WORDS: usize = 8;
/// Byte pattern painted on fresh stacks to measure the high-water mark.
const PAINT: u64 = 0xA5A5_A5A5_A5A5_A5A5;

global_asm!(
    r#"
.pushsection .text.coro, "ax"
/* k10_switch(save: *mut u64 [rdi], next_rsp: u64 [rsi]) */
.global k10_switch
k10_switch:
    push %rbp
    push %rbx
    push %r12
    push %r13
    push %r14
    push %r15
    mov %rsp, (%rdi)
    mov %rsi, %rsp
    pop %r15
    pop %r14
    pop %r13
    pop %r12
    pop %rbx
    pop %rbp
    ret

/* First activation of a coroutine: r12 = entry, r13 = argument. rsp is 16-byte aligned here. */
.global k10_coro_trampoline
k10_coro_trampoline:
    mov %r13, %rdi
    call *%r12
    ud2
.popsection
"#,
    options(att_syntax)
);

extern "C" {
    fn k10_switch(save: *mut u64, next_rsp: u64);
    fn k10_coro_trampoline();
}

pub struct Stack {
    base: u64,
    size: usize,
}

impl Stack {
    fn top(&self) -> u64 {
        self.base + self.size as u64
    }
    fn canary_ok(&self) -> bool {
        let p = self.base as *const u64;
        (0..CANARY_WORDS).all(|i| unsafe { core::ptr::read_volatile(p.add(i)) } == CANARY)
    }
    /// Bytes of this stack ever written (painted-pattern scan from the bottom).
    pub fn high_water(&self) -> usize {
        let p = self.base as *const u64;
        let words = self.size / 8;
        let mut i = CANARY_WORDS;
        while i < words && unsafe { *p.add(i) } == PAINT {
            i += 1;
        }
        self.size - i * 8
    }
}

struct Pool {
    free: Vec<Stack>,
    allocated: usize,
    high_water: usize,
}

/// Shared by all CPUs (a stack is only ever used by the CPU whose coroutine runs on it).
static POOL: IrqLock<Pool> = IrqLock::new(Pool { free: Vec::new(), allocated: 0, high_water: 0 });

/// Stacks kept for reuse beyond the ones in use.
const POOL_KEEP: usize = 32;

fn stack_alloc() -> Option<Stack> {
    let mut pool = POOL.lock();
    if let Some(s) = pool.free.pop() {
        return Some(s);
    }
    let size = stack_size();
    // New stacks must leave the heap its reserve.
    if frames::free_bytes() < size as u64 + crate::reserve_bytes() {
        return None;
    }
    let base = frames::alloc(size / FRAME as usize, 1, Tag::Stack)?;
    pool.allocated += 1;
    drop(pool);
    let p = base as *mut u64;
    unsafe {
        for i in 0..size / 8 {
            p.add(i).write(if i < CANARY_WORDS { CANARY } else { PAINT });
        }
    }
    Some(Stack { base, size })
}

fn stack_free(s: Stack) {
    let mut pool = POOL.lock();
    if pool.free.len() < POOL_KEEP && s.size == stack_size() {
        // (The high-water scan is done lazily, in `stats`: it would cost microseconds per request here.)
        pool.free.push(s);
    } else {
        pool.high_water = pool.high_water.max(s.high_water());
        pool.allocated -= 1;
        frames::free(s.base, s.size / FRAME as usize, Tag::Stack);
    }
}

/// Returns pooled stacks to the frame allocator (memory pressure).
pub fn trim_pool() {
    let mut pool = POOL.lock();
    while let Some(s) = pool.free.pop() {
        pool.high_water = pool.high_water.max(s.high_water());
        pool.allocated -= 1;
        frames::free(s.base, s.size / FRAME as usize, Tag::Stack);
    }
}

/// (stacks allocated, deepest use seen in bytes — over the stacks that are not in use right now)
pub fn stats() -> (usize, usize) {
    let mut pool = POOL.lock();
    let hw = pool.free.iter().map(|s| s.high_water()).max().unwrap_or(0);
    pool.high_water = pool.high_water.max(hw);
    (pool.allocated, pool.high_water)
}

/// Why a coroutine gave control back to the scheduler.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Yield {
    /// Time slice over (TIR safepoint saw a preemption request).
    Preempted,
    /// Waiting for an instance of a busy service (the argument is an opaque wait key).
    Wait(usize),
    /// The entry function finished; the coroutine must not be resumed again.
    Finished,
}

pub struct Coroutine {
    stack: Option<Stack>,
    rsp: u64,
    finished: bool,
}

struct Current {
    co: *mut Coroutine,
    sched_rsp: u64,
    reason: Yield,
}

// Per CPU; only that CPU's scheduler and running coroutine touch its entry (never interrupt handlers).
static mut CUR: [Current; MAX_CPUS] =
    [const { Current { co: core::ptr::null_mut(), sched_rsp: 0, reason: Yield::Finished } }; MAX_CPUS];

/// This CPU's coroutine bookkeeping.
fn cur() -> *mut Current {
    // SAFETY: in bounds; each CPU only uses its own entry.
    unsafe { (&raw mut CUR).cast::<Current>().add(percpu::id()) }
}

impl Coroutine {
    /// Prepares a coroutine that will run `entry(arg)`; `entry` must end with [`finish`].
    pub fn new(entry: extern "C" fn(usize) -> !, arg: usize) -> Option<Coroutine> {
        let stack = stack_alloc()?;
        // Initial frame popped by k10_switch: r15, r14, r13 (arg), r12 (entry), rbx, rbp, then the return
        // address (the trampoline). After `ret`, rsp = top - 16 (16-byte aligned, as the ABI requires before the
        // trampoline's `call`).
        let top = stack.top();
        let frame = (top - 72) as *mut u64;
        unsafe {
            frame.add(0).write(0); // r15
            frame.add(1).write(0); // r14
            frame.add(2).write(arg as u64); // r13
            frame.add(3).write(entry as *const () as u64); // r12
            frame.add(4).write(0); // rbx
            frame.add(5).write(0); // rbp
            frame.add(6).write(k10_coro_trampoline as *const () as u64); // return address
            frame.add(7).write(0);
            frame.add(8).write(0);
        }
        Some(Coroutine { stack: Some(stack), rsp: frame as u64, finished: false })
    }

    /// Runs the coroutine until it yields or finishes. Must be called from the scheduler (not a coroutine).
    pub fn resume(&mut self) -> Yield {
        assert!(!self.finished, "resume of a finished coroutine");
        unsafe {
            let cur = cur();
            assert!((*cur).co.is_null(), "nested coroutine resume");
            (*cur).co = self;
            (*cur).reason = Yield::Finished;
            k10_switch(&raw mut (*cur).sched_rsp, self.rsp);
            (*cur).co = core::ptr::null_mut();
            let reason = (*cur).reason;
            let stack = self.stack.as_ref().unwrap();
            if !stack.canary_ok() {
                panic!("coroutine stack overflow (stack {:#x})", stack.base);
            }
            if reason == Yield::Finished {
                self.finished = true;
                stack_free(self.stack.take().unwrap());
            }
            reason
        }
    }

}

impl Drop for Coroutine {
    fn drop(&mut self) {
        // The runtime always runs coroutines to completion; dropping a suspended one would only leak whatever its
        // frames own, never touch freed memory (the stack goes back to the pool).
        if let Some(s) = self.stack.take() {
            stack_free(s);
        }
    }
}

/// Suspends the running coroutine, handing `reason` to the scheduler; returns when resumed.
pub fn yield_now(reason: Yield) {
    // The scheduler and other coroutines must not be preempted through this coroutine's JIT context.
    let word = crate::time::set_preempt_word(core::ptr::null_mut());
    unsafe {
        let cur = cur();
        let co = (*cur).co;
        assert!(!co.is_null(), "yield outside a coroutine");
        (*cur).reason = reason;
        k10_switch(&raw mut (*co).rsp, (*cur).sched_rsp);
    }
    crate::time::set_preempt_word(word);
}

/// Ends the running coroutine (called at the end of its entry function).
pub fn finish() -> ! {
    yield_now(Yield::Finished);
    unreachable!("finished coroutine resumed")
}

/// `[bottom, top)` of the running coroutine's stack (for fault diagnostics).
pub fn current_stack() -> Option<(u64, u64)> {
    unsafe {
        let co = (*cur()).co;
        if co.is_null() {
            return None;
        }
        (*co).stack.as_ref().map(|s| (s.base, s.top()))
    }
}
