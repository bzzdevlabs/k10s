//! SFI verifier (tests/xdis/sfi.rs) on JIT output in every `IndirectBranch` mode and `br_table` lowering: random
//! modules, br_table modules, Ten examples, and violations it must reject (red-team modules: redteam.rs).
//! `K10_SFI_MODULES` (default 1500); `K10_FUZZ_LONG=1`: 50 000.

mod common;
#[path = "xdis/sfi.rs"]
mod sfi;
mod xdis;

use common::diff::{cfg_for, opts_for};
use common::RETPOLINE_THUNK;
use k10_ir::Module;
use k10_jit::x64::*;
use k10_jit::{compile_blob, CodeBlob, CompileOptions, CpuFeatures, IndirectBranch};
use sfi::Layout;
use std::path::PathBuf;
use xdis::Insn;
use IndirectBranch::{Jump, Lfence, Retpoline};
use Reg::*;

const MODES: [IndirectBranch; 3] = [Jump, Lfence, Retpoline];
/// `br_tree_max`: tables whenever possible, the default, trees whenever possible.
const TREES: [u16; 3] = [0, k10_jit::DEFAULT_BR_TREE_MAX, u16::MAX];

fn report(what: &str, s: &sfi::Stats) {
    eprintln!(
        "sfi {what}: {} modules, {} functions, {} instructions verified; guest accesses {} masked + {} constant, \
         frame {}, vmctx {}, host calls {}, helper calls {}, jump tables {} ({} entries), direct branches {}, \
         hardened indirect branches {}",
        s.modules,
        s.functions,
        s.instructions,
        s.heap_masked,
        s.heap_const,
        s.frame,
        s.vmctx,
        s.host_calls,
        s.helper_calls,
        s.tables,
        s.table_entries,
        s.direct_branches,
        s.hardened
    );
}

fn verify(objdump: &std::path::Path, blobs: &[(Module, CodeBlob, IndirectBranch)]) -> Result<sfi::Stats, String> {
    let refs: Vec<_> = blobs.iter().map(|(m, b, i)| (m, b, *i)).collect();
    sfi::verify_compiled(objdump, &refs)
}

#[test]
fn random_modules_pass() {
    let Some(objdump) = xdis::tool("random_modules_pass") else { return };
    let long = std::env::var_os("K10_FUZZ_LONG").is_some();
    let n: u64 =
        std::env::var("K10_SFI_MODULES").ok().and_then(|s| s.parse().ok()).unwrap_or(if long { 50_000 } else { 1500 });
    let seed0: u64 = std::env::var("K10_FUZZ_SEED").ok().and_then(|s| s.parse().ok()).unwrap_or(0x5F1);
    let mut total = sfi::Stats::default();
    let mut per_mode = [0u64; 3];
    let mut seed = seed0;
    while seed < seed0 + n {
        let hi = (seed + 1000).min(seed0 + n);
        // opts_for rotates CPU features, pinned registers, IndirectBranch and br_tree_max.
        let blobs: Vec<(Module, CodeBlob, IndirectBranch)> = (seed..hi)
            .filter_map(|s| {
                let (m, o) = (k10_ir::gen::gen_module(s, &cfg_for(s, 100_000)), opts_for(s));
                compile_blob(&m, &o).ok().map(|b| (m, b, o.indirect))
            })
            .collect();
        for (_, _, i) in &blobs {
            per_mode[*i as usize] += 1;
        }
        let st = verify(&objdump, &blobs).unwrap_or_else(|e| panic!("seeds {seed}..{hi}: {e}"));
        total.add(&st);
        seed = hi;
    }
    report(&format!("random (jump/lfence/retpoline modules {per_mode:?})"), &total);
    assert!(total.modules >= n * 9 / 10);
    assert!(total.heap_masked > 0 && total.heap_const > 0 && total.host_calls > 0 && total.helper_calls > 0);
    assert!(total.tables > 0 && total.hardened > 0 && per_mode.iter().all(|&k| k > 0));
}

/// `br_table`s of 1..300 cases (index from a parameter, memory or a loop counter) in every mode and lowering.
#[test]
fn br_table_modules_pass() {
    use k10_ir::{FuncBuilder, ModuleBuilder, Op::*};
    let Some(objdump) = xdis::tool("br_table_modules_pass") else { return };
    let mut blobs = Vec::new();
    for (k, n) in [1usize, 2, 3, 5, 6, 8, 9, 17, 33, 100, 300].into_iter().enumerate() {
        for src in 0..3 {
            let mut mb = ModuleBuilder::new();
            let mut f = FuncBuilder::new(1, 1);
            f.locals(1);
            if src == 2 {
                f.op(Loop);
            }
            f.op(Block);
            for _ in 0..n {
                f.op(Block);
            }
            match src {
                0 => f.local_get(0),
                1 => f.ops(&[LocalGet(0), Load64(8)]),
                _ => f.ops(&[LocalGet(0), LocalGet(1), Add]),
            };
            let depths: Vec<u32> = (0..n as u32).map(|d| (d * 7 + k as u32) % (n as u32 + 1)).collect();
            f.br_table(&depths, n as u32);
            for j in 0..n {
                f.op(End);
                f.ops(&[Const(j as i64), Return]);
            }
            f.op(End);
            if src == 2 {
                f.ops(&[LocalGet(1), Const(1), Add, LocalSet(1), LocalGet(1), Const(3), LtS, BrIf(0), End]);
            }
            f.ops(&[Const(-1), Return]);
            let idx = mb.add_func(f);
            mb.export("sw", idx);
            let m = mb.build();
            for indirect in MODES {
                for br_tree_max in TREES {
                    let o = CompileOptions { indirect, br_tree_max, ..opts_for(src as u64) };
                    let b = compile_blob(&m, &o).unwrap_or_else(|e| panic!("n {n} src {src}: {e}"));
                    blobs.push((m.clone(), b, indirect));
                }
            }
        }
    }
    let st = verify(&objdump, &blobs).unwrap_or_else(|e| panic!("{e}"));
    report("br_table", &st);
    assert!(st.tables > 0 && st.hardened > 0);
}

#[test]
fn ten_examples_pass() {
    let Some(objdump) = xdis::tool("ten_examples_pass") else { return };
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut files = Vec::new();
    for dir in ["examples", "bench/k10s/ten"] {
        for e in std::fs::read_dir(root.join(dir)).unwrap() {
            let p = e.unwrap().path();
            if p.extension().is_some_and(|x| x == "ten") {
                files.push(p);
            }
        }
    }
    files.sort();
    let det = CpuFeatures::detect();
    let mut configs = Vec::new();
    for (features, max_pinned) in [(det, 6), (CpuFeatures::BASELINE, 6), (det, 0)] {
        for indirect in MODES {
            for br_tree_max in TREES {
                configs.push(CompileOptions { features, max_pinned, indirect, br_tree_max });
            }
        }
    }
    let mut blobs = Vec::new();
    for f in &files {
        let m = k10_lang::compile_file(f).unwrap_or_else(|e| panic!("{}: {e:?}", f.display()));
        for o in &configs {
            blobs.push((m.clone(), compile_blob(&m, o).unwrap(), o.indirect));
        }
    }
    let st = verify(&objdump, &blobs).unwrap_or_else(|e| panic!("{e}"));
    report(&format!("examples ({} files x {} configs)", files.len(), configs.len()), &st);
    assert_eq!(st.modules as usize, files.len() * configs.len());
}

// ---- negative tests on hand-written blobs

/// Labels a fake function body can use.
#[derive(Clone, Copy)]
struct Fx {
    /// Prelude trap stub (`mov rsp,[r14+0x40]; ret`).
    trap: Label,
    /// Prelude returning stub (`ret`): call only.
    ret: Label,
    /// Retpoline thunk (bound in Retpoline mode only).
    thunk: Label,
    /// Helper slot 0.
    helper: Label,
    /// The function's entry, its `sub rsp,F` and its entry stub.
    entry: Label,
    sub_rsp: Label,
    stub: Label,
}

/// Builds hand-written blobs (prelude with stubs and, in Retpoline mode, the thunk; one function with the codegen's
/// prologue and entry stub around a body; a 2-slot helper table) and verifies them.
struct Fake {
    od: PathBuf,
}

impl Fake {
    fn new(test: &str) -> Option<Fake> {
        xdis::tool(test).map(|od| Fake { od })
    }

    fn build(mode: IndirectBranch, verify_as: IndirectBranch, body: impl FnOnce(&mut Asm, Fx)) -> (Vec<u8>, Layout) {
        let mut a = Asm::new();
        let at0 = a.new_label(); // Label(0): the "trampoline" at offset 0
        a.bind(at0);
        a.ret();
        let trap = a.new_label();
        a.bind(trap);
        a.load(Size::S64, Rsp, Mem::b(R14, 0x40));
        a.ret();
        let ret = a.new_label();
        a.bind(ret);
        a.ret();
        let thunk = a.new_label();
        if mode == Retpoline {
            a.align(16, 0xCC);
            a.bind(thunk);
            let (set, spin) = (a.new_label(), a.new_label());
            a.call(set);
            a.bind(spin);
            a.pause();
            a.lfence();
            a.jmp(spin);
            a.bind(set);
            a.store(Size::S64, Mem::b(Rsp, 0), R11);
            a.ret();
        }
        a.align(16, 0xCC);
        let (entry, sub_rsp, stub, helper) = (a.new_label(), a.new_label(), a.new_label(), a.new_label());
        let f = a.pos();
        a.bind(entry);
        a.lea(Rax, Mem::b(Rsp, -0x18));
        a.alu(Alu::Cmp, Rax, RM::M(Mem::b(R14, 0x10)));
        a.jcc(Cond::B, stub);
        a.bind(sub_rsp);
        a.alu_ri(Alu::Sub, RM::R(Rsp), 0x18);
        body(&mut a, Fx { trap, ret, thunk, helper, entry, sub_rsp, stub });
        a.alu_ri(Alu::Add, RM::R(Rsp), 0x18);
        a.ret();
        a.bind(stub);
        a.call(ret);
        a.jmp(sub_rsp);
        a.align(8, 0xCC);
        let ht = a.pos();
        a.bind(helper);
        a.imm64(0);
        a.imm64(0);
        if a.bound(thunk).is_none() {
            a.bind(thunk);
        }
        let lay = Layout {
            funcs: vec![(f, 1)],
            helper_table: ht,
            num_helpers: 2,
            num_imports: 2,
            min_mem_log2: 12,
            mode: sfi::mode(verify_as),
        };
        (a.finish().unwrap(), lay)
    }

    fn run(
        &self,
        mode: IndirectBranch,
        verify_as: IndirectBranch,
        body: impl FnOnce(&mut Asm, Fx),
        patch: impl FnOnce(&mut Vec<u8>),
    ) -> Result<sfi::Stats, String> {
        let (mut code, lay) = Self::build(mode, verify_as, body);
        patch(&mut code);
        let mut b = [sfi::Blob::new(&code, lay)];
        sfi::verify_blobs(&self.od, &mut b).map(|(s, _)| s)
    }

    fn in_mode(
        &self,
        mode: IndirectBranch,
        body: impl FnOnce(&mut Asm, Fx),
        patch: impl FnOnce(&mut Vec<u8>),
    ) -> Result<sfi::Stats, String> {
        self.run(mode, mode, body, patch)
    }

    fn jump(&self, body: impl FnOnce(&mut Asm, Fx), patch: impl FnOnce(&mut Vec<u8>)) -> Result<sfi::Stats, String> {
        self.run(Jump, Jump, body, patch)
    }

    /// Verifies the function of a table-free Jump-mode fake after `edit` changed its decoded instructions.
    fn edited(&self, body: impl FnOnce(&mut Asm, Fx), edit: impl FnOnce(&mut Vec<Insn>)) -> Result<sfi::Stats, String> {
        let (code, lay) = Self::build(Jump, Jump, body);
        let (f, ht) = (lay.funcs[0].0 as u64, lay.helper_table as u64);
        let insns = xdis::disassemble_many(&self.od, &[&code[..ht as usize]]).remove(0);
        let (prelude, mut func): (Vec<Insn>, Vec<Insn>) = insns.into_iter().partition(|i| i.addr < f);
        let stubs = sfi::prelude_stubs(&prelude, lay.mode, sfi::find_thunk(&code, &lay)?)?;
        edit(&mut func);
        sfi::verify_function(&lay, &code, &stubs, f, ht, 1, &func, &[])
    }
}

#[track_caller]
fn rejects(what: &str, r: Result<sfi::Stats, String>, needle: &str) {
    match r {
        Ok(_) => panic!("{what}: accepted"),
        Err(e) => assert!(e.contains(needle), "{what}: wrong reason (want `{needle}`): {e}"),
    }
}

/// Patches the first occurrence of `from` in `c` into `to`.
fn patch(c: &mut [u8], from: &[u8], to: &[u8]) {
    let p = c.windows(from.len()).position(|w| w == from).unwrap_or_else(|| panic!("{from:02x?} not found"));
    c[p..p + to.len()].copy_from_slice(to);
}

/// `br_table`-style dispatch on rcx over `n` entries: `guard` proves the bound, `mid` runs between the `lea` and the
/// table load, `jump` jumps.
fn table_with(
    a: &mut Asm,
    n: u32,
    guard: impl FnOnce(&mut Asm, Label),
    mid: impl FnOnce(&mut Asm, Label),
    jump: impl FnOnce(&mut Asm),
) {
    let out = a.new_label();
    let tl = a.new_label();
    let dflt = a.new_label();
    a.load(Size::S64, Rcx, Mem::b(Rsp, 0x20)); // parameter 0
    guard(a, dflt);
    a.lea(R11, Mem::rip(tl));
    mid(a, tl);
    a.movsxd(Rcx, RM::M(Mem::bi(R11, Rcx, 2, 0)));
    jump(a);
    a.align(4, 0xCC);
    a.bind(tl);
    let base = a.pos();
    let targets: Vec<Label> = (0..n).map(|_| a.new_label()).collect();
    for &t in &targets {
        a.rel32_entry(t, base);
    }
    for t in targets {
        a.bind(t);
        a.jmp(out);
    }
    a.bind(dflt);
    a.bind(out);
}

fn table(a: &mut Asm, n: u32, guard: impl FnOnce(&mut Asm, Label)) {
    table_with(a, n, guard, |_, _| {}, jump_form);
}

fn jump_form(a: &mut Asm) {
    a.alu(Alu::Add, Rcx, RM::R(R11));
    a.jmp_rm(RM::R(Rcx));
}

fn cmp_jae(a: &mut Asm, d: Label) {
    a.alu_ri(Alu::Cmp, RM::R(Rcx), 6);
    a.jcc(Cond::Ae, d);
}

/// The codegen's clamp: `mov r11d,n; cmp rcx,r11; cmovae rcx,r11` (n cases + the default entry).
fn clamp(a: &mut Asm, n: i64) {
    a.mov_ri(R11, n);
    a.alu(Alu::Cmp, Rcx, RM::R(R11));
    a.cmov(Cond::Ae, Rcx, RM::R(R11));
}

/// The table jump in `mode`'s form.
fn mode_jump(a: &mut Asm, mode: IndirectBranch, thunk: Label) {
    match mode {
        Jump => jump_form(a),
        Lfence => {
            a.alu(Alu::Add, R11, RM::R(Rcx));
            a.lfence();
            a.jmp_rm(RM::R(R11));
        }
        Retpoline => {
            a.alu(Alu::Add, R11, RM::R(Rcx));
            a.jmp(thunk);
        }
    }
}

/// A host call in `mode`'s form (import `k`).
fn host_call(a: &mut Asm, mode: IndirectBranch, k: i32, thunk: Label) {
    a.load(Size::S64, Rax, Mem::b(R14, 0x20));
    match mode {
        Jump => a.call_rm(RM::M(Mem::b(Rax, 8 * k))),
        Lfence => {
            a.load(Size::S64, R11, Mem::b(Rax, 8 * k));
            a.lfence();
            a.call_rm(RM::R(R11));
        }
        Retpoline => {
            a.load(Size::S64, R11, Mem::b(Rax, 8 * k));
            a.call(thunk);
        }
    }
}

fn masked_load(a: &mut Asm) {
    a.load(Size::S64, Rcx, Mem::b(Rsp, 0x8));
    a.alu(Alu::And, Rcx, RM::R(R13));
    a.load(Size::S64, Rax, Mem::bi(R15, Rcx, 0, 0));
}

#[test]
fn verifier_memory_and_registers() {
    let Some(fk) = Fake::new("verifier_memory_and_registers") else { return };
    let s = fk.jump(|a, _| masked_load(a), |_| {}).unwrap();
    assert_eq!((s.heap_masked, s.frame), (1, 1));
    fk.jump(|a, _| a.store(Size::S32, Mem::b(R15, 0xff8), Rax), |_| {}).unwrap();
    fk.jump(|a, _| a.load(Size::S64, Rax, Mem::b(Rsp, 0x20)), |_| {}).unwrap(); // parameter 0
    rejects(
        "unmasked index",
        fk.jump(|a, _| a.load(Size::S64, Rax, Mem::bi(R15, Rcx, 0, 0)), |_| {}),
        "not masked with r13",
    );
    rejects(
        "masked with r12",
        fk.jump(
            |a, _| {
                a.alu(Alu::And, Rcx, RM::R(R12));
                a.load(Size::S64, Rax, Mem::bi(R15, Rcx, 0, 0));
            },
            |_| {},
        ),
        "not masked with r13",
    );
    rejects(
        "clobbered after masking",
        fk.jump(
            |a, _| {
                a.alu(Alu::And, Rcx, RM::R(R13));
                a.alu_ri(Alu::Add, RM::R(Rcx), 8);
                a.load(Size::S64, Rax, Mem::bi(R15, Rcx, 0, 0));
            },
            |_| {},
        ),
        "not masked with r13",
    );
    rejects(
        "mask on one path only",
        fk.jump(
            |a, _| {
                let l = a.new_label();
                a.test_rr(RM::R(Rdx), Rdx);
                a.jcc(Cond::E, l);
                a.alu(Alu::And, Rcx, RM::R(R13));
                a.bind(l);
                a.load(Size::S64, Rax, Mem::bi(R15, Rcx, 0, 0));
            },
            |_| {},
        ),
        "not masked with r13",
    );
    rejects(
        "scaled index",
        fk.jump(
            |a, _| {
                a.alu(Alu::And, Rcx, RM::R(R13));
                a.load(Size::S64, Rax, Mem::bi(R15, Rcx, 3, 0));
            },
            |_| {},
        ),
        "outside guest memory",
    );
    rejects(
        "displacement after masking",
        fk.jump(
            |a, _| {
                a.alu(Alu::And, Rcx, RM::R(R13));
                a.load(Size::S64, Rax, Mem::bi(R15, Rcx, 0, 0x10));
            },
            |_| {},
        ),
        "outside guest memory",
    );
    rejects(
        "constant beyond min memory",
        fk.jump(|a, _| a.load(Size::S64, Rax, Mem::b(R15, 0x1000)), |_| {}),
        "constant guest address",
    );
    rejects(
        "negative constant",
        fk.jump(|a, _| a.load(Size::S8, Rax, Mem::b(R15, -1)), |_| {}),
        "constant guest address",
    );
    rejects(
        "other base register",
        fk.jump(|a, _| a.load(Size::S64, Rax, Mem::b(Rcx, 0)), |_| {}),
        "outside guest memory",
    );
    // `mov rax,[r15+rcx*1]` = 49 8b 04 0f: REX.B cleared makes the base rdi.
    rejects(
        "REX.B flipped",
        fk.jump(|a, _| masked_load(a), |c| patch(c, &[0x49, 0x8b, 0x04, 0x0f], &[0x48])),
        "outside guest memory",
    );
    rejects("write r15", fk.jump(|a, _| a.mov_rr(R15, Rax), |_| {}), "pinned register");
    rejects("write r13", fk.jump(|a, _| a.alu_ri(Alu::Or, RM::R(R13), 1), |_| {}), "pinned register");
    rejects("write r14", fk.jump(|a, _| a.load(Size::S64, R14, Mem::b(Rsp, 0)), |_| {}), "pinned register");
    rejects("push", fk.jump(|a, _| a.push(Rax), |_| {}), "not in the whitelist");
    rejects("pause", fk.jump(|a, _| a.pause(), |_| {}), "not in the whitelist");
    rejects(
        "return address",
        fk.jump(|a, _| a.store(Size::S64, Mem::b(Rsp, 0x18), Rax), |_| {}),
        "frame access outside",
    );
    rejects(
        "above the parameters",
        fk.jump(|a, _| a.load(Size::S64, Rax, Mem::b(Rsp, 0x28)), |_| {}),
        "frame access outside",
    );
    rejects("vmctx write", fk.jump(|a, _| a.store(Size::S64, Mem::b(R14, 0x10), Rax), |_| {}), "write to vmctx");
    rejects("vmctx mem_base read", fk.jump(|a, _| a.load(Size::S64, Rax, Mem::b(R14, 0)), |_| {}), "vmctx field");
    // Bit tests on memory: an immediate bit index stays in the operand, a register one reaches ±2^60 bytes.
    let bt_mem = |a: &mut Asm, _: Fx| a.imul(Rax, RM::M(Mem::b(Rsp, 0x8))); // 48 0f af 44 24 08, patched below
    fk.jump(|a, _| a.bit_i(BitOp::Bt, RM::M(Mem::b(Rsp, 0x8)), 63), |_| {}).unwrap();
    rejects(
        "bt [m], reg",
        fk.jump(bt_mem, |c| patch(c, &[0x48, 0x0f, 0xaf, 0x44], &[0x48, 0x0f, 0xa3])),
        "register bit index",
    );
    rejects(
        "bts [m], reg",
        fk.jump(bt_mem, |c| patch(c, &[0x48, 0x0f, 0xaf, 0x44], &[0x48, 0x0f, 0xab])),
        "register bit index",
    );
    // An operand objdump prints without a size.
    rejects(
        "unsized operand",
        fk.edited(
            |a, _| a.load(Size::S64, Rax, Mem::b(Rsp, 0x8)),
            |insns| {
                let i = insns.iter_mut().find(|i| i.operands == "rax,QWORD PTR [rsp+0x8]").unwrap();
                i.operands = "rax,[rsp+0x8]".into();
            },
        ),
        "unsized memory operand",
    );
    // Forbidden instructions (patched over a ud2 placeholder).
    for (name, bytes) in [
        ("syscall", [0x0f, 0x05]),
        ("rdtsc", [0x0f, 0x31]),
        ("cpuid", [0x0f, 0xa2]),
        ("int 0x80", [0xcd, 0x80]),
        ("sysenter", [0x0f, 0x34]),
        ("rdpmc", [0x0f, 0x33]),
    ] {
        rejects(name, fk.jump(|a, _| a.ud2(), |c| patch(c, &[0x0f, 0x0b], &bytes)), "forbidden");
    }
}

#[test]
fn verifier_stack_and_control_flow() {
    let Some(fk) = Fake::new("verifier_stack_and_control_flow") else { return };
    // Local zeroing.
    let zero = |n: i64| {
        move |a: &mut Asm, _: Fx| {
            a.lea(Rdi, Mem::b(Rsp, 0));
            a.mov_ri(Rcx, n);
            a.zero(Rax);
            a.rep_stosq();
        }
    };
    fk.jump(zero(2), |_| {}).unwrap();
    rejects("rep stos past the frame", fk.jump(zero(4), |_| {}), "rep stos outside the frame");
    rejects("rep stos count overflowing", fk.jump(zero(1 << 61), |_| {}), "rep stos outside the frame");
    // `lea rdi,[rsp]` = 48 8d 3c 24; with 66 instead of REX.W it is `lea di,[rsp]`: no 64-bit fact.
    rejects("16-bit lea", fk.jump(zero(2), |c| patch(c, &[0x48, 0x8d, 0x3c, 0x24], &[0x66])), "rdi not frame-relative");
    // Epilogue: `add rsp,F` only right before an untargeted ret.
    rejects("rsp arithmetic", fk.jump(|a, _| a.alu_ri(Alu::Sub, RM::R(Rsp), 8), |_| {}), "rsp modified");
    rejects(
        "add rsp twice",
        fk.jump(|a, _| a.alu_ri(Alu::Add, RM::R(Rsp), 0x18), |_| {}),
        "directly followed by a ret",
    );
    rejects(
        "store between add rsp and ret",
        fk.jump(
            |a, _| {
                a.alu_ri(Alu::Add, RM::R(Rsp), 0x18);
                a.store(Size::S64, Mem::b(Rsp, 0), Rax);
            },
            |_| {},
        ),
        "directly followed by a ret",
    );
    rejects(
        "ret as a branch target",
        fk.jump(
            |a, _| {
                let l = a.new_label();
                a.test_rr(RM::R(Rax), Rax);
                a.jcc(Cond::E, l);
                a.alu_ri(Alu::Add, RM::R(Rsp), 0x18);
                a.bind(l);
                a.ret();
            },
            |_| {},
        ),
        "branch target",
    );
    rejects("ret without epilogue", fk.jump(|a, _| a.ret(), |_| {}), "ret must directly follow");
    // Prologue: entered only at the start, and at `sub rsp` by its entry stub.
    rejects("jump to sub rsp", fk.jump(|a, x| a.jcc(Cond::E, x.sub_rsp), |_| {}), "outside the function body");
    rejects("jump to the entry", fk.jump(|a, x| a.jcc(Cond::E, x.entry), |_| {}), "outside the function body");
    rejects("second way into the entry stub", fk.jump(|a, x| a.jcc(Cond::E, x.stub), |_| {}), "entry slow path");
    // Calls and jumps.
    rejects("call through a register", fk.jump(|a, _| a.call_rm(RM::R(Rax)), |_| {}), "indirect control transfer");
    rejects("jmp through memory", fk.jump(|a, _| a.jmp_rm(RM::M(Mem::b(Rsp, 0))), |_| {}), "indirect control transfer");
    rejects("host call not via host_fns", fk.jump(|a, _| a.call_rm(RM::M(Mem::b(Rax, 0))), |_| {}), "host or helper");
    rejects("host call past the imports", fk.jump(|a, x| host_call(a, Jump, 2, x.thunk), |_| {}), "host or helper");
    rejects("jump into the trampoline", fk.jump(|a, _| a.jcc(Cond::E, Label(0)), |_| {}), "direct branch");
    rejects("call into the trampoline", fk.jump(|a, _| a.call(Label(0)), |_| {}), "direct branch");
    fk.jump(|a, x| a.call(x.ret), |_| {}).unwrap();
    fk.jump(|a, x| a.jcc(Cond::E, x.trap), |_| {}).unwrap();
    rejects("jump to a returning stub", fk.jump(|a, x| a.jcc(Cond::E, x.ret), |_| {}), "direct branch");
}

#[test]
fn verifier_jump_tables() {
    let Some(fk) = Fake::new("verifier_jump_tables") else { return };
    let good = fk.jump(|a, _| table(a, 6, cmp_jae), |_| {}).unwrap();
    assert_eq!((good.tables, good.table_entries), (1, 6));
    // Other proofs of the bound: a clamp after the branch, a clamp alone, the sbb/and mask, a 32-bit and.
    fk.jump(
        |a, _| {
            table(a, 6, |a, d| {
                cmp_jae(a, d);
                a.zero(Rdx);
                a.alu_ri(Alu::Cmp, RM::R(Rcx), 6);
                a.cmov(Cond::Ae, Rcx, RM::R(Rdx));
            })
        },
        |_| {},
    )
    .unwrap();
    let xor_clamp = |a: &mut Asm, _: Label| {
        a.zero(Rdx); // 33 d2
        a.alu_ri(Alu::Cmp, RM::R(Rcx), 6);
        a.cmov(Cond::Ae, Rcx, RM::R(Rdx));
    };
    fk.jump(|a, _| table(a, 6, xor_clamp), |_| {}).unwrap();
    fk.jump(
        |a, _| {
            table(a, 6, |a, _| {
                a.alu_ri(Alu::Cmp, RM::R(Rcx), 6);
                a.alu(Alu::Sbb, Rdx, RM::R(Rdx));
                a.alu(Alu::And, Rcx, RM::R(Rdx));
            })
        },
        |_| {},
    )
    .unwrap();
    let and_ecx = |a: &mut Asm, _: Label| a.alu_ri32(Alu::And, RM::R(Rcx), 5); // 83 e1 05
    fk.jump(|a, _| table(a, 6, and_ecx), |_| {}).unwrap();
    // Partial-register writes define no bound: `and cl,5` (80 e1 05), `xor dl,dl` (32 d2).
    rejects(
        "and cl,imm",
        fk.jump(|a, _| table(a, 6, and_ecx), |c| patch(c, &[0x83, 0xe1, 0x05], &[0x80])),
        "other than a jump table",
    );
    rejects(
        "xor dl,dl clamp",
        fk.jump(|a, _| table(a, 6, xor_clamp), |c| patch(c, &[0x33, 0xd2], &[0x32])),
        "other than a jump table",
    );
    // Missing or wrong bounds (no table is recognized, so its `lea` is refused first).
    rejects("no bounds check", fk.jump(|a, _| table(a, 6, |_, _| {}), |_| {}), "other than a jump table");
    rejects(
        "sbb mask of the index itself",
        fk.jump(
            |a, _| {
                table(a, 6, |a, _| {
                    a.alu_ri(Alu::Cmp, RM::R(Rcx), 6);
                    a.alu(Alu::Sbb, Rcx, RM::R(Rcx));
                    a.alu(Alu::And, Rcx, RM::R(Rcx));
                })
            },
            |_| {},
        ),
        "other than a jump table",
    );
    rejects(
        "wrong branch direction",
        fk.jump(
            |a, _| {
                table(a, 6, |a, d| {
                    a.alu_ri(Alu::Cmp, RM::R(Rcx), 6);
                    a.jcc(Cond::B, d);
                })
            },
            |_| {},
        ),
        "other than a jump table",
    );
    rejects(
        "index clobbered after the check",
        fk.jump(
            |a, _| {
                table(a, 6, |a, d| {
                    cmp_jae(a, d);
                    a.alu_ri(Alu::Add, RM::R(Rcx), 1);
                })
            },
            |_| {},
        ),
        "other than a jump table",
    );
    // A bound above the entry count makes code bytes entries.
    rejects(
        "clamp to an out-of-range constant",
        fk.jump(
            |a, _| {
                table(a, 6, |a, _| {
                    a.mov_ri(Rdx, 6);
                    a.alu_ri(Alu::Cmp, RM::R(Rcx), 6);
                    a.cmov(Cond::Ae, Rcx, RM::R(Rdx));
                })
            },
            |_| {},
        ),
        "is not an instruction of the function",
    );
    rejects(
        "bound larger than the table",
        fk.jump(
            |a, _| {
                table(a, 6, |a, d| {
                    a.alu_ri(Alu::Cmp, RM::R(Rcx), 7);
                    a.jcc(Cond::Ae, d);
                })
            },
            |_| {},
        ),
        "is not an instruction of the function",
    );
    rejects(
        "table entry outside the function",
        fk.jump(
            |a, _| table(a, 6, cmp_jae),
            |c| {
                // The first entry follows `jmp rcx` (ff e1) and padding.
                let p = c.windows(2).position(|w| w == [0xff, 0xe1]).unwrap() + 2;
                let p = (p + 3) & !3;
                c[p..p + 4].copy_from_slice(&0x4000i32.to_le_bytes());
            },
        ),
        "entry 0",
    );
    // The table base serves only the table load.
    let bad_mid = |mid: fn(&mut Asm, Label)| move |a: &mut Asm, _: Fx| table_with(a, 6, cmp_jae, mid, jump_form);
    rejects(
        "store through the table base",
        fk.jump(bad_mid(|a, _| a.store(Size::S32, Mem::bi(R11, Rcx, 2, 0), Rax)), |_| {}),
        "r11-based access other than the jump-table load",
    );
    rejects(
        "plain load from the table",
        fk.jump(bad_mid(|a, _| a.load(Size::S32, Rax, Mem::bi(R11, Rcx, 2, 0))), |_| {}),
        "r11-based access other than the jump-table load",
    );
    rejects(
        "table load into another register",
        fk.jump(bad_mid(|a, _| a.movsxd(Rdx, RM::M(Mem::bi(R11, Rcx, 2, 0)))), |_| {}),
        "r11-based access other than the jump-table load",
    );
    rejects(
        "lea r11 of a helper slot",
        fk.jump(
            |a, x| {
                table(a, 6, cmp_jae);
                a.lea(R11, Mem::rip(x.helper));
            },
            |_| {},
        ),
        "other than a jump table",
    );
    rejects(
        "lea of a table into another register",
        fk.jump(bad_mid(|a, tl| a.lea(Rax, Mem::rip(tl))), |_| {}),
        "other than a jump table",
    );
}

/// The hardened forms of docs/JIT-ABI.md in every mode, and their misuse.
#[test]
fn verifier_hardened_forms() {
    let Some(fk) = Fake::new("verifier_hardened_forms") else { return };
    for mode in MODES {
        let tab = |n: u32, guard: fn(&mut Asm, Label)| {
            move |a: &mut Asm, x: Fx| table_with(a, n, guard, |_, _| {}, |a| mode_jump(a, mode, x.thunk))
        };
        let s = fk.in_mode(mode, tab(7, |a, _| clamp(a, 6)), |_| {}).unwrap();
        assert_eq!((s.tables, s.table_entries), (1, 7), "{mode:?}");
        let s = fk.in_mode(mode, |a, x| host_call(a, mode, 1, x.thunk), |_| {}).unwrap();
        assert_eq!(s.host_calls, 1, "{mode:?}");
        let s = fk
            .in_mode(
                mode,
                |a, x| match mode {
                    Jump => a.call_rm(RM::M(Mem::rip(x.helper))),
                    Lfence => {
                        a.load(Size::S64, R11, Mem::rip(x.helper));
                        a.lfence();
                        a.call_rm(RM::R(R11));
                    }
                    Retpoline => {
                        a.load(Size::S64, R11, Mem::rip(x.helper));
                        a.call(x.thunk);
                    }
                },
                |_| {},
            )
            .unwrap();
        assert_eq!(s.helper_calls, 1, "{mode:?}");
        rejects(
            "default entry missing",
            fk.in_mode(mode, tab(6, |a, _| clamp(a, 6)), |_| {}),
            "is not an instruction of the function",
        );
        rejects("no clamp", fk.in_mode(mode, tab(7, |a, _| a.mov_ri(R11, 6)), |_| {}), "other than a jump table");
        rejects(
            "clamp from an unknown value",
            fk.in_mode(
                mode,
                tab(7, |a, _| {
                    a.alu(Alu::Cmp, Rcx, RM::R(R11));
                    a.cmov(Cond::Ae, Rcx, RM::R(R11));
                }),
                |_| {},
            ),
            "other than a jump table",
        );
        let past = if mode == Jump { "host or helper" } else { "host-function table access outside the imports" };
        rejects("host call past the imports", fk.in_mode(mode, |a, x| host_call(a, mode, 2, x.thunk), |_| {}), past);
    }
    let clamped = |form: IndirectBranch| {
        move |a: &mut Asm, x: Fx| table_with(a, 7, |a, _| clamp(a, 6), |_, _| {}, |a| mode_jump(a, form, x.thunk))
    };
    rejects("lfence table jump in jump mode", fk.in_mode(Jump, clamped(Lfence), |_| {}), "not in Jump form");
    rejects("jump-mode table jump in lfence mode", fk.in_mode(Lfence, clamped(Jump), |_| {}), "not in Lfence form");
    rejects(
        "plain host call in lfence mode",
        fk.in_mode(Lfence, |a, x| host_call(a, Jump, 1, x.thunk), |_| {}),
        "not allowed in Lfence mode",
    );
    rejects(
        "lfence host call in jump mode",
        fk.in_mode(Jump, |a, x| host_call(a, Lfence, 1, x.thunk), |_| {}),
        "not allowed in Jump mode",
    );
    rejects(
        "plain host call in retpoline mode",
        fk.in_mode(Retpoline, |a, x| host_call(a, Jump, 1, x.thunk), |_| {}),
        "not allowed in Retpoline mode",
    );
    rejects(
        "call r11 without lfence",
        fk.in_mode(
            Lfence,
            |a, _| {
                a.load(Size::S64, Rax, Mem::b(R14, 0x20));
                a.load(Size::S64, R11, Mem::b(Rax, 8));
                a.call_rm(RM::R(R11));
            },
            |_| {},
        ),
        "not allowed in Lfence mode",
    );
    rejects(
        "call r11 = arbitrary register",
        fk.in_mode(
            Lfence,
            |a, _| {
                a.mov_rr(R11, Rax);
                a.lfence();
                a.call_rm(RM::R(R11));
            },
            |_| {},
        ),
        "not a host/helper pointer",
    );
    rejects(
        "retpoline call with r11 = frame data",
        fk.in_mode(
            Retpoline,
            |a, x| {
                a.load(Size::S64, R11, Mem::b(Rsp, 0));
                a.call(x.thunk);
            },
            |_| {},
        ),
        "not a host/helper pointer",
    );
    rejects(
        "retpoline jump without a table",
        fk.in_mode(
            Retpoline,
            |a, x| {
                a.mov_rr(R11, Rax);
                a.jmp(x.thunk);
            },
            |_| {},
        ),
        "without a recognized jump table",
    );
    rejects(
        "conditional branch to the thunk",
        fk.in_mode(Retpoline, |a, x| a.jcc(Cond::E, x.thunk), |_| {}),
        "conditional branch to the retpoline thunk",
    );
    let at = std::cell::Cell::new(0usize);
    rejects(
        "call into the thunk",
        fk.in_mode(
            Retpoline,
            |a, x| {
                host_call(a, Retpoline, 1, x.thunk);
                at.set(a.pos() as usize - 5);
            },
            |c| {
                // Retarget the call to the thunk's `mov [rsp],r11; ret` (+12).
                let p = at.get();
                let rel = i32::from_le_bytes(c[p + 1..p + 5].try_into().unwrap()) + 12;
                c[p + 1..p + 5].copy_from_slice(&rel.to_le_bytes());
            },
        ),
        "direct branch",
    );
    // Preludes: the thunk exactly in Retpoline mode.
    rejects("retpoline mode without thunk", fk.run(Jump, Retpoline, |_, _| {}, |_| {}), "thunk missing");
    rejects("thunk in jump mode", fk.run(Retpoline, Jump, |_, _| {}, |_| {}), "thunk present");
    rejects(
        "damaged thunk",
        fk.in_mode(
            Retpoline,
            |_, _| {},
            |c| {
                let p = c.windows(RETPOLINE_THUNK.len()).position(|w| w == RETPOLINE_THUNK).unwrap();
                c[p + 12] = 0x48; // mov [rsp],r11 -> mov [rsp],rbx
            },
        ),
        "thunk missing",
    );
}

/// Mutated real code must be rejected: `and R,r13` → rbp, clamp `cmovae` → `cmovb`, an `lfence` removed.
#[test]
fn verifier_rejects_mutated_real_code() {
    let Some(objdump) = xdis::tool("verifier_rejects_mutated_real_code") else { return };
    let mut checked = [0usize; 3];
    for s in 0..2000u64 {
        let mut o = opts_for(s);
        o.indirect = MODES[(s % 3) as usize];
        o.br_tree_max = 0;
        let m = k10_ir::gen::gen_module(s, &cfg_for(s, 100_000));
        let Ok(blob) = compile_blob(&m, &o) else { continue };
        let first = blob.meta().funcs().first().map_or(0, |f| f.offset as u64);
        let insns = xdis::disassemble_many(&objdump, &[blob.code()]).remove(0);
        let mut mutate = |kind: usize, at: usize, f: &dyn Fn(&mut Vec<u8>), needle: &str| {
            if checked[kind] >= 10 {
                return;
            }
            let mut code = blob.code().to_vec();
            f(&mut code);
            let mut b = [sfi::Blob::new(&code, sfi::layout(&m, &blob, o.indirect))];
            match sfi::verify_blobs(&objdump, &mut b) {
                Ok(_) => panic!("seed {s} {:?}: mutation {kind} at {at:#x} accepted", o.indirect),
                Err(e) => {
                    assert!(e.contains(needle), "seed {s} {:?} mutation {kind} (want `{needle}`): {e}", o.indirect)
                }
            }
            checked[kind] += 1;
        };
        if let Some(i) = insns.iter().find(|i| i.mnemonic == "and" && i.operands.ends_with(",r13")) {
            let at = i.addr as usize;
            assert!(blob.code()[at] & 0xF0 == 0x40, "REX expected: {:02x?}", i.bytes);
            mutate(0, at, &|c| c[at] &= !0x01, "not masked with r13"); // REX.B: r13 -> rbp
        }
        if let Some(i) = insns.iter().find(|i| i.mnemonic == "cmovae" && i.operands.ends_with(",r11")) {
            let at = i.addr as usize + i.bytes.iter().position(|&b| b == 0x43).unwrap();
            mutate(1, at, &|c| c[at] = 0x42, "jump table"); // cmovae -> cmovb: bound unproven or too weak
        }
        if let Some(i) = insns.iter().find(|i| i.mnemonic == "lfence" && i.addr >= first && o.indirect == Lfence) {
            let at = i.addr as usize;
            mutate(2, at, &|c| c[at..at + 3].copy_from_slice(&[0x48, 0x89, 0xC0]), "Lfence");
            // mov rax,rax
        }
        if checked.iter().all(|&k| k >= 10) {
            break;
        }
    }
    assert!(checked.iter().all(|&k| k >= 5), "not enough mutation sites: {checked:?}");
}
