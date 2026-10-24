//! TIR → x86-64 code generation (no_std + alloc). Module driver and the shared prelude; the per-function
//! single-pass compiler is in [`func`].

mod func;

use super::{CompileError, CompileOptions, IndirectBranch};
use crate::abi::*;
use crate::blob::{BlobMeta, CodeBlob, FuncEntry};
use crate::helpers::NUM_HELPERS;
use crate::x64::{Alu, Asm, AsmError, Cond, Label, Mem, Reg, Shift, Size, RM};
use alloc::vec::Vec;
use k10_ir::{Module, ModuleInfo};

pub(super) use func::MAX_PINNED;

/// Maximum blob size: keeps every rel32 displacement in range with a wide margin.
pub(super) const MAX_BLOB_BYTES: usize = 1 << 30;

/// Module-level labels shared by all functions.
pub(crate) struct Shared {
    /// Defined function `i` → its entry label.
    pub funcs: Vec<Label>,
    /// `eax` = trap code; restores the `invoke` frame and returns the code.
    pub trap_common: Label,
    pub trap_unreachable: Label,
    pub trap_divzero: Label,
    pub trap_overflow: Label,
    /// After a host call with `trap_request != 0`.
    pub host_trap: Label,
    /// Safepoint slow path: `rax` = prospective `rsp`; preserves every register when it returns.
    pub safepoint: Label,
    /// Helper address table entries.
    pub helpers: [Label; NUM_HELPERS],
    /// `(params, results)` of each import.
    pub imports: Vec<(u8, u8)>,
    pub indirect: IndirectBranch,
    /// Retpoline thunk (target in `r11`, entered by `call` or `jmp`); bound in retpoline mode only.
    pub retpoline: Label,
}

/// An indirect transfer: a call through a register (never `r11`) or memory, or a jump-table jump to `r + r11`.
#[derive(Clone, Copy)]
pub(crate) enum Indirect {
    Call(RM),
    TableJump(Reg),
}

/// Emits `x` in the configured [`IndirectBranch`] form; the hardened forms go through `r11`.
pub(crate) fn indirect(a: &mut Asm, sh: &Shared, x: Indirect) {
    use Reg::R11;
    match (sh.indirect, x) {
        (IndirectBranch::Jump, Indirect::Call(t)) => a.call_rm(t),
        (IndirectBranch::Jump, Indirect::TableJump(r)) => {
            a.alu(Alu::Add, r, RM::R(R11));
            a.jmp_rm(RM::R(r));
        }
        (mode, x) => {
            match x {
                Indirect::Call(RM::R(r)) => a.mov_rr(R11, r),
                Indirect::Call(RM::M(m)) => a.load(Size::S64, R11, m),
                Indirect::TableJump(r) => a.alu(Alu::Add, R11, RM::R(r)),
            }
            match (mode, x) {
                (IndirectBranch::Retpoline, Indirect::Call(_)) => a.call(sh.retpoline),
                (IndirectBranch::Retpoline, Indirect::TableJump(_)) => a.jmp(sh.retpoline),
                (_, Indirect::Call(_)) => {
                    a.lfence();
                    a.call_rm(RM::R(R11));
                }
                (_, Indirect::TableJump(_)) => {
                    a.lfence();
                    a.jmp_rm(RM::R(R11));
                }
            }
        }
    }
}

/// Compiles a validated module (`info` from `k10_ir::validate`) to a position-independent [`CodeBlob`].
pub(super) fn compile(m: &Module, info: &ModuleInfo, opts: &CompileOptions) -> Result<CodeBlob, CompileError> {
    let tir_bytes: usize = info.funcs.iter().map(|f| f.code_bytes as usize).sum();
    let mut a = Asm::with_capacity(512 + tir_bytes * 6);
    let mut sh = Shared {
        funcs: Vec::with_capacity(m.funcs.len()),
        trap_common: a.new_label(),
        trap_unreachable: a.new_label(),
        trap_divzero: a.new_label(),
        trap_overflow: a.new_label(),
        host_trap: a.new_label(),
        safepoint: a.new_label(),
        helpers: [a.new_label(), a.new_label()],
        imports: m.imports.iter().map(|i| (i.params, i.results)).collect(),
        indirect: opts.indirect,
        retpoline: a.new_label(),
    };
    for _ in 0..m.funcs.len() {
        let l = a.new_label();
        sh.funcs.push(l);
    }
    let entry = emit_prelude(&mut a, &sh);
    let mut scratch = func::Scratch::default();
    let mut funcs = Vec::with_capacity(m.funcs.len());
    for (i, (f, fi)) in m.funcs.iter().zip(&info.funcs).enumerate() {
        a.align(16, 0xCC);
        a.bind(sh.funcs[i]);
        funcs.push(FuncEntry { offset: a.pos(), params: f.params, results: f.results });
        func::compile_func(&mut a, &sh, m, f, fi, opts, &mut scratch);
        if a.pos() as usize > MAX_BLOB_BYTES {
            return Err(CompileError::TooLarge);
        }
    }
    a.align(8, 0xCC);
    let helper_table = a.pos();
    for l in sh.helpers {
        a.bind(l);
        a.imm64(0);
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

/// Emits the entry trampoline, the trap stubs and the safepoint thunk. Returns the entry offset.
fn emit_prelude(a: &mut Asm, sh: &Shared) -> u32 {
    use Reg::*;
    let entry = a.pos();
    // ---- entry trampoline: rdi = ctx, rsi = target, rdx = args, rcx = nargs, r8 = out → eax = status.
    // Save the SysV callee-saved registers (JIT code clobbers all of them except r13-r15 semantics), the out
    // pointer and the previous saved rsp (nested invocations on the same ctx), then publish our rsp.
    for r in [Rbp, Rbx, R12, R13, R14, R15] {
        a.push(r);
    }
    a.push(R8);
    // Entry rsp ≡ 8 (mod 16) (SysV); after these 8 pushes the anchor rsp ≡ 8 (mod 16).
    a.push_m(Mem::b(Rdi, OFF_SAVED_RSP));
    a.store(Size::S64, Mem::b(Rdi, OFF_SAVED_RSP), Rsp);
    a.mov_rr(R14, Rdi);
    a.load(Size::S64, R15, Mem::b(R14, OFF_MEM_BASE));
    a.load(Size::S64, R13, Mem::b(R14, OFF_MEM_MASK));
    // Outgoing argument area of A = (8 * nargs) | 8 bytes (A ≡ 8 mod 16, A ≥ 8 * nargs): the JIT convention is
    // rsp ≡ 0 (mod 16) at a call instruction with argument i at [rsp + 8i].
    a.mov_rr(Rax, Rcx);
    a.shift_ri(Shift::Shl, Rax.into(), 3);
    a.alu_ri(Alu::Or, Rax.into(), 8);
    a.alu(Alu::Sub, Rsp, Rax.into());
    let call = a.new_label();
    let copy = a.new_label();
    a.test_rr(Rcx.into(), Rcx);
    a.jcc(Cond::E, call);
    a.bind(copy);
    a.load(Size::S64, Rax, Mem::bi(Rdx, Rcx, 3, -8));
    a.store(Size::S64, Mem::bi(Rsp, Rcx, 3, -8), Rax);
    a.alu_ri(Alu::Sub, Rcx.into(), 1);
    a.jcc(Cond::Ne, copy);
    a.bind(call);
    indirect(a, sh, Indirect::Call(Rsi.into()));
    // Normal return: rax = result.
    a.load(Size::S64, Rsp, Mem::b(R14, OFF_SAVED_RSP));
    a.load(Size::S64, Rcx, Mem::b(Rsp, 8));
    a.store(Size::S64, Mem::b(Rcx, 0), Rax);
    a.zero(Rax);
    let exit = a.new_label();
    a.bind(exit);
    a.pop(Rcx);
    a.store(Size::S64, Mem::b(R14, OFF_SAVED_RSP), Rcx);
    a.pop(Rcx);
    for r in [R15, R14, R13, R12, Rbx, Rbp] {
        a.pop(r);
    }
    a.ret();

    // ---- trap_common: eax = trap code. Only JIT frames lie between here and the invoke frame.
    a.bind(sh.trap_common);
    a.load(Size::S64, Rsp, Mem::b(R14, OFF_SAVED_RSP));
    a.jmp(exit);
    for (l, code) in [(sh.trap_unreachable, 1), (sh.trap_divzero, 2), (sh.trap_overflow, 3)] {
        a.bind(l);
        a.mov_ri(Rax, code);
        a.jmp(sh.trap_common);
    }
    // ---- host_trap: trap_request != 0 after a host call; the JIT clears it.
    a.bind(sh.host_trap);
    a.mov32(Rax, Mem::b(R14, OFF_TRAP_REQUEST).into());
    a.store_imm(Size::S32, Mem::b(R14, OFF_TRAP_REQUEST), 0);
    a.jmp(sh.trap_common);

    // ---- safepoint thunk: rax = prospective rsp. Preserves all registers when it returns.
    a.bind(sh.safepoint);
    a.alu(Alu::Cmp, Rax, Mem::b(R14, OFF_REAL_STACK_LIMIT).into());
    a.jcc(Cond::B, sh.trap_overflow);
    a.push(Rbp);
    a.mov_rr(Rbp, Rsp);
    a.alu_ri(Alu::And, Rsp.into(), -16);
    let saved = [Rax, Rcx, Rdx, Rsi, Rdi, R8, R9, R10, R11];
    for r in saved {
        a.push(r);
    }
    a.alu_ri(Alu::Sub, Rsp.into(), 8); // 9 pushes + 8 = 80 bytes: aligned
    a.mov_rr(Rdi, R14);
    indirect(a, sh, Indirect::Call(Mem::b(R14, OFF_INTERRUPT).into())); // r11 saved above
    a.test32(Rax, Rax); // u32 result: only eax is defined
    a.jcc(Cond::Ne, sh.trap_common);
    a.alu_ri(Alu::Add, Rsp.into(), 8);
    for r in saved.iter().rev() {
        a.pop(*r);
    }
    a.mov_rr(Rsp, Rbp);
    a.pop(Rbp);
    a.ret();

    // ---- retpoline thunk: `call/jmp thunk` = `call/jmp r11`; the `ret` is predicted to the pause/lfence trap.
    if sh.indirect == IndirectBranch::Retpoline {
        a.align(16, 0xCC);
        a.bind(sh.retpoline);
        let set = a.new_label();
        let spin = a.new_label();
        a.call(set);
        a.bind(spin);
        a.pause();
        a.lfence();
        a.jmp(spin);
        a.bind(set);
        a.store(Size::S64, Mem::b(Rsp, 0), R11);
        a.ret();
    }
    entry
}
