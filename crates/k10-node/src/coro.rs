//! Stackful coroutines: pooled stacks with guard pages and a hand-written context switch (x86-64 SysV, AArch64
//! AAPCS64, i686 SysV, armv7 AAPCS-VFP).
//!
//! A [`Coro`] owns a [`Stack`] and two saved stack pointers: its own (`sp`, valid while suspended) and the
//! scheduler's (`back`, valid while the coroutine runs). [`Coro::resume`] switches scheduler -> coroutine,
//! [`Coro::suspend`] coroutine -> scheduler. A switch saves the callee-saved registers on the current stack, stores
//! the stack pointer, loads the other one and restores the same set: no syscall, no signal mask.
//!
//! | arch | saved | frame |
//! |---|---|---|
//! | x86_64 | rbx rbp r12-r15 (+ return address); no xmm register is callee-saved | 56 B |
//! | aarch64 | x19-x28, x29 (fp), x30 (lr), d8-d15 (low halves of v8-v15) | 160 B |
//! | x86 (i686) | ebx esi edi ebp (+ return address); no x87/xmm register is callee-saved | 20 B |
//! | arm (armv7, hard-float) | r4-r11, r12 (8-byte alignment padding), lr, d8-d15 | 104 B |
//!
//! Invariant: the floating-point control state (MXCSR and the x87 control word, FPCR, FPSCR) is *not* switched.
//! Nothing that runs on fibers changes it (TIR has no instruction for it, host functions are plain Rust, JIT code
//! must not either), so it always holds the process defaults.
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

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64", target_arch = "x86", target_arch = "arm")))]
compile_error!("coroutines: no context switch for this architecture");

#[cfg(target_arch = "x86_64")]
mod arch {
    /// Switches stacks: saves the callee-saved state on the current stack, stores `rsp` into `*save`, loads `to`
    /// and restores the state found there.
    ///
    /// # Safety
    /// `to` must be a stack pointer previously stored by this function (or prepared by [`frame`]) whose stack is
    /// still alive and not running.
    #[unsafe(naked)]
    pub unsafe extern "C" fn ctx_switch(save: *mut *mut u8, to: *mut u8) {
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

    /// The initial frame below the 16-aligned `top` that [`ctx_switch`] pops: r15 r14 r13 r12 rbx rbp ret, then 16
    /// bytes of padding. After `ret`, rsp = top - 16 (16-byte aligned), as the `call` in `ctx_start` requires.
    pub fn frame(top: usize, entry: usize, arg: usize) -> (usize, [usize; 9]) {
        (top - 72, [0, 0, entry, arg, 0, 0, ctx_start as *const () as usize, 0, 0])
    }
}

#[cfg(target_arch = "aarch64")]
mod arch {
    /// Switches stacks: saves the callee-saved state on the current stack, stores `sp` into `*save`, loads `to`
    /// and restores the state found there.
    ///
    /// # Safety
    /// `to` must be a stack pointer previously stored by this function (or prepared by [`frame`]) whose stack is
    /// still alive and not running.
    #[unsafe(naked)]
    pub unsafe extern "C" fn ctx_switch(save: *mut *mut u8, to: *mut u8) {
        core::arch::naked_asm!(
            "sub sp, sp, #160",
            "stp x19, x20, [sp, #0]",
            "stp x21, x22, [sp, #16]",
            "stp x23, x24, [sp, #32]",
            "stp x25, x26, [sp, #48]",
            "stp x27, x28, [sp, #64]",
            "stp x29, x30, [sp, #80]",
            "stp d8, d9, [sp, #96]",
            "stp d10, d11, [sp, #112]",
            "stp d12, d13, [sp, #128]",
            "stp d14, d15, [sp, #144]",
            "mov x9, sp",
            "str x9, [x0]",
            "mov sp, x1",
            "ldp x19, x20, [sp, #0]",
            "ldp x21, x22, [sp, #16]",
            "ldp x23, x24, [sp, #32]",
            "ldp x25, x26, [sp, #48]",
            "ldp x27, x28, [sp, #64]",
            "ldp x29, x30, [sp, #80]",
            "ldp d8, d9, [sp, #96]",
            "ldp d10, d11, [sp, #112]",
            "ldp d12, d13, [sp, #128]",
            "ldp d14, d15, [sp, #144]",
            "add sp, sp, #160",
            "ret",
        )
    }

    /// First code a new coroutine runs (reached by `ret` from [`ctx_switch`]): `x19` = argument, `x20` = entry
    /// function; `x29` = 0 ends the frame chain.
    #[unsafe(naked)]
    unsafe extern "C" fn ctx_start() {
        core::arch::naked_asm!("mov x0, x19", "blr x20", "brk #1")
    }

    /// The initial frame below the 16-aligned `top` that [`ctx_switch`] pops: x19 (argument), x20 (entry),
    /// x21-x28, x29, x30 (`ctx_start`), d8-d15. After the pop, sp = top.
    pub fn frame(top: usize, entry: usize, arg: usize) -> (usize, [usize; 20]) {
        let mut f = [0; 20];
        f[0] = arg;
        f[1] = entry;
        f[11] = ctx_start as *const () as usize;
        (top - 160, f)
    }
}

#[cfg(target_arch = "x86")]
mod arch {
    /// Switches stacks: saves the callee-saved state on the current stack, stores `esp` into `*save`, loads `to`
    /// and restores the state found there (cdecl: both arguments on the stack).
    ///
    /// # Safety
    /// `to` must be a stack pointer previously stored by this function (or prepared by [`frame`]) whose stack is
    /// still alive and not running.
    #[unsafe(naked)]
    pub unsafe extern "C" fn ctx_switch(save: *mut *mut u8, to: *mut u8) {
        core::arch::naked_asm!(
            "mov eax, [esp + 4]",
            "mov ecx, [esp + 8]",
            "push ebp",
            "push ebx",
            "push esi",
            "push edi",
            "mov [eax], esp",
            "mov esp, ecx",
            "pop edi",
            "pop esi",
            "pop ebx",
            "pop ebp",
            "ret",
        )
    }

    /// First code a new coroutine runs (reached by `ret` from [`ctx_switch`] with `esp` 16-byte aligned): `esi` =
    /// argument, `edi` = entry function, called with `esp` 16-byte aligned at the `call` as the i386 Linux ABI wants.
    #[unsafe(naked)]
    unsafe extern "C" fn ctx_start() {
        core::arch::naked_asm!("sub esp, 12", "push esi", "call edi", "ud2")
    }

    /// The initial frame below the 16-aligned `top` that [`ctx_switch`] pops: edi (entry), esi (argument), ebx,
    /// ebp, ret (`ctx_start`). After `ret`, esp = top.
    pub fn frame(top: usize, entry: usize, arg: usize) -> (usize, [usize; 5]) {
        (top - 20, [entry, arg, 0, 0, ctx_start as *const () as usize])
    }
}

#[cfg(all(target_arch = "arm", not(target_abi = "eabihf")))]
compile_error!("coroutines: 32-bit ARM needs the hard-float ABI (VFP registers d8-d15)");

#[cfg(target_arch = "arm")]
mod arch {
    /// Switches stacks: saves the callee-saved state on the current stack, stores `sp` into `*save`, loads `to`
    /// and restores the state found there. r12 is pushed only to keep the frame a multiple of 8 bytes.
    ///
    /// # Safety
    /// `to` must be a stack pointer previously stored by this function (or prepared by [`frame`]) whose stack is
    /// still alive and not running.
    #[unsafe(naked)]
    pub unsafe extern "C" fn ctx_switch(save: *mut *mut u8, to: *mut u8) {
        core::arch::naked_asm!(
            "push {{r4-r12, lr}}",
            "vpush {{d8-d15}}",
            "mov r2, sp",
            "str r2, [r0]",
            "mov sp, r1",
            "vpop {{d8-d15}}",
            "pop {{r4-r12, lr}}",
            "bx lr",
        )
    }

    /// First code a new coroutine runs (reached by `bx lr` from [`ctx_switch`]): `r4` = argument, `r5` = entry
    /// function; r7 and r11 (the Thumb and ARM frame pointers) are 0, which ends the frame chain.
    #[unsafe(naked)]
    unsafe extern "C" fn ctx_start() {
        core::arch::naked_asm!("mov r0, r4", "blx r5", "udf #0")
    }

    /// The initial frame below the 16-aligned `top` that [`ctx_switch`] pops: d8-d15 (16 words), r4 (argument),
    /// r5 (entry), r6-r12, lr (`ctx_start`). After the pop, sp = top.
    pub fn frame(top: usize, entry: usize, arg: usize) -> (usize, [usize; 26]) {
        let mut f = [0; 26];
        f[16] = arg;
        f[17] = entry;
        f[25] = ctx_start as *const () as usize;
        (top - 104, f)
    }
}

use arch::ctx_switch;

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
        let (sp, frame) = arch::frame(top, entry as *const () as usize, arg as usize);
        debug_assert_eq!(sp + size_of::<usize>() * frame.len(), top);
        // SAFETY: [sp, top) lies within the stack's usable range (one frame below its 16-aligned top).
        unsafe { ptr::copy_nonoverlapping(frame.as_ptr(), sp as *mut usize, frame.len()) };
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

    /// d8-d15 are callee-saved on AArch64: the scheduler's values survive a coroutine that overwrites them.
    #[cfg(target_arch = "aarch64")]
    #[test]
    fn fp_callee_saved_registers_survive() {
        use core::arch::asm;
        struct C {
            coro: *mut Coro,
        }
        extern "C" fn entry(arg: *mut c_void) -> ! {
            let c = arg as *const C;
            loop {
                // SAFETY: plain register writes, declared as clobbered.
                unsafe {
                    asm!("fmov d8, #-1.0", "fmov d9, #-1.0", "fmov d10, #-1.0", "fmov d11, #-1.0", "fmov d12, #-1.0",
                        "fmov d13, #-1.0", "fmov d14, #-1.0", "fmov d15, #-1.0",
                        out("d8") _, out("d9") _, out("d10") _, out("d11") _, out("d12") _, out("d13") _, out("d14") _, out("d15") _);
                    Coro::suspend((*c).coro);
                }
            }
        }
        extern "C" fn resume(c: *mut Coro) {
            unsafe { Coro::resume(c) }
        }
        let mut c = Box::new(C { coro: ptr::null_mut() });
        let cp: *mut C = &mut *c;
        let coro = Box::into_raw(Box::new(Coro::new(Stack::new(64 * 1024).unwrap(), entry, cp as *mut c_void)));
        c.coro = coro;
        let mut out = [0f64; 8];
        // SAFETY: d8-d15 are set, kept across a call that must preserve them and stored to `out`; the output
        // pointer is kept on the stack across the call.
        unsafe {
            asm!(
                "str {o}, [sp, #-16]!",
                "fmov d8, #1.0", "fmov d9, #2.0", "fmov d10, #3.0", "fmov d11, #4.0",
                "fmov d12, #5.0", "fmov d13, #6.0", "fmov d14, #7.0", "fmov d15, #8.0",
                "blr {f}",
                "ldr x9, [sp], #16",
                "stp d8, d9, [x9]", "stp d10, d11, [x9, #16]", "stp d12, d13, [x9, #32]", "stp d14, d15, [x9, #48]",
                o = in(reg) out.as_mut_ptr(),
                f = in(reg) resume as extern "C" fn(*mut Coro),
                in("x0") coro,
                out("d8") _, out("d9") _, out("d10") _, out("d11") _, out("d12") _, out("d13") _, out("d14") _, out("d15") _,
                clobber_abi("C"),
            );
        }
        assert_eq!(out, [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0]);
        unsafe { drop(Box::from_raw(coro)) };
    }

    /// d8-d15 are callee-saved on 32-bit ARM (AAPCS-VFP): the scheduler's values survive a coroutine that overwrites
    /// them.
    #[cfg(target_arch = "arm")]
    #[test]
    fn fp_callee_saved_registers_survive() {
        use core::arch::asm;
        struct C {
            coro: *mut Coro,
        }
        extern "C" fn entry(arg: *mut c_void) -> ! {
            let c = arg as *const C;
            loop {
                // SAFETY: plain register writes, declared as clobbered.
                unsafe {
                    asm!("vmov.f64 d8, #-1.0", "vmov.f64 d9, #-1.0", "vmov.f64 d10, #-1.0", "vmov.f64 d11, #-1.0",
                        "vmov.f64 d12, #-1.0", "vmov.f64 d13, #-1.0", "vmov.f64 d14, #-1.0", "vmov.f64 d15, #-1.0",
                        out("d8") _, out("d9") _, out("d10") _, out("d11") _, out("d12") _, out("d13") _, out("d14") _, out("d15") _);
                    Coro::suspend((*c).coro);
                }
            }
        }
        extern "C" fn resume(c: *mut Coro) {
            unsafe { Coro::resume(c) }
        }
        let mut c = Box::new(C { coro: ptr::null_mut() });
        let cp: *mut C = &mut *c;
        let coro = Box::into_raw(Box::new(Coro::new(Stack::new(64 * 1024).unwrap(), entry, cp as *mut c_void)));
        c.coro = coro;
        let mut out = [0f64; 8];
        // SAFETY: d8-d15 are set, kept across a call that must preserve them and stored to `out`, whose address is
        // in r5 (callee-saved).
        unsafe {
            asm!(
                "vmov.f64 d8, #1.0", "vmov.f64 d9, #2.0", "vmov.f64 d10, #3.0", "vmov.f64 d11, #4.0",
                "vmov.f64 d12, #5.0", "vmov.f64 d13, #6.0", "vmov.f64 d14, #7.0", "vmov.f64 d15, #8.0",
                "blx {f}",
                "vstmia r5, {{d8-d15}}",
                f = in(reg) resume as extern "C" fn(*mut Coro),
                in("r0") coro,
                in("r5") out.as_mut_ptr(),
                out("d8") _, out("d9") _, out("d10") _, out("d11") _, out("d12") _, out("d13") _, out("d14") _, out("d15") _,
                clobber_abi("C"),
            );
        }
        assert_eq!(out, [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0]);
        unsafe { drop(Box::from_raw(coro)) };
    }
}
