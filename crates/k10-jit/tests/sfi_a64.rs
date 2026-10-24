//! AArch64 SFI verifier (common/a64sfi.rs) on JIT output in every `IndirectBranch` mode, barrier form and
//! `br_table` lowering: random modules, br_table modules and the Ten examples pass; the verifier's decoder reads
//! the JIT's output exactly as objdump does; and violations patched into real code are rejected. Every other suite
//! verifies whatever it compiles too (common::compile_with). `K10_SFI_MODULES` (default 1500); `K10_FUZZ_LONG=1`:
//! 50 000. AArch64 hosts (x86-64: xdis_sfi.rs).
#![cfg(target_arch = "aarch64")]

mod common;
mod xdis;

use common::a64dec::decode;
use common::a64sfi::{self, Layout, Stats};
use common::diff::{cfg_for, opts_for};
use k10_ir::{FuncBuilder, Module, ModuleBuilder, Op::*};
use k10_jit::a64::{Asm, Domain, LdSt, Reg, VReg};
use k10_jit::{compile_blob, CodeBlob, CompileOptions, CpuFeatures, IndirectBranch};
use IndirectBranch::{Jump, Lfence, Retpoline};
use Reg::*;

const MODES: [IndirectBranch; 3] = [Jump, Lfence, Retpoline];
/// `br_tree_max`: tables whenever possible, the default, trees whenever possible.
const TREES: [u16; 3] = [0, k10_jit::DEFAULT_BR_TREE_MAX, u16::MAX];

fn report(what: &str, s: &Stats) {
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

fn verify_all(blobs: &[(Module, CodeBlob, IndirectBranch)]) -> Stats {
    let mut total = Stats::default();
    for (k, (m, b, i)) in blobs.iter().enumerate() {
        let st = a64sfi::verify_blob(m, b, *i).unwrap_or_else(|e| panic!("blob {k} ({i:?}): {e}"));
        total.add(&st);
    }
    total
}

/// Every word the decoder accepts in `blobs` (functions, prelude, tables) reads as objdump reads it.
fn reads_like_objdump(blobs: &[(Module, CodeBlob, IndirectBranch)]) {
    let Some(objdump) = xdis::tool("sfi_a64") else { return };
    let codes: Vec<&[u8]> = blobs.iter().map(|(_, b, _)| b.code()).collect();
    let dis = xdis::disassemble_a64(&objdump, &codes);
    let mut n = 0u64;
    for (k, d) in dis.iter().enumerate() {
        assert_eq!(d.len() * 4, codes[k].len(), "blob {k}: objdump skipped words");
        for (at, w, text) in d {
            if let Some(i) = decode(*w) {
                assert_eq!(&i.text(*at), text, "blob {k} {at:#x}: {w:#010x}");
                n += 1;
            }
        }
    }
    eprintln!("sfi_a64: {n} decoded words read as objdump does");
}

fn feature_sets() -> [CpuFeatures; 2] {
    [CpuFeatures::BASELINE, CpuFeatures { sb: true, ..CpuFeatures::BASELINE }]
}

#[test]
fn random_modules_pass() {
    let long = std::env::var_os("K10_FUZZ_LONG").is_some();
    let n: u64 =
        std::env::var("K10_SFI_MODULES").ok().and_then(|s| s.parse().ok()).unwrap_or(if long { 50_000 } else { 1500 });
    let seed0: u64 = std::env::var("K10_FUZZ_SEED").ok().and_then(|s| s.parse().ok()).unwrap_or(0x5F1);
    let mut total = Stats::default();
    let mut per_mode = [0u64; 3];
    let mut seed = seed0;
    while seed < seed0 + n {
        let hi = (seed + 500).min(seed0 + n);
        // opts_for rotates CPU features, pinned registers, IndirectBranch and br_tree_max; the barrier form too.
        let blobs: Vec<(Module, CodeBlob, IndirectBranch)> = (seed..hi)
            .filter_map(|s| {
                let m = k10_ir::gen::gen_module(s, &cfg_for(s, 100_000));
                let o = CompileOptions { features: feature_sets()[(s / 7 % 2) as usize], ..opts_for(s) };
                compile_blob(&m, &o).ok().map(|b| (m, b, o.indirect))
            })
            .collect();
        for (_, _, i) in &blobs {
            per_mode[*i as usize] += 1;
        }
        total.add(&verify_all(&blobs));
        if seed == seed0 {
            reads_like_objdump(&blobs);
        }
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
                    for features in feature_sets() {
                        let o = CompileOptions { indirect, br_tree_max, features, ..opts_for(src as u64) };
                        let b = compile_blob(&m, &o).unwrap_or_else(|e| panic!("n {n} src {src}: {e}"));
                        blobs.push((m.clone(), b, indirect));
                    }
                }
            }
        }
    }
    let st = verify_all(&blobs);
    reads_like_objdump(&blobs);
    report("br_table", &st);
    assert!(st.tables > 0 && st.hardened > 0);
}

#[test]
fn ten_examples_pass() {
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
    let mut configs = Vec::new();
    let base = CpuFeatures::BASELINE;
    for (features, max_pinned) in [(CpuFeatures::detect(), k10_jit::MAX_PINNED), (base, 6), (base, 0)] {
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
    let st = verify_all(&blobs);
    reads_like_objdump(&blobs);
    report(&format!("examples ({} files x {} configs)", files.len(), configs.len()), &st);
    assert_eq!(st.modules as usize, files.len() * configs.len());
}

// ---- violations patched into real code

/// A module touching every construct: guest loads/stores (masked and constant), a loop, a host call, a bulk
/// helper, a TIR call, a jump table, floats, and big frames.
fn sample() -> Module {
    let mut mb = ModuleBuilder::new();
    let h = mb.import("h", 1, 1);
    let leaf = mb.func(1, 1, |f| {
        f.ops(&[LocalGet(0), Const(3), Mul]);
    });
    let mut f = FuncBuilder::new(2, 1);
    f.locals(3);
    #[rustfmt::skip]
    f.ops(&[
        Block, Loop,
            LocalGet(2), Const(10), GeS, BrIf(1),
            LocalGet(0), LocalGet(2), Add, Load64(16), LocalGet(1), Call(h), Add, LocalSet(3),
            LocalGet(1), LocalGet(3), Store32(4),
            Const(64), LocalGet(3), Store64(0),
            LocalGet(2), Const(1), Add, LocalSet(2),
            Br(0),
        End, End,
        Const(0), LocalGet(3), Const(32), MemFill,
        LocalGet(4), Call(leaf), LocalSet(4),
        LocalGet(3), FFromI, LocalGet(1), FFromI, FMul, FSqrt, LocalSet(4),
        Block, Block, Block, Block, Block, Block, Block, Block, Block, Block, LocalGet(0),
    ]);
    f.br_table(&[0, 1, 2, 3, 4, 5, 6, 7, 8], 9);
    for k in 0..10 {
        f.ops(&[End, Const(k), LocalGet(4), Add, Return]);
    }
    let main = mb.add_func(f);
    mb.export("main", main);
    mb.build()
}

fn word(code: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(code[at..at + 4].try_into().unwrap())
}

fn set(code: &mut [u8], at: usize, w: u32) {
    code[at..at + 4].copy_from_slice(&w.to_le_bytes());
}

fn one(f: impl FnOnce(&mut Asm)) -> Vec<u32> {
    let mut a = Asm::new();
    f(&mut a);
    a.finish().unwrap().chunks(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect()
}

/// The offsets of the words in the functions' code (after the prelude) matching `pred`.
fn find_all(code: &[u8], l: &Layout, pred: impl Fn(u32) -> bool) -> Vec<usize> {
    let first = l.funcs[0].0 as usize;
    (first / 4..code.len() / 4).map(|k| 4 * k).filter(|&p| pred(word(code, p))).collect()
}

/// Applies `patch` to a fresh copy of the sample compiled in `mode` and asserts the verifier rejects it with a
/// message containing `needle`.
#[track_caller]
fn rejects(mode: IndirectBranch, what: &str, needle: &str, patch: impl Fn(&mut Vec<u8>, &Layout) -> bool) {
    let m = sample();
    let b = compile_blob(&m, &CompileOptions { indirect: mode, ..CompileOptions::default() }).unwrap();
    let l = a64sfi::layout(&m, &b, mode);
    a64sfi::verify(b.code(), &l).unwrap_or_else(|e| panic!("{what}: the unpatched sample fails: {e}"));
    let mut code = b.code().to_vec();
    assert!(patch(&mut code, &l), "{what}: nothing to patch");
    match a64sfi::verify(&code, &l) {
        Ok(_) => panic!("{what}: accepted"),
        Err(e) => assert!(e.contains(needle), "{what}: rejected for another reason: {e}"),
    }
}

/// Mask with x28 (`and Xd, Xn, x28`), shifted-register form.
fn is_mask(w: u32) -> bool {
    w & 0xFFFF_FC00 == 0x8A1C_0000
}

/// `ldr/str` (register offset, any size) with base x27.
fn is_guest_reg_access(w: u32) -> bool {
    w & 0x3B20_0C00 == 0x3820_0800 && (w >> 5) & 31 == 27
}

#[test]
fn rejects_violations() {
    for mode in MODES {
        // Guest memory.
        rejects(mode, "mask dropped", "unmasked index", |c, l| {
            let p = find_all(c, l, is_mask)[0];
            let w = word(c, p);
            set(c, p, w & !(31 << 16) | (9 << 16)); // and Xd, Xn, x9
            true
        });
        rejects(mode, "mask replaced by a move", "unmasked index", |c, l| {
            let p = find_all(c, l, is_mask)[0];
            let (rd, rn) = (word(c, p) & 31, (word(c, p) >> 5) & 31);
            set(c, p, one(|a| a.mov(Reg::x(rd as u8), Reg::x(rn as u8)))[0]);
            true
        });
        rejects(mode, "scaled guest index", "", |c, l| {
            let p = find_all(c, l, is_guest_reg_access)[0];
            let w = word(c, p);
            set(c, p, w | 1 << 12);
            true
        });
        rejects(mode, "guest access through another base", "memory access through x9", |c, l| {
            let p = find_all(c, l, is_guest_reg_access)[0];
            let w = word(c, p);
            set(c, p, w & !(31 << 5) | (9 << 5));
            true
        });
        rejects(mode, "constant address above the fold limit", "not below", |c, l| {
            let p = find_all(c, l, |w| w & 0xFFC0_03E0 == 0xF900_0360)[0]; // str x, [x27, #imm]
            let w = word(c, p);
            set(c, p, w | 0x3FF << 10);
            true
        });
        // Pinned registers, sp, the return address and callee-saved FP registers.
        for (what, needle, w) in [
            ("write to x27", "writes x27", one(|a| a.add_imm(X27, X27, 8))[0]),
            ("write to x28", "writes x28", one(|a| a.mov_imm(X28, -1))[0]),
            ("write to x26", "writes x26", one(|a| a.mov(X26, X9))[0]),
            ("write to x29", "writes x29", one(|a| a.mov(X29, X9))[0]),
            ("write to x18", "writes x18", one(|a| a.mov(X18, X9))[0]),
            ("write to x30", "writes x30", one(|a| a.mov(X30, X9))[0]),
            ("sp moved", "writes sp", one(|a| a.sub_imm(Sp, Sp, 16))[0]),
            ("sp from a register", "writes sp", one(|a| a.mov_sp(Sp, X9))[0]),
            ("write to d8", "callee-saved", one(|a| a.fmov_to_d(VReg(8), X9))[0]),
            ("write to d15 by a load", "callee-saved", one(|a| a.ldst_d_off(true, VReg(15), Sp, 0))[0]),
            ("store into vmctx", "through x26", one(|a| a.ldst_imm(LdSt::StrX, X9, X26, 0))[0]),
            ("vmctx mem_base read", "vmctx", one(|a| a.ldst_imm(LdSt::LdrX, X9, X26, 0))[0]),
            ("return address overwritten", "outside the frame", one(|a| a.ldst_imm(LdSt::StrX, X9, Sp, 0x7FF8))[0]),
            ("indirect call through x9", "indirect call", one(|a| a.blr(X9))[0]),
            ("indirect jump through x9", "indirect jump", one(|a| a.br(X9))[0]),
            ("ret from the body", "ret outside an epilogue", one(|a| a.ret(X30))[0]),
            ("literal load", "literal", one(|a| {
                let l = a.new_label();
                a.bind(l);
                a.ldr_lit(X9, l)
            })[0]),
            ("system register access", "undecodable", 0xD53B_4200), // mrs x0, nzcv
            ("svc", "undecodable", 0xD400_0001),
            ("udf in the code", "udf other than", 0),
        ] {
            rejects(mode, what, needle, |c, l| {
                // The first plain arithmetic instruction of the loop body.
                let p = find_all(c, l, |w| w & 0x7F20_0000 == 0x0B00_0000 && w >> 31 == 1)[1];
                set(c, p, w);
                true
            });
        }
        rejects(mode, "multiply right after a load", "835769", |c, l| {
            let p = find_all(c, l, is_guest_reg_access)[0];
            set(c, p + 4, one(|a| a.mul(X9, X9, X9))[0]);
            true
        });
        // Control flow.
        rejects(mode, "branch out of the function", "outside the function", |c, l| {
            let p = find_all(c, l, |w| w & 0xFF00_0010 == 0x5400_0000)[0];
            let w = word(c, p);
            set(c, p, w & !(0x7_FFFF << 5) | (0x4_0000 << 5));
            true
        });
        rejects(mode, "stack check removed", "prologue", |c, l| {
            let p = l.funcs[1].0 as usize;
            let k = (p..p + 64).step_by(4).find(|&q| word(c, q) & 0xFF00_001F == 0x5400_0003).unwrap();
            set(c, k, one(|a| a.nop())[0]);
            true
        });
        rejects(mode, "host function index beyond the imports", "", |c, l| {
            let p = find_all(c, l, |w| w == one(|a| a.ldst_imm(LdSt::LdrX, X16, X16, 0))[0])[0];
            set(c, p, one(|a| a.ldst_imm(LdSt::LdrX, X16, X16, 8))[0]);
            true
        });
        // Jump table: the csdb, the clamp, the jump form.
        rejects(mode, "csdb removed", "table load", |c, l| {
            let p = find_all(c, l, |w| w == 0xD503_229F)[0];
            set(c, p, one(|a| a.nop())[0]);
            true
        });
        rejects(mode, "clamp removed", "table load", |c, l| {
            let p = find_all(c, l, |w| w & 0xFFFF_FC00 == 0x9A90_3000)[0]; // csel R, R, x16, lo
            set(c, p, one(|a| a.nop())[0]);
            true
        });
        rejects(mode, "clamp bound off by one", "", |c, l| {
            let p = find_all(c, l, |w| w == one(|a| a.movz(X16, 9, 0))[0])[0];
            set(c, p, one(|a| a.movz(X16, 10, 0))[0]);
            true
        });
        rejects(mode, "table entry out of the function", "outside the function", |c, l| {
            let p = find_all(c, l, |w| w & 0x9F00_001F == 0x1000_0010)[0]; // adr x16, T
            let off = ((((word(c, p) >> 5) & 0x7_FFFF) << 2) | ((word(c, p) >> 29) & 3)) as i64;
            let t = p as i64 + ((off << 43) >> 43);
            set(c, t as usize, 0x0100_0000);
            true
        });
    }
    // Mode-specific forms.
    rejects(Lfence, "barrier removed before a host call", "indirect call", |c, l| {
        let ps = find_all(c, l, |w| w == 0xD503_30FF || w == 0xD503_3FDF);
        set(c, ps[0], one(|a| a.nop())[0]);
        true
    });
    rejects(Jump, "barrier added in jump mode", "indirect call", |c, l| {
        let p = find_all(c, l, |w| w == one(|a| a.blr(X16))[0])[0];
        set(c, p - 4, one(|a| a.isb())[0]);
        set(c, p - 8, one(|a| a.dsb(Domain::Nsh))[0]);
        true
    });
    rejects(Retpoline, "plain blr in retpoline mode", "indirect call", |c, l| {
        let p = find_all(c, l, |w| w >> 26 == 0b100101).into_iter().find(|&p| {
            let t = p as i64 + 4 * (((word(c, p) << 6) as i32) >> 6) as i64;
            word(c, t as usize) == one(|a| a.mov(X17, X30))[0]
        });
        set(c, p.unwrap(), one(|a| a.blr(X16))[0]);
        true
    });
}
