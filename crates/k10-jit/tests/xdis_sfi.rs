//! SFI verifier (tests/xdis/sfi.rs) on JIT output: random modules, br_table modules, Ten examples, and violations
//! it must reject (red-team modules: redteam.rs). `K10_SFI_MODULES` (default 1500); `K10_FUZZ_LONG=1`: 50 000.

mod common;
mod xdis;

use common::diff::{cfg_for, opts_for};
use k10_jit::x64::*;
use k10_jit::{compile_blob, CodeBlob, CompileOptions, CpuFeatures};
use xdis::sfi::{self, Layout};
use Reg::*;

fn report(what: &str, s: &sfi::Stats) {
    eprintln!(
        "sfi {what}: {} modules, {} functions, {} instructions verified; guest accesses {} masked + {} constant, \
         frame {}, vmctx {}, host calls {}, helper calls {}, jump tables {} ({} entries), direct branches {}",
        s.modules, s.functions, s.instructions, s.heap_masked, s.heap_const, s.frame, s.vmctx, s.host_calls,
        s.helper_calls, s.tables, s.table_entries, s.direct_branches
    );
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
    let mut seed = seed0;
    while seed < seed0 + n {
        let hi = (seed + 1000).min(seed0 + n);
        let blobs: Vec<CodeBlob> = (seed..hi)
            .filter_map(|s| compile_blob(&k10_ir::gen::gen_module(s, &cfg_for(s, 100_000)), &opts_for(s)).ok())
            .collect();
        let refs: Vec<&CodeBlob> = blobs.iter().collect();
        let st = xdis::verify_compiled(&objdump, &refs).unwrap_or_else(|e| panic!("seeds {seed}..{hi}: {e}"));
        total.add(&st);
        seed = hi;
    }
    report("random", &total);
    assert!(total.modules >= n * 9 / 10);
    assert!(total.heap_masked > 0 && total.heap_const > 0 && total.host_calls > 0 && total.helper_calls > 0);
}

/// Jump tables (`br_table` > 4 targets; the generator only makes small ones): sizes 5..300, index from a
/// parameter, memory or a loop counter.
#[test]
fn br_table_modules_pass() {
    use k10_ir::{FuncBuilder, ModuleBuilder, Op::*};
    let Some(objdump) = xdis::objdump() else {
        eprintln!("objdump not found: skipped");
        return;
    };
    let mut blobs = Vec::new();
    for (k, n) in [5usize, 6, 8, 17, 33, 100, 300].into_iter().enumerate() {
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
            for o in [opts_for(0), opts_for(1), opts_for(2)] {
                blobs.push(compile_blob(&m, &o).unwrap_or_else(|e| panic!("n {n} src {src}: {e}")));
            }
        }
    }
    let refs: Vec<&CodeBlob> = blobs.iter().collect();
    let st = xdis::verify_compiled(&objdump, &refs).unwrap_or_else(|e| panic!("{e}"));
    report("br_table", &st);
    assert_eq!(st.tables as usize, blobs.len());
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
    let configs = [
        CompileOptions { features: det, max_pinned: 6 },
        CompileOptions { features: CpuFeatures::BASELINE, max_pinned: 6 },
        CompileOptions { features: det, max_pinned: 0 },
    ];
    let mut blobs = Vec::new();
    for f in &files {
        let m = k10_lang::compile_file(f).unwrap_or_else(|e| panic!("{}: {e:?}", f.display()));
        for o in &configs {
            blobs.push(compile_blob(&m, o).unwrap());
        }
    }
    let refs: Vec<&CodeBlob> = blobs.iter().collect();
    let st = xdis::verify_compiled(&objdump, &refs).unwrap_or_else(|e| panic!("{e}"));
    report(&format!("examples ({} files x {} configs)", files.len(), configs.len()), &st);
    assert_eq!(st.modules as usize, files.len() * configs.len());
}

// ------------------------------------------------------------------------------------------ negative tests

/// Hand-written blob: prelude `ret` (offset 0, the "trampoline"), a trap stub (`mov rsp,[r14+0x40]; ret`) and a
/// returning stub (`ret`); one function at 16 (correct prologue, frame 0x18, 1 parameter) whose body gets
/// `(trap stub, returning stub)`; then an empty helper table. `patch` edits the bytes.
fn fake(body: impl FnOnce(&mut Asm, (Label, Label)), patch: impl FnOnce(&mut Vec<u8>)) -> Result<sfi::Stats, String> {
    let mut a = Asm::new();
    let at0 = a.new_label(); // Label(0)
    a.bind(at0);
    a.ret();
    let stub = a.new_label();
    a.bind(stub);
    a.load(Size::S64, Rsp, Mem::b(R14, 0x40));
    a.ret();
    let rstub = a.new_label();
    a.bind(rstub);
    a.ret();
    a.align(16, 0xCC);
    let f = a.pos();
    a.lea(Rax, Mem::b(Rsp, -0x18));
    a.alu(Alu::Cmp, Rax, RM::M(Mem::b(R14, 0x10)));
    a.jcc(Cond::B, stub);
    a.alu_ri(Alu::Sub, RM::R(Rsp), 0x18);
    body(&mut a, (stub, rstub));
    a.alu_ri(Alu::Add, RM::R(Rsp), 0x18);
    a.ret();
    a.align(8, 0xCC);
    let ht = a.pos();
    let mut code = a.finish().unwrap();
    patch(&mut code);
    let lay = Layout { funcs: vec![(f, 1)], helper_table: ht, num_helpers: 0, num_imports: 2, min_mem_log2: 12 };
    let objdump = xdis::objdump().unwrap();
    let mut b = [sfi::Blob::new(&code, lay)];
    sfi::verify_blobs(&objdump, &mut b).map(|(s, _)| s)
}

fn rejects(what: &str, r: Result<sfi::Stats, String>, needle: &str) {
    match r {
        Ok(_) => panic!("{what}: accepted"),
        Err(e) => assert!(e.contains(needle), "{what}: wrong reason: {e}"),
    }
}

/// `br_table`-style dispatch on rcx over `n` targets, with `guard` emitting the bounds proof.
fn table(a: &mut Asm, n: u32, guard: impl FnOnce(&mut Asm, Label)) {
    let out = a.new_label();
    let tl = a.new_label();
    let dflt = a.new_label();
    a.load(Size::S64, Rcx, Mem::b(Rsp, 0x20)); // parameter 0
    guard(a, dflt);
    a.lea(R11, Mem::rip(tl));
    a.movsxd(Rcx, RM::M(Mem::bi(R11, Rcx, 2, 0)));
    a.alu(Alu::Add, Rcx, RM::R(R11));
    a.jmp_rm(RM::R(Rcx));
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
    fake(|a, _| {
        a.load(Size::S64, Rax, Mem::b(R14, 0x20));
        a.call_rm(RM::M(Mem::b(Rax, 8)));
    }, |_| {}).unwrap();
    rejects("host call past the imports", fake(|a, _| {
        a.load(Size::S64, Rax, Mem::b(R14, 0x20));
        a.call_rm(RM::M(Mem::b(Rax, 16)));
    }, |_| {}), "import table");
    rejects("jump into the trampoline", fake(|a, _| a.jcc(Cond::E, Label(0)), |_| {}), "direct branch");
    rejects("call into the trampoline", fake(|a, _| a.call(Label(0)), |_| {}), "direct branch");
    fake(|a, (_, r)| a.call(r), |_| {}).unwrap();
    fake(|a, (t, _)| a.jcc(Cond::E, t), |_| {}).unwrap();
    rejects("jump to a returning stub", fake(|a, (_, r)| a.jcc(Cond::E, r), |_| {}), "direct branch");
    rejects("ret without epilogue", fake(|a, _| a.ret(), |_| {}), "ret without");
    // Jump tables.
    let good = fake(|a, _| table(a, 6, |a, d| {
        a.alu_ri(Alu::Cmp, RM::R(Rcx), 6);
        a.jcc(Cond::Ae, d);
    }), |_| {}).unwrap();
    assert_eq!((good.tables, good.table_entries), (1, 6));
    // Spectre-style clamp after the branch (cmp + cmovae with a zero register).
    fake(|a, _| table(a, 6, |a, d| {
        a.alu_ri(Alu::Cmp, RM::R(Rcx), 6);
        a.jcc(Cond::Ae, d);
        a.zero(Rdx);
        a.alu_ri(Alu::Cmp, RM::R(Rcx), 6);
        a.cmov(Cond::Ae, Rcx, RM::R(Rdx));
    }), |_| {}).unwrap();
    // Clamp alone (no branch), and the sbb/and mask.
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

/// Real compiled code with one `and R,r13` retargeted to rbp must be rejected.
#[test]
fn verifier_rejects_mutated_real_code() {
    let Some(objdump) = xdis::objdump() else {
        eprintln!("objdump not found: skipped");
        return;
    };
    let mut checked = 0;
    for s in 0..200u64 {
        let blob = match compile_blob(&k10_ir::gen::gen_module(s, &cfg_for(s, 100_000)), &opts_for(s)) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let insns = xdis::disassemble(&objdump, blob.code());
        let Some(i) = insns.iter().find(|i| i.mnemonic == "and" && i.operands.ends_with(",r13")) else { continue };
        let mut code = blob.code().to_vec();
        let at = i.addr as usize;
        assert!(code[at] & 0xF0 == 0x40, "REX expected: {:02x?}", i.bytes);
        code[at] &= !0x01; // REX.B: r13 -> rbp
        let mut b = [sfi::Blob::new(&code, xdis::layout(&blob))];
        match sfi::verify_blobs(&objdump, &mut b) {
            Ok(_) => panic!("seed {s}: mutation at {at:#x} accepted"),
            Err(e) => assert!(e.contains("not masked"), "seed {s}: {e}"),
        }
        checked += 1;
        if checked == 20 {
            break;
        }
    }
    assert!(checked >= 10, "not enough modules with masked accesses ({checked})");
}
