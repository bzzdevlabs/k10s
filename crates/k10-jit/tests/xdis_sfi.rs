//! SFI verifier (tests/xdis/sfi.rs) on JIT output in every `IndirectBranch` mode and `br_table` lowering: random
//! modules, br_table modules, Ten examples, and violations it must reject (red-team modules: redteam.rs).
//! `K10_SFI_MODULES` (default 1500); `K10_FUZZ_LONG=1`: 50 000.

mod common;
mod xdis;

use common::diff::{cfg_for, opts_for};
use k10_jit::x64::*;
use k10_jit::{compile_blob, CodeBlob, CompileOptions, CpuFeatures, IndirectBranch};
use xdis::sfi::{self, Layout};
use Reg::*;

const MODES: [IndirectBranch; 3] = [IndirectBranch::Jump, IndirectBranch::Lfence, IndirectBranch::Retpoline];
/// `br_tree_max` values: tables whenever possible, the default, trees whenever possible.
const TREES: [u16; 3] = [0, k10_jit::DEFAULT_BR_TREE_MAX, u16::MAX];

fn report(what: &str, s: &sfi::Stats) {
    eprintln!(
        "sfi {what}: {} modules, {} functions, {} instructions verified; guest accesses {} masked + {} constant, \
         frame {}, vmctx {}, host calls {}, helper calls {}, jump tables {} ({} entries), direct branches {}, \
         hardened indirect branches {}",
        s.modules, s.functions, s.instructions, s.heap_masked, s.heap_const, s.frame, s.vmctx, s.host_calls,
        s.helper_calls, s.tables, s.table_entries, s.direct_branches, s.hardened
    );
}

fn verify(objdump: &std::path::Path, blobs: &[(CodeBlob, IndirectBranch)]) -> Result<sfi::Stats, String> {
    let refs: Vec<(&CodeBlob, IndirectBranch)> = blobs.iter().map(|(b, i)| (b, *i)).collect();
    xdis::verify_compiled(objdump, &refs)
}

#[test]
fn random_modules_pass() {
    let Some(objdump) = xdis::objdump() else {
        eprintln!("objdump not found: skipped (set K10_OBJDUMP)");
        return;
    };
    let long = std::env::var_os("K10_FUZZ_LONG").is_some();
    let n: u64 = std::env::var("K10_SFI_MODULES").ok().and_then(|s| s.parse().ok()).unwrap_or(if long { 50_000 } else { 1500 });
    let seed0: u64 = std::env::var("K10_FUZZ_SEED").ok().and_then(|s| s.parse().ok()).unwrap_or(0x5F1);
    let mut total = sfi::Stats::default();
    let mut per_mode = [0u64; 3];
    let mut seed = seed0;
    while seed < seed0 + n {
        let hi = (seed + 1000).min(seed0 + n);
        // opts_for rotates CPU features, pinned registers, IndirectBranch and br_tree_max (as the diff tests do).
        let blobs: Vec<(CodeBlob, IndirectBranch)> = (seed..hi)
            .filter_map(|s| {
                let o = opts_for(s);
                compile_blob(&k10_ir::gen::gen_module(s, &cfg_for(s, 100_000)), &o).ok().map(|b| (b, o.indirect))
            })
            .collect();
        for (_, i) in &blobs {
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
    let Some(objdump) = xdis::objdump() else {
        eprintln!("objdump not found: skipped");
        return;
    };
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
                    blobs.push((compile_blob(&m, &o).unwrap_or_else(|e| panic!("n {n} src {src}: {e}")), indirect));
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
    let Some(objdump) = xdis::objdump() else {
        eprintln!("objdump not found: skipped (set K10_OBJDUMP)");
        return;
    };
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
            blobs.push((compile_blob(&m, o).unwrap(), o.indirect));
        }
    }
    let st = verify(&objdump, &blobs).unwrap_or_else(|e| panic!("{e}"));
    report(&format!("examples ({} files x {} configs)", files.len(), configs.len()), &st);
    assert_eq!(st.modules as usize, files.len() * configs.len());
}

// ------------------------------------------------------------------------------------------ negative tests

/// Labels a fake function body can use.
#[derive(Clone, Copy)]
struct Fx {
    /// Trap stub (`mov rsp,[r14+0x40]; ret`).
    trap: Label,
    /// Returning stub (`ret`): call only.
    ret: Label,
    /// The retpoline thunk (Retpoline mode only; a never-bound placeholder otherwise).
    thunk: Label,
    /// Helper slot 0.
    helper: Label,
}

/// Hand-written blob: prelude (`ret` at 0, trap stub, returning stub, thunk in Retpoline mode), one function
/// (prologue, frame 0x18, 1 parameter) around `body`, a 2-slot helper table; `patch` edits the bytes.
fn fake_in(mode: IndirectBranch, body: impl FnOnce(&mut Asm, Fx), patch: impl FnOnce(&mut Vec<u8>)) -> Result<sfi::Stats, String> {
    fake_with(mode, mode, body, patch)
}

/// [`fake_in`] built for `mode`, verified as a `verify_as` blob.
fn fake_with(
    mode: IndirectBranch,
    verify_as: IndirectBranch,
    body: impl FnOnce(&mut Asm, Fx),
    patch: impl FnOnce(&mut Vec<u8>),
) -> Result<sfi::Stats, String> {
    let mut a = Asm::new();
    let at0 = a.new_label(); // Label(0)
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
    if mode == IndirectBranch::Retpoline {
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
    let f = a.pos();
    let helper = a.new_label();
    a.lea(Rax, Mem::b(Rsp, -0x18));
    a.alu(Alu::Cmp, Rax, RM::M(Mem::b(R14, 0x10)));
    a.jcc(Cond::B, trap);
    a.alu_ri(Alu::Sub, RM::R(Rsp), 0x18);
    body(&mut a, Fx { trap, ret, thunk, helper });
    a.alu_ri(Alu::Add, RM::R(Rsp), 0x18);
    a.ret();
    a.align(8, 0xCC);
    let ht = a.pos();
    a.bind(helper);
    a.imm64(0);
    a.imm64(0);
    if a.bound(thunk).is_none() {
        a.bind(thunk);
    }
    let mut code = a.finish().unwrap();
    patch(&mut code);
    let lay = Layout { funcs: vec![(f, 1)], helper_table: ht, num_helpers: 2, num_imports: 2, min_mem_log2: 12, mode: xdis::mode(verify_as) };
    let objdump = xdis::objdump().unwrap();
    let mut b = [sfi::Blob::new(&code, lay)];
    sfi::verify_blobs(&objdump, &mut b).map(|(s, _)| s)
}

fn fake(body: impl FnOnce(&mut Asm, Fx), patch: impl FnOnce(&mut Vec<u8>)) -> Result<sfi::Stats, String> {
    fake_in(IndirectBranch::Jump, body, patch)
}

fn rejects(what: &str, r: Result<sfi::Stats, String>, needle: &str) {
    match r {
        Ok(_) => panic!("{what}: accepted"),
        Err(e) => assert!(e.contains(needle), "{what}: wrong reason: {e}"),
    }
}

/// `br_table`-style dispatch on rcx over `n` entries, with `guard` emitting the bounds proof and `jump` the jump.
fn table_with(a: &mut Asm, n: u32, guard: impl FnOnce(&mut Asm, Label), jump: impl FnOnce(&mut Asm)) {
    let out = a.new_label();
    let tl = a.new_label();
    let dflt = a.new_label();
    a.load(Size::S64, Rcx, Mem::b(Rsp, 0x20)); // parameter 0
    guard(a, dflt);
    a.lea(R11, Mem::rip(tl));
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
    table_with(a, n, guard, |a| {
        a.alu(Alu::Add, Rcx, RM::R(R11));
        a.jmp_rm(RM::R(Rcx));
    });
}

/// The codegen's clamp (docs/JIT-ABI.md): `mov r11d,n; cmp rcx,r11; cmovae rcx,r11` (n cases + default entry).
fn clamp(a: &mut Asm, n: i64) {
    a.mov_ri(R11, n);
    a.alu(Alu::Cmp, Rcx, RM::R(R11));
    a.cmov(Cond::Ae, Rcx, RM::R(R11));
}

/// The table jump in `mode`'s form.
fn mode_jump(a: &mut Asm, mode: IndirectBranch, thunk: Label) {
    match mode {
        IndirectBranch::Jump => {
            a.alu(Alu::Add, Rcx, RM::R(R11));
            a.jmp_rm(RM::R(Rcx));
        }
        IndirectBranch::Lfence => {
            a.alu(Alu::Add, R11, RM::R(Rcx));
            a.lfence();
            a.jmp_rm(RM::R(R11));
        }
        IndirectBranch::Retpoline => {
            a.alu(Alu::Add, R11, RM::R(Rcx));
            a.jmp(thunk);
        }
    }
}

/// A host call in `mode`'s form (import `k`).
fn host_call(a: &mut Asm, mode: IndirectBranch, k: i32, thunk: Label) {
    a.load(Size::S64, Rax, Mem::b(R14, 0x20));
    match mode {
        IndirectBranch::Jump => a.call_rm(RM::M(Mem::b(Rax, 8 * k))),
        IndirectBranch::Lfence => {
            a.load(Size::S64, R11, Mem::b(Rax, 8 * k));
            a.lfence();
            a.call_rm(RM::R(R11));
        }
        IndirectBranch::Retpoline => {
            a.load(Size::S64, R11, Mem::b(Rax, 8 * k));
            a.call(thunk);
        }
    }
}

#[test]
fn verifier_accepts_good_and_rejects_violations() {
    if xdis::objdump().is_none() {
        eprintln!("objdump not found: skipped");
        return;
    }
    let masked_load = |a: &mut Asm| {
        a.load(Size::S64, Rcx, Mem::b(Rsp, 0x8));
        a.alu(Alu::And, Rcx, RM::R(R13));
        a.load(Size::S64, Rax, Mem::bi(R15, Rcx, 0, 0));
    };
    // Good code passes.
    let s = fake(|a, _| masked_load(a), |_| {}).unwrap();
    assert_eq!((s.heap_masked, s.frame), (1, 1));
    fake(|a, _| a.store(Size::S32, Mem::b(R15, 0xff8), Rax), |_| {}).unwrap();
    fake(|a, _| a.load(Size::S64, Rax, Mem::b(Rsp, 0x20)), |_| {}).unwrap(); // parameter 0
    // Guest memory.
    rejects("unmasked index", fake(|a, _| a.load(Size::S64, Rax, Mem::bi(R15, Rcx, 0, 0)), |_| {}), "not masked");
    rejects(
        "masked with r12",
        fake(|a, _| {
            a.alu(Alu::And, Rcx, RM::R(R12));
            a.load(Size::S64, Rax, Mem::bi(R15, Rcx, 0, 0));
        }, |_| {}),
        "not masked",
    );
    rejects(
        "clobbered after masking",
        fake(|a, _| {
            a.alu(Alu::And, Rcx, RM::R(R13));
            a.alu_ri(Alu::Add, RM::R(Rcx), 8);
            a.load(Size::S64, Rax, Mem::bi(R15, Rcx, 0, 0));
        }, |_| {}),
        "not masked",
    );
    rejects(
        "mask on one path only (branch target in between)",
        fake(|a, _| {
            let l = a.new_label();
            a.test_rr(RM::R(Rdx), Rdx);
            a.jcc(Cond::E, l);
            a.alu(Alu::And, Rcx, RM::R(R13));
            a.bind(l);
            a.load(Size::S64, Rax, Mem::bi(R15, Rcx, 0, 0));
        }, |_| {}),
        "not masked",
    );
    rejects("scaled index", fake(|a, _| {
        a.alu(Alu::And, Rcx, RM::R(R13));
        a.load(Size::S64, Rax, Mem::bi(R15, Rcx, 3, 0));
    }, |_| {}), "outside guest memory");
    rejects("displacement after masking", fake(|a, _| {
        a.alu(Alu::And, Rcx, RM::R(R13));
        a.load(Size::S64, Rax, Mem::bi(R15, Rcx, 0, 0x10));
    }, |_| {}), "outside guest memory");
    rejects("constant beyond min memory", fake(|a, _| a.load(Size::S64, Rax, Mem::b(R15, 0x1000)), |_| {}), "constant guest address");
    rejects("negative constant", fake(|a, _| a.load(Size::S8, Rax, Mem::b(R15, -1)), |_| {}), "constant guest address");
    rejects("other base register", fake(|a, _| a.load(Size::S64, Rax, Mem::b(Rcx, 0)), |_| {}), "outside guest memory");
    rejects("REX.B flipped: r15 base becomes rdi", fake(|a, _| masked_load(a), |c| {
        // `mov rax,[r15+rcx*1]` = 49 8b 04 0f: clear REX.B.
        let p = c.windows(4).position(|w| w == [0x49, 0x8b, 0x04, 0x0f]).unwrap();
        c[p] = 0x48;
    }), "outside guest memory");
    // Pinned registers, stack.
    rejects("write r15", fake(|a, _| a.mov_rr(R15, Rax), |_| {}), "pinned register");
    rejects("write r13", fake(|a, _| a.alu_ri(Alu::Or, RM::R(R13), 1), |_| {}), "pinned register");
    rejects("write r14", fake(|a, _| a.load(Size::S64, R14, Mem::b(Rsp, 0)), |_| {}), "pinned register");
    rejects("rsp arithmetic", fake(|a, _| a.alu_ri(Alu::Sub, RM::R(Rsp), 8), |_| {}), "rsp modified");
    rejects("push", fake(|a, _| a.push(Rax), |_| {}), "not in the whitelist");
    rejects("pause", fake(|a, _| a.pause(), |_| {}), "not in the whitelist");
    rejects("return address", fake(|a, _| a.store(Size::S64, Mem::b(Rsp, 0x18), Rax), |_| {}), "frame access outside");
    rejects("above the parameters", fake(|a, _| a.load(Size::S64, Rax, Mem::b(Rsp, 0x28)), |_| {}), "frame access outside");
    rejects("vmctx write", fake(|a, _| a.store(Size::S64, Mem::b(R14, 0x10), Rax), |_| {}), "write to vmctx");
    rejects("vmctx mem_base read", fake(|a, _| a.load(Size::S64, Rax, Mem::b(R14, 0)), |_| {}), "vmctx field");
    // Forbidden instructions (patched over a ud2 placeholder).
    for (name, bytes, needle) in [
        ("syscall", [0x0f, 0x05], "forbidden"),
        ("rdtsc", [0x0f, 0x31], "forbidden"),
        ("cpuid", [0x0f, 0xa2], "forbidden"),
        ("int 0x80", [0xcd, 0x80], "forbidden"),
        ("sysenter", [0x0f, 0x34], "forbidden"),
        ("rdpmc", [0x0f, 0x33], "forbidden"),
    ] {
        rejects(name, fake(|a, _| a.ud2(), |c| {
            let p = c.windows(2).position(|w| w == [0x0f, 0x0b]).unwrap();
            c[p..p + 2].copy_from_slice(&bytes);
        }), needle);
    }
    // Control flow.
    rejects("call through a register", fake(|a, _| a.call_rm(RM::R(Rax)), |_| {}), "indirect control transfer");
    rejects("jmp through memory", fake(|a, _| a.jmp_rm(RM::M(Mem::b(Rsp, 0))), |_| {}), "indirect control transfer");
    rejects("host call not via host_fns", fake(|a, _| a.call_rm(RM::M(Mem::b(Rax, 0))), |_| {}), "host or helper");
    rejects("host call past the imports", fake(|a, _| {
        a.load(Size::S64, Rax, Mem::b(R14, 0x20));
        a.call_rm(RM::M(Mem::b(Rax, 16)));
    }, |_| {}), "host or helper");
    rejects("jump into the trampoline", fake(|a, _| a.jcc(Cond::E, Label(0)), |_| {}), "direct branch");
    rejects("call into the trampoline", fake(|a, _| a.call(Label(0)), |_| {}), "direct branch");
    fake(|a, x| a.call(x.ret), |_| {}).unwrap();
    fake(|a, x| a.jcc(Cond::E, x.trap), |_| {}).unwrap();
    rejects("jump to a returning stub", fake(|a, x| a.jcc(Cond::E, x.ret), |_| {}), "direct branch");
    rejects("ret without epilogue", fake(|a, _| a.ret(), |_| {}), "ret without");
    // Jump tables (branch-based proofs).
    let good = fake(|a, _| table(a, 6, |a, d| {
        a.alu_ri(Alu::Cmp, RM::R(Rcx), 6);
        a.jcc(Cond::Ae, d);
    }), |_| {}).unwrap();
    assert_eq!((good.tables, good.table_entries), (1, 6));
    fake(|a, _| table(a, 6, |a, d| {
        a.alu_ri(Alu::Cmp, RM::R(Rcx), 6);
        a.jcc(Cond::Ae, d);
        a.zero(Rdx);
        a.alu_ri(Alu::Cmp, RM::R(Rcx), 6);
        a.cmov(Cond::Ae, Rcx, RM::R(Rdx));
    }), |_| {}).unwrap();
    fake(|a, _| table(a, 6, |a, _| {
        a.mov_ri(Rdx, 5);
        a.alu_ri(Alu::Cmp, RM::R(Rcx), 6);
        a.cmov(Cond::Ae, Rcx, RM::R(Rdx));
    }), |_| {}).unwrap();
    fake(|a, _| table(a, 6, |a, _| {
        a.alu_ri(Alu::Cmp, RM::R(Rcx), 6);
        a.alu(Alu::Sbb, Rdx, RM::R(Rdx));
        a.alu(Alu::And, Rcx, RM::R(Rdx));
    }), |_| {}).unwrap();
    rejects("no bounds check", fake(|a, _| table(a, 6, |_, _| {}), |_| {}), "table");
    rejects("sbb mask of the index itself", fake(|a, _| table(a, 6, |a, _| {
        a.alu_ri(Alu::Cmp, RM::R(Rcx), 6);
        a.alu(Alu::Sbb, Rcx, RM::R(Rcx));
        a.alu(Alu::And, Rcx, RM::R(Rcx));
    }), |_| {}), "table");
    rejects("clamp to an out-of-range constant", fake(|a, _| table(a, 6, |a, _| {
        a.mov_ri(Rdx, 6);
        a.alu_ri(Alu::Cmp, RM::R(Rcx), 6);
        a.cmov(Cond::Ae, Rcx, RM::R(Rdx));
    }), |_| {}), "table");
    rejects("bound larger than the table", fake(|a, _| table(a, 6, |a, d| {
        a.alu_ri(Alu::Cmp, RM::R(Rcx), 7);
        a.jcc(Cond::Ae, d);
    }), |_| {}), "");
    rejects("wrong branch direction", fake(|a, _| table(a, 6, |a, d| {
        a.alu_ri(Alu::Cmp, RM::R(Rcx), 6);
        a.jcc(Cond::B, d);
    }), |_| {}), "table");
    rejects("index clobbered after the check", fake(|a, _| table(a, 6, |a, d| {
        a.alu_ri(Alu::Cmp, RM::R(Rcx), 6);
        a.jcc(Cond::Ae, d);
        a.alu_ri(Alu::Add, RM::R(Rcx), 1);
    }), |_| {}), "table");
    rejects("table entry outside the function", fake(|a, _| table(a, 6, |a, d| {
        a.alu_ri(Alu::Cmp, RM::R(Rcx), 6);
        a.jcc(Cond::Ae, d);
    }), |c| {
        // The first entry follows `jmp rcx` (ff e1) + padding: make it point far away.
        let p = c.windows(2).position(|w| w == [0xff, 0xe1]).unwrap() + 2;
        let p = (p + 3) & !3;
        c[p..p + 4].copy_from_slice(&0x4000i32.to_le_bytes());
    }), "entry 0");
}

/// The hardened forms of docs/JIT-ABI.md in every mode, and their misuse.
#[test]
fn verifier_hardened_forms() {
    if xdis::objdump().is_none() {
        eprintln!("objdump not found: skipped");
        return;
    }
    use IndirectBranch::*;
    for mode in MODES {
        // The codegen's clamped table (n cases + default) with the mode's jump.
        let s = fake_in(mode, |a, x| table_with(a, 7, |a, _| clamp(a, 6), |a| mode_jump(a, mode, x.thunk)), |_| {}).unwrap();
        assert_eq!((s.tables, s.table_entries), (1, 7), "{mode:?}");
        // Host and helper calls.
        let s = fake_in(mode, |a, x| host_call(a, mode, 1, x.thunk), |_| {}).unwrap();
        assert_eq!(s.host_calls, 1, "{mode:?}");
        let s = fake_in(mode, |a, x| match mode {
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
        }, |_| {}).unwrap();
        assert_eq!(s.helper_calls, 1, "{mode:?}");
        // Clamped table without its default entry, without the clamp, clamped from an unknown register.
        rejects("table of n entries under an n+1 clamp", fake_in(mode, |a, x| table_with(a, 6, |a, _| clamp(a, 6), |a| mode_jump(a, mode, x.thunk)), |_| {}), "");
        rejects("no clamp", fake_in(mode, |a, x| table_with(a, 7, |a, _| a.mov_ri(R11, 6), |a| mode_jump(a, mode, x.thunk)), |_| {}), "");
        rejects("clamp from an unknown value", fake_in(mode, |a, x| table_with(a, 7, |a, _| {
            a.alu(Alu::Cmp, Rcx, RM::R(R11));
            a.cmov(Cond::Ae, Rcx, RM::R(R11));
        }, |a| mode_jump(a, mode, x.thunk)), |_| {}), "");
        // A host call past the imports, in the mode's form.
        rejects("host call past the imports", fake_in(mode, |a, x| host_call(a, mode, 2, x.thunk), |_| {}), "");
    }
    // Forms of another mode.
    rejects("lfence table jump in jump mode", fake_in(Jump, |a, x| table_with(a, 7, |a, _| clamp(a, 6), |a| mode_jump(a, Lfence, x.thunk)), |_| {}), "not in Jump form");
    rejects("jump-mode table jump in lfence mode", fake_in(Lfence, |a, x| table_with(a, 7, |a, _| clamp(a, 6), |a| mode_jump(a, Jump, x.thunk)), |_| {}), "not in Lfence form");
    rejects("plain host call in lfence mode", fake_in(Lfence, |a, x| host_call(a, Jump, 1, x.thunk), |_| {}), "not allowed in Lfence mode");
    rejects("lfence host call in jump mode", fake_in(Jump, |a, x| host_call(a, Lfence, 1, x.thunk), |_| {}), "not allowed in Jump mode");
    rejects("plain host call in retpoline mode", fake_in(Retpoline, |a, x| host_call(a, Jump, 1, x.thunk), |_| {}), "not allowed in Retpoline mode");
    rejects("call r11 without lfence", fake_in(Lfence, |a, _| {
        a.load(Size::S64, Rax, Mem::b(R14, 0x20));
        a.load(Size::S64, R11, Mem::b(Rax, 8));
        a.call_rm(RM::R(R11));
    }, |_| {}), "not allowed in Lfence mode");
    rejects("call r11 = arbitrary register", fake_in(Lfence, |a, _| {
        a.mov_rr(R11, Rax);
        a.lfence();
        a.call_rm(RM::R(R11));
    }, |_| {}), "not a host/helper pointer");
    rejects("retpoline call with r11 = frame data", fake_in(Retpoline, |a, x| {
        a.load(Size::S64, R11, Mem::b(Rsp, 0));
        a.call(x.thunk);
    }, |_| {}), "not a host/helper pointer");
    rejects("retpoline jump without a table", fake_in(Retpoline, |a, x| {
        a.mov_rr(R11, Rax);
        a.jmp(x.thunk);
    }, |_| {}), "jump table");
    rejects("conditional branch to the thunk", fake_in(Retpoline, |a, x| a.jcc(Cond::E, x.thunk), |_| {}), "thunk");
    let at = std::cell::Cell::new(0usize);
    rejects("call into the thunk", fake_in(Retpoline, |a, x| {
        let r11 = |a: &mut Asm| {
            a.load(Size::S64, Rax, Mem::b(R14, 0x20));
            a.load(Size::S64, R11, Mem::b(Rax, 8));
        };
        r11(a);
        at.set(a.pos() as usize);
        a.call(x.thunk);
    }, |c| {
        // retarget the call to the thunk's `mov [rsp],r11; ret` (+12)
        let p = at.get();
        let rel = i32::from_le_bytes(c[p + 1..p + 5].try_into().unwrap()) + 12;
        c[p + 1..p + 5].copy_from_slice(&rel.to_le_bytes());
    }), "direct branch");
    // Preludes: the thunk exactly in Retpoline mode, the indirect calls in the mode's form.
    rejects("retpoline mode without thunk", fake_with(Jump, Retpoline, |_, _| {}, |_| {}), "thunk missing");
    rejects("thunk in jump mode", fake_with(Retpoline, Jump, |_, _| {}, |_| {}), "thunk present");
    rejects("damaged thunk", fake_in(Retpoline, |_, _| {}, |c| {
        let p = c.windows(sfi::THUNK.len()).position(|w| w == sfi::THUNK).unwrap();
        c[p + 12] = 0x48; // mov [rsp],r11 -> mov [rsp],rbx
    }), "thunk missing");
}

/// Mutated real code must be rejected: `and R,r13` → rbp, clamp `cmovae` → `cmovb`, an `lfence` removed.
#[test]
fn verifier_rejects_mutated_real_code() {
    let Some(objdump) = xdis::objdump() else {
        eprintln!("objdump not found: skipped");
        return;
    };
    let mut checked = [0usize; 3];
    for s in 0..2000u64 {
        let mut o = opts_for(s);
        o.indirect = MODES[(s % 3) as usize];
        o.br_tree_max = 0;
        let blob = match compile_blob(&k10_ir::gen::gen_module(s, &cfg_for(s, 100_000)), &o) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let insns = xdis::disassemble(&objdump, blob.code());
        let mut mutate = |kind: usize, at: usize, f: &dyn Fn(&mut Vec<u8>), needle: &str| {
            if checked[kind] >= 10 {
                return;
            }
            let mut code = blob.code().to_vec();
            f(&mut code);
            let mut b = [sfi::Blob::new(&code, xdis::layout(&blob, o.indirect))];
            match sfi::verify_blobs(&objdump, &mut b) {
                Ok(_) => panic!("seed {s} {:?}: mutation {kind} at {at:#x} accepted", o.indirect),
                Err(e) => assert!(e.contains(needle), "seed {s} {:?} mutation {kind}: {e}", o.indirect),
            }
            checked[kind] += 1;
        };
        if let Some(i) = insns.iter().find(|i| i.mnemonic == "and" && i.operands.ends_with(",r13")) {
            let at = i.addr as usize;
            assert!(blob.code()[at] & 0xF0 == 0x40, "REX expected: {:02x?}", i.bytes);
            mutate(0, at, &|c| c[at] &= !0x01, "not masked"); // REX.B: r13 -> rbp
        }
        if let Some(i) = insns.iter().find(|i| i.mnemonic == "cmovae" && i.operands.ends_with(",r11")) {
            let at = i.addr as usize + i.bytes.iter().position(|&b| b == 0x43).unwrap();
            mutate(1, at, &|c| c[at] = 0x42, "table"); // cmovae -> cmovb
        }
        let first = blob.meta().funcs().first().map_or(0, |f| f.offset as u64);
        if let Some(i) = insns.iter().find(|i| i.mnemonic == "lfence" && i.addr >= first && o.indirect == IndirectBranch::Lfence) {
            let at = i.addr as usize;
            mutate(2, at, &|c| c[at..at + 3].copy_from_slice(&[0x48, 0x89, 0xC0]), "Lfence"); // mov rax,rax
        }
        if checked.iter().all(|&k| k >= 10) {
            break;
        }
    }
    assert!(checked.iter().all(|&k| k >= 5), "not enough mutation sites: {checked:?}");
}
