//! The JIT ⇄ runtime ABI (docs/JIT-ABI.md, x86-64 and AArch64, v1).
//!
//! [`VmCtx`] field offsets are part of the ABI: generated code addresses the fields through the `OFF_*`
//! constants below, and compile-time assertions tie those constants to the Rust layout.
//!
//! Registers inside JIT code: `r15` = memory base, `r14` = `*mut VmCtx`, `r13` = memory mask `S - 1` (AArch64:
//! `x27`, `x26`, `x28`).
//!
//! `jit_private` (owned by the JIT): `jit_private[0]` holds the stack pointer of the innermost active `invoke`
//! frame (the trap-unwind target). Each `invoke` saves the previous value on its own stack and restores it on exit
//! (normal or trap), so nested invocations that share one `VmCtx` (a host function calling `invoke` again) are
//! supported. On AArch64 `jit_private[1..=NUM_HELPERS]` hold the helper addresses (stored by every `invoke`). The
//! other words are reserved.
//!
//! Runtime-side assumptions (beyond docs/JIT-ABI.md):
//! * `mem_base` / `mem_mask` are read once when `invoke` starts (into `r15` / `r13`); changing them during an
//!   invocation has no effect on the running code. `invoke` rejects masks that are not `2^k - 1` with
//!   `max(12, min_mem_log2) <= k <= 36`.
//! * `trap_request` must be 0 when an invocation starts; it is only checked after host calls.
//! * Trap codes from the interrupt hook or `trap_request` that are not a valid `Trap::code()` surface as
//!   `Trap::Interrupted`.
//! * Floating point follows the current MXCSR (the SysV default: round-to-nearest, no FTZ/DAZ, exceptions
//!   masked) or FPCR (AArch64: round-to-nearest, no flush-to-zero, no default NaN, exceptions untrapped); the
//!   direction flag must be clear (SysV).
//! * The entry trampoline uses up to `72 + 8 * (nargs + 1)` bytes of stack above the first JIT frame (AArch64:
//!   `128 + 8 * nargs` rounded up to 16), which is not counted by the safepoint check; the caller of `invoke` must
//!   have that much stack.

use core::ffi::c_void;
use core::sync::atomic::{AtomicUsize, Ordering};

/// Per-invocation context shared by the runtime and JIT code. `#[repr(C)]`; offsets are fixed by the ABI.
#[repr(C)]
pub struct VmCtx {
    /// +0: memory base of the instance (`S + TAIL` readable/writable bytes).
    pub mem_base: *mut u8,
    /// +8: `S - 1`.
    pub mem_mask: u64,
    /// +16: safepoint check: trap/interrupt if the stack pointer (after the frame) `<` this. Any thread may store
    /// `usize::MAX` here (Relaxed) to request an interrupt at the next safepoint.
    pub stack_limit: AtomicUsize,
    /// +24: the real stack limit; distinguishes stack overflow from an interrupt request.
    pub real_stack_limit: usize,
    /// +32: host functions, indexed by import index.
    pub host_fns: *const HostFn,
    /// +40: called when an interrupt is requested. Returns 0 to continue, else a trap code.
    pub interrupt: InterruptFn,
    /// +48: host functions set a trap code ([`k10_ir::Trap::code`]) here to abort after returning; the JIT
    /// clears it when it raises the trap.
    pub trap_request: u32,
    /// +52
    pub _pad: u32,
    /// +56: owned by the runtime (current invocation, instance, …).
    pub user: *mut c_void,
    /// +64: owned by the JIT (saved stack pointer for trap unwinding, AArch64 helper addresses).
    pub jit_private: [u64; 8],
}

/// A host function: `rdi = ctx`, arguments in `rsi, rdx, rcx, r8, r9` (AArch64: `x0`, then `x1`–`x5`; unused ones
/// are don't-care).
pub type HostFn = unsafe extern "C" fn(ctx: *mut VmCtx, a0: i64, a1: i64, a2: i64, a3: i64, a4: i64) -> i64;
/// Interrupt hook: 0 = continue, else a trap code.
pub type InterruptFn = unsafe extern "C" fn(ctx: *mut VmCtx) -> u32;

pub const OFF_MEM_BASE: i32 = 0;
pub const OFF_MEM_MASK: i32 = 8;
pub const OFF_STACK_LIMIT: i32 = 16;
pub const OFF_REAL_STACK_LIMIT: i32 = 24;
pub const OFF_HOST_FNS: i32 = 32;
pub const OFF_INTERRUPT: i32 = 40;
pub const OFF_TRAP_REQUEST: i32 = 48;
pub const OFF_USER: i32 = 56;
pub const OFF_JIT_PRIVATE: i32 = 64;
/// `jit_private[0]`: stack pointer of the innermost active `invoke` frame.
pub const OFF_SAVED_RSP: i32 = OFF_JIT_PRIVATE;

const _: () = {
    use core::mem::{align_of, offset_of, size_of};
    assert!(offset_of!(VmCtx, mem_base) == OFF_MEM_BASE as usize);
    assert!(offset_of!(VmCtx, mem_mask) == OFF_MEM_MASK as usize);
    assert!(offset_of!(VmCtx, stack_limit) == OFF_STACK_LIMIT as usize);
    assert!(offset_of!(VmCtx, real_stack_limit) == OFF_REAL_STACK_LIMIT as usize);
    assert!(offset_of!(VmCtx, host_fns) == OFF_HOST_FNS as usize);
    assert!(offset_of!(VmCtx, interrupt) == OFF_INTERRUPT as usize);
    assert!(offset_of!(VmCtx, trap_request) == OFF_TRAP_REQUEST as usize);
    assert!(offset_of!(VmCtx, _pad) == 52);
    assert!(offset_of!(VmCtx, user) == OFF_USER as usize);
    assert!(offset_of!(VmCtx, jit_private) == OFF_JIT_PRIVATE as usize);
    assert!(size_of::<VmCtx>() == 128);
    assert!(align_of::<VmCtx>() == 8);
    assert!(size_of::<AtomicUsize>() == 8);
    assert!(size_of::<HostFn>() == 8);
    assert!(size_of::<InterruptFn>() == 8);
};

impl VmCtx {
    /// A context with no stack limit (`stack_limit = real_stack_limit = 0`: only interrupt requests take the
    /// safepoint slow path). Runtimes set the limits for their stacks; see docs/JIT-ABI.md.
    pub fn new(
        mem_base: *mut u8,
        mem_mask: u64,
        host_fns: *const HostFn,
        interrupt: InterruptFn,
        user: *mut c_void,
    ) -> VmCtx {
        VmCtx {
            mem_base,
            mem_mask,
            stack_limit: AtomicUsize::new(0),
            real_stack_limit: 0,
            host_fns,
            interrupt,
            trap_request: 0,
            _pad: 0,
            user,
            jit_private: [0; 8],
        }
    }

    /// Sets both stack limits (no pending interrupt).
    pub fn set_stack_limit(&mut self, limit: usize) {
        self.real_stack_limit = limit;
        self.stack_limit.store(limit, Ordering::Relaxed);
    }

    /// Requests an interrupt at the next safepoint (callable from any thread through a shared reference to the
    /// `stack_limit` field).
    pub fn request_interrupt(&self) {
        self.stack_limit.store(usize::MAX, Ordering::Relaxed);
    }

    /// Clears an interrupt request (the interrupt hook must do this before returning 0).
    pub fn clear_interrupt(&self) {
        self.stack_limit.store(self.real_stack_limit, Ordering::Relaxed);
    }
}

/// An interrupt hook that clears the request and continues.
///
/// # Safety
/// `ctx` must point to a valid [`VmCtx`].
pub unsafe extern "C" fn interrupt_continue(ctx: *mut VmCtx) -> u32 {
    // SAFETY: the caller (JIT code) passes the live context.
    unsafe { (*ctx).clear_interrupt() };
    0
}
