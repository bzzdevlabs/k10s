//! Test harness: runs JIT code against a `k10_ir::Host` with canary-guarded instance memory.
#![allow(dead_code)]

pub mod diff;

use k10_ir::{Host, Module, Trap, TAIL_BYTES};
use k10_jit::{CompiledModule, HostFn, VmCtx};
use std::ffi::c_void;
use std::sync::atomic::Ordering;

/// The retpoline thunk of docs/JIT-ABI.md: `call 1f; 2: pause; lfence; jmp 2b; 1: mov [rsp], r11; ret`.
pub const RETPOLINE_THUNK: [u8; 17] =
    [0xE8, 7, 0, 0, 0, 0xF3, 0x90, 0x0F, 0xAE, 0xE8, 0xEB, 0xF9, 0x4C, 0x89, 0x1C, 0x24, 0xC3];

/// Canary bytes placed before and after the instance region `[base, base + S + 8)`.
pub const CANARY_LEN: usize = 4096;
const CANARY: u8 = 0xA5;

/// Instance memory `S + TAIL` bytes surrounded by canaries.
pub struct GuardedMem {
    buf: Vec<u8>,
    pub log2: u8,
}

impl GuardedMem {
    pub fn new(log2: u8) -> GuardedMem {
        let s = 1usize << log2;
        let mut buf = vec![CANARY; CANARY_LEN + s + TAIL_BYTES + CANARY_LEN];
        buf[CANARY_LEN..CANARY_LEN + s + TAIL_BYTES].fill(0);
        GuardedMem { buf, log2 }
    }

    /// Fresh memory initialised with the module's data segments.
    pub fn for_module(m: &Module, log2: u8) -> GuardedMem {
        let mut g = GuardedMem::new(log2);
        k10_ir::init_memory(m, g.mem_mut());
        g
    }

    pub fn size(&self) -> usize {
        1usize << self.log2
    }

    pub fn mem(&self) -> &[u8] {
        &self.buf[CANARY_LEN..CANARY_LEN + self.size() + TAIL_BYTES]
    }

    pub fn mem_mut(&mut self) -> &mut [u8] {
        let s = self.size();
        &mut self.buf[CANARY_LEN..CANARY_LEN + s + TAIL_BYTES]
    }

    pub fn base(&mut self) -> *mut u8 {
        // SAFETY: CANARY_LEN < buf.len().
        unsafe { self.buf.as_mut_ptr().add(CANARY_LEN) }
    }

    /// Panics if any canary byte changed (a write outside `[base, base + S + 8)`).
    pub fn check_canaries(&self) {
        let s = self.size();
        static EXPECTED: [u8; CANARY_LEN] = [CANARY; CANARY_LEN];
        let pre = &self.buf[..CANARY_LEN];
        let post = &self.buf[CANARY_LEN + s + TAIL_BYTES..];
        // Slice equality is a memcmp: fast even in debug builds.
        if pre == &EXPECTED[..] && post == &EXPECTED[..] {
            return;
        }
        if let Some(p) = pre.iter().position(|&b| b != CANARY) {
            panic!("ISOLATION BREACH: write {} bytes before the instance memory", CANARY_LEN - p);
        }
        if let Some(p) = post.iter().position(|&b| b != CANARY) {
            panic!("ISOLATION BREACH: write at base + S + 8 + {p}");
        }
    }
}

/// Per-invocation state reached through `VmCtx::user`.
pub struct Dispatch<'a> {
    pub host: &'a mut dyn Host,
    pub mem: *mut u8,
    pub mem_len: usize,
    pub params: Vec<u8>,
    /// Interrupt hook calls.
    pub interrupts: u64,
    /// Request an interrupt (stack_limit = MAX) on every host call: exercises the safepoint slow path.
    pub preempt_on_call: bool,
}

/// Asserts that the stack is 16-byte aligned (SysV) inside a non-leaf extern "C" function body: a misaligned
/// entry rsp would leave the body's rsp misaligned by 8.
#[inline(always)]
pub fn assert_stack_aligned() {
    let rsp: usize;
    // SAFETY: reads rsp only.
    unsafe { std::arch::asm!("mov {}, rsp", out(reg) rsp, options(nomem, nostack, preserves_flags)) };
    assert_eq!(rsp % 16, 0, "host function entered with a misaligned stack");
}

/// Overwrites every SysV volatile register (GPRs and xmm0-15): JIT code must not rely on them across calls.
#[inline(always)]
pub fn clobber_volatile() {
    // SAFETY: only writes registers declared as clobbered.
    unsafe {
        std::arch::asm!(
            "mov rax, -1", "mov rcx, -1", "mov rdx, -1", "mov rsi, -1", "mov rdi, -1",
            "mov r8, -1", "mov r9, -1", "mov r10, -1", "mov r11, -1",
            "pcmpeqd xmm0, xmm0", "pcmpeqd xmm1, xmm1", "pcmpeqd xmm2, xmm2", "pcmpeqd xmm3, xmm3",
            "pcmpeqd xmm4, xmm4", "pcmpeqd xmm5, xmm5", "pcmpeqd xmm6, xmm6", "pcmpeqd xmm7, xmm7",
            "pcmpeqd xmm8, xmm8", "pcmpeqd xmm15, xmm15",
            out("rax") _, out("rcx") _, out("rdx") _, out("rsi") _, out("rdi") _,
            out("r8") _, out("r9") _, out("r10") _, out("r11") _,
            out("xmm0") _, out("xmm1") _, out("xmm2") _, out("xmm3") _, out("xmm4") _, out("xmm5") _,
            out("xmm6") _, out("xmm7") _, out("xmm8") _, out("xmm15") _,
            options(nomem, nostack)
        )
    };
}

unsafe extern "C" fn tramp<const I: usize>(ctx: *mut VmCtx, a0: i64, a1: i64, a2: i64, a3: i64, a4: i64) -> i64 {
    // SAFETY: the harness sets `user` to a live Dispatch for the duration of the invocation and `mem_base` to
    // a buffer of `mem_len` bytes that no Rust reference aliases while JIT code runs.
    unsafe {
        assert_stack_aligned();
        let d = &mut *((*ctx).user as *mut Dispatch);
        let args = [a0, a1, a2, a3, a4];
        let n = d.params[I] as usize;
        let mem = std::slice::from_raw_parts_mut(d.mem, d.mem_len);
        if d.preempt_on_call {
            (*ctx).stack_limit.store(usize::MAX, Ordering::Relaxed);
        }
        let r = match d.host.call(I as u32, &args[..n], mem) {
            Ok(v) => v,
            Err(t) => {
                (*ctx).trap_request = t.code();
                0
            }
        };
        clobber_volatile();
        r
    }
}

pub unsafe extern "C" fn on_interrupt(ctx: *mut VmCtx) -> u32 {
    // SAFETY: as in `tramp`.
    unsafe {
        assert_stack_aligned();
        let d = &mut *((*ctx).user as *mut Dispatch);
        (*ctx).stack_limit.store((*ctx).real_stack_limit, Ordering::Relaxed);
        d.interrupts += 1;
        let r = match d.host.interrupt() {
            Ok(()) => 0,
            Err(t) => t.code(),
        };
        clobber_volatile();
        r
    }
}

macro_rules! tramps {
    ($($i:literal)*) => { [$(tramp::<$i> as HostFn),*] };
}

/// One trampoline per import index (host functions do not receive the import index).
pub static TRAMPS: [HostFn; 16] = tramps!(0 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15);

/// `[lo, hi)` of the current thread's stack (Linux), cached per thread.
pub fn stack_bounds() -> (usize, usize) {
    thread_local!(static B: (usize, usize) = stack_bounds_uncached());
    B.with(|b| *b)
}

fn stack_bounds_uncached() -> (usize, usize) {
    // SAFETY: standard pthread attribute queries on the current thread; attr is destroyed afterwards.
    unsafe {
        let mut attr: libc::pthread_attr_t = std::mem::zeroed();
        assert_eq!(libc::pthread_getattr_np(libc::pthread_self(), &mut attr), 0);
        let mut addr: *mut c_void = std::ptr::null_mut();
        let mut size: libc::size_t = 0;
        assert_eq!(libc::pthread_attr_getstack(&attr, &mut addr, &mut size), 0);
        libc::pthread_attr_destroy(&mut attr);
        (addr as usize, addr as usize + size)
    }
}

/// Host reserve below the deepest JIT frame (docs/JIT-ABI.md: ≥ 32 KiB).
pub const HOST_RESERVE: usize = 256 * 1024;

#[derive(Clone, Copy, Debug, Default)]
pub struct RunOpts {
    pub preempt_on_call: bool,
}

/// Runs `func` of `jit` on `mem` with `host`. Returns the result and the number of interrupt-hook calls.
pub fn run_jit(
    jit: &CompiledModule,
    m: &Module,
    mem: &mut GuardedMem,
    host: &mut dyn Host,
    func: u32,
    args: &[i64],
    opts: RunOpts,
) -> (Result<Option<i64>, Trap>, u64) {
    let mut ctx = VmCtx::new(std::ptr::null_mut(), 0, TRAMPS.as_ptr(), on_interrupt, std::ptr::null_mut());
    run_jit_in(&mut ctx, on_interrupt, jit, m, mem, host, func, args, opts)
}

/// Like [`run_jit`] with a caller-provided (reused) context and interrupt hook; `ctx.stack_limit` may be stored
/// to concurrently (watchdog).
#[allow(clippy::too_many_arguments)]
pub fn run_jit_in(
    ctx: &mut VmCtx,
    hook: k10_jit::InterruptFn,
    jit: &CompiledModule,
    m: &Module,
    mem: &mut GuardedMem,
    host: &mut dyn Host,
    func: u32,
    args: &[i64],
    opts: RunOpts,
) -> (Result<Option<i64>, Trap>, u64) {
    let mem_len = mem.size() + TAIL_BYTES;
    let mut d = Dispatch {
        host,
        mem: mem.base(),
        mem_len,
        params: m.imports.iter().map(|i| i.params).collect(),
        interrupts: 0,
        preempt_on_call: opts.preempt_on_call,
    };
    assert!(m.imports.len() <= TRAMPS.len());
    ctx.mem_base = mem.base();
    ctx.mem_mask = (mem.size() - 1) as u64;
    ctx.host_fns = TRAMPS.as_ptr();
    ctx.interrupt = hook;
    ctx.user = &mut d as *mut Dispatch as *mut c_void;
    let (lo, _) = stack_bounds();
    ctx.real_stack_limit = lo + HOST_RESERVE;
    ctx.stack_limit.store(lo + HOST_RESERVE, Ordering::Relaxed);
    // SAFETY: ctx/user/memory are valid for the call; host_fns covers ≤ 16 imports (asserted).
    let r = unsafe { jit.invoke(ctx, func, args) };
    assert_eq!(ctx.trap_request, 0, "trap_request not cleared");
    ctx.user = std::ptr::null_mut();
    (r, d.interrupts)
}

/// Normalizes a float-ish result for comparison (only used for tests that produce raw NaNs).
pub fn canon_nan(v: i64) -> i64 {
    let f = f64::from_bits(v as u64);
    if f.is_nan() {
        k10_ir::gen::CANONICAL_NAN
    } else {
        v
    }
}
