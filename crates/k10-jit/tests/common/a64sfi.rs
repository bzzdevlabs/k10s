//! SFI verifier for k10-jit's AArch64 output (docs/JIT-ABI.md), independent of the code generator: it decodes each
//! function with a64dec.rs (itself checked against objdump by tests/sfi_a64.rs) and checks the isolation rules
//! given the module (arities, imports, memory size), the blob's function offsets and the indirect-branch mode,
//! rejecting anything it does not recognize. Register facts hold on straight-line paths only (reset at branch
//! targets and calls) and come from the instructions that write the register.
//! * Instructions: the decoder's set only; `udf #0` only as padding after a function's code.
//! * Registers: x26 (vmctx), x27 (heap base), x28 (mask), x29 and x18 are never written; x30 only by `bl`/`blr`
//!   and the epilogue's reload; v8-v15 (AAPCS64 callee-saved d8-d15) never written.
//! * Stack: prologue `str x30,[sp,#-16]!`, `x16 = sp - F` checked against vmctx.stack_limit (`b.lo` to a stub
//!   `bl safepoint; b back`), then `mov sp,x16`; sp is written nowhere else but the epilogue
//!   `add sp,sp,#F; ldr x30,[sp],#16; ret` (no branch target after its first instruction).
//! * Guest memory: `[x27, Xm]` with Xm from `and Xm,Xn,x28`, or a constant c (immediate or register) with
//!   0 <= c < 2^min(min_mem_log2,31); single registers only (≤ 8 bytes), no writeback, no scaled index.
//! * Frame: `[sp+d]` or `[Xr+d]` (Xr = sp + k) within [0,F) or the parameters [F+16,F+16+8*params), pairs too;
//!   the prologue's zeroing loop (`stp xzr,xzr,[x16],#16` counted by x17) within [0,F).
//! * vmctx: loads of stack_limit, host_fns, trap_request and the helper slots only.
//! * Calls: `bl` to function entries, or to the safepoint thunk from the stubs; host/helper calls in the mode's
//!   form (`blr x16`, barrier + `blr x16`, `bl thunk`) with x16 loaded from host_fns[k < imports] or a helper slot.
//! * Branches: to instructions of the function or to the prelude's trap stubs; no fall-through past the code.
//! * Jump tables: `cmp R,x16` (x16 = n), `csel R,R,x16,lo`, `csdb`, `adr x16,T`, `ldrsw E,[x16,R,lsl #2]`,
//!   `add x16,x16,E`, the mode's jump (`br x16`, barrier + `br x16`, `b thunk`), then the n + 1 entries of T, each
//!   targeting an instruction of the function.
//!
//! Trusted: the prelude (identical for every blob of a mode and barrier form; its stubs located by pattern) and
//! the bulk-memory helpers.
#![allow(dead_code)]

use super::a64dec::{decode, xn, BrKind, Form, Insn, SP, ZR};
use k10_ir::Module;
use k10_jit::{CodeBlob, IndirectBranch};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, OnceLock};

const CTX: u8 = 26;
const BASE: u8 = 27;
const MASK: u8 = 28;
const OFF_STACK_LIMIT: i64 = 16;
const OFF_HOST_FNS: i64 = 32;
const OFF_TRAP_REQUEST: i64 = 48;
const OFF_HELPERS: i64 = 72;
const LO: u8 = 3;
const NE: u8 = 1;
const _: () = {
    use std::mem::offset_of;
    assert!(offset_of!(k10_jit::VmCtx, stack_limit) == OFF_STACK_LIMIT as usize);
    assert!(offset_of!(k10_jit::VmCtx, host_fns) == OFF_HOST_FNS as usize);
    assert!(offset_of!(k10_jit::VmCtx, trap_request) == OFF_TRAP_REQUEST as usize);
    assert!(offset_of!(k10_jit::VmCtx, jit_private) + 8 == OFF_HELPERS as usize);
};

/// What the verifier knows about a blob.
#[derive(Clone, Debug)]
pub struct Layout {
    /// `(offset, params)` of every defined function, in order.
    pub funcs: Vec<(u32, u8)>,
    /// Offset of the entry trampoline (the prelude runs from here to the first function).
    pub entry: u32,
    pub num_helpers: usize,
    pub num_imports: usize,
    pub min_mem_log2: u8,
    pub mode: IndirectBranch,
    /// The blob was compiled for FEAT_SB (keys the prelude identity check).
    pub sb: bool,
}

/// Layout of `m` compiled to `b` in `mode`.
pub fn layout(m: &Module, b: &CodeBlob, mode: IndirectBranch) -> Layout {
    let meta = b.meta();
    assert_eq!(meta.funcs().len(), m.funcs.len());
    Layout {
        funcs: meta.funcs().iter().zip(&m.funcs).map(|(e, f)| (e.offset, f.params)).collect(),
        entry: meta.entry_offset(),
        num_helpers: meta.num_helpers(),
        num_imports: m.imports.len(),
        min_mem_log2: m.min_mem_log2,
        mode,
        sb: meta.features().sb,
    }
}

#[derive(Default, Debug, Clone, Copy)]
pub struct Stats {
    pub modules: u64,
    pub functions: u64,
    pub instructions: u64,
    pub heap_masked: u64,
    pub heap_const: u64,
    pub frame: u64,
    pub vmctx: u64,
    pub host_calls: u64,
    pub helper_calls: u64,
    pub tables: u64,
    pub table_entries: u64,
    pub direct_branches: u64,
    /// Indirect calls/jumps behind a speculation barrier or through the retpoline thunk.
    pub hardened: u64,
}

impl Stats {
    pub fn add(&mut self, o: &Stats) {
        self.modules += o.modules;
        self.functions += o.functions;
        self.instructions += o.instructions;
        self.heap_masked += o.heap_masked;
        self.heap_const += o.heap_const;
        self.frame += o.frame;
        self.vmctx += o.vmctx;
        self.host_calls += o.host_calls;
        self.helper_calls += o.helper_calls;
        self.tables += o.tables;
        self.table_entries += o.table_entries;
        self.direct_branches += o.direct_branches;
        self.hardened += o.hardened;
    }
}

/// Verifies `m` compiled to `b` (code as generated, or as loaded: the helper table is not inspected).
pub fn verify_blob(m: &Module, b: &CodeBlob, mode: IndirectBranch) -> Result<Stats, String> {
    verify(b.code(), &layout(m, b, mode))
}

/// Verifies a loaded module (the code in its executable mapping).
pub fn verify_loaded(m: &Module, jit: &k10_jit::CompiledModule, mode: IndirectBranch) -> Result<Stats, String> {
    // SAFETY: the mapping is readable and `code_bytes` long for the module's lifetime.
    let code = unsafe { std::slice::from_raw_parts(jit.code_ptr(), jit.code_bytes()) };
    let meta = jit.meta();
    let l = Layout {
        funcs: meta.funcs().iter().zip(&m.funcs).map(|(e, f)| (e.offset, f.params)).collect(),
        entry: meta.entry_offset(),
        num_helpers: meta.num_helpers(),
        num_imports: m.imports.len(),
        min_mem_log2: m.min_mem_log2,
        mode,
        sb: meta.features().sb,
    };
    verify(code, &l)
}

/// Addresses of the prelude's stubs.
#[derive(Clone, Copy, Debug, Default)]
struct Prelude {
    trap_common: u64,
    trap_unreachable: u64,
    trap_divzero: u64,
    trap_overflow: u64,
    host_trap: u64,
    safepoint: u64,
    retpoline: Option<u64>,
}

fn word(code: &[u8], pc: u64) -> u32 {
    let p = pc as usize;
    u32::from_le_bytes(code[p..p + 4].try_into().unwrap())
}

/// Decodes the prelude, locates its stubs and checks that it is identical to every other prelude seen for the
/// same mode and barrier form.
fn prelude(code: &[u8], l: &Layout, end: u64) -> Result<Prelude, String> {
    let start = l.entry as u64;
    if start % 16 != 0 || start < 8 * l.num_helpers as u64 || end > code.len() as u64 || start > end {
        return Err(format!("prelude bounds {start:#x}..{end:#x}"));
    }
    static SEEN: OnceLock<Mutex<HashMap<(IndirectBranch, bool), Vec<u8>>>> = OnceLock::new();
    let bytes = &code[start as usize..end as usize];
    {
        let mut seen = SEEN.get_or_init(Default::default).lock().unwrap();
        let reference = seen.entry((l.mode, l.sb)).or_insert_with(|| bytes.to_vec());
        if reference[..] != bytes[..] {
            return Err(format!("prelude differs from the first {:?} prelude (sb {})", l.mode, l.sb));
        }
    }
    let mut insns = Vec::new();
    let mut pc = start;
    while pc < end {
        let w = word(code, pc);
        let i = decode(w).ok_or_else(|| format!("prelude {pc:#x}: undecodable {w:#010x}"))?;
        insns.push((pc, i));
        pc += 4;
    }
    let mut p = Prelude::default();
    for (k, &(pc, i)) in insns.iter().enumerate() {
        let next = insns.get(k + 1).map(|&(_, n)| n);
        match (i, next) {
            (Insn::MovWide { opc: 2, rd: 0, imm16, hw: 0 }, Some(Insn::B { link: false, off }))
                if (1..=3).contains(&imm16) =>
            {
                let common = (pc as i64 + 4 + off) as u64;
                if p.trap_common != 0 && p.trap_common != common {
                    return Err("prelude: trap stubs disagree on trap_common".into());
                }
                p.trap_common = common;
                *[&mut p.trap_unreachable, &mut p.trap_divzero, &mut p.trap_overflow][imm16 as usize - 1] = pc;
            }
            (Insn::LdSt { size: 2, opc: 1, rt: 0, rn: CTX, off: OFF_TRAP_REQUEST, form: Form::Scaled }, _) => {
                p.host_trap = pc
            }
            (Insn::LdSt { size: 3, opc: 1, rt: 17, rn: CTX, off: 24, form: Form::Scaled }, _) => p.safepoint = pc,
            (Insn::LogReg { opc: 1, neg: false, rd: 17, rn: ZR, rm: 30, shift: 0, amount: 0 }, _) => {
                p.retpoline = Some(pc)
            }
            _ => {}
        }
    }
    if [p.trap_common, p.trap_unreachable, p.trap_divzero, p.trap_overflow, p.host_trap, p.safepoint].contains(&0) {
        return Err(format!("prelude: stubs not found ({p:?})"));
    }
    if p.retpoline.is_some() != (l.mode == IndirectBranch::Retpoline) {
        return Err("prelude: retpoline thunk present iff retpoline mode".into());
    }
    Ok(p)
}

/// A register fact.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum F {
    None,
    /// The result of `and Xr, Xn, x28`.
    Masked,
    Const(u64),
    /// `sp + k` (sp as fixed in the function body).
    SpPlus(i64),
    HostFns,
    /// A host function or helper address (a legitimate call target).
    Callee { helper: bool },
    /// The address of the jump table at T.
    Table(u64),
    /// An index `<= n`; `csdb` seen after the clamp.
    Clamped { n: u64, csdb: bool },
    /// An entry of table T (n + 1 entries).
    Entry { t: u64, n: u64 },
    /// T + entry: a jump-table target.
    Target { t: u64, n: u64 },
}

#[derive(Clone, Copy)]
struct Facts {
    r: [F; 33],
    /// `cmp R, x16` with x16 = n.
    cmp: Option<(u8, u64)>,
}

impl Facts {
    fn new() -> Facts {
        Facts { r: [F::None; 33], cmp: None }
    }
}

/// GPRs an instruction writes (`SP` included; `ZR` dropped), and whether it sets the flags.
fn effects(i: &Insn) -> ([Option<u8>; 3], bool) {
    let w = |r: u8| if r == ZR { None } else { Some(r) };
    match *i {
        Insn::AddImm { s, rd, .. } | Insn::AddReg { s, rd, .. } | Insn::AddExt { s, rd, .. } => ([w(rd), None, None], s),
        Insn::LogImm { opc, rd, .. } | Insn::LogReg { opc, rd, .. } => ([w(rd), None, None], opc == 3),
        Insn::MovWide { rd, .. }
        | Insn::Bitfield { rd, .. }
        | Insn::Extr { rd, .. }
        | Insn::Dp2 { rd, .. }
        | Insn::Dp1 { rd, .. }
        | Insn::Dp3 { rd, .. }
        | Insn::CondSel { rd, .. }
        | Insn::Adr { rd, .. }
        | Insn::FmovFromD { rd, .. }
        | Insn::FmovFromS { rd, .. }
        | Insn::Fcvtzs { rd, .. } => ([w(rd), None, None], false),
        Insn::LdSt { opc, rt, rn, form, .. } => {
            let t = if opc != 0 { w(rt) } else { None };
            let b = if matches!(form, Form::Pre | Form::Post) { Some(rn) } else { None };
            ([t, b, None], false)
        }
        Insn::LdStReg { opc, rt, .. } => ([if opc != 0 { w(rt) } else { None }, None, None], false),
        Insn::LdrLit { rt, .. } => ([w(rt), None, None], false),
        Insn::Pair { load, rt, rt2, rn, form, .. } => {
            let b = if matches!(form, Form::Pre | Form::Post) { Some(rn) } else { None };
            if load {
                ([w(rt), w(rt2), b], false)
            } else {
                ([b, None, None], false)
            }
        }
        Insn::B { link: true, .. } | Insn::BrReg { kind: BrKind::Blr, .. } => ([Some(30), None, None], false),
        Insn::Fcmp { .. } => ([None, None, None], true),
        _ => ([None, None, None], false),
    }
}

/// Vector register an instruction writes.
fn vwrite(i: &Insn) -> Option<u8> {
    match *i {
        Insn::FmovToD { vd, .. }
        | Insn::Fp { vd, .. }
        | Insn::Fsqrt { vd, .. }
        | Insn::Fneg { vd, .. }
        | Insn::FmovDD { vd, .. }
        | Insn::Fabs { vd, .. }
        | Insn::LdStD { load: true, vt: vd, .. }
        | Insn::LdStDReg { load: true, vt: vd, .. }
        | Insn::Scvtf { vd, .. }
        | Insn::Cnt8b { vd, .. }
        | Insn::Addv8b { vd, .. } => Some(vd),
        _ => None,
    }
}

fn is_mem(i: &Insn) -> bool {
    matches!(
        i,
        Insn::LdSt { .. }
            | Insn::LdStReg { .. }
            | Insn::Pair { .. }
            | Insn::PairD { .. }
            | Insn::LdStD { .. }
            | Insn::LdStDReg { .. }
            | Insn::LdrLit { .. }
    )
}

fn is_barrier_end(insns: &[(u64, Insn)], k: usize) -> bool {
    match k.checked_sub(1).map(|j| insns[j].1) {
        Some(Insn::Sb) => true,
        Some(Insn::Isb) => k >= 2 && insns[k - 2].1 == Insn::Dsb { crm: 0x7 },
        _ => false,
    }
}

/// Verifies one blob; `code` holds the whole blob.
pub fn verify(code: &[u8], l: &Layout) -> Result<Stats, String> {
    let len = code.len() as u64;
    let first = l.funcs.first().map_or(len, |f| f.0 as u64);
    let pre = prelude(code, l, first)?;
    let entries: Vec<u64> = l.funcs.iter().map(|f| f.0 as u64).collect();
    let mut st = Stats { modules: 1, ..Stats::default() };
    for (fi, &(start, params)) in l.funcs.iter().enumerate() {
        let start = start as u64;
        let end = l.funcs.get(fi + 1).map_or(len, |f| f.0 as u64);
        verify_func(code, l, &pre, &entries, start, end, params, &mut st).map_err(|e| {
            let listing =
                if std::env::var_os("K10_SFI_LISTING").is_some() { listing(code, start, end) } else { String::new() };
            format!("function {fi} ({start:#x}..{end:#x}): {e}{listing}")
        })?;
        st.functions += 1;
    }
    Ok(st)
}

/// The decoder's text of `[start, end)` (`K10_SFI_LISTING=1` appends it to verifier errors).
pub fn listing(code: &[u8], start: u64, end: u64) -> String {
    let mut s = String::from("\n");
    let mut pc = start;
    while pc < end {
        let w = word(code, pc);
        let t = decode(w).map_or_else(|| "?".to_string(), |i| i.text(pc));
        s.push_str(&format!("  {pc:6x}: {w:08x}  {t}\n"));
        pc += 4;
    }
    s
}

/// Linear decoding of one function: instructions, jump tables (located by their jump sequence) and padding.
struct Decoded {
    insns: Vec<(u64, Insn)>,
    index: HashMap<u64, usize>,
    /// Jump table at T: n + 1 entry targets.
    tables: BTreeMap<u64, Vec<u64>>,
}

fn decode_func(code: &[u8], l: &Layout, pre: &Prelude, start: u64, end: u64) -> Result<Decoded, String> {
    let mut d = Decoded { insns: Vec::new(), index: HashMap::new(), tables: BTreeMap::new() };
    let mut facts = Facts::new(); // linear facts: only to size tables, re-checked with resets later
    let mut pc = start;
    while pc < end {
        let w = word(code, pc);
        let i = decode(w).ok_or_else(|| format!("{pc:#x}: undecodable {w:#010x}"))?;
        if let Insn::Udf { .. } = i {
            if w != 0 || code[pc as usize..end as usize].iter().any(|&b| b != 0) || pc % 4 != 0 {
                return Err(format!("{pc:#x}: udf other than trailing padding"));
            }
            break;
        }
        if let (Insn::Dp3 { .. }, Some((_, prev))) = (i, d.insns.last()) {
            if is_mem(prev) {
                return Err(format!("{pc:#x}: multiply-accumulate right after a load or store (Cortex-A53 835769)"));
            }
        }
        d.index.insert(pc, d.insns.len());
        d.insns.push((pc, i));
        let table_jump = match i {
            Insn::BrReg { kind: BrKind::Br, rn: 16 } => true,
            Insn::B { link: false, off } => Some((pc as i64 + off) as u64) == pre.retpoline,
            _ => false,
        };
        // Table sizing does not require the clamp to be speculation-safe (the checks with resets do): a `cmp`
        // bound or an unhardened clamp sizes the table too, so a missing `csel`/`csdb` is reported as such.
        let loose = match i {
            Insn::LdStReg { size: 2, opc: 2, rt, rn: 16, rm, scaled: true } => {
                match (facts.r[16], facts.r[rm as usize], facts.cmp) {
                    (F::Table(t), F::Clamped { n, .. }, _) => Some((rt, F::Entry { t, n })),
                    (F::Table(t), _, Some((r, n))) if r == rm => Some((rt, F::Entry { t, n })),
                    _ => None,
                }
            }
            _ => None,
        };
        transfer(&mut facts, &i, pc, l);
        if let Some((rt, e)) = loose {
            facts.r[rt as usize] = e;
        }
        pc += 4;
        if table_jump {
            if let F::Target { t, n } = facts.r[16] {
                if t != pc || n > k10_ir::MAX_CODE_BYTES as u64 || t + 4 * (n + 1) > end {
                    return Err(format!("{:#x}: jump table at {t:#x} (n {n}) not right after its jump", pc - 4));
                }
                let mut targets = Vec::with_capacity(n as usize + 1);
                for k in 0..=n {
                    let e = i32::from_le_bytes(code[(t + 4 * k) as usize..(t + 4 * k + 4) as usize].try_into().unwrap());
                    targets.push((t as i64 + e as i64) as u64);
                }
                d.tables.insert(t, targets);
                pc = t + 4 * (n + 1);
                facts = Facts::new();
            }
        }
    }
    Ok(d)
}

/// Fact transfer of one instruction (no checks).
fn transfer(f: &mut Facts, i: &Insn, pc: u64, l: &Layout) {
    let (writes, flags) = effects(i);
    let mut new: Option<(u8, F)> = None;
    let get = |f: &Facts, r: u8| if r == ZR { F::Const(0) } else { f.r[r as usize] };
    match *i {
        Insn::AddImm { sub, s: false, rd, rn, imm, .. } if rd != SP => {
            let d = if sub { -(imm as i64) } else { imm as i64 };
            let v = match (rn, get(f, rn)) {
                (SP, _) => F::SpPlus(d),
                (_, F::SpPlus(k)) => F::SpPlus(k + d),
                (_, F::Const(c)) => F::Const(c.wrapping_add(d as u64)),
                _ => F::None,
            };
            new = Some((rd, v));
        }
        Insn::AddReg { sub: false, s: false, rd: 16, rn: 16, rm, shift: 0, amount: 0 } => {
            if let (F::Table(t), F::Entry { t: te, n }) = (get(f, 16), get(f, rm)) {
                if t == te {
                    new = Some((16, F::Target { t, n }));
                }
            }
        }
        Insn::AddExt { sub: true, s: true, rd: 16, rn: SP, rm: 16 } => {
            if let F::Const(c) = get(f, 16) {
                new = Some((16, F::SpPlus(-(c as i64))));
            }
        }
        Insn::LogImm { opc: 1, rd, rn: ZR, imm } => new = Some((rd, F::Const(imm))),
        Insn::LogReg { opc: 0, neg: false, rd, rm: MASK, shift: 0, amount: 0, .. } => new = Some((rd, F::Masked)),
        Insn::LogReg { opc: 1, neg: false, rd, rn: ZR, rm, shift: 0, amount: 0 } => new = Some((rd, get(f, rm))),
        Insn::MovWide { opc, rd, imm16, hw } => {
            let v = (imm16 as u64) << (16 * hw);
            new = Some((
                rd,
                match (opc, get(f, rd)) {
                    (0, _) => F::Const(!v),
                    (2, _) => F::Const(v),
                    (_, F::Const(c)) => F::Const((c & !(0xFFFFu64 << (16 * hw))) | v),
                    _ => F::None,
                },
            ));
        }
        Insn::CondSel { inc: false, rd, rn, rm: 16, cond: LO } if rd == rn => {
            if let (Some((r, n)), F::Const(c)) = (f.cmp, get(f, 16)) {
                if r == rd && c == n {
                    new = Some((rd, F::Clamped { n, csdb: false }));
                }
            }
        }
        Insn::Csdb => {
            for v in f.r.iter_mut() {
                if let F::Clamped { n, .. } = *v {
                    *v = F::Clamped { n, csdb: true };
                }
            }
        }
        Insn::Adr { rd, off } => new = Some((rd, F::Table((pc as i64 + off) as u64))),
        Insn::LdSt { size: 3, opc: 1, rt: 16, rn: CTX, off, form: Form::Scaled } => {
            let h = (off - OFF_HELPERS) / 8;
            if off == OFF_HOST_FNS {
                new = Some((16, F::HostFns));
            } else if off >= OFF_HELPERS && (off - OFF_HELPERS) % 8 == 0 && (h as usize) < l.num_helpers {
                new = Some((16, F::Callee { helper: true }));
            }
        }
        Insn::LdSt { size: 3, opc: 1, rt: 16, rn: 16, off, form: Form::Scaled } => {
            if get(f, 16) == F::HostFns && off % 8 == 0 && ((off / 8) as usize) < l.num_imports {
                new = Some((16, F::Callee { helper: false }));
            }
        }
        Insn::LdStReg { size: 3, opc: 1, rt: 16, rn: 16, rm: 17, scaled: false } => {
            if let (F::HostFns, F::Const(c)) = (get(f, 16), get(f, 17)) {
                if c % 8 == 0 && ((c / 8) as usize) < l.num_imports {
                    new = Some((16, F::Callee { helper: false }));
                }
            }
        }
        Insn::LdStReg { size: 2, opc: 2, rt, rn: 16, rm, scaled: true } => {
            if let (F::Table(t), F::Clamped { n, csdb: true }) = (get(f, 16), get(f, rm)) {
                new = Some((rt, F::Entry { t, n }));
            }
        }
        _ => {}
    }
    for r in writes.into_iter().flatten() {
        if r != SP {
            f.r[r as usize] = F::None;
        }
    }
    if flags {
        f.cmp = match *i {
            Insn::AddReg { sub: true, s: true, rd: ZR, rn, rm: 16, shift: 0, amount: 0 } => match get(f, 16) {
                F::Const(n) => Some((rn, n)),
                _ => None,
            },
            _ => None,
        };
    }
    if let Some((r, v)) = new {
        if r != SP && r != ZR {
            f.r[r as usize] = v;
        }
    }
    if matches!(i, Insn::B { link: true, .. } | Insn::BrReg { kind: BrKind::Blr, .. }) {
        *f = Facts::new();
    }
}

#[allow(clippy::too_many_arguments)]
fn verify_func(
    code: &[u8],
    l: &Layout,
    pre: &Prelude,
    entries: &[u64],
    start: u64,
    end: u64,
    params: u8,
    st: &mut Stats,
) -> Result<(), String> {
    let d = decode_func(code, l, pre, start, end)?;
    let insns = &d.insns;
    if insns.is_empty() {
        return Err("no code".into());
    }
    let traps = [pre.trap_common, pre.trap_unreachable, pre.trap_divzero, pre.trap_overflow, pre.host_trap];
    // Reachable code from the entry (fall-through, call return sites, in-function branches, table entries) and the
    // branch sources among it; only reachable code is checked (dead code after e.g. `block; return; end` never runs).
    let mut reach = vec![false; insns.len()];
    let mut targets: HashMap<u64, Vec<u64>> = HashMap::new();
    let mut work = vec![0usize];
    while let Some(k) = work.pop() {
        if std::mem::replace(&mut reach[k], true) {
            continue;
        }
        let (pc, i) = insns[k];
        let mut branch = |t: u64, src: u64, work: &mut Vec<usize>| -> Result<(), String> {
            match d.index.get(&t) {
                Some(&j) => {
                    targets.entry(t).or_default().push(src);
                    work.push(j);
                    Ok(())
                }
                None if traps.contains(&t) => Ok(()),
                None => Err(format!("{src:#x}: branch to {t:#x}, outside the function's instructions")),
            }
        };
        let table = match i {
            Insn::BrReg { kind: BrKind::Br, .. } => d.tables.get(&(pc + 4)),
            Insn::B { link: false, off } if Some((pc as i64 + off) as u64) == pre.retpoline => d.tables.get(&(pc + 4)),
            _ => None,
        };
        if let Some(entries_t) = table {
            for &e in entries_t {
                branch(e, pc + 4, &mut work)?;
            }
            st.tables += 1;
            st.table_entries += entries_t.len() as u64;
            continue;
        }
        match i {
            Insn::B { link: false, off } => {
                branch((pc as i64 + off) as u64, pc, &mut work)?;
                st.direct_branches += 1;
                continue;
            }
            Insn::BCond { off, .. } | Insn::Cbz { off, .. } => {
                branch((pc as i64 + off) as u64, pc, &mut work)?;
                st.direct_branches += 1;
            }
            Insn::BrReg { kind: BrKind::Br | BrKind::Ret, .. } => continue,
            _ => {}
        }
        match insns.get(k + 1) {
            Some(&(next, _)) if next == pc + 4 => work.push(k + 1),
            _ => return Err(format!("{pc:#x}: falls through past the function's code or into a jump table")),
        }
    }
    // ---- prologue
    let at = |k: usize| insns.get(k).map(|&(_, i)| i);
    if at(0) != Some(Insn::LdSt { size: 3, opc: 0, rt: 30, rn: SP, off: -16, form: Form::Pre }) {
        return Err("prologue: no `str x30, [sp, #-16]!`".into());
    }
    let mut k = 1;
    let frame: u64 = match at(1) {
        Some(Insn::AddImm { sub: false, s: false, rd: 16, rn: SP, imm: 0, .. }) => {
            k = 2;
            0
        }
        Some(Insn::AddImm { sub: true, s: false, rd: 16, rn: SP, imm, .. }) => {
            k = 2;
            imm as u64
        }
        _ => {
            let mut f = Facts::new();
            while let Some(i @ (Insn::MovWide { rd: 16, .. } | Insn::LogImm { opc: 1, rd: 16, rn: ZR, .. })) = at(k) {
                transfer(&mut f, &i, insns[k].0, l);
                k += 1;
            }
            let wrap = Insn::AddExt { sub: true, s: true, rd: 16, rn: SP, rm: 16 };
            let (true, Some(Insn::BCond { cond: LO, off }), F::Const(c)) = (at(k) == Some(wrap), at(k + 1), f.r[16])
            else {
                return Err("prologue: frame size computation not recognized".into());
            };
            if (insns[k + 1].0 as i64 + off) as u64 != pre.trap_overflow {
                return Err("prologue: frame wrap check does not trap StackOverflow".into());
            }
            k += 2;
            c
        }
    };
    if frame % 16 != 0 || frame >= 1 << 30 {
        return Err(format!("prologue: frame size {frame:#x}"));
    }
    let (Some(Insn::LdSt { size: 3, opc: 1, rt: 17, rn: CTX, off: OFF_STACK_LIMIT, form: Form::Scaled }),
        Some(Insn::AddReg { sub: true, s: true, rd: ZR, rn: 16, rm: 17, shift: 0, amount: 0 }),
        Some(Insn::BCond { cond: LO, off: stub_off }),
        Some(Insn::AddImm { sub: false, s: false, rd: SP, rn: 16, imm: 0, lsl12: false })) =
        (at(k), at(k + 1), at(k + 2), at(k + 3))
    else {
        return Err("prologue: stack check / `mov sp, x16` not recognized".into());
    };
    let back = insns[k + 3].0;
    let stub = (insns[k + 2].0 as i64 + stub_off) as u64;
    let body = k + 4;
    // The entry stub: `bl safepoint; b back`, reached only from the stack check; `back` only from the stub.
    let si = *d.index.get(&stub).ok_or("prologue: stub outside the function")?;
    match (at(si), at(si + 1)) {
        (Some(Insn::B { link: true, off: o1 }), Some(Insn::B { link: false, off: o2 }))
            if (stub as i64 + o1) as u64 == pre.safepoint && (stub as i64 + 4 + o2) as u64 == back => {}
        _ => return Err("prologue: entry stub is not `bl safepoint; b back`".into()),
    }
    let only_from_check = targets.get(&stub).map(Vec::as_slice) == Some(&[insns[k + 2].0][..]);
    if !only_from_check || (reach[si - 1] && !falls_nowhere(at(si - 1))) {
        return Err("prologue: entry stub reachable from elsewhere".into());
    }
    if targets.get(&back).map(Vec::as_slice) != Some(&[stub + 4][..]) {
        return Err("prologue: `mov sp, x16` reachable from elsewhere".into());
    }
    for kk in 1..=k + 2 {
        if targets.contains_key(&insns[kk].0) {
            return Err("prologue: branch target inside the stack check".into());
        }
    }
    // ---- body
    let fold_limit = 1u64 << l.min_mem_log2.min(31);
    let frame_ok = |a: i64, w: i64| {
        let f = frame as i64;
        (a >= 0 && a + w <= f) || (a >= f + 16 && a + w <= f + 16 + 8 * params as i64)
    };
    st.instructions += body as u64;
    let mut f = Facts::new();
    let mut prev_facts = f;
    // Instructions of a verified epilogue (`add sp`, `ldr x30, [sp], #16`, `ret`) end before this index.
    let mut epilogue_end = 0usize;
    let mut kk = body;
    while kk < insns.len() {
        let (pc, i) = insns[kk];
        if targets.contains_key(&pc) {
            prev_facts = f;
            f = Facts::new();
        }
        if !reach[kk] {
            kk += 1;
            continue;
        }
        st.instructions += 1;
        let mut epilogue = kk < epilogue_end;
        if !epilogue {
            match i {
                Insn::AddImm { sub: false, s: false, rd: SP, rn: SP, .. } | Insn::LdSt { rt: 30, form: Form::Post, .. } => {
                    // add sp, sp, #a [; add sp, sp, #b]; ldr x30, [sp], #16; ret
                    let mut total = 0u64;
                    let mut j = kk;
                    while let Some(Insn::AddImm { sub: false, s: false, rd: SP, rn: SP, imm, .. }) = at(j) {
                        total += imm as u64;
                        j += 1;
                    }
                    if total != frame
                        || at(j) != Some(Insn::LdSt { size: 3, opc: 1, rt: 30, rn: SP, off: 16, form: Form::Post })
                        || at(j + 1) != Some(Insn::BrReg { kind: BrKind::Ret, rn: 30 })
                    {
                        return Err(format!("{pc:#x}: stack pointer change outside an epilogue of frame {frame:#x}"));
                    }
                    if (kk + 1..=j + 1).any(|x| targets.contains_key(&insns[x].0)) {
                        return Err(format!("{pc:#x}: branch target inside an epilogue"));
                    }
                    epilogue_end = j + 2;
                    epilogue = true;
                }
                Insn::BrReg { kind: BrKind::Ret, .. } => return Err(format!("{pc:#x}: ret outside an epilogue")),
                Insn::Pair { load: false, rt: ZR, rt2: ZR, rn: 16, off: 16, form: Form::Post } => {
                    // Zeroing loop, x16 = sp + a and x17 = n before it:
                    // L: stp xzr, xzr, [x16], #16; subs x17, x17, #1; b.ne L
                    let (F::SpPlus(a), F::Const(n)) = (prev_facts.r[16], prev_facts.r[17]) else {
                        return Err(format!("{pc:#x}: `stp xzr, xzr, [x16], #16` without the zeroing loop's setup"));
                    };
                    let ok = at(kk + 1) == Some(Insn::AddImm { sub: true, s: true, rd: 17, rn: 17, imm: 1, lsl12: false })
                        && matches!(at(kk + 2), Some(Insn::BCond { cond: NE, off })
                            if (insns[kk + 2].0 as i64 + off) as u64 == pc)
                        && targets.get(&pc).map(Vec::as_slice) == Some(&[insns[kk + 2].0][..])
                        && !targets.contains_key(&insns[kk + 1].0)
                        && !targets.contains_key(&insns[kk + 2].0)
                        && n > 0
                        && n < 1 << 30
                        && frame_ok(a, 16 * n as i64);
                    if !ok {
                        return Err(format!("{pc:#x}: zeroing loop not recognized or outside the frame"));
                    }
                    f = Facts::new();
                    f.r[16] = F::SpPlus(a + 16 * n as i64);
                    st.frame += 1;
                    st.instructions += 2;
                    kk += 3;
                    continue;
                }
                _ => {}
            }
        }
        // ---- register writes
        let (writes, _) = effects(&i);
        for r in writes.into_iter().flatten() {
            let ok = match r {
                18 | 26 | 27 | 28 | 29 => false,
                30 => matches!(i, Insn::B { link: true, .. } | Insn::BrReg { kind: BrKind::Blr, .. }) || epilogue,
                SP => epilogue,
                _ => true,
            };
            if !ok {
                return Err(format!("{pc:#x}: writes {} (`{}`)", xn(r), i.text(pc)));
            }
        }
        if let Some(v) = vwrite(&i) {
            if (8..16).contains(&v) {
                return Err(format!("{pc:#x}: writes v{v} (callee-saved d8-d15)"));
            }
        }
        // ---- memory accesses
        let fact = |r: u8| if r == SP || r == ZR { F::None } else { f.r[r as usize] };
        match i {
            Insn::LdrLit { .. } => return Err(format!("{pc:#x}: literal load in a function")),
            Insn::LdSt { rn: BASE, off, form: Form::Scaled | Form::Unscaled, .. }
            | Insn::LdStD { rn: BASE, off, form: Form::Scaled | Form::Unscaled, .. } => {
                if off < 0 || off as u64 >= fold_limit {
                    return Err(format!("{pc:#x}: constant guest address {off:#x} not below {fold_limit:#x}"));
                }
                st.heap_const += 1;
            }
            Insn::LdStReg { rn: BASE, rm, scaled: false, .. } | Insn::LdStDReg { rn: BASE, rm, .. } => match fact(rm) {
                F::Masked => st.heap_masked += 1,
                F::Const(c) if c < fold_limit => st.heap_const += 1,
                v => return Err(format!("{pc:#x}: guest access with an unmasked index ({v:?}): `{}`", i.text(pc))),
            },
            Insn::LdStD { rn, off, form: Form::Scaled | Form::Unscaled, .. } if rn == SP || matches!(fact(rn), F::SpPlus(_)) => {
                let base = if let F::SpPlus(k) = fact(rn) { k } else { 0 };
                if !frame_ok(base + off, 8) {
                    return Err(format!("{pc:#x}: frame access [sp + {}] outside the frame {frame:#x}", base + off));
                }
                st.frame += 1;
            }
            Insn::LdSt { size, rn, off, form, .. } if rn == SP || matches!(fact(rn), F::SpPlus(_)) => {
                let base = if let F::SpPlus(k) = fact(rn) { k } else { 0 };
                let ok = match form {
                    Form::Scaled | Form::Unscaled => frame_ok(base + off, 1 << size),
                    // The epilogue's reload of the return address.
                    _ => epilogue && rn == SP,
                };
                if !ok {
                    let t = i.text(pc);
                    return Err(format!("{pc:#x}: frame access [sp + {}] outside the frame {frame:#x}: `{t}`", base + off));
                }
                st.frame += 1;
            }
            Insn::Pair { rn, off, form: Form::Scaled, .. } if rn == SP || matches!(fact(rn), F::SpPlus(_)) => {
                let base = if let F::SpPlus(k) = fact(rn) { k } else { 0 };
                if !frame_ok(base + off, 16) {
                    return Err(format!("{pc:#x}: frame access [sp + {}] outside the frame {frame:#x}", base + off));
                }
                st.frame += 1;
            }
            Insn::LdSt { size, opc: 1, rn: CTX, off, form: Form::Scaled, .. } => {
                let ok = match (off, size) {
                    (OFF_STACK_LIMIT | OFF_HOST_FNS, 3) | (OFF_TRAP_REQUEST, 2) => true,
                    (_, 3) => matches!(fact_after(&f, &i, pc, l, 16), F::Callee { helper: true }),
                    _ => false,
                };
                if !ok {
                    return Err(format!("{pc:#x}: vmctx access not allowed: `{}`", i.text(pc)));
                }
                st.vmctx += 1;
            }
            Insn::LdSt { rn: 16, .. } | Insn::LdStReg { rn: 16, .. } if matches!(fact(16), F::HostFns | F::Table(_)) => {
                // Only a host_fns slot below the import count, or a clamped table entry, gives these facts.
                let ok = match i {
                    Insn::LdStReg { opc: 2, rt, .. } => matches!(fact_after(&f, &i, pc, l, rt), F::Entry { .. }),
                    _ => matches!(fact_after(&f, &i, pc, l, 16), F::Callee { helper: false }),
                };
                if !ok {
                    return Err(format!("{pc:#x}: host-function or table load not recognized: `{}`", i.text(pc)));
                }
            }
            Insn::LdSt { rn, .. }
            | Insn::LdStReg { rn, .. }
            | Insn::Pair { rn, .. }
            | Insn::LdStD { rn, .. }
            | Insn::LdStDReg { rn, .. }
            | Insn::PairD { rn, .. } => {
                return Err(format!("{pc:#x}: memory access through {} ({:?}): `{}`", xn(rn), fact(rn), i.text(pc)));
            }
            _ => {}
        }
        // ---- control flow
        let form_ok = match l.mode {
            IndirectBranch::Jump => !is_barrier_end(insns, kk),
            IndirectBranch::Lfence => is_barrier_end(insns, kk),
            IndirectBranch::Retpoline => false,
        };
        match i {
            Insn::B { link: true, off } => {
                let t = (pc as i64 + off) as u64;
                if t == pre.safepoint {
                    // Loop stub `mov x16, sp; bl safepoint; b back` (the entry stub was checked with the prologue).
                    let mov = Insn::AddImm { sub: false, s: false, rd: 16, rn: SP, imm: 0, lsl12: false };
                    let loop_stub = at(kk - 1) == Some(mov)
                        && !targets.contains_key(&pc);
                    if !(pc == stub || loop_stub) || !matches!(at(kk + 1), Some(Insn::B { link: false, .. })) {
                        return Err(format!("{pc:#x}: safepoint call outside a stub"));
                    }
                } else if Some(t) == pre.retpoline {
                    match f.r[16] {
                        F::Callee { helper } => {
                            *if helper { &mut st.helper_calls } else { &mut st.host_calls } += 1;
                            st.hardened += 1;
                        }
                        v => return Err(format!("{pc:#x}: call through the retpoline thunk with x16 = {v:?}")),
                    }
                } else if !entries.contains(&t) {
                    return Err(format!("{pc:#x}: bl to {t:#x}, not a function entry"));
                }
            }
            Insn::BrReg { kind: BrKind::Blr, rn } => match (rn, f.r[16]) {
                (16, F::Callee { helper }) if form_ok => {
                    *if helper { &mut st.helper_calls } else { &mut st.host_calls } += 1;
                    st.hardened += (l.mode != IndirectBranch::Jump) as u64;
                }
                _ => return Err(format!("{pc:#x}: indirect call not in the {:?} form (x16 = {:?})", l.mode, f.r[16])),
            },
            Insn::BrReg { kind: BrKind::Br, rn } => {
                let table = matches!(f.r[16], F::Target { t, .. } if t == pc + 4 && d.tables.contains_key(&t));
                if rn != 16 || !table || !form_ok {
                    let (mode, x16) = (l.mode, f.r[16]);
                    return Err(format!("{pc:#x}: indirect jump not a clamped jump table in the {mode:?} form (x16 = {x16:?})"));
                }
                st.hardened += (l.mode != IndirectBranch::Jump) as u64;
            }
            Insn::B { link: false, off } if Some((pc as i64 + off) as u64) == pre.retpoline => {
                if !matches!(f.r[16], F::Target { t, .. } if t == pc + 4 && d.tables.contains_key(&t)) {
                    return Err(format!("{pc:#x}: jump through the retpoline thunk with x16 = {:?}", f.r[16]));
                }
                st.hardened += 1;
            }
            _ => {}
        }
        transfer(&mut f, &i, pc, l);
        kk += 1;
    }
    Ok(())
}

/// The fact `i` leaves in register `r`.
fn fact_after(f: &Facts, i: &Insn, pc: u64, l: &Layout, r: u8) -> F {
    let mut g = *f;
    transfer(&mut g, i, pc, l);
    g.r[r as usize]
}

/// True for instructions after which execution never falls through.
fn falls_nowhere(i: Option<Insn>) -> bool {
    matches!(i, Some(Insn::B { link: false, .. }) | Some(Insn::BrReg { kind: BrKind::Br | BrKind::Ret, .. }))
}
