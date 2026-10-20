//! SFI verifier (VeriWasm-style) for k10-jit output, independent of the codegen: checks the docs/JIT-ABI.md
//! invariants on objdump's decoding, knowing only the blob metadata and the [`Mode`]. Every function instruction must
//! satisfy:
//! * whitelisted instructions only (integer/SSE2 ops, moves, lea, cmov/setcc, jmp/jcc/call/ret, rep stos, lfence,
//!   int3 as unreachable padding); never syscall/sysenter/int/rdtsc(p)/rdpmc/cpuid/port I/O/far or privileged ops;
//! * r13 (mask), r14 (vmctx), r15 (heap base) never written; rsp only by the prologue's `sub rsp,F` and an
//!   `add rsp,F` right before `ret`; no push/pop;
//! * entry safepoint: `rsp-F` compared with vmctx.stack_limit (`cmp rax,[r14+0x10]; jb`) before `sub rsp,F`, same F;
//! * memory accesses (≤ 8 bytes): guest `[r15+R*1]` with R from `and R,r13` unclobbered on the straight-line path
//!   (state resets at branch targets, calls and after jmp/ret) or `[r15+c]`, 0 ≤ c < 2^min(min_mem_log2,31); frame
//!   `[rsp+d]` in [0,F) or the parameters [F+8, F+8+8*params), never the return address; `rep stos` as `lea
//!   rdi,[rsp+X]; mov ecx,N` inside [0,F); vmctx reads of +0x10/+0x20/+0x30 only; reads of a host-function slot
//!   (`[H+8k]`, H = `mov H,[r14+0x20]`, k < imports) or a helper slot (`[rip+X]`, X in the helper table); nothing else;
//! * calls: direct to function entries or prelude stubs; indirect host/helper calls only, in the mode's form:
//!   Jump `call [H+8k]` / `call [rip+X]`, Lfence `lfence; call r11`, Retpoline `call thunk`, r11 loaded from such a slot;
//! * jumps: direct targets inside the function or prelude trap stubs; no fall-through past the end; a jump table only
//!   as `cmp`-proven index → `movsxd R,[r11+R*4]` (r11 = `lea [rip+T]`) → Jump `add R,r11; jmp R` | Lfence
//!   `add r11,R; lfence; jmp r11` | Retpoline `add r11,R; jmp thunk`, the entries right after the jump (int3 padding),
//!   each a boundary of the function; the proven bound must equal the entry count. Bounds: `cmp R,n`+`jae` (or
//!   `cmp R,n-1`+`ja`), `cmp R,n`+`cmovae R,S` (`cmp R,n-1`+`cmova`) with S a known constant c (bound max(n, c+1):
//!   the codegen's `mov r11d,n; cmp R,r11; cmovae R,r11` gives n+1 = its table of n cases + default),
//!   `cmp R,n`+`sbb M,M`+`and R,M`, or `and R,imm` (bound imm+1);
//! * the retpoline thunk (`call 1f; pause; lfence; jmp .-2; 1: mov [rsp],r11; ret`) exists, byte-exact and 16-aligned
//!   in the prelude, in Retpoline mode only; functions reach it only by the calls and table jumps above.
//!
//! Trusted, not covered: the prelude (checked identical per mode, its indirect branches in the mode's form), the
//! bulk-memory helpers, host functions.

use super::Insn;
use std::collections::{BTreeSet, HashMap};

pub const OFF_STACK_LIMIT: i64 = 0x10;
pub const OFF_HOST_FNS: i64 = 0x20;
pub const OFF_TRAP_REQUEST: i64 = 0x30;
pub const OFF_INTERRUPT: i64 = 0x28;
pub const OFF_SAVED_RSP: i64 = 0x40;

/// The retpoline thunk of docs/JIT-ABI.md.
pub const THUNK: [u8; 17] = [0xE8, 7, 0, 0, 0, 0xF3, 0x90, 0x0F, 0xAE, 0xE8, 0xEB, 0xF9, 0x4C, 0x89, 0x1C, 0x24, 0xC3];

/// Indirect-branch form the blob was compiled with (`k10_jit::IndirectBranch`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Mode {
    Jump,
    Lfence,
    Retpoline,
}

/// What the verifier needs to know about a blob (from `k10_jit::BlobMeta` and the compile options).
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
        self.funcs.first().map(|f| f.0 as u64).unwrap_or(self.helper_table as u64)
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

// ------------------------------------------------------------------------------------------------- operands

const RSP: u8 = 4;
const RAX: u8 = 0;
const RCX: u8 = 1;
const RDX: u8 = 2;
const RDI: u8 = 7;
const R11: u8 = 11;
const R13: u8 = 13;
const R14: u8 = 14;
const R15: u8 = 15;

fn reg(name: &str) -> Option<(u8, u8)> {
    const R64: [&str; 16] = ["rax", "rcx", "rdx", "rbx", "rsp", "rbp", "rsi", "rdi", "r8", "r9", "r10", "r11", "r12", "r13", "r14", "r15"];
    const R32: [&str; 16] =
        ["eax", "ecx", "edx", "ebx", "esp", "ebp", "esi", "edi", "r8d", "r9d", "r10d", "r11d", "r12d", "r13d", "r14d", "r15d"];
    const R16: [&str; 16] = ["ax", "cx", "dx", "bx", "sp", "bp", "si", "di", "r8w", "r9w", "r10w", "r11w", "r12w", "r13w", "r14w", "r15w"];
    const R8: [&str; 16] = ["al", "cl", "dl", "bl", "spl", "bpl", "sil", "dil", "r8b", "r9b", "r10b", "r11b", "r12b", "r13b", "r14b", "r15b"];
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
    /// Access width in bytes (0: none given, e.g. `lea`).
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
    if let Some(h) = s.strip_prefix("0x") {
        u64::from_str_radix(h, 16).ok()
    } else if s == "1" {
        Some(1) // `shl rax,1` (D1 form)
    } else {
        None
    }
}

fn parse_mem(s: &str) -> Option<MemOp> {
    let mut s = s.trim();
    let mut size = 0u8;
    for (name, n) in [("BYTE PTR ", 1u8), ("WORD PTR ", 2), ("DWORD PTR ", 4), ("QWORD PTR ", 8), ("XMMWORD PTR ", 16), ("TBYTE PTR ", 10), ("YMMWORD PTR ", 32), ("FWORD PTR ", 6)] {
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
    let inner = match s.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
        Some(i) => i,
        None => {
            // `ds:0x1234`: absolute address.
            return Some(MemOp { size, seg: seg.or(Some("abs".into())), base: None, index: None, disp: parse_num(s)? as i64 });
        }
    };
    let mut m = MemOp { size, seg, base: None, index: None, disp: 0 };
    let mut rest = inner;
    let mut sign = 1i64;
    let mut first = true;
    while !rest.is_empty() {
        let end = rest[1..].find(['+', '-']).map(|i| i + 1).unwrap_or(rest.len());
        let mut term = &rest[..end];
        rest = &rest[end..];
        if let Some(t) = term.strip_prefix('+') {
            term = t;
            sign = 1;
        } else if let Some(t) = term.strip_prefix('-') {
            term = t;
            sign = -1;
        } else if !first {
            return None;
        }
        first = false;
        if term == "rip" {
            m.base = Some(Base::Rip);
        } else if let Some((r, sc)) = term.split_once('*') {
            let (id, bits) = reg(r)?;
            if bits != 64 {
                return None;
            }
            let scale: u8 = sc.parse().ok()?;
            m.index = Some((id, scale));
        } else if let Some((id, bits)) = reg(term) {
            if bits != 64 || m.base.is_some() {
                return None;
            }
            m.base = Some(Base::Reg(id));
        } else {
            let v = parse_num(term)? as i64;
            m.disp = m.disp.wrapping_add(sign.wrapping_mul(v));
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

// --------------------------------------------------------------------------------------------- classification

const CC: [&str; 16] = ["o", "no", "b", "ae", "e", "ne", "be", "a", "s", "ns", "p", "np", "l", "ge", "le", "g"];

fn is_jcc(m: &str) -> bool {
    m.strip_prefix('j').is_some_and(|c| CC.contains(&c))
}

/// Instructions that read operand 0 without writing it.
fn reads_only(m: &str) -> bool {
    matches!(m, "cmp" | "test" | "bt" | "ucomisd")
}

/// The whitelist (besides control flow, handled separately).
fn known(m: &str) -> bool {
    matches!(
        m,
        "mov" | "movabs" | "movzx" | "movsx" | "movsxd" | "lea"
            | "add" | "or" | "adc" | "sbb" | "and" | "sub" | "xor" | "cmp" | "test"
            | "imul" | "mul" | "div" | "idiv" | "not" | "neg" | "cqo"
            | "shl" | "shr" | "sar" | "rol" | "ror"
            | "bt" | "bts" | "btr" | "btc" | "bsr" | "bsf" | "lzcnt" | "tzcnt" | "popcnt"
            | "movq" | "addsd" | "subsd" | "mulsd" | "divsd" | "sqrtsd" | "ucomisd" | "cvtsi2sd" | "cvttsd2si" | "xorpd"
            | "rep stos" | "lfence"
    ) || m.strip_prefix("cmov").is_some_and(|c| CC.contains(&c))
        || m.strip_prefix("set").is_some_and(|c| CC.contains(&c))
        || (m.starts_with("cmp") && m.ends_with("sd") && m.len() > 5)
}

/// Mnemonics that must never appear anywhere in generated code (clearer messages than "not whitelisted").
fn forbidden(m: &str) -> bool {
    let base = m.rsplit(' ').next().unwrap_or(m);
    matches!(
        base,
        "syscall" | "sysenter" | "sysexit" | "sysret" | "int" | "int1" | "into" | "iret" | "iretd" | "iretq"
            | "rdtsc" | "rdtscp" | "rdpmc" | "rdmsr" | "wrmsr" | "cpuid" | "hlt" | "in" | "out" | "ins" | "outs"
            | "insb" | "insd" | "outsb" | "outsd" | "cli" | "sti" | "lgdt" | "lidt" | "lldt" | "ltr" | "sgdt" | "sidt"
            | "ljmp" | "lcall" | "lret" | "retf" | "xgetbv" | "xsetbv" | "wbinvd" | "invd" | "invlpg" | "swapgs"
            | "rdfsbase" | "rdgsbase" | "wrfsbase" | "wrgsbase" | "vmcall" | "vmmcall" | "ud0" | "ud1" | "ud2"
            | "(bad)"
    )
}

// ------------------------------------------------------------------------------------------------ the prelude

/// Prelude entry points usable by functions.
#[derive(Default)]
pub struct Stubs {
    /// Call targets: blocks right after an unconditional jmp/ret (not padding, not inside the thunk).
    pub all: BTreeSet<u64>,
    /// Jump targets: trap stubs only (a jump to a returning stub would `ret` into frame data).
    pub trap: BTreeSet<u64>,
    /// The retpoline thunk (Retpoline mode).
    pub thunk: Option<u64>,
}

/// The retpoline thunk's offset: exactly one 16-aligned copy in the prelude in Retpoline mode, none otherwise.
pub fn find_thunk(code: &[u8], lay: &Layout) -> Result<Option<u64>, String> {
    let first = (lay.first_func() as usize).min(code.len());
    let hits: Vec<usize> = code[..first].windows(THUNK.len()).enumerate().filter(|(_, w)| *w == THUNK).map(|(p, _)| p).collect();
    match (lay.mode, hits.as_slice()) {
        (Mode::Retpoline, [p]) if p % 16 == 0 => Ok(Some(*p as u64)),
        (Mode::Retpoline, _) => Err(format!("prelude: retpoline thunk missing, misaligned or duplicated (at {hits:?})")),
        (_, []) => Ok(None),
        (m, _) => Err(format!("prelude: retpoline thunk present in {m:?} mode")),
    }
}

/// Checks the prelude (forbidden instructions, indirect branches in the mode's form) and classifies its stubs.
pub fn prelude_stubs(prelude: &[Insn], mode: Mode, thunk: Option<u64>) -> Result<Stubs, String> {
    let in_thunk = |a: u64| thunk.is_some_and(|t| a >= t && a < t + THUNK.len() as u64);
    let mut st = Stubs { thunk, ..Stubs::default() };
    let mut after_uncond = false;
    let mut prev = "";
    for i in prelude {
        if forbidden(&i.mnemonic) {
            return Err(format!("prelude: forbidden instruction `{}` at {:#x}", i.text, i.addr));
        }
        let m = i.mnemonic.as_str();
        if (m == "call" || m == "jmp") && !i.operands.starts_with("0x") {
            let ok = match mode {
                Mode::Jump => m == "call" && (i.operands == "rsi" || i.operands == "QWORD PTR [r14+0x28]"),
                Mode::Lfence => m == "call" && i.operands == "r11" && prev == "lfence",
                Mode::Retpoline => false,
            };
            if !ok {
                return Err(format!("prelude: indirect branch `{}` at {:#x} not in {mode:?} form", i.text, i.addr));
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
        let mut k = at[&s0];
        for _ in 0..256 {
            let i = &prelude[k];
            if i.mnemonic == "ret" {
                break;
            }
            if i.mnemonic == "mov" && i.operands == format!("rsp,QWORD PTR [r14+{OFF_SAVED_RSP:#x}]") {
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

// ------------------------------------------------------------------------------------------- straight-line facts

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

/// Facts on the straight-line path (reset at branch targets, calls, after jmp/ret).
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
    /// Flags hold `cmp R, v` (an immediate or a register of known value).
    flags: Option<(u8, u64)>,
    /// `sbb M,M` after `cmp R,n`: M = (R < n) ? !0 : 0.
    lt_mask: [Option<(u8, u64)>; 16],
    /// `lea R,[rip+T]`: a jump table address.
    table_addr: [Option<u64>; 16],
    /// `movsxd R,[r11+R*4]` from table (T, bound of the index).
    table_entry: [Option<(u64, u64)>; 16],
    /// Entry + table address: a target from table (T, bound).
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

/// Registers written implicitly.
fn implicit_writes(m: &str, nops: usize) -> &'static [u8] {
    match (m, nops) {
        ("mul" | "div" | "idiv", _) | ("imul", 1) => &[RAX, RDX],
        ("cqo", _) => &[RDX],
        ("rep stos", _) => &[RDI, RCX],
        _ => &[],
    }
}

/// Explicit destination register.
fn dst_reg(m: &str, ops: &[Op]) -> Option<(u8, u8)> {
    match ops.first() {
        Some(Op::Reg(r, bits)) if writes_op0(m) && !matches!(m, "mul" | "div" | "idiv") && !(m == "imul" && ops.len() == 1) => {
            Some((*r, *bits))
        }
        _ => None,
    }
}

/// A host-function slot (`[H+8k]`, k < imports) or a helper slot (`[rip+X]`) read as a 64-bit value.
fn callee_slot(lay: &Layout, st: &State, mm: &MemOp, rip_target: Option<u64>) -> Option<Callee> {
    if mm.size != 8 || mm.seg.is_some() || mm.index.is_some() {
        return None;
    }
    match mm.base {
        Some(Base::Reg(b)) if st.host_fns & (1 << b) != 0 => {
            (mm.disp >= 0 && mm.disp % 8 == 0 && ((mm.disp / 8) as usize) < lay.num_imports).then_some(Callee::Host)
        }
        Some(Base::Rip) => {
            let t = rip_target?;
            let h0 = lay.helper_table as u64;
            (t >= h0 && t < h0 + 8 * lay.num_helpers as u64 && (t - h0).is_multiple_of(8)).then_some(Callee::Helper)
        }
        _ => None,
    }
}

fn tighten(b: &mut Option<u64>, v: u64) {
    *b = Some(b.map_or(v, |o| o.min(v)));
}

/// The facts after `i` (no checks: those are in [`verify_function`]). Shared by table discovery and verification.
fn transfer(lay: &Layout, st: &State, i: &Insn, ops: &[Op]) -> State {
    let m = i.mnemonic.as_str();
    if matches!(m, "call" | "jmp" | "ret") {
        return State::default();
    }
    let mut new = st.clone();
    if is_jcc(m) {
        // Fall-through facts of a bounds check.
        if let Some((r, n)) = st.flags {
            match m {
                "jae" => tighten(&mut new.bound[r as usize], n),
                "ja" => {
                    if let Some(b) = n.checked_add(1) {
                        tighten(&mut new.bound[r as usize], b)
                    }
                }
                _ => {}
            }
        }
        return new;
    }
    for &r in implicit_writes(m, ops.len()) {
        new.write(r);
    }
    if let Some((r, bits)) = dst_reg(m, ops) {
        let ri = r as usize;
        new.write(r);
        match (m, ops) {
            ("and", [_, Op::Reg(R13, 64)]) if bits == 64 => new.masked |= 1 << r,
            ("and", [_, Op::Imm(v)]) => {
                let v = if bits == 32 { *v & 0xffff_ffff } else { *v };
                if v < (1 << 40) {
                    new.bound[ri] = Some(v + 1);
                }
            }
            ("and", [_, Op::Reg(mreg, 64)]) => {
                if let Some((x, n)) = st.lt_mask[*mreg as usize] {
                    if x == r {
                        new.bound[ri] = Some(n);
                    }
                }
            }
            ("mov", [_, Op::Imm(v)]) => new.konst[ri] = Some(if bits == 32 { *v & 0xffff_ffff } else { *v }),
            ("movabs", [_, Op::Imm(v)]) => new.konst[ri] = Some(*v),
            ("xor", [Op::Reg(a, _), Op::Reg(b, _)]) if a == b => new.konst[ri] = Some(0),
            ("lea", [_, Op::Mem(MemOp { base: Some(Base::Reg(RSP)), index: None, disp, .. })]) => new.rsp_rel[ri] = Some(*disp),
            ("lea", [_, Op::Mem(MemOp { base: Some(Base::Rip), .. })]) if r == R11 => new.table_addr[ri] = i.comment_target,
            ("mov", [_, Op::Mem(mm)]) if bits == 64 => {
                if mm.base == Some(Base::Reg(R14)) && mm.index.is_none() && mm.disp == OFF_HOST_FNS {
                    new.host_fns |= 1 << r;
                }
                new.callee[ri] = callee_slot(lay, st, mm, i.comment_target);
            }
            // M = (R < n) ? !0 : 0, valid while R is unchanged (and R is not M itself).
            ("sbb", [Op::Reg(a, 64), Op::Reg(b, 64)]) if a == b => new.lt_mask[ri] = st.flags.filter(|&(x, n)| x != r && n > 0),
            ("movsxd", [_, Op::Mem(MemOp { base: Some(Base::Reg(R11)), index: Some((x, 4)), disp: 0, size: 4, seg: None })]) => {
                if let (Some(t), Some(b), true) = (st.table_addr[R11 as usize], st.bound[*x as usize], *x == r) {
                    new.table_entry[ri] = Some((t, b));
                }
            }
            ("add", [_, Op::Reg(src, 64)]) if bits == 64 => {
                // Jump form `add R, r11`, or Lfence/Retpoline form `add r11, R`.
                let entry = if *src == R11 { st.table_entry[ri] } else if r == R11 { st.table_entry[*src as usize] } else { None };
                if let (Some((t, n)), Some(t2)) = (entry, st.table_addr[R11 as usize]) {
                    if t == t2 {
                        new.table_target[ri] = Some((t, n));
                    }
                }
            }
            (c, [_, Op::Reg(s, 64)]) if c.starts_with("cmov") && bits == 64 => {
                // R stays or becomes cv: max(bound, cv+1); after `cmp R,n` a cmovae (cmova: n+1) gives max(n, cv+1).
                let mut cands = Vec::new();
                if let Some(cv1) = st.konst[*s as usize].and_then(|cv| cv.checked_add(1)) {
                    if let Some(b) = st.bound[ri] {
                        cands.push(b.max(cv1));
                    }
                    if let Some((x, n)) = st.flags.filter(|&(x, _)| x == r) {
                        let lim = match c {
                            "cmovae" => Some(n),
                            "cmova" => n.checked_add(1),
                            _ => None,
                        };
                        let _ = x;
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

// ------------------------------------------------------------------------------------------------ verification

struct Fn<'a> {
    lay: &'a Layout,
    stubs: &'a Stubs,
    entries: &'a BTreeSet<u64>,
    start: u64,
    end: u64,
    params: u64,
    frame: Option<u64>,
    stats: Stats,
}

impl<'a> Fn<'a> {
    fn err(&self, i: &Insn, what: &str) -> String {
        format!("function at {:#x}: {:#x} `{}` ({:02x?}): {what}", self.start, i.addr, i.text, i.bytes)
    }

    /// Checks one memory access (not `lea`).
    fn access(&mut self, i: &Insn, m: &MemOp, st: &State, write: bool) -> Result<(), String> {
        if m.seg.is_some() {
            return Err(self.err(i, "segment override / absolute address"));
        }
        if m.size > 8 {
            return Err(self.err(i, "access wider than 8 bytes"));
        }
        let w = m.size.max(1) as i64;
        match (&m.base, m.index) {
            // Guest memory, masked index.
            (Some(Base::Reg(R15)), Some((x, 1))) if m.disp == 0 => {
                if st.masked & (1 << x) == 0 {
                    return Err(self.err(i, "guest access through an index not masked with r13"));
                }
                self.stats.heap_masked += 1;
                Ok(())
            }
            // Guest memory, folded constant address.
            (Some(Base::Reg(R15)), None) => {
                let limit = 1i64 << self.lay.min_mem_log2.min(31);
                if m.disp < 0 || m.disp >= limit {
                    return Err(self.err(i, &format!("constant guest address outside [0, {limit:#x})")));
                }
                self.stats.heap_const += 1;
                Ok(())
            }
            // The frame.
            (Some(Base::Reg(RSP)), None) => {
                let f = self.frame.ok_or_else(|| self.err(i, "frame access before the prologue's `sub rsp`"))? as i64;
                let d = m.disp;
                let in_frame = d >= 0 && d + w <= f;
                let in_params = d >= f + 8 && d + w <= f + 8 + 8 * self.params as i64;
                if !(in_frame || in_params) {
                    return Err(self.err(i, &format!("frame access outside [0,{f:#x}) and the parameter area (return address at {f:#x})")));
                }
                self.stats.frame += 1;
                Ok(())
            }
            // vmctx: reads of three fields.
            (Some(Base::Reg(R14)), None) => {
                if write {
                    return Err(self.err(i, "write to vmctx"));
                }
                if ![OFF_STACK_LIMIT, OFF_HOST_FNS, OFF_TRAP_REQUEST].contains(&m.disp) {
                    return Err(self.err(i, "vmctx field not readable by generated code"));
                }
                self.stats.vmctx += 1;
                Ok(())
            }
            // Jump-table load (its bound is checked by the movsxd rule).
            (Some(Base::Reg(R11)), Some((_, 4))) if m.disp == 0 && m.size == 4 => {
                st.table_addr[R11 as usize].map(|_| ()).ok_or_else(|| self.err(i, "r11 is not a jump table address"))
            }
            // Host-function slot.
            (Some(Base::Reg(b)), None) if st.host_fns & (1 << b) != 0 => {
                if write || callee_slot(self.lay, st, m, None).is_none() {
                    return Err(self.err(i, "host-function table access outside the imports"));
                }
                Ok(())
            }
            _ => Err(self.err(i, "memory operand outside guest memory, frame and vmctx")),
        }
    }

    fn target_ok(&self, t: u64, call: bool, boundaries: &BTreeSet<u64>) -> bool {
        if call {
            self.entries.contains(&t) || self.stubs.all.contains(&t)
        } else {
            (t >= self.start && t < self.end && boundaries.contains(&t)) || self.stubs.trap.contains(&t)
        }
    }

    /// The table jump at `i` through `target` (its table, bound, padding).
    fn table_jump(&mut self, i: &Insn, code: &[u8], table_of: &HashMap<u64, Table>, target: Option<(u64, u64)>) -> Result<(), String> {
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
    let mut f = Fn { lay, stubs, entries: &entries, start, end, params: params as u64, frame: None, stats: Stats::default() };
    f.stats.functions = 1;
    let boundaries: BTreeSet<u64> = insns.iter().map(|i| i.addr).collect();
    // Coverage: instructions, tables and padding tile [start, end) exactly.
    {
        let mut pos = start;
        let mut ti = tables.iter().peekable();
        for i in insns {
            while let Some(t) = ti.peek() {
                if t.at == pos {
                    pos += 4 * t.n;
                    ti.next();
                } else {
                    break;
                }
            }
            if i.addr != pos {
                return Err(format!("function at {start:#x}: gap or overlap at {pos:#x} (next instruction at {:#x})", i.addr));
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
    }
    // Branch targets (state resets there): direct jumps and table entries.
    let mut targets: BTreeSet<u64> = BTreeSet::new();
    for i in insns {
        if (i.mnemonic == "jmp" || is_jcc(&i.mnemonic)) && i.operands.starts_with("0x") {
            if let Op::Imm(t) = parse_operand(&i.operands) {
                targets.insert(t);
            }
        }
    }
    let mut table_of: HashMap<u64, Table> = HashMap::new();
    for t in tables {
        if t.at < start || t.at + 4 * t.n > end {
            return Err(format!("function at {start:#x}: jump table {t:?} outside the function"));
        }
        for k in 0..t.n {
            let p = (t.at + 4 * k) as usize;
            let rel = i32::from_le_bytes(code[p..p + 4].try_into().unwrap()) as i64;
            let tgt = (t.at as i64 + rel) as u64;
            if !(tgt >= start && tgt < end && boundaries.contains(&tgt)) {
                return Err(format!("function at {start:#x}: jump table at {:#x} entry {k} → {tgt:#x} is not an instruction of the function", t.at));
            }
            targets.insert(tgt);
            f.stats.table_entries += 1;
        }
        table_of.insert(t.jmp_at, *t);
        f.stats.tables += 1;
    }

    // ---- the prologue: entry safepoint, then `sub rsp, F`.
    let k;
    {
        let i0 = insns.first().ok_or("empty function")?;
        let (after_rax, fsz) = match (i0.mnemonic.as_str(), operands(i0).as_slice()) {
            ("lea", [Op::Reg(RAX, 64), Op::Mem(MemOp { base: Some(Base::Reg(RSP)), index: None, disp, .. })]) if *disp <= 0 => {
                (1, (-*disp) as u64)
            }
            ("mov", [Op::Reg(RAX, 64), Op::Reg(RSP, 64)]) => {
                let i1 = insns.get(1).ok_or("truncated prologue")?;
                let i2 = insns.get(2).ok_or("truncated prologue")?;
                match (i1.mnemonic.as_str(), operands(i1).as_slice(), i2.mnemonic.as_str()) {
                    ("sub", [Op::Reg(RAX, 64), Op::Imm(v)], "jb") => {
                        // jb: stack overflow trap (rsp - F wrapped)
                        let Op::Imm(t) = parse_operand(&i2.operands) else { return Err(f.err(i2, "bad jb")) };
                        if !stubs.trap.contains(&t) {
                            return Err(f.err(i2, "wrap check must go to a prelude trap stub"));
                        }
                        (3, *v)
                    }
                    _ => return Err(f.err(i1, "unexpected prologue")),
                }
            }
            _ => return Err(f.err(i0, "prologue must start with the entry safepoint (lea rax,[rsp-F])")),
        };
        let c = insns.get(after_rax).ok_or("truncated prologue")?;
        let ok_cmp = c.mnemonic == "cmp"
            && matches!(operands(c).as_slice(), [Op::Reg(RAX, 64), Op::Mem(MemOp { base: Some(Base::Reg(R14)), index: None, disp: OFF_STACK_LIMIT, size: 8, seg: None })]);
        let jb = insns.get(after_rax + 1).ok_or("truncated prologue")?;
        let s = insns.get(after_rax + 2).ok_or("truncated prologue")?;
        if !ok_cmp || jb.mnemonic != "jb" {
            return Err(f.err(c, "entry safepoint must compare rax with vmctx.stack_limit then jb"));
        }
        match (s.mnemonic.as_str(), operands(s).as_slice()) {
            ("sub", [Op::Reg(RSP, 64), Op::Imm(v)]) if *v == fsz => f.frame = Some(fsz),
            _ => return Err(f.err(s, &format!("expected `sub rsp,{fsz:#x}` right after the entry safepoint"))),
        }
        k = after_rax + 3;
        f.stats.instructions += k as u64;
        f.stats.vmctx += 1;
    }
    let mode = lay.mode;
    let mut st = State::default();
    let mut prev_uncond = false;
    let mut prev_lfence = false;
    let mut restored = false;
    for (idx, i) in insns.iter().enumerate() {
        let m = i.mnemonic.as_str();
        if forbidden(m) {
            return Err(f.err(i, "forbidden instruction"));
        }
        let is_target = targets.contains(&i.addr) || i.addr == start;
        if is_target {
            st = State::default();
        }
        let was_lfence = std::mem::replace(&mut prev_lfence, m == "lfence");
        let was_restored = std::mem::replace(&mut restored, false);
        // int3: padding only (never reached by fall-through, never a target).
        if m == "int3" {
            if !prev_uncond || is_target {
                return Err(f.err(i, "int3 reachable"));
            }
            continue;
        }
        if idx < k {
            // Prologue (matched above): rax/flags clobbered, branch targets checked.
            if let ("jb", Op::Imm(t)) = (m, parse_operand(&i.operands)) {
                if !f.target_ok(t, false, &boundaries) {
                    return Err(f.err(i, "prologue branch target"));
                }
            }
            st.write(RAX);
            st.flags = None;
            prev_uncond = false;
            continue;
        }
        f.stats.instructions += 1;
        let ops = operands(i);
        // ---------------- control flow
        if m == "ret" {
            if !was_restored {
                return Err(f.err(i, "ret without the matching `add rsp,F` right before it"));
            }
            prev_uncond = true;
            st = State::default();
            continue;
        }
        if m == "jmp" || is_jcc(m) || m == "call" {
            match ops.as_slice() {
                [Op::Imm(t)] if Some(*t) == stubs.thunk => match m {
                    // Retpoline call: r11 holds a host/helper pointer.
                    "call" => {
                        let c = st.callee[R11 as usize].ok_or_else(|| f.err(i, "retpoline call with r11 not a host/helper pointer"))?;
                        f.count_callee(c);
                        f.stats.hardened += 1;
                    }
                    // Retpoline table jump: r11 = entry + table.
                    "jmp" => {
                        f.table_jump(i, code, &table_of, st.table_target[R11 as usize])?;
                        f.stats.hardened += 1;
                    }
                    _ => return Err(f.err(i, "conditional branch to the retpoline thunk")),
                },
                [Op::Imm(t)] => {
                    if !f.target_ok(*t, m == "call", &boundaries) {
                        return Err(f.err(i, "direct branch/call target outside the function, the function entries and the prelude stubs"));
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
                    let c = st.callee[R11 as usize].ok_or_else(|| f.err(i, "call r11 with r11 not a host/helper pointer"))?;
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
            prev_uncond = m == "jmp";
            st = transfer(lay, &st, i, &ops);
            continue;
        }
        prev_uncond = false;
        if !known(m) {
            return Err(f.err(i, "instruction not in the whitelist"));
        }
        // ---------------- rsp
        if let Some(Op::Reg(RSP, _)) = ops.first() {
            if writes_op0(m) {
                match (m, ops.as_slice()) {
                    ("add", [_, Op::Imm(v)]) if Some(*v) == f.frame => {
                        restored = true;
                        st.flags = None;
                        continue;
                    }
                    _ => return Err(f.err(i, "rsp modified outside the prologue/epilogue")),
                }
            }
        }
        // ---------------- memory operands
        if m == "rep stos" {
            // rep stos QWORD PTR es:[rdi],rax: local zeroing.
            let x = st.rsp_rel[RDI as usize].ok_or_else(|| f.err(i, "rep stos with rdi not frame-relative"))?;
            let n = st.konst[RCX as usize].ok_or_else(|| f.err(i, "rep stos with unknown count"))? as i64;
            let fr = f.frame.unwrap() as i64;
            if x < 0 || x + 8 * n > fr {
                return Err(f.err(i, "rep stos outside the frame"));
            }
            f.stats.frame += 1;
        } else {
            for (pos, o) in ops.iter().enumerate() {
                if let Op::Mem(mm) = o {
                    if m == "lea" {
                        continue;
                    }
                    if mm.base == Some(Base::Rip) {
                        // Only a helper slot read into a register.
                        if !(m == "mov" && pos == 1 && callee_slot(lay, &st, mm, i.comment_target) == Some(Callee::Helper)) {
                            return Err(f.err(i, "RIP-relative data access"));
                        }
                        continue;
                    }
                    f.access(i, mm, &st, pos == 0 && writes_op0(m))?;
                }
                if let Op::Other(s) = o {
                    return Err(f.err(i, &format!("unknown operand `{s}`")));
                }
            }
        }
        // ---------------- pinned registers
        let written = dst_reg(m, &ops).map(|(r, _)| r).into_iter().chain(implicit_writes(m, ops.len()).iter().copied());
        for r in written {
            if r == R13 || r == R14 || r == R15 {
                return Err(f.err(i, "write to a pinned register (r13 mask, r14 vmctx, r15 heap base)"));
            }
        }
        // ---------------- bound proof at the table load
        if let ("movsxd", [_, Op::Mem(MemOp { base: Some(Base::Reg(R11)), index: Some((x, 4)), .. })]) = (m, ops.as_slice()) {
            let t = st.table_addr[R11 as usize].ok_or_else(|| f.err(i, "table load without lea r11"))?;
            let tab = tables.iter().find(|tb| tb.at == t).ok_or_else(|| f.err(i, "table load from an unknown table"))?;
            if dst_reg(m, &ops).map(|d| d.0) != Some(*x) {
                return Err(f.err(i, "table load: index and destination differ"));
            }
            match st.bound[*x as usize] {
                Some(b) if b == tab.n => {}
                Some(b) => return Err(f.err(i, &format!("table index bound {b} != table length {}", tab.n))),
                None => return Err(f.err(i, "table index not proven below the table length")),
            }
        }
        st = transfer(lay, &st, i, &ops);
    }
    if !prev_uncond {
        return Err(format!("function at {start:#x}: falls through its end"));
    }
    Ok(f.stats)
}

// ------------------------------------------------------------------------------------------------ whole blobs

/// A blob being verified: decoded incrementally (each jump table ends a decoding segment).
pub struct Blob<'a> {
    pub code: &'a [u8],
    pub lay: Layout,
    thunk: Result<Option<u64>, String>,
    insns: Vec<Insn>,
    tables: Vec<Table>,
    /// Next segment to decode `[from, to)`, if any.
    pub next: Option<(u64, u64)>,
}

impl<'a> Blob<'a> {
    pub fn new(code: &'a [u8], lay: Layout) -> Blob<'a> {
        let end = lay.helper_table as u64;
        let thunk = find_thunk(code, &lay);
        Blob { code, lay, thunk, insns: Vec::new(), tables: Vec::new(), next: Some((0, end)) }
    }

    /// Bytes of the pending segment.
    pub fn segment(&self) -> Option<&'a [u8]> {
        self.next.map(|(a, b)| &self.code[a as usize..b as usize])
    }

    /// Feeds the pending segment's decoding; a jump through a jump table ends the segment, the next starts after it.
    pub fn feed(&mut self, mut seg: Vec<Insn>) {
        let (from, to) = self.next.take().unwrap();
        for i in seg.iter_mut() {
            i.addr += from;
            if let Some(t) = i.comment_target.as_mut() {
                *t = t.wrapping_add(from);
            }
            if i.mnemonic == "jmp" || is_jcc(&i.mnemonic) || i.mnemonic == "call" {
                if let Some(h) = i.operands.strip_prefix("0x") {
                    if let Ok(v) = u64::from_str_radix(h, 16) {
                        i.operands = format!("0x{:x}", v.wrapping_add(from));
                        i.text = format!("{} {}", i.mnemonic, i.operands);
                    }
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
                if let Some((at, n)) = tt {
                    if at >= i.end() && n > 0 && n < (1 << 31) && at + 4 * n <= to {
                        let t = Table { jmp_at: i.addr, at, n };
                        self.insns.truncate(self.insns.iter().position(|i| i.addr >= at).unwrap_or(self.insns.len()));
                        self.tables.push(t);
                        if at + 4 * n < to {
                            self.next = Some((at + 4 * n, to));
                        }
                        return;
                    }
                }
            }
            st = transfer(&self.lay, &st, i, &ops);
        }
    }

    /// Runs the checks once decoding is complete; returns the stats and the prelude bytes.
    pub fn verify(&self) -> Result<(Stats, Vec<u8>), String> {
        assert!(self.next.is_none());
        let lay = &self.lay;
        let thunk = self.thunk.clone()?;
        let first = lay.first_func();
        let prelude: Vec<Insn> = self.insns.iter().filter(|i| i.addr < first).cloned().collect();
        let stubs = prelude_stubs(&prelude, lay.mode, thunk)?;
        let mut stats = Stats { modules: 1, ..Stats::default() };
        for (fi, &(off, params)) in lay.funcs.iter().enumerate() {
            let start = off as u64;
            let end = lay.funcs.get(fi + 1).map(|f| f.0 as u64).unwrap_or(lay.helper_table as u64);
            let insns: Vec<Insn> = self.insns.iter().filter(|i| i.addr >= start && i.addr < end).cloned().collect();
            let tables: Vec<Table> = self.tables.iter().filter(|t| t.at >= start && t.at < end).copied().collect();
            let s = verify_function(lay, self.code, &stubs, start, end, params, &insns, &tables)
                .map_err(|e| format!("{e}\n{}", listing(&insns)))?;
            stats.add(&s);
        }
        Ok((stats, self.code[..first as usize].to_vec()))
    }
}

pub fn listing(insns: &[Insn]) -> String {
    let mut s = String::new();
    for i in insns.iter().take(400) {
        s += &format!("  {:6x}: {}\n", i.addr, i.text);
    }
    s
}

/// Decodes (batched objdump runs) and verifies blobs; returns merged stats and every blob's prelude.
pub fn verify_blobs(objdump: &std::path::Path, blobs: &mut [Blob]) -> Result<(Stats, Vec<Vec<u8>>), String> {
    loop {
        let pending: Vec<usize> = (0..blobs.len()).filter(|&i| blobs[i].next.is_some()).collect();
        if pending.is_empty() {
            break;
        }
        let segs: Vec<&[u8]> = pending.iter().map(|&i| blobs[i].segment().unwrap()).collect();
        let decoded = super::disassemble_many(objdump, &segs);
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
