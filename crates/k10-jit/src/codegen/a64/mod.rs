//! TIR → AArch64 code generation (no_std + alloc). Module driver and the shared prelude; the per-function
//! single-pass compiler is in [`func`].
//!
//! Blob layout: the helper address table at offset 0 (the entry trampoline loads it with `ldr` literal and
//! publishes it in `VmCtx::jit_private`), then the prelude (entry trampoline, trap stubs, safepoint thunk,
//! retpoline thunk), then the functions, 16-byte aligned. A function whose conditional branches do not all reach
//! (±1 MiB) is compiled again in far mode (`Asm::set_far`).

mod func;

use super::{CompileError, CompileOptions, IndirectBranch};
use crate::a64::{Alu, Asm, AsmError, Domain, Label, LdSt, Logic, Reg, VReg};
use crate::abi::*;
use crate::blob::{BlobMeta, CodeBlob, FuncEntry};
use crate::helpers::NUM_HELPERS;
use alloc::vec::Vec;
use k10_ir::{Module, ModuleInfo};

pub(super) use func::MAX_PINNED;

/// Maximum blob size: every `b`/`bl` inside the blob reaches (±128 MiB).
pub(super) const MAX_BLOB_BYTES: usize = 1 << 27;

/// `*mut VmCtx` (callee-saved in AAPCS64, like the next two: host functions preserve them).
pub(crate) const CTX: Reg = Reg::X26;
/// Memory base of the instance.
pub(crate) const BASE: Reg = Reg::X27;
/// Memory mask `S - 1`.
pub(crate) const MASK: Reg = Reg::X28;
/// The codegen's temporaries (never allocated): the indirect-branch target register, constants, addresses.
pub(crate) const TMP: Reg = Reg::X16;
pub(crate) const TMP2: Reg = Reg::X17;

/// FP/SIMD registers that hold pinned locals (caller-saved; the safepoint thunk preserves them), in assignment
/// order.
pub(crate) const FP_PINS: [u8; 8] = [31, 30, 29, 28, 27, 26, 25, 24];

/// `jit_private[1 + h]`: the address of helper `h`, stored by the entry trampoline.
pub(crate) const OFF_HELPERS: i32 = OFF_JIT_PRIVATE + 8;
const _: () = assert!(NUM_HELPERS <= 7);

/// Module-level labels shared by all functions.
pub(crate) struct Shared {
    /// Defined function `i` → its entry label.
    pub funcs: Vec<Label>,
    /// `w0` = trap code; restores the `invoke` frame and returns the code.
    pub trap_common: Label,
    pub trap_unreachable: Label,
    pub trap_divzero: Label,
    pub trap_overflow: Label,
    /// After a host call with `trap_request != 0`.
    pub host_trap: Label,
    /// Safepoint slow path: `x16` = prospective `sp`; preserves every register but `x17` and the flags.
    pub safepoint: Label,
    /// `(params, results)` of each import.
    pub imports: Vec<(u8, u8)>,
    pub indirect: IndirectBranch,
    /// Retpoline thunk (target in `x16`, entered by `bl` or `b`); bound in retpoline mode only.
    pub retpoline: Label,
    /// FEAT_SB: `sb` is the speculation barrier, else `dsb nsh; isb`.
    pub sb: bool,
}

/// The speculation barrier of the hardened modes.
pub(crate) fn spec_barrier(a: &mut Asm, sh: &Shared) {
    if sh.sb {
        a.sb();
    } else {
        a.dsb(Domain::Nsh);
        a.isb();
    }
}

/// An indirect call (`call`) or jump to the address in `x16`, in the configured [`IndirectBranch`] form.
pub(crate) fn indirect(a: &mut Asm, sh: &Shared, call: bool) {
    match sh.indirect {
        IndirectBranch::Jump | IndirectBranch::Lfence => {
            if sh.indirect == IndirectBranch::Lfence {
                spec_barrier(a, sh);
            }
            if call {
                a.blr(TMP);
            } else {
                a.br(TMP);
            }
        }
        IndirectBranch::Retpoline => {
            if call {
                a.bl(sh.retpoline);
            } else {
                a.b(sh.retpoline);
            }
        }
    }
}

/// Compiles a validated module (`info` from `k10_ir::validate`) to a position-independent [`CodeBlob`].
pub(super) fn compile(m: &Module, info: &ModuleInfo, opts: &CompileOptions) -> Result<CodeBlob, CompileError> {
    let tir_bytes: usize = info.funcs.iter().map(|f| f.code_bytes as usize).sum();
    let mut a = Asm::with_capacity(1024 + tir_bytes * 8);
    let mut sh = Shared {
        funcs: Vec::with_capacity(m.funcs.len()),
        trap_common: a.new_label(),
        trap_unreachable: a.new_label(),
        trap_divzero: a.new_label(),
        trap_overflow: a.new_label(),
        host_trap: a.new_label(),
        safepoint: a.new_label(),
        imports: m.imports.iter().map(|i| (i.params, i.results)).collect(),
        indirect: opts.indirect,
        retpoline: a.new_label(),
        sb: opts.features.sb,
    };
    for _ in 0..m.funcs.len() {
        let l = a.new_label();
        sh.funcs.push(l);
    }
    let helper_table = a.pos();
    let mut helpers = [Label(0); NUM_HELPERS];
    for h in &mut helpers {
        *h = a.new_label();
        a.bind(*h);
        a.u64_data(0);
    }
    let entry = emit_prelude(&mut a, &sh, &helpers);
    let mut scratch = func::Scratch::default();
    let mut funcs = Vec::with_capacity(m.funcs.len());
    for (i, (f, fi)) in m.funcs.iter().zip(&info.funcs).enumerate() {
        a.align(16);
        let start = a.checkpoint();
        a.bind(sh.funcs[i]);
        funcs.push(FuncEntry { offset: a.pos(), params: f.params, results: f.results });
        func::compile_func(&mut a, &sh, m, f, fi, opts, &mut scratch);
        if !a.short_fixups_reach(start) {
            a.rollback(start);
            a.set_far(true);
            a.bind(sh.funcs[i]);
            func::compile_func(&mut a, &sh, m, f, fi, opts, &mut scratch);
            a.set_far(false);
        }
        if a.pos() as usize > MAX_BLOB_BYTES {
            return Err(CompileError::TooLarge);
        }
    }
    let code = a.finish().map_err(|e| match e {
        AsmError::OutOfRange => CompileError::TooLarge,
        AsmError::UnboundLabel(_) => CompileError::Internal("branch to an unbound label"),
    })?;
    let len = code.len() as u32;
    Ok(CodeBlob {
        code,
        meta: BlobMeta {
            entry,
            helper_table,
            len,
            funcs,
            imports: sh.imports,
            exports: m.exports.iter().map(|e| (e.name.clone(), e.func)).collect(),
            min_mem_log2: m.min_mem_log2,
            features: opts.features,
        },
    })
}

/// Saved registers of the entry trampoline's frame: `x29, x30` at `[sp]`, then these pairs.
const ENTRY_PAIRS: [(Reg, Reg); 5] =
    [(Reg::X19, Reg::X20), (Reg::X21, Reg::X22), (Reg::X23, Reg::X24), (Reg::X25, Reg::X26), (Reg::X27, Reg::X28)];
const ENTRY_FRAME: i32 = 16 * (1 + ENTRY_PAIRS.len() as i32);

/// Registers the safepoint thunk saves around the interrupt hook: the AAPCS64 caller-saved general registers JIT
/// code uses (`x0`–`x16`) and the link register (`x17` is its own scratch register), then the FP/SIMD registers of
/// pinned locals (`FP_PINS`: the operand stack is empty at safepoints, so no other FP/SIMD value is live).
const SAFEPOINT_SAVED: [Reg; 18] = [
    Reg::X0,
    Reg::X1,
    Reg::X2,
    Reg::X3,
    Reg::X4,
    Reg::X5,
    Reg::X6,
    Reg::X7,
    Reg::X8,
    Reg::X9,
    Reg::X10,
    Reg::X11,
    Reg::X12,
    Reg::X13,
    Reg::X14,
    Reg::X15,
    Reg::X16,
    Reg::X30,
];
const SAFEPOINT_FRAME: u32 = 8 * (SAFEPOINT_SAVED.len() + FP_PINS.len()) as u32;
const _: () =
    assert!(SAFEPOINT_FRAME.is_multiple_of(16) && SAFEPOINT_SAVED.len().is_multiple_of(2) && SAFEPOINT_FRAME < 504);

/// Emits the entry trampoline, the trap stubs, the safepoint thunk and (retpoline mode) the retpoline thunk.
/// Returns the entry offset.
fn emit_prelude(a: &mut Asm, sh: &Shared, helpers: &[Label; NUM_HELPERS]) -> u32 {
    use Reg::*;
    a.align(16);
    let entry = a.pos();
    // ---- entry trampoline: x0 = ctx, x1 = target, x2 = args, x3 = nargs, x4 = out → w0 = status.
    // A frame record, the AAPCS64 callee-saved registers JIT code uses (it never touches x29 or d8-d15), the out
    // pointer and the previous saved sp (nested invocations on the same ctx); then publish our sp.
    a.stp_pre(X29, X30, Sp, -ENTRY_FRAME);
    a.mov_sp(X29, Sp);
    for (k, &(r1, r2)) in ENTRY_PAIRS.iter().enumerate() {
        a.stp(r1, r2, Sp, 16 * (k as i32 + 1));
    }
    a.ldst_imm(LdSt::LdrX, X9, X0, OFF_SAVED_RSP as u32);
    a.stp_pre(X4, X9, Sp, -16);
    a.mov_sp(X9, Sp);
    a.ldst_imm(LdSt::StrX, X9, X0, OFF_SAVED_RSP as u32);
    a.mov(CTX, X0);
    a.ldp(BASE, MASK, CTX, OFF_MEM_BASE);
    const _: () = assert!(OFF_MEM_MASK == OFF_MEM_BASE + 8);
    // Helper addresses for JIT code (`ldr x16, [x26, #OFF_HELPERS + 8h]`): any distance from any function.
    for (h, &l) in helpers.iter().enumerate() {
        a.ldr_lit(X9, l);
        a.ldst_imm(LdSt::StrX, X9, CTX, (OFF_HELPERS + 8 * h as i32) as u32);
    }
    // Outgoing argument area of round_up(8 * nargs, 16) bytes: argument i at [sp + 8i] when the function is
    // called, sp 16-byte aligned.
    a.lsl_imm(X9, X3, 3);
    a.add_imm(X9, X9, 15);
    a.logic_imm(Logic::And, X9, X9, !15);
    a.alu_ext(Alu::Sub, Sp, Sp, X9);
    let call = a.new_label();
    let copy = a.new_label();
    a.cbz(X3, call);
    a.bind(copy);
    a.sub_imm(X3, X3, 1);
    a.ldst_reg(LdSt::LdrX, X9, X2, X3, true);
    a.ldst_reg(LdSt::StrX, X9, Sp, X3, true);
    a.cbnz(X3, copy);
    a.bind(call);
    a.mov(TMP, X1);
    indirect(a, sh, true);
    // Normal return: x0 = result.
    a.ldst_imm(LdSt::LdrX, X9, CTX, OFF_SAVED_RSP as u32);
    a.mov_sp(Sp, X9);
    a.ldst_imm(LdSt::LdrX, X1, Sp, 0);
    a.ldst_imm(LdSt::StrX, X0, X1, 0);
    a.movz(X0, 0, 0);
    let exit = a.new_label();
    a.bind(exit);
    a.ldp_post(X4, X9, Sp, 16);
    a.ldst_imm(LdSt::StrX, X9, CTX, OFF_SAVED_RSP as u32);
    for (k, &(r1, r2)) in ENTRY_PAIRS.iter().enumerate().rev() {
        a.ldp(r1, r2, Sp, 16 * (k as i32 + 1));
    }
    a.ldp_post(X29, X30, Sp, ENTRY_FRAME);
    a.ret(X30);

    // ---- trap_common: w0 = trap code. Only JIT frames lie between here and the invoke frame.
    a.bind(sh.trap_common);
    a.ldst_imm(LdSt::LdrX, X9, CTX, OFF_SAVED_RSP as u32);
    a.mov_sp(Sp, X9);
    a.b(exit);
    for (l, code) in [(sh.trap_unreachable, 1), (sh.trap_divzero, 2), (sh.trap_overflow, 3)] {
        a.bind(l);
        a.movz(X0, code, 0);
        a.b(sh.trap_common);
    }
    // ---- host_trap: trap_request != 0 after a host call; the JIT clears it.
    a.bind(sh.host_trap);
    a.ldst_imm(LdSt::LdrW, X0, CTX, OFF_TRAP_REQUEST as u32);
    a.ldst_imm(LdSt::StrW, Xzr, CTX, OFF_TRAP_REQUEST as u32);
    a.b(sh.trap_common);

    // ---- safepoint thunk: x16 = prospective sp, entered by `bl`. Preserves every register but x17 and the flags.
    a.bind(sh.safepoint);
    a.ldst_imm(LdSt::LdrX, TMP2, CTX, OFF_REAL_STACK_LIMIT as u32);
    a.cmp(TMP, TMP2);
    a.b_cond(crate::a64::Cond::Lo, sh.trap_overflow);
    a.sub_imm(Sp, Sp, SAFEPOINT_FRAME);
    for (k, p) in SAFEPOINT_SAVED.chunks(2).enumerate() {
        a.stp(p[0], p[1], Sp, 16 * k as i32);
    }
    let fp_base = 8 * SAFEPOINT_SAVED.len() as i32;
    for (k, p) in FP_PINS.chunks(2).enumerate() {
        a.stp_d(VReg(p[0]), VReg(p[1]), Sp, fp_base + 16 * k as i32);
    }
    a.mov(X0, CTX);
    a.ldst_imm(LdSt::LdrX, TMP, CTX, OFF_INTERRUPT as u32);
    indirect(a, sh, true);
    a.cbnz_w(X0, sh.trap_common); // u32 result: only w0 is defined
    for (k, p) in FP_PINS.chunks(2).enumerate() {
        a.ldp_d(VReg(p[0]), VReg(p[1]), Sp, fp_base + 16 * k as i32);
    }
    for (k, p) in SAFEPOINT_SAVED.chunks(2).enumerate() {
        a.ldp(p[0], p[1], Sp, 16 * k as i32);
    }
    a.add_imm(Sp, Sp, SAFEPOINT_FRAME);
    a.ret(X30);

    // ---- retpoline thunk: `bl/b thunk` = `blr/br x16`; the `ret` is predicted to the speculation trap.
    if sh.indirect == IndirectBranch::Retpoline {
        a.align(16);
        a.bind(sh.retpoline);
        let set = a.new_label();
        let spin = a.new_label();
        a.mov(TMP2, X30);
        a.bl(set);
        a.bind(spin);
        spec_barrier(a, sh);
        a.b(spin);
        a.bind(set);
        a.mov(X30, TMP2);
        a.ret(TMP);
    }
    entry
}
