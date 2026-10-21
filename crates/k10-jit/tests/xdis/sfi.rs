//! SFI verifier (VeriWasm-style) for k10-jit output, independent of the codegen: it checks docs/JIT-ABI.md on
//! objdump's decoding of every function, given the module, the blob's function offsets and helper table and the
//! IndirectBranch mode, and rejects anything it does not recognize. Register facts hold on straight-line paths only
//! (reset at branch targets, calls, jmp, ret) and come only from 64-bit or zero-extending 32-bit writes.
//! * Instructions: integer/SSE2 ops, moves, lea, cmov/setcc, jmp/jcc/call/ret, rep stos, lfence; int3 only as padding.
//! * Registers: r13 (mask), r14 (vmctx), r15 (heap base) are never written; no push/pop.
//! * Stack: `rsp-F` checked against vmctx.stack_limit, then `sub rsp,F`; `add rsp,F` only right before an untargeted ret.
//! * Prologue: entered only at the function start, and at `sub rsp,F` by the entry stub (`call safepoint; jmp`).
//! * Guest memory: `[r15+R*1]` with R from `and R,r13`, or `[r15+c]` with 0 <= c < 2^min(min_mem_log2,31).
//! * Frame: `[rsp+d]` in [0,F) or the parameters [F+8,F+8+8*params); `rep stos` from `lea rdi,[rsp+X]` within [0,F).
//! * vmctx: reads of stack_limit, host_fns and trap_request only.
//! * Operands: sized, at most 8 bytes, no segment; bit tests on memory only with an immediate bit index.
//! * Calls: function entries, prelude stubs, or host/helper slots in the mode's form (`call [slot]`, `lfence; call r11`,
//!   `call thunk` with r11 loaded from the slot).
//! * Jumps: inside the function or to prelude trap stubs; no fall-through past the function end.
//! * Jump tables: `lea r11,[rip+T]`, `movsxd R,[r11+R*4]`, the mode's jump, then the n entries of T (int3 padding first).
//! * Table bounds: R < n by `cmp`+`jae`/`ja`, a `cmov` clamp to a constant, `cmp`+`sbb M,M`+`and R,M`, or `and R,imm`.
//! * Retpoline thunk: byte-exact, 16-aligned, in the prelude in Retpoline mode only, reached only as above.
//!
//! Trusted: the prelude (identical per mode, its indirect calls in the mode's form), the bulk-memory helpers.

use crate::common::RETPOLINE_THUNK;
use crate::xdis::{disassemble_many, Insn};
use k10_ir::Module;
use k10_jit::{CodeBlob, IndirectBranch, VmCtx};
use std::collections::{BTreeSet, HashMap};
use std::mem::offset_of;

const OFF_STACK_LIMIT: i64 = 0x10;
const OFF_HOST_FNS: i64 = 0x20;
const OFF_INTERRUPT: i64 = 0x28;
const OFF_TRAP_REQUEST: i64 = 0x30;
const OFF_SAVED_RSP: i64 = 0x40;
const _: () = assert!(offset_of!(VmCtx, stack_limit) == OFF_STACK_LIMIT as usize);
const _: () = assert!(offset_of!(VmCtx, host_fns) == OFF_HOST_FNS as usize);
const _: () = assert!(offset_of!(VmCtx, interrupt) == OFF_INTERRUPT as usize);
const _: () = assert!(offset_of!(VmCtx, trap_request) == OFF_TRAP_REQUEST as usize);
const _: () = assert!(offset_of!(VmCtx, jit_private) == OFF_SAVED_RSP as usize);

/// Indirect-branch form the blob was compiled with.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Mode {
    Jump,
    Lfence,
    Retpoline,
}

pub fn mode(i: IndirectBranch) -> Mode {
    match i {
        IndirectBranch::Jump => Mode::Jump,
        IndirectBranch::Lfence => Mode::Lfence,
        IndirectBranch::Retpoline => Mode::Retpoline,
    }
}

/// What the verifier knows about a blob.
#[derive(Clone, Debug)]
pub struct Layout {
    /// `(offset, params)` of every defined function, in order.
    pub funcs: Vec<(u32, u8)>,
    pub helper_table: u32,
    pub num_helpers: usize,
    pub num_imports: usize,
    pub min_mem_log2: u8,
    pub mode: Mode,
}

impl Layout {
    fn first_func(&self) -> u64 {
        self.funcs.first().map_or(self.helper_table as u64, |f| f.0 as u64)
    }
}

/// Layout of `m` compiled to `b`: arities, imports and memory size from the module, code offsets from the blob.
pub fn layout(m: &Module, b: &CodeBlob, indirect: IndirectBranch) -> Layout {
    let meta = b.meta();
    assert_eq!(meta.funcs().len(), m.funcs.len());
    Layout {
        funcs: meta.funcs().iter().zip(&m.funcs).map(|(e, f)| (e.offset, f.params)).collect(),
        helper_table: meta.helper_table_offset(),
        num_helpers: meta.num_helpers(),
        num_imports: m.imports.len(),
        min_mem_log2: m.min_mem_log2,
        mode: mode(indirect),
    }
}

/// Verifies compiled modules in one batch; preludes must be identical within a mode.
pub fn verify_compiled(
    objdump: &std::path::Path,
    blobs: &[(&Module, &CodeBlob, IndirectBranch)],
) -> Result<Stats, String> {
    let mut bs: Vec<Blob> = blobs.iter().map(|(m, b, i)| Blob::new(b.code(), layout(m, b, *i))).collect();
    let (stats, preludes) = verify_blobs(objdump, &mut bs)?;
    let mut first: HashMap<Mode, usize> = HashMap::new();
    for (k, (_, _, i)) in blobs.iter().enumerate() {
        let r = *first.entry(mode(*i)).or_insert(k);
        if preludes[k] != preludes[r] {
            return Err(format!("prelude of blob {k} differs from blob {r}'s ({:?} mode)", mode(*i)));
        }
    }
    Ok(stats)
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
    /// Indirect calls/jumps behind an lfence or through the retpoline thunk.
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

// ---- operands

const RAX: u8 = 0;
const RCX: u8 = 1;
const RDX: u8 = 2;
const RSP: u8 = 4;
const RDI: u8 = 7;
const R11: u8 = 11;
const R13: u8 = 13;
const R14: u8 = 14;
const R15: u8 = 15;

/// `(register id, width in bits)` of a general-purpose register name.
fn reg(name: &str) -> Option<(u8, u8)> {
    const R64: [&str; 16] =
        ["rax", "rcx", "rdx", "rbx", "rsp", "rbp", "rsi", "rdi", "r8", "r9", "r10", "r11", "r12", "r13", "r14", "r15"];
    const R32: [&str; 16] = [
        "eax", "ecx", "edx", "ebx", "esp", "ebp", "esi", "edi", "r8d", "r9d", "r10d", "r11d", "r12d", "r13d", "r14d",
        "r15d",
    ];
    const R16: [&str; 16] =
        ["ax", "cx", "dx", "bx", "sp", "bp", "si", "di", "r8w", "r9w", "r10w", "r11w", "r12w", "r13w", "r14w", "r15w"];
    const R8: [&str; 16] = [
        "al", "cl", "dl", "bl", "spl", "bpl", "sil", "dil", "r8b", "r9b", "r10b", "r11b", "r12b", "r13b", "r14b",
        "r15b",
    ];
    for (bits, t) in [(64u8, &R64), (32, &R32), (16, &R16), (8, &R8)] {
        if let Some(i) = t.iter().position(|&n| n == name) {
            return Some((i as u8, bits));
        }
    }
    None
}

#[derive(Clone, Debug, PartialEq)]
pub enum Base {
    Reg(u8),
    Rip,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MemOp {
    /// Width in bytes; 0 when objdump prints none.
    pub size: u8,
    pub seg: Option<String>,
    pub base: Option<Base>,
    pub index: Option<(u8, u8)>,
    pub disp: i64,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Op {
    Reg(u8, u8),
    Xmm(u8),
    Imm(u64),
    Mem(MemOp),
    Other(String),
}

fn parse_num(s: &str) -> Option<u64> {
    match s.strip_prefix("0x") {
        Some(h) => u64::from_str_radix(h, 16).ok(),
        None => (s == "1").then_some(1), // `shl rax,1` (D1 form)
    }
}

fn parse_mem(s: &str) -> Option<MemOp> {
    let mut s = s.trim();
    let mut size = 0u8;
    for (name, n) in [
        ("BYTE PTR ", 1u8),
        ("WORD PTR ", 2),
        ("DWORD PTR ", 4),
        ("QWORD PTR ", 8),
        ("XMMWORD PTR ", 16),
        ("TBYTE PTR ", 10),
        ("YMMWORD PTR ", 32),
        ("FWORD PTR ", 6),
    ] {
        if let Some(r) = s.strip_prefix(name) {
            size = n;
            s = r;
        }
    }
    let mut seg = None;
    if s.len() > 3 && &s[2..3] == ":" && ["es", "cs", "ss", "ds", "fs", "gs"].contains(&&s[..2]) {
        seg = Some(s[..2].to_string());
        s = &s[3..];
    }
    let Some(inner) = s.strip_prefix('[').and_then(|r| r.strip_suffix(']')) else {
        // `ds:0x1234`: an absolute address.
        return Some(MemOp {
            size,
            seg: seg.or(Some("abs".into())),
            base: None,
            index: None,
            disp: parse_num(s)? as i64,
        });
    };
    let mut m = MemOp { size, seg, base: None, index: None, disp: 0 };
    let mut rest = inner;
    let mut first = true;
    while !rest.is_empty() {
        let end = rest[1..].find(['+', '-']).map_or(rest.len(), |i| i + 1);
        let (mut term, mut sign) = (&rest[..end], 1i64);
        rest = &rest[end..];
        if let Some(t) = term.strip_prefix('+') {
            term = t;
        } else if let Some(t) = term.strip_prefix('-') {
            (term, sign) = (t, -1);
        } else if !first {
            return None;
        }
        first = false;
        if term == "rip" {
            m.base = Some(Base::Rip);
        } else if let Some((r, sc)) = term.split_once('*') {
            let (id, 64) = reg(r)? else { return None };
            m.index = Some((id, sc.parse().ok()?));
        } else if let Some((id, bits)) = reg(term) {
            if bits != 64 || m.base.is_some() {
                return None;
            }
            m.base = Some(Base::Reg(id));
        } else {
            m.disp = m.disp.wrapping_add(sign.wrapping_mul(parse_num(term)? as i64));
        }
    }
    Some(m)
}

pub fn parse_operand(s: &str) -> Op {
    let s = s.trim();
    if let Some((id, bits)) = reg(s) {
        return Op::Reg(id, bits);
    }
    if let Some(x) = s.strip_prefix("xmm").and_then(|n| n.parse::<u8>().ok()) {
        return Op::Xmm(x);
    }
    if let Some(v) = parse_num(s) {
        return Op::Imm(v);
    }
    if s.contains('[') || s.contains("PTR") || s.contains(':') {
        if let Some(m) = parse_mem(s) {
            return Op::Mem(m);
        }
    }
    Op::Other(s.to_string())
}

pub fn operands(i: &Insn) -> Vec<Op> {
    if i.operands.is_empty() {
        return Vec::new();
    }
    i.operands.split(',').map(parse_operand).collect()
}

// ---- classification

const CC: [&str; 16] = ["o", "no", "b", "ae", "e", "ne", "be", "a", "s", "ns", "p", "np", "l", "ge", "le", "g"];

fn is_jcc(m: &str) -> bool {
    m.strip_prefix('j').is_some_and(|c| CC.contains(&c))
}

/// Instructions that read operand 0 without writing it.
fn reads_only(m: &str) -> bool {
    matches!(m, "cmp" | "test" | "bt" | "ucomisd")
}

/// The whitelist besides control flow.
fn known(m: &str) -> bool {
    matches!(
        m,
        "mov"
            | "movabs"
            | "movzx"
            | "movsx"
            | "movsxd"
            | "lea"
            | "add"
            | "or"
            | "adc"
            | "sbb"
            | "and"
            | "sub"
            | "xor"
            | "cmp"
            | "test"
            | "imul"
            | "mul"
            | "div"
            | "idiv"
            | "not"
            | "neg"
            | "cqo"
            | "shl"
            | "shr"
            | "sar"
            | "rol"
            | "ror"
            | "bt"
            | "bts"
            | "btr"
            | "btc"
            | "bsr"
            | "bsf"
            | "lzcnt"
            | "tzcnt"
            | "popcnt"
            | "movq"
            | "addsd"
            | "subsd"
            | "mulsd"
            | "divsd"
            | "sqrtsd"
            | "ucomisd"
            | "cvtsi2sd"
            | "cvttsd2si"
            | "xorpd"
            | "rep stos"
            | "lfence"
    ) || m.strip_prefix("cmov").is_some_and(|c| CC.contains(&c))
        || m.strip_prefix("set").is_some_and(|c| CC.contains(&c))
        || (m.starts_with("cmp") && m.ends_with("sd") && m.len() > 5)
}

/// Never allowed anywhere, the prelude included (clearer messages than "not in the whitelist").
fn forbidden(m: &str) -> bool {
    let base = m.rsplit(' ').next().unwrap_or(m);
    matches!(
        base,
        "syscall"
            | "sysenter"
            | "sysexit"
            | "sysret"
            | "int"
            | "int1"
            | "into"
            | "iret"
            | "iretd"
            | "iretq"
            | "rdtsc"
            | "rdtscp"
            | "rdpmc"
            | "rdmsr"
            | "wrmsr"
            | "cpuid"
            | "hlt"
            | "in"
            | "out"
            | "ins"
            | "outs"
            | "insb"
            | "insd"
            | "outsb"
            | "outsd"
            | "cli"
            | "sti"
            | "lgdt"
            | "lidt"
            | "lldt"
            | "ltr"
            | "sgdt"
            | "sidt"
            | "ljmp"
            | "lcall"
            | "lret"
            | "retf"
            | "xgetbv"
            | "xsetbv"
            | "wbinvd"
            | "invd"
            | "invlpg"
            | "swapgs"
            | "rdfsbase"
            | "rdgsbase"
            | "wrfsbase"
            | "wrgsbase"
            | "vmcall"
            | "vmmcall"
            | "ud0"
            | "ud1"
            | "ud2"
            | "(bad)"
    )
}

// ---- the prelude

/// Prelude entry points usable by functions.
#[derive(Default)]
pub struct Stubs {
    /// Call targets: blocks right after an unconditional jmp/ret, not padding, not inside the thunk.
    pub all: BTreeSet<u64>,
    /// Jump targets: trap stubs only (a jump to a returning stub would `ret` into frame data).
    pub trap: BTreeSet<u64>,
    pub thunk: Option<u64>,
}

/// The retpoline thunk's offset: one 16-aligned copy in the prelude in Retpoline mode, none otherwise.
pub fn find_thunk(code: &[u8], lay: &Layout) -> Result<Option<u64>, String> {
    let first = (lay.first_func() as usize).min(code.len());
    let n = RETPOLINE_THUNK.len();
    let hits: Vec<usize> =
        code[..first].windows(n).enumerate().filter(|(_, w)| *w == RETPOLINE_THUNK).map(|(p, _)| p).collect();
    match (lay.mode, hits.as_slice()) {
        (Mode::Retpoline, [p]) if p % 16 == 0 => Ok(Some(*p as u64)),
        (Mode::Retpoline, _) => {
            Err(format!("prelude: retpoline thunk missing, misaligned or duplicated (at {hits:?})"))
        }
        (_, []) => Ok(None),
        (m, _) => Err(format!("prelude: retpoline thunk present in {m:?} mode")),
    }
}

/// Checks the prelude (forbidden instructions, indirect branches in the mode's form) and classifies its stubs.
pub fn prelude_stubs(prelude: &[Insn], mode: Mode, thunk: Option<u64>) -> Result<Stubs, String> {
    let in_thunk = |a: u64| thunk.is_some_and(|t| a >= t && a < t + RETPOLINE_THUNK.len() as u64);
    let interrupt = format!("QWORD PTR [r14+{OFF_INTERRUPT:#x}]");
    let saved_rsp = format!("rsp,QWORD PTR [r14+{OFF_SAVED_RSP:#x}]");
    let mut st = Stubs { thunk, ..Stubs::default() };
    let mut after_uncond = false;
    let mut prev = "";
    for i in prelude {
        let m = i.mnemonic.as_str();
        if forbidden(m) {
            return Err(format!("prelude: forbidden instruction `{}` at {:#x}", i.text(), i.addr));
        }
        if (m == "call" || m == "jmp") && !i.operands.starts_with("0x") {
            let ok = match mode {
                Mode::Jump => m == "call" && (i.operands == "rsi" || i.operands == interrupt),
                Mode::Lfence => m == "call" && i.operands == "r11" && prev == "lfence",
                Mode::Retpoline => false,
            };
            if !ok {
                return Err(format!("prelude: indirect branch `{}` at {:#x} not in {mode:?} form", i.text(), i.addr));
            }
        }
        if after_uncond && m != "int3" && !in_thunk(i.addr) {
            st.all.insert(i.addr);
        }
        after_uncond = matches!(m, "jmp" | "ret");
        prev = m;
    }
    let at: HashMap<u64, usize> = prelude.iter().enumerate().map(|(k, i)| (i.addr, k)).collect();
    for &s0 in &st.all {
        // Trap stub: reaches the restore of the invoke frame before any `ret`.
        let mut k = at[&s0];
        for _ in 0..256 {
            let i = &prelude[k];
            if i.mnemonic == "ret" {
                break;
            }
            if i.mnemonic == "mov" && i.operands == saved_rsp {
                st.trap.insert(s0);
                break;
            }
            k = match (i.mnemonic.as_str(), parse_operand(&i.operands)) {
                ("jmp", Op::Imm(t)) => match at.get(&t) {
                    Some(&n) => n,
                    None => break,
                },
                _ if k + 1 < prelude.len() => k + 1,
                _ => break,
            };
        }
    }
    Ok(st)
}

// ---- straight-line facts

/// A jump table found while decoding: `n` rel32 entries at `at`, used by the jump at `jmp_at`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Table {
    pub jmp_at: u64,
    pub at: u64,
    pub n: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Callee {
    Host,
    Helper,
}

#[derive(Clone, Default)]
struct State {
    /// `and R, r13` result, unchanged since.
    masked: u16,
    konst: [Option<u64>; 16],
    /// Register value `< bound` (unsigned).
    bound: [Option<u64>; 16],
    /// `rsp + X` (from `lea R,[rsp+X]`).
    rsp_rel: [Option<i64>; 16],
    /// Holds `vmctx.host_fns`.
    host_fns: u16,
    /// Holds a host-function or helper pointer read from its slot.
    callee: [Option<Callee>; 16],
    /// Flags hold `cmp R, v`.
    flags: Option<(u8, u64)>,
    /// `sbb M,M` after `cmp R,n`: M = (R < n) ? !0 : 0.
    lt_mask: [Option<(u8, u64)>; 16],
    /// `lea r11,[rip+T]`.
    table_addr: [Option<u64>; 16],
    /// `movsxd R,[r11+R*4]` from table T with R < bound: (T, bound).
    table_entry: [Option<(u64, u64)>; 16],
    /// Entry + table address: a target from (T, bound).
    table_target: [Option<(u64, u64)>; 16],
}

impl State {
    fn write(&mut self, r: u8) {
        let i = r as usize;
        self.masked &= !(1 << r);
        self.konst[i] = None;
        self.bound[i] = None;
        self.rsp_rel[i] = None;
        self.host_fns &= !(1 << r);
        self.callee[i] = None;
        self.lt_mask[i] = None;
        self.table_addr[i] = None;
        self.table_entry[i] = None;
        self.table_target[i] = None;
        if matches!(self.flags, Some((x, _)) if x == r) {
            self.flags = None;
        }
        for m in self.lt_mask.iter_mut() {
            if matches!(m, Some((x, _)) if *x == r) {
                *m = None;
            }
        }
    }
}

fn writes_op0(m: &str) -> bool {
    !reads_only(m) && !matches!(m, "rep stos" | "lfence" | "int3" | "call" | "jmp" | "ret") && !is_jcc(m)
}

fn sets_flags(m: &str) -> bool {
    !matches!(m, "mov" | "movabs" | "movzx" | "movsx" | "movsxd" | "lea" | "movq" | "not" | "lfence" | "rep stos")
        && !m.starts_with("cmov")
        && !m.starts_with("set")
        && !matches!(m, "cvtsi2sd" | "cvttsd2si" | "xorpd" | "addsd" | "subsd" | "mulsd" | "divsd" | "sqrtsd")
        && !(m.starts_with("cmp") && m.ends_with("sd"))
}

fn implicit_writes(m: &str, nops: usize) -> &'static [u8] {
    match (m, nops) {
        ("mul" | "div" | "idiv", _) | ("imul", 1) => &[RAX, RDX],
        ("cqo", _) => &[RDX],
        ("rep stos", _) => &[RDI, RCX],
        _ => &[],
    }
}

/// Explicit destination register `(id, bits)`.
fn dst_reg(m: &str, ops: &[Op]) -> Option<(u8, u8)> {
    match ops.first() {
        Some(Op::Reg(r, bits))
            if writes_op0(m) && !matches!(m, "mul" | "div" | "idiv") && !(m == "imul" && ops.len() == 1) =>
        {
            Some((*r, *bits))
        }
        _ => None,
    }
}

/// A 64-bit read of a host-function slot (`[H+8k]`, k < imports) or a helper slot (`[rip+X]`).
fn callee_slot(lay: &Layout, st: &State, mm: &MemOp, rip_target: Option<u64>) -> Option<Callee> {
    if mm.size != 8 || mm.seg.is_some() || mm.index.is_some() {
        return None;
    }
    match mm.base {
        Some(Base::Reg(b)) if st.host_fns & (1 << b) != 0 => {
            (mm.disp >= 0 && mm.disp % 8 == 0 && ((mm.disp / 8) as usize) < lay.num_imports).then_some(Callee::Host)
        }
        Some(Base::Rip) => {
            let (t, h0) = (rip_target?, lay.helper_table as u64);
            (t >= h0 && t < h0 + 8 * lay.num_helpers as u64 && (t - h0).is_multiple_of(8)).then_some(Callee::Helper)
        }
        _ => None,
    }
}

fn tighten(b: &mut Option<u64>, v: u64) {
    *b = Some(b.map_or(v, |o| o.min(v)));
}

/// The facts after `i`; shared by table discovery and verification (the checks are in [`verify_function`]).
fn transfer(lay: &Layout, st: &State, i: &Insn, ops: &[Op]) -> State {
    let m = i.mnemonic.as_str();
    if matches!(m, "call" | "jmp" | "ret") {
        return State::default();
    }
    let mut new = st.clone();
    if is_jcc(m) {
        // Fall-through facts of a bounds check.
        match (m, st.flags) {
            ("jae", Some((r, n))) => tighten(&mut new.bound[r as usize], n),
            ("ja", Some((r, n))) => {
                if let Some(b) = n.checked_add(1) {
                    tighten(&mut new.bound[r as usize], b)
                }
            }
            _ => {}
        }
        return new;
    }
    for &r in implicit_writes(m, ops.len()) {
        new.write(r);
    }
    if let Some((r, bits)) = dst_reg(m, ops) {
        let ri = r as usize;
        new.write(r);
        // Facts only from writes that define all 64 bits.
        let (full, wide) = (bits >= 32, bits == 64);
        match (m, ops) {
            ("and", [_, Op::Reg(R13, 64)]) if wide => new.masked |= 1 << r,
            ("and", [_, Op::Imm(v)]) if full => {
                let v = if wide { *v } else { *v & 0xffff_ffff };
                if v < (1 << 40) {
                    new.bound[ri] = Some(v + 1);
                }
            }
            ("and", [_, Op::Reg(mreg, 64)]) if wide => {
                if let Some((x, n)) = st.lt_mask[*mreg as usize] {
                    if x == r {
                        new.bound[ri] = Some(n);
                    }
                }
            }
            ("mov", [_, Op::Imm(v)]) if full => new.konst[ri] = Some(if wide { *v } else { *v & 0xffff_ffff }),
            ("movabs", [_, Op::Imm(v)]) if wide => new.konst[ri] = Some(*v),
            ("xor", [Op::Reg(a, _), Op::Reg(b, _)]) if a == b && full => new.konst[ri] = Some(0),
            ("lea", [_, Op::Mem(MemOp { base: Some(Base::Reg(RSP)), index: None, disp, .. })]) if wide => {
                new.rsp_rel[ri] = Some(*disp)
            }
            ("lea", [_, Op::Mem(MemOp { base: Some(Base::Rip), .. })]) if r == R11 && wide => {
                new.table_addr[ri] = i.comment_target
            }
            ("mov", [_, Op::Mem(mm)]) if wide => {
                if mm.base == Some(Base::Reg(R14)) && mm.index.is_none() && mm.disp == OFF_HOST_FNS {
                    new.host_fns |= 1 << r;
                }
                new.callee[ri] = callee_slot(lay, st, mm, i.comment_target);
            }
            // M = (R < n) ? !0 : 0, valid while R is unchanged and R is not M.
            ("sbb", [Op::Reg(a, 64), Op::Reg(b, 64)]) if a == b => {
                new.lt_mask[ri] = st.flags.filter(|&(x, n)| x != r && n > 0)
            }
            (
                "movsxd",
                [_, Op::Mem(MemOp { base: Some(Base::Reg(R11)), index: Some((x, 4)), disp: 0, size: 4, seg: None })],
            ) if wide => {
                if let (Some(t), Some(b), true) = (st.table_addr[R11 as usize], st.bound[*x as usize], *x == r) {
                    new.table_entry[ri] = Some((t, b));
                }
            }
            ("add", [_, Op::Reg(src, 64)]) if wide => {
                // `add R,r11` (Jump) or `add r11,R` (Lfence, Retpoline).
                let entry = if *src == R11 {
                    st.table_entry[ri]
                } else if r == R11 {
                    st.table_entry[*src as usize]
                } else {
                    None
                };
                if let (Some((t, n)), Some(t2)) = (entry, st.table_addr[R11 as usize]) {
                    if t == t2 {
                        new.table_target[ri] = Some((t, n));
                    }
                }
            }
            (c, [_, Op::Reg(s, 64)]) if c.starts_with("cmov") && wide => {
                // R stays or becomes cv: max(bound, cv+1); after `cmp R,n`, cmovae (cmova: n+1) gives max(n, cv+1).
                let mut cands = Vec::new();
                if let Some(cv1) = st.konst[*s as usize].and_then(|cv| cv.checked_add(1)) {
                    if let Some(b) = st.bound[ri] {
                        cands.push(b.max(cv1));
                    }
                    if let Some((_, n)) = st.flags.filter(|&(x, _)| x == r) {
                        let lim = match c {
                            "cmovae" => Some(n),
                            "cmova" => n.checked_add(1),
                            _ => None,
                        };
                        if let Some(lim) = lim {
                            cands.push(lim.max(cv1));
                        }
                    }
                }
                new.bound[ri] = cands.into_iter().min();
            }
            _ => {}
        }
    }
    if sets_flags(m) {
        new.flags = match (m, ops) {
            ("cmp", [Op::Reg(r, 64), Op::Imm(v)]) => Some((*r, *v)),
            ("cmp", [Op::Reg(r, 64), Op::Reg(s, 64)]) => st.konst[*s as usize].map(|v| (*r, v)),
            _ => None,
        };
    }
    new
}

// ---- verification

struct Fn<'a> {
    lay: &'a Layout,
    entries: BTreeSet<u64>,
    start: u64,
    params: u64,
    frame: u64,
    stats: Stats,
}

impl Fn<'_> {
    fn err(&self, i: &Insn, what: &str) -> String {
        format!("function at {:#x}: {:#x} `{}` ({:02x?}): {what}", self.start, i.addr, i.text(), i.bytes)
    }

    /// Checks one memory access (not `lea`, not the table load).
    fn access(&mut self, i: &Insn, m: &MemOp, st: &State, write: bool) -> Result<(), String> {
        if m.seg.is_some() {
            return Err(self.err(i, "segment override / absolute address"));
        }
        if m.size == 0 || m.size > 8 {
            return Err(self.err(i, "unsized memory operand or wider than 8 bytes"));
        }
        let w = m.size as i64;
        match (&m.base, m.index) {
            (Some(Base::Reg(R15)), Some((x, 1))) if m.disp == 0 => {
                if st.masked & (1 << x) == 0 {
                    return Err(self.err(i, "guest access through an index not masked with r13"));
                }
                self.stats.heap_masked += 1;
            }
            (Some(Base::Reg(R15)), None) => {
                let limit = 1i64 << self.lay.min_mem_log2.min(31);
                if m.disp < 0 || m.disp >= limit {
                    return Err(self.err(i, &format!("constant guest address outside [0, {limit:#x})")));
                }
                self.stats.heap_const += 1;
            }
            (Some(Base::Reg(RSP)), None) => {
                let (f, d) = (self.frame as i64, m.disp);
                let in_frame = d >= 0 && d + w <= f;
                let in_params = d >= f + 8 && d + w <= f + 8 + 8 * self.params as i64;
                if !(in_frame || in_params) {
                    return Err(self.err(
                        i,
                        &format!("frame access outside [0,{f:#x}) and the parameters (return address at {f:#x})"),
                    ));
                }
                self.stats.frame += 1;
            }
            (Some(Base::Reg(R14)), None) => {
                if write {
                    return Err(self.err(i, "write to vmctx"));
                }
                if ![OFF_STACK_LIMIT, OFF_HOST_FNS, OFF_TRAP_REQUEST].contains(&m.disp) {
                    return Err(self.err(i, "vmctx field not readable by generated code"));
                }
                self.stats.vmctx += 1;
            }
            (Some(Base::Reg(b)), None) if st.host_fns & (1 << b) != 0 => {
                if write || callee_slot(self.lay, st, m, None).is_none() {
                    return Err(self.err(i, "host-function table access outside the imports"));
                }
            }
            _ => return Err(self.err(i, "memory operand outside guest memory, frame and vmctx")),
        }
        Ok(())
    }

    /// The table jump at `i` through `target`: its table, bound and padding.
    fn table_jump(
        &mut self,
        i: &Insn,
        code: &[u8],
        table_of: &HashMap<u64, Table>,
        target: Option<(u64, u64)>,
    ) -> Result<(), String> {
        let t = table_of.get(&i.addr).ok_or_else(|| self.err(i, "indirect jump without a recognized jump table"))?;
        if target != Some((t.at, t.n)) {
            return Err(self.err(i, "indirect jump target not computed from its jump table with a proven bound"));
        }
        if !code[i.end() as usize..t.at as usize].iter().all(|&b| b == 0xCC) {
            return Err(self.err(i, "bytes between the indirect jump and its table are not int3 padding"));
        }
        Ok(())
    }

    fn count_callee(&mut self, c: Callee) {
        match c {
            Callee::Host => self.stats.host_calls += 1,
            Callee::Helper => self.stats.helper_calls += 1,
        }
    }
}

fn is_add_rsp(i: &Insn, frame: u64) -> bool {
    i.mnemonic == "add" && matches!(operands(i).as_slice(), [Op::Reg(RSP, 64), Op::Imm(v)] if *v == frame)
}

/// Verifies one function: `insns` decoded (table bytes excluded), `tables` found while decoding, `code` the blob.
#[allow(clippy::too_many_arguments)]
pub fn verify_function(
    lay: &Layout,
    code: &[u8],
    stubs: &Stubs,
    start: u64,
    end: u64,
    params: u8,
    insns: &[Insn],
    tables: &[Table],
) -> Result<Stats, String> {
    let entries: BTreeSet<u64> = lay.funcs.iter().map(|&(o, _)| o as u64).collect();
    let mut f = Fn { lay, entries, start, params: params as u64, frame: 0, stats: Stats::default() };
    f.stats.functions = 1;
    let index: HashMap<u64, usize> = insns.iter().enumerate().map(|(k, i)| (i.addr, k)).collect();
    // Coverage: instructions and tables tile [start, end).
    let mut pos = start;
    let mut ti = tables.iter().peekable();
    for i in insns {
        while let Some(t) = ti.next_if(|t| t.at == pos) {
            pos += 4 * t.n;
        }
        if i.addr != pos {
            return Err(format!(
                "function at {start:#x}: gap or overlap at {pos:#x} (next instruction at {:#x})",
                i.addr
            ));
        }
        pos = i.end();
    }
    for t in ti {
        if t.at != pos {
            return Err(format!("function at {start:#x}: table at {:#x} not contiguous", t.at));
        }
        pos += 4 * t.n;
    }
    if pos != end {
        return Err(format!("function at {start:#x}: decoded up to {pos:#x}, region ends at {end:#x}"));
    }
    let mut table_of: HashMap<u64, (Table, Vec<u64>)> = HashMap::new();
    for t in tables {
        if t.at < start || t.at + 4 * t.n > end {
            return Err(format!("function at {start:#x}: jump table {t:?} outside the function"));
        }
        let mut targets = Vec::new();
        for k in 0..t.n {
            let p = (t.at + 4 * k) as usize;
            let tgt = (t.at as i64 + i32::from_le_bytes(code[p..p + 4].try_into().unwrap()) as i64) as u64;
            if !(tgt >= start && tgt < end && index.contains_key(&tgt)) {
                return Err(format!("function at {start:#x}: jump table at {:#x} entry {k} → {tgt:#x} is not an instruction of the function", t.at));
            }
            targets.push(tgt);
        }
        table_of.insert(t.jmp_at, (*t, targets));
    }
    // Reachable code from the entry (fall-through, call return sites, in-function branches, table entries) and the
    // branch sources among it; only reachable code is checked (dead code after e.g. `block; return; end` never runs).
    let mut reach = vec![false; insns.len()];
    let mut sources: HashMap<u64, usize> = HashMap::new();
    let mut work = vec![0usize];
    while let Some(k) = work.pop() {
        if std::mem::replace(&mut reach[k], true) {
            continue;
        }
        let i = &insns[k];
        let m = i.mnemonic.as_str();
        let mut branch = |t: u64, work: &mut Vec<usize>| {
            if let Some(&j) = index.get(&t) {
                *sources.entry(t).or_default() += 1;
                work.push(j);
            }
        };
        if let Some((_, targets)) = table_of.get(&i.addr) {
            for &t in targets {
                branch(t, &mut work);
            }
            continue;
        }
        if m == "jmp" || is_jcc(m) {
            if let Op::Imm(t) = parse_operand(&i.operands) {
                branch(t, &mut work);
            }
        }
        if matches!(m, "jmp" | "ret" | "int3") {
            continue;
        }
        match insns.get(k + 1) {
            Some(n) if n.addr == i.end() => work.push(k + 1),
            _ => return Err(f.err(i, "falls through the end of the function or into a jump table")),
        }
    }
    f.stats.tables += table_of.len() as u64;
    f.stats.table_entries += table_of.values().map(|(t, _)| t.n).sum::<u64>();
    let table_of: HashMap<u64, Table> = table_of.into_iter().map(|(a, (t, _))| (a, t)).collect();

    // ---- prologue: entry check, then `sub rsp, F`
    let i0 = insns.first().ok_or("empty function")?;
    let (after_rax, fsz) = match (i0.mnemonic.as_str(), operands(i0).as_slice()) {
        ("lea", [Op::Reg(RAX, 64), Op::Mem(MemOp { base: Some(Base::Reg(RSP)), index: None, disp, .. })])
            if *disp <= 0 =>
        {
            (1, (-*disp) as u64)
        }
        ("mov", [Op::Reg(RAX, 64), Op::Reg(RSP, 64)]) => {
            let (i1, i2) = (insns.get(1).ok_or("truncated prologue")?, insns.get(2).ok_or("truncated prologue")?);
            match (i1.mnemonic.as_str(), operands(i1).as_slice(), i2.mnemonic.as_str(), parse_operand(&i2.operands)) {
                // `jb`: rsp - F wrapped
                ("sub", [Op::Reg(RAX, 64), Op::Imm(v)], "jb", Op::Imm(t)) if stubs.trap.contains(&t) => (3, *v),
                _ => return Err(f.err(i1, "unexpected prologue")),
            }
        }
        _ => return Err(f.err(i0, "prologue must start with the entry check (lea rax,[rsp-F])")),
    };
    let (c, jb, s) = match (insns.get(after_rax), insns.get(after_rax + 1), insns.get(after_rax + 2)) {
        (Some(c), Some(jb), Some(s)) => (c, jb, s),
        _ => return Err(f.err(i0, "truncated prologue")),
    };
    let stack_check = [
        Op::Reg(RAX, 64),
        Op::Mem(MemOp { size: 8, seg: None, base: Some(Base::Reg(R14)), index: None, disp: OFF_STACK_LIMIT }),
    ];
    let Op::Imm(slow) = parse_operand(&jb.operands) else {
        return Err(f.err(jb, "entry check must branch to its slow path"));
    };
    if c.mnemonic != "cmp" || operands(c) != stack_check || jb.mnemonic != "jb" {
        return Err(f.err(c, "entry check must compare rax with vmctx.stack_limit, then jb"));
    }
    if !(s.mnemonic == "sub" && matches!(operands(s).as_slice(), [Op::Reg(RSP, 64), Op::Imm(v)] if *v == fsz)) {
        return Err(f.err(s, &format!("expected `sub rsp,{fsz:#x}` right after the entry check")));
    }
    f.frame = fsz;
    let k = after_rax + 3;
    let body = insns.get(k).map_or(end, |i| i.addr);
    // The slow path: a trap stub, or the entry stub `call <stub>; jmp <sub rsp>` reached only from the entry check.
    let mut back_jmp = None;
    if !stubs.trap.contains(&slow) {
        let ks = *index
            .get(&slow)
            .filter(|&&ks| slow >= body && ks > 0)
            .ok_or_else(|| f.err(jb, "entry slow path outside the body"))?;
        let ok = sources.get(&slow) == Some(&1)
            && (!reach[ks - 1] || matches!(insns[ks - 1].mnemonic.as_str(), "jmp" | "ret"))
            && insns[ks].mnemonic == "call"
            && insns.get(ks + 1).is_some_and(|j| j.mnemonic == "jmp" && parse_operand(&j.operands) == Op::Imm(s.addr));
        if !ok {
            return Err(f.err(
                &insns[ks],
                "entry slow path must be `call <stub>; jmp <sub rsp>`, reached only from the entry check",
            ));
        }
        back_jmp = Some(insns[ks + 1].addr);
    }
    // Jumps may enter the prologue only at `sub rsp` from the entry stub.
    let jump_ok = |from: u64, t: u64| -> bool {
        if t >= start && t < body {
            return t == s.addr && Some(from) == back_jmp;
        }
        (t >= body && t < end && index.contains_key(&t)) || stubs.trap.contains(&t)
    };
    f.stats.instructions += k as u64;
    f.stats.vmctx += 1;

    let mode = lay.mode;
    let mut st = State::default();
    let mut prev_lfence = false;
    for (idx, i) in insns.iter().enumerate().skip(k) {
        let m = i.mnemonic.as_str();
        if forbidden(m) {
            return Err(f.err(i, "forbidden instruction"));
        }
        let is_target = sources.contains_key(&i.addr);
        if is_target {
            st = State::default();
        }
        let was_lfence = std::mem::replace(&mut prev_lfence, m == "lfence");
        if !reach[idx] {
            continue;
        }
        if m == "int3" {
            return Err(f.err(i, "int3 reachable"));
        }
        f.stats.instructions += 1;
        let ops = operands(i);
        // ---- control flow
        if m == "ret" {
            if is_target || !is_add_rsp(&insns[idx - 1], f.frame) {
                return Err(f.err(i, "ret must directly follow `add rsp,F` and not be a branch target"));
            }
            st = State::default();
            continue;
        }
        if m == "jmp" || is_jcc(m) || m == "call" {
            match ops.as_slice() {
                [Op::Imm(t)] if Some(*t) == stubs.thunk => match m {
                    "call" => {
                        let c = st.callee[R11 as usize]
                            .ok_or_else(|| f.err(i, "retpoline call with r11 not a host/helper pointer"))?;
                        f.count_callee(c);
                        f.stats.hardened += 1;
                    }
                    "jmp" => {
                        f.table_jump(i, code, &table_of, st.table_target[R11 as usize])?;
                        f.stats.hardened += 1;
                    }
                    _ => return Err(f.err(i, "conditional branch to the retpoline thunk")),
                },
                [Op::Imm(t)] => {
                    let ok =
                        if m == "call" { f.entries.contains(t) || stubs.all.contains(t) } else { jump_ok(i.addr, *t) };
                    if !ok {
                        return Err(f.err(i, "direct branch/call target outside the function body, the function entries and the prelude stubs"));
                    }
                    f.stats.direct_branches += 1;
                }
                [Op::Reg(r, 64)] if m == "jmp" => {
                    match mode {
                        Mode::Jump if *r != R11 => {}
                        Mode::Lfence if *r == R11 && was_lfence => f.stats.hardened += 1,
                        _ => return Err(f.err(i, &format!("indirect jump not in {mode:?} form"))),
                    }
                    f.table_jump(i, code, &table_of, st.table_target[*r as usize])?;
                }
                [Op::Reg(R11, 64)] if m == "call" && mode == Mode::Lfence && was_lfence => {
                    let c = st.callee[R11 as usize]
                        .ok_or_else(|| f.err(i, "call r11 with r11 not a host/helper pointer"))?;
                    f.count_callee(c);
                    f.stats.hardened += 1;
                }
                [Op::Mem(mm)] if m == "call" && mode == Mode::Jump => {
                    let c = callee_slot(lay, &st, mm, i.comment_target)
                        .ok_or_else(|| f.err(i, "indirect call other than a host or helper call"))?;
                    f.count_callee(c);
                }
                _ => return Err(f.err(i, &format!("indirect control transfer not allowed in {mode:?} mode"))),
            }
            st = transfer(lay, &st, i, &ops);
            continue;
        }
        if !known(m) {
            return Err(f.err(i, "instruction not in the whitelist"));
        }
        // ---- rsp
        if let (Some(Op::Reg(RSP, _)), true) = (ops.first(), writes_op0(m)) {
            if !is_add_rsp(i, f.frame) {
                return Err(f.err(i, "rsp modified outside the prologue/epilogue"));
            }
            if !insns.get(idx + 1).is_some_and(|n| n.mnemonic == "ret" && !sources.contains_key(&n.addr)) {
                return Err(f.err(i, "`add rsp,F` must be directly followed by a ret that no branch targets"));
            }
            st.flags = None;
            continue;
        }
        // ---- operands
        if matches!(m, "bt" | "bts" | "btr" | "btc") && matches!(ops.as_slice(), [Op::Mem(_), Op::Reg(..)]) {
            return Err(f.err(i, "bit test on memory with a register bit index"));
        }
        if m == "rep stos" {
            let x = st.rsp_rel[RDI as usize].ok_or_else(|| f.err(i, "rep stos with rdi not frame-relative"))?;
            let n = st.konst[RCX as usize].ok_or_else(|| f.err(i, "rep stos with an unknown count"))?;
            let end = u64::try_from(x).ok().and_then(|x| n.checked_mul(8)?.checked_add(x));
            if end.is_none_or(|e| e > f.frame) {
                return Err(f.err(i, "rep stos outside the frame"));
            }
            f.stats.frame += 1;
        } else {
            for (pos, o) in ops.iter().enumerate() {
                match o {
                    Op::Mem(mm) if m == "lea" => {
                        if mm.base == Some(Base::Rip) {
                            let ok = matches!(ops[0], Op::Reg(R11, 64))
                                && i.comment_target.is_some_and(|t| tables.iter().any(|tb| tb.at == t));
                            if !ok {
                                return Err(
                                    f.err(i, "lea of a RIP-relative address other than a jump table of this function")
                                );
                            }
                        }
                    }
                    Op::Mem(mm) if mm.base == Some(Base::Reg(R11)) => {
                        // The only r11-based access: `movsxd R, DWORD PTR [r11+R*4]` from a table start with R < n.
                        let shape = MemOp { size: 4, seg: None, base: Some(Base::Reg(R11)), index: None, disp: 0 };
                        let x = match (m, pos, &ops[0], mm.index) {
                            ("movsxd", 1, Op::Reg(r, 64), Some((x, 4)))
                                if *r == x && MemOp { index: None, ..mm.clone() } == shape =>
                            {
                                x
                            }
                            _ => return Err(f.err(i, "r11-based access other than the jump-table load")),
                        };
                        let tab =
                            st.table_addr[R11 as usize].and_then(|t| tables.iter().find(|tb| tb.at == t)).ok_or_else(
                                || f.err(i, "table load from an address that is not a jump table of this function"),
                            )?;
                        match st.bound[x as usize] {
                            Some(b) if b == tab.n => {}
                            Some(b) => {
                                return Err(f.err(i, &format!("table index bound {b} != table length {}", tab.n)))
                            }
                            None => return Err(f.err(i, "table index not proven below the table length")),
                        }
                    }
                    Op::Mem(mm) if mm.base == Some(Base::Rip) => {
                        let helper_load = m == "mov"
                            && pos == 1
                            && matches!(ops[0], Op::Reg(_, 64))
                            && callee_slot(lay, &st, mm, i.comment_target) == Some(Callee::Helper);
                        if !helper_load {
                            return Err(f.err(i, "RIP-relative data access"));
                        }
                    }
                    Op::Mem(mm) => f.access(i, mm, &st, pos == 0 && writes_op0(m))?,
                    Op::Other(s) => return Err(f.err(i, &format!("unknown operand `{s}`"))),
                    _ => {}
                }
            }
        }
        // ---- pinned registers
        let written = dst_reg(m, &ops).map(|(r, _)| r).into_iter().chain(implicit_writes(m, ops.len()).iter().copied());
        if let Some(r) = written.into_iter().find(|r| [R13, R14, R15].contains(r)) {
            return Err(f.err(i, &format!("write to pinned register {r} (r13 mask, r14 vmctx, r15 heap base)")));
        }
        st = transfer(lay, &st, i, &ops);
    }
    Ok(f.stats)
}

// ---- whole blobs

/// A blob being verified, decoded segment by segment (each jump table ends a segment).
pub struct Blob<'a> {
    code: &'a [u8],
    lay: Layout,
    thunk: Result<Option<u64>, String>,
    insns: Vec<Insn>,
    tables: Vec<Table>,
    /// Next segment to decode `[from, to)`.
    next: Option<(u64, u64)>,
}

impl<'a> Blob<'a> {
    pub fn new(code: &'a [u8], lay: Layout) -> Blob<'a> {
        let thunk = find_thunk(code, &lay);
        let next = Some((0, lay.helper_table as u64));
        Blob { code, lay, thunk, insns: Vec::new(), tables: Vec::new(), next }
    }

    /// Feeds the pending segment's decoding; a jump through a jump table ends it, the next segment starts after the table.
    fn feed(&mut self, mut seg: Vec<Insn>) {
        let (from, to) = self.next.take().unwrap();
        for i in seg.iter_mut() {
            i.addr += from;
            if let Some(t) = i.comment_target.as_mut() {
                *t = t.wrapping_add(from);
            }
            if i.mnemonic == "jmp" || is_jcc(&i.mnemonic) || i.mnemonic == "call" {
                if let Some(v) = i.operands.strip_prefix("0x").and_then(|h| u64::from_str_radix(h, 16).ok()) {
                    i.operands = format!("0x{:x}", v.wrapping_add(from));
                }
            }
        }
        let base = self.insns.len();
        self.insns.extend(seg);
        let first = self.lay.first_func();
        let entries: BTreeSet<u64> = self.lay.funcs.iter().map(|&(o, _)| o as u64).collect();
        let thunk = self.thunk.clone().ok().flatten();
        let mut st = State::default();
        for k in base..self.insns.len() {
            let i = &self.insns[k];
            if i.addr < first {
                continue;
            }
            if entries.contains(&i.addr) {
                st = State::default();
            }
            let ops = operands(i);
            if i.mnemonic == "jmp" {
                let tt = match ops.as_slice() {
                    [Op::Reg(r, 64)] => st.table_target[*r as usize],
                    [Op::Imm(t)] if Some(*t) == thunk => st.table_target[R11 as usize],
                    _ => None,
                };
                if let Some((at, n)) = tt.filter(|&(at, n)| at >= i.end() && n > 0 && n < (1 << 31) && at + 4 * n <= to)
                {
                    self.tables.push(Table { jmp_at: i.addr, at, n });
                    self.insns.truncate(self.insns.iter().position(|i| i.addr >= at).unwrap_or(self.insns.len()));
                    self.next = (at + 4 * n < to).then_some((at + 4 * n, to));
                    return;
                }
            }
            st = transfer(&self.lay, &st, i, &ops);
        }
    }

    /// Checks the decoded blob; returns its stats and prelude bytes.
    fn verify(&self) -> Result<(Stats, Vec<u8>), String> {
        let lay = &self.lay;
        let first = lay.first_func();
        let prelude: Vec<Insn> = self.insns.iter().filter(|i| i.addr < first).cloned().collect();
        let stubs = prelude_stubs(&prelude, lay.mode, self.thunk.clone()?)?;
        let mut stats = Stats { modules: 1, ..Stats::default() };
        for (fi, &(off, params)) in lay.funcs.iter().enumerate() {
            let (start, end) = (off as u64, lay.funcs.get(fi + 1).map_or(lay.helper_table as u64, |f| f.0 as u64));
            let insns: Vec<Insn> = self.insns.iter().filter(|i| i.addr >= start && i.addr < end).cloned().collect();
            let tables: Vec<Table> = self.tables.iter().filter(|t| t.at >= start && t.at < end).copied().collect();
            let s = verify_function(lay, self.code, &stubs, start, end, params, &insns, &tables)
                .map_err(|e| format!("{e}\n{}", listing(&insns)))?;
            stats.add(&s);
        }
        Ok((stats, self.code[..first as usize].to_vec()))
    }
}

fn listing(insns: &[Insn]) -> String {
    insns.iter().take(400).map(|i| format!("  {:6x}: {}\n", i.addr, i.text())).collect()
}

/// Decodes (batched objdump runs) and verifies blobs; returns merged stats and every blob's prelude.
pub fn verify_blobs(objdump: &std::path::Path, blobs: &mut [Blob]) -> Result<(Stats, Vec<Vec<u8>>), String> {
    loop {
        let pending: Vec<usize> = (0..blobs.len()).filter(|&i| blobs[i].next.is_some()).collect();
        if pending.is_empty() {
            break;
        }
        let segs: Vec<&[u8]> = pending
            .iter()
            .map(|&i| {
                let (a, b) = blobs[i].next.unwrap();
                &blobs[i].code[a as usize..b as usize]
            })
            .collect();
        let decoded = disassemble_many(objdump, &segs);
        for (&i, d) in pending.iter().zip(decoded) {
            blobs[i].feed(d);
        }
    }
    let mut stats = Stats::default();
    let mut preludes = Vec::new();
    for b in blobs.iter() {
        let (s, p) = b.verify()?;
        stats.add(&s);
        preludes.push(p);
    }
    Ok((stats, preludes))
}
