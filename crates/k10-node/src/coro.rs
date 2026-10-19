//! Stackful coroutines: pooled stacks with guard pages and a hand-written x86-64 (SysV) context switch.
//!
//! A [`Coro`] owns a [`Stack`] and two saved stack pointers: its own (`sp`, valid while suspended) and the
//! scheduler's (`back`, valid while the coroutine runs). [`Coro::resume`] switches scheduler -> coroutine,
//! [`Coro::suspend`] coroutine -> scheduler. A switch pushes the callee-saved registers (rbx, rbp, r12-r15) on the
//! current stack, stores `rsp`, loads the other `rsp` and pops the same set: 16 instructions, no syscall, no signal
//! mask.
//!
//! Invariant: MXCSR and the x87 control word are *not* switched. Nothing that runs on fibers changes them (TIR has
//! no instruction for it, host functions are plain Rust, JIT code must not either), so they always hold the
//! process defaults.
//!
//! The coroutine's entry function (`extern "C" fn(*mut c_void) -> !`) must never return and must not let a panic
//! escape (there is no unwind information above it); the runtime's fiber loop catches panics per job.
//!
//! Stack overflow hits the guard page (PROT_NONE) at the low end and kills the process with SIGSEGV: Rust code
//! running on these stacks (interpreter, host functions) has bounded depth, so this is a bug guard, not a limit
//! workloads can reach (TIR stack budgets are enforced by the executors).

use crate::sys;
use std::ffi::c_void;
use std::io;
use std::ptr;

/// A coroutine stack: `usable` bytes plus one guard page below them, reserved with `MAP_NORESERVE` (only touched
/// pages are committed).
pub struct Stack {
    map: *mut u8,
    map_len: usize,
}

// SAFETY: plain owned memory.
unsafe impl Send for Stack {}

impl Stack {
    pub fn new(usable: usize) -> io::Result<Stack> {
        let usable = usable.div_ceil(sys::PAGE) * sys::PAGE;
        let map_len = usable + sys::PAGE;
        let map = sys::mmap_reserve(map_len)?;
        // SAFETY: the first page of our fresh mapping becomes the guard page.
        if let Err(e) = unsafe { sys::mprotect_none(map, sys::PAGE) } {
            // SAFETY: unmapping our own fresh mapping.
            unsafe { sys::munmap(map, map_len) };
            return Err(e);
        }
        Ok(Stack { map, map_len })
    }

    /// Highest address (exclusive); 16-byte aligned.
    pub fn top(&self) -> *mut u8 {
        // SAFETY: one past the end of our mapping.
        unsafe { self.map.add(self.map_len) }
    }

    /// Lowest usable address (just above the guard page).
    pub fn bottom(&self) -> *mut u8 {
        // SAFETY: inside our mapping.
        unsafe { self.map.add(sys::PAGE) }
    }

    pub fn usable(&self) -> usize {
        self.map_len - sys::PAGE
    }

    /// Releases the physical pages of the stack below `keep` bytes from the top (stack shrink after a deep run).
    ///
    /// # Safety
    /// The stack must not be in use below `top - keep` (e.g. the coroutine is suspended near its entry frame).
    pub unsafe fn trim(&self, keep: usize) {
        let keep = keep.div_ceil(sys::PAGE) * sys::PAGE;
        if keep < self.usable() {
            let _ = sys::madv_dontneed(self.bottom(), self.usable() - keep);
        }
    }
}

impl Drop for Stack {
    fn drop(&mut self) {
        // SAFETY: the owning coroutine is gone (or never ran); nothing points into this stack.
        unsafe { sys::munmap(self.map, self.map_len) };
    }
}

/// Switches stacks: saves the callee-saved state on the current stack, stores `rsp` into `*save`, loads `to` and
/// restores the state found there.
///
/// # Safety
/// `to` must be a stack pointer previously stored by this function (or prepared by [`Coro::new`]) whose stack is
/// still alive and not running.
#[unsafe(naked)]
unsafe extern "C" fn ctx_switch(save: *mut *mut u8, to: *mut u8) {
    core::arch::naked_asm!(
        "push rbp",
        "push rbx",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        "mov [rdi], rsp",
        "mov rsp, rsi",
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop rbx",
        "pop rbp",
        "ret",
    )
}

/// First code a new coroutine runs (reached by `ret` from [`ctx_switch`]): `r12` = argument, `r13` = entry
/// function. `rsp` is 16-byte aligned here, so the `call` gives the entry function a standard frame.
#[unsafe(naked)]
unsafe extern "C" fn ctx_start() {
    core::arch::naked_asm!("mov rdi, r12", "call r13", "ud2")
}

pub type Entry = extern "C" fn(*mut c_void) -> !;

/// A suspended-or-running coroutine.
pub struct Coro {
    stack: Stack,
    /// Coroutine stack pointer while suspended.
    sp: *mut u8,
    /// Scheduler stack pointer while the coroutine runs.
    back: *mut u8,
}

// SAFETY: a coroutine is moved between threads only while suspended (never in this runtime: shards own theirs).
unsafe impl Send for Coro {}

impl Coro {
    /// Prepares a coroutine that will call `entry(arg)` on its first resume.
    pub fn new(stack: Stack, entry: Entry, arg: *mut c_void) -> Coro {
        let top = stack.top() as usize & !15;
        // Initial frame popped by ctx_switch: r15 r14 r13 r12 rbx rbp ret, then 16 bytes of padding. After `ret`,
        // rsp = top - 16 (16-byte aligned), as the `call` in `ctx_start` requires.
        let sp = top - 72;
        let frame: [u64; 9] = [
            0,                                      // r15
            0,                                      // r14
            entry as *const () as usize as u64,     // r13
            arg as usize as u64,                    // r12
            0,                                      // rbx
            0,                                      // rbp
            ctx_start as *const () as usize as u64, // return address
            0,
            0,
        ];
        // SAFETY: [sp, top) lies within the stack's usable range (72 bytes below its 16-aligned top).
        unsafe { ptr::copy_nonoverlapping(frame.as_ptr(), sp as *mut u64, frame.len()) };
        Coro { stack, sp: sp as *mut u8, back: ptr::null_mut() }
    }

    pub fn stack(&self) -> &Stack {
        &self.stack
    }

    /// Runs the coroutine until it suspends.
    ///
    /// # Safety
    /// Must be called from outside the coroutine (scheduler side), on a coroutine that is suspended (or new).
    /// `this` must stay valid until the coroutine suspends again. No `&mut` borrow of `*this` may be held across
    /// the call (the coroutine accesses it through its own raw pointer).
    #[inline]
    pub unsafe fn resume(this: *mut Coro) {
        let to = (*this).sp;
        ctx_switch(ptr::addr_of_mut!((*this).back), to);
    }

    /// Returns control to the scheduler that resumed us.
    ///
    /// # Safety
    /// Must be called from inside the running coroutine `this`.
    #[inline]
    pub unsafe fn suspend(this: *mut Coro) {
        let to = (*this).back;
        ctx_switch(ptr::addr_of_mut!((*this).sp), to);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    struct Ping {
        coro: *mut Coro,
        counter: Cell<u64>,
        log: std::cell::RefCell<Vec<u64>>,
    }

    extern "C" fn ping_entry(arg: *mut c_void) -> ! {
        let p = arg as *const Ping;
        loop {
            unsafe {
                let v = (*p).counter.get();
                (*p).log.borrow_mut().push(v);
                (*p).counter.set(v + 1);
                // exercise floating point + callee-saved registers across switches
                let x = (v as f64).sqrt() * 2.0;
                Coro::suspend((*p).coro);
                assert_eq!(x, (v as f64).sqrt() * 2.0);
            }
        }
    }

    #[test]
    fn ping_pong() {
        let mut ping = Box::new(Ping { coro: ptr::null_mut(), counter: Cell::new(0), log: Default::default() });
        let pp: *mut Ping = &mut *ping;
        let coro = Box::into_raw(Box::new(Coro::new(Stack::new(64 * 1024).unwrap(), ping_entry, pp as *mut c_void)));
        ping.coro = coro;
        let mut acc = 0u64;
        for i in 0..1000u64 {
            let before = acc.wrapping_mul(31).wrapping_add(i);
            unsafe { Coro::resume(coro) };
            acc = before; // callee-saved state on our side must survive
            assert_eq!(ping.counter.get(), i + 1);
        }
        assert_eq!(ping.log.borrow().len(), 1000);
        // The coroutine is suspended forever; dropping its stack is fine (nothing references it).
        unsafe { drop(Box::from_raw(coro)) };
    }

    struct Many {
        coro: *mut Coro,
        id: u64,
        steps: Cell<u64>,
    }

    extern "C" fn many_entry(arg: *mut c_void) -> ! {
        let m = arg as *const Many;
        fn rec(m: *const Many, depth: u32) -> u64 {
            // some stack usage between switches
            let buf = [depth as u64; 32];
            if depth == 0 {
                unsafe {
                    (*m).steps.set((*m).steps.get() + 1);
                    Coro::suspend((*m).coro);
                }
                return buf.iter().sum::<u64>() + unsafe { (*m).id };
            }
            rec(m, depth - 1) + buf[3]
        }
        loop {
            let r = rec(m, 20);
            assert_eq!(r, unsafe { (*m).id } + (1..=20).sum::<u64>());
        }
    }

    #[test]
    fn many_interleaved() {
        let n = 200;
        let mut ms: Vec<Box<Many>> =
            (0..n).map(|i| Box::new(Many { coro: ptr::null_mut(), id: i, steps: Cell::new(0) })).collect();
        let coros: Vec<*mut Coro> = ms
            .iter_mut()
            .map(|m| {
                let mp: *mut Many = &mut **m;
                let c = Box::into_raw(Box::new(Coro::new(Stack::new(32 * 1024).unwrap(), many_entry, mp as *mut c_void)));
                m.coro = c;
                c
            })
            .collect();
        for round in 0..50 {
            for &c in coros.iter().rev() {
                unsafe { Coro::resume(c) };
            }
            for m in &ms {
                assert_eq!(m.steps.get(), round + 1);
            }
        }
        for c in coros {
            unsafe { drop(Box::from_raw(c)) };
        }
    }

    #[test]
    fn stack_alignment_for_simd() {
        // A function that requires a 16-byte aligned stack (movaps spills of f64x2-sized locals) running on a
        // coroutine stack: misalignment would fault.
        struct A {
            coro: *mut Coro,
            out: Cell<f64>,
        }
        extern "C" fn entry(arg: *mut c_void) -> ! {
            let a = arg as *const A;
            let v: Vec<f64> = (0..100).map(|i| i as f64 * 0.5).collect();
            let s = format!("{:.3}", v.iter().sum::<f64>());
            unsafe {
                (*a).out.set(s.parse().unwrap());
                loop {
                    Coro::suspend((*a).coro);
                }
            }
        }
        let mut a = Box::new(A { coro: ptr::null_mut(), out: Cell::new(0.0) });
        let ap: *mut A = &mut *a;
        let c = Box::into_raw(Box::new(Coro::new(Stack::new(64 * 1024).unwrap(), entry, ap as *mut c_void)));
        a.coro = c;
        unsafe { Coro::resume(c) };
        assert_eq!(a.out.get(), 2475.0);
        unsafe { drop(Box::from_raw(c)) };
    }
}
