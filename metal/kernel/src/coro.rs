//! Coroutines: every in-flight invocation runs on its own small stack, so the scheduler (on the boot stack) can
//! suspend it at a TIR safepoint — time-slice preemption — or while it waits for a busy service, and run network
//! I/O and other invocations meanwhile. A switch saves the six callee-saved registers and `rsp`, nothing else:
//! a switch only happens at a call boundary, and the kernel is compiled soft-float (no vector state to save).
//!
//! Stacks are 32 KiB, carved from physical frames, recycled through a pool. There are no guard pages (all RAM
//! is mapped with 2 MiB pages), so each stack has a canary at its bottom that is checked at every switch back to
//! the scheduler; the deepest native use is bounded by the nested-call limit (see runtime::MAX_CALL_DEPTH).

use alloc::vec::Vec;
use core::arch::global_asm;

use crate::mem::frames::{self, Tag, FRAME};
use crate::sync::Global;

/// Measured high-water mark with the test services (incl. nested calls) is under 3 KiB; 32 KiB leaves room for
/// MAX_CALL_DEPTH nested invocations, formatting and interrupt frames.
pub const STACK_SIZE: usize = 32 * 1024;
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
}

impl Stack {
    fn top(&self) -> u64 {
        self.base + STACK_SIZE as u64
    }
    fn canary_ok(&self) -> bool {
        let p = self.base as *const u64;
        (0..CANARY_WORDS).all(|i| unsafe { core::ptr::read_volatile(p.add(i)) } == CANARY)
    }
    /// Bytes of this stack ever written (painted-pattern scan from the bottom).
    pub fn high_water(&self) -> usize {
        let p = self.base as *const u64;
        let words = STACK_SIZE / 8;
        let mut i = CANARY_WORDS;
        while i < words && unsafe { *p.add(i) } == PAINT {
            i += 1;
        }
        STACK_SIZE - i * 8
    }
}

struct Pool {
    free: Vec<Stack>,
    allocated: usize,
    high_water: usize,
}

static POOL: Global<Pool> = Global::new(Pool { free: Vec::new(), allocated: 0, high_water: 0 });

/// Stacks kept for reuse beyond the ones in use.
const POOL_KEEP: usize = 32;

fn stack_alloc() -> Option<Stack> {
    let mut pool = POOL.borrow_mut();
    if let Some(s) = pool.free.pop() {
        return Some(s);
    }
    // New stacks must leave the heap its reserve.
    if frames::free_bytes() < STACK_SIZE as u64 + crate::reserve_bytes() {
        return None;
    }
    let base = frames::alloc(STACK_SIZE / FRAME as usize, 1, Tag::Stack)?;
    pool.allocated += 1;
    drop(pool);
    let p = base as *mut u64;
    unsafe {
        for i in 0..STACK_SIZE / 8 {
            p.add(i).write(if i < CANARY_WORDS { CANARY } else { PAINT });
        }
    }
    Some(Stack { base })
}

fn stack_free(s: Stack) {
    let mut pool = POOL.borrow_mut();
    pool.high_water = pool.high_water.max(s.high_water());
    if pool.free.len() < POOL_KEEP {
        pool.free.push(s);
    } else {
        pool.allocated -= 1;
        frames::free(s.base, STACK_SIZE / FRAME as usize, Tag::Stack);
    }
}

/// Returns pooled stacks to the frame allocator (memory pressure).
pub fn trim_pool() {
    let mut pool = POOL.borrow_mut();
    while let Some(s) = pool.free.pop() {
        pool.high_water = pool.high_water.max(s.high_water());
        pool.allocated -= 1;
        frames::free(s.base, STACK_SIZE / FRAME as usize, Tag::Stack);
    }
}

/// (stacks allocated, deepest use seen in bytes)
pub fn stats() -> (usize, usize) {
    let pool = POOL.borrow();
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

// Only the scheduler and the running coroutine touch this (never interrupt handlers).
static mut CUR: Current = Current { co: core::ptr::null_mut(), sched_rsp: 0, reason: Yield::Finished };

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
            let cur = &raw mut CUR;
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
    unsafe {
        let cur = &raw mut CUR;
        let co = (*cur).co;
        assert!(!co.is_null(), "yield outside a coroutine");
        (*cur).reason = reason;
        k10_switch(&raw mut (*co).rsp, (*cur).sched_rsp);
    }
}

/// Ends the running coroutine (called at the end of its entry function).
pub fn finish() -> ! {
    yield_now(Yield::Finished);
    unreachable!("finished coroutine resumed")
}

/// `[bottom, top)` of the running coroutine's stack (for fault diagnostics).
pub fn current_stack() -> Option<(u64, u64)> {
    unsafe {
        let co = (*(&raw const CUR)).co;
        if co.is_null() {
            return None;
        }
        (*co).stack.as_ref().map(|s| (s.base, s.top()))
    }
}
