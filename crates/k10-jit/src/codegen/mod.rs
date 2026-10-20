//! TIR → x86-64 code generation (no_std + alloc). Module driver and the shared prelude; the per-function
//! single-pass compiler is in [`func`].

mod func;

use crate::abi::*;
use crate::blob::{BlobMeta, CodeBlob, FuncEntry};
use crate::cpu::CpuFeatures;
use crate::helpers::NUM_HELPERS;
use crate::x64::{Alu, Asm, AsmError, Cond, Label, Mem, Reg, Shift, Size, RM};
use alloc::vec::Vec;
use core::fmt;
use k10_ir::{validate, Module, ValidateError};

/// Code generation knobs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompileOptions {
    /// Optional instructions the code may use (default: [`CpuFeatures::detect`]).
    pub features: CpuFeatures,
    /// Maximum number of locals kept in registers per function, `0..=6` (default 6).
    pub max_pinned: u8,
    /// How generated code performs indirect branches (Spectre v2 hardening; default [`IndirectBranch::Jump`]).
    pub indirect: IndirectBranch,
    /// A `br_table` whose targets form at most this many runs (maximal ranges of consecutive indices with the same
    /// target, the default range included) is lowered to a binary tree of compares — no indirect jump at all;
    /// larger ones use a jump table (default [`DEFAULT_BR_TREE_MAX`]; 0 or 1: always a jump table, unless every
    /// index has the same target).
    pub br_tree_max: u16,
}

/// Default [`CompileOptions::br_tree_max`]: a tree of at most 3 levels of compares.
pub const DEFAULT_BR_TREE_MAX: u16 = 8;

impl Default for CompileOptions {
    fn default() -> Self {
        CompileOptions {
            features: CpuFeatures::detect(),
            max_pinned: func::MAX_PINNED as u8,
            indirect: IndirectBranch::Jump,
            br_tree_max: DEFAULT_BR_TREE_MAX,
        }
    }
}

/// How generated code performs its indirect branches: the `br_table` jump-table dispatch, host-function and helper
/// calls, the interrupt hook call and the entry trampoline's call. Only the branch *prediction* differs; every
/// mode computes the same target, and `br_table` indices are always clamped branch-free first (Spectre v1).
///
/// An indirect branch is predicted from the branch target buffer, which any code on the same core — another
/// workload included — can train: the CPU may then speculatively run an attacker-chosen target (Spectre v2,
/// branch target injection) with the victim's registers. docs/JIT-ABI.md lists the exact sequences.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum IndirectBranch {
    /// Plain `jmp reg` / `call [mem]` (default; no overhead).
    #[default]
    Jump,
    /// The target is loaded into `r11` and an `lfence` precedes `jmp/call r11`: the predicted target cannot run
    /// before the real one is known, which shrinks the window to the branch's own resolution (AMD's former
    /// recommendation; cheaper than a retpoline, but not a complete fix).
    Lfence,
    /// Retpoline: the branch goes through a shared thunk that turns it into a `ret` whose prediction (the return
    /// stack, pointing at a `pause; lfence` loop) never uses the branch target buffer. Costs a return
    /// misprediction (~20–30 cycles) per indirect branch.
    Retpoline,
}

impl IndirectBranch {
    /// `jump` | `lfence` | `retpoline`.
    pub fn parse(s: &str) -> Option<IndirectBranch> {
        match s {
            "jump" => Some(IndirectBranch::Jump),
            "lfence" => Some(IndirectBranch::Lfence),
            "retpoline" => Some(IndirectBranch::Retpoline),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            IndirectBranch::Jump => "jump",
            IndirectBranch::Lfence => "lfence",
            IndirectBranch::Retpoline => "retpoline",
        }
    }
}

/// Why a module could not be compiled.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompileError {
    /// The module is not valid TIR.
    Invalid(ValidateError),
    /// The generated code would exceed the blob size limit (1 GiB).
    TooLarge,
    /// Executable memory could not be allocated or protected (OS error code). `std` only.
    Memory(i32),
    /// An internal invariant of the code generator failed (a compiler bug); no code is produced.
    Internal(&'static str),
}

impl fmt::Display for CompileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CompileError::Invalid(e) => write!(f, "invalid module: {e}"),
            CompileError::TooLarge => write!(f, "generated code too large"),
            CompileError::Memory(e) => write!(f, "executable memory allocation failed (os error {e})"),
            CompileError::Internal(what) => write!(f, "internal code generator error: {what}"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for CompileError {}

/// Maximum blob size: keeps every rel32 displacement in range with a wide margin.
pub const MAX_BLOB_BYTES: usize = 1 << 30;

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
    /// The retpoline thunk (target in `r11`; reached by `call` or `jmp`). Bound only in retpoline mode.
    pub retpoline: Label,
}

/// Indirect `call` to the address in `target` with the configured [`IndirectBranch`] mode. Clobbers `r11` except
/// in `Jump` mode (the call then reads `target` directly).
pub(crate) fn indirect_call(a: &mut Asm, sh: &Shared, target: RM) {
    match sh.indirect {
        IndirectBranch::Jump => a.call_rm(target),
        IndirectBranch::Lfence => {
            load_r11(a, target);
            a.lfence();
            a.call_rm(RM::R(Reg::R11));
        }
        IndirectBranch::Retpoline => {
            load_r11(a, target);
            a.call(sh.retpoline);
        }
    }
}

/// `r11 ← target`.
pub(crate) fn load_r11(a: &mut Asm, target: RM) {
    match target {
        RM::R(Reg::R11) => {}
        RM::R(r) => a.mov_rr(Reg::R11, r),
        RM::M(m) => a.load(Size::S64, Reg::R11, m),
    }
}

/// Compiles a module to a position-independent [`CodeBlob`] (validates it first).
pub fn compile_blob(m: &Module, opts: &CompileOptions) -> Result<CodeBlob, CompileError> {
    let info = validate(m).map_err(CompileError::Invalid)?;
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
    indirect_call(a, sh, Rsi.into());
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
    indirect_call(a, sh, Mem::b(R14, OFF_INTERRUPT).into()); // r11 was saved above
    a.test32(Rax, Rax); // u32 result: only eax is defined
    a.jcc(Cond::Ne, sh.trap_common);
    a.alu_ri(Alu::Add, Rsp.into(), 8);
    for r in saved.iter().rev() {
        a.pop(*r);
    }
    a.mov_rr(Rsp, Rbp);
    a.pop(Rbp);
    a.ret();

    // ---- retpoline thunk: jumps to r11 (`call thunk` = `call r11`, `jmp thunk` = `jmp r11`) without consulting
    // the branch target buffer: the inner `call` pushes the address of a speculation trap (a `pause; lfence`
    // loop), which the return stack predicts for the `ret`; the `ret` really goes to r11 once the store executed.
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
