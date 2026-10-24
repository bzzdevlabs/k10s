//! Per-operation tests: every op in many operand shapes (memory locals, pinned locals, registers, immediates on
//! either side, values spilled across calls, high register pressure, in-place local updates, comparisons fused
//! into br_if / if / select) under several compiler configurations, against the interpreter on edge values.

mod common;
use common::*;
use k10_ir::{new_memory, FuncBuilder, Host, Interp, Module, ModuleBuilder, Op, Trap};
use k10_jit::{CompileOptions, CpuFeatures, IndirectBranch};

struct NoHost;
impl Host for NoHost {
    fn call(&mut self, _: u32, _: &[i64], _: &mut [u8]) -> Result<i64, Trap> {
        Err(Trap::Host(0xDEAD))
    }
}

fn configs() -> Vec<CompileOptions> {
    let det = CpuFeatures::detect();
    vec![
        CompileOptions { features: det, max_pinned: k10_jit::MAX_PINNED, ..CompileOptions::default() },
        // Jump tables for every br_table (tree_max 0) through each indirect-branch mode, and all-trees.
        CompileOptions { features: CpuFeatures::BASELINE, max_pinned: 6, indirect: IndirectBranch::Retpoline, br_tree_max: 0 },
        CompileOptions { features: det, max_pinned: 0, indirect: IndirectBranch::Lfence, br_tree_max: 0 },
        CompileOptions { features: CpuFeatures::BASELINE, max_pinned: 1, indirect: IndirectBranch::Jump, br_tree_max: 0 },
        CompileOptions {
            features: det,
            max_pinned: k10_jit::MAX_PINNED,
            indirect: IndirectBranch::Jump,
            br_tree_max: u16::MAX,
        },
    ]
}

const F_NAN: i64 = 0x7FF8_0000_0000_0000;

fn values() -> Vec<i64> {
    let mut v = vec![
        0,
        1,
        -1,
        2,
        3,
        7,
        -7,
        63,
        64,
        65,
        255,
        256,
        -2,
        i64::MIN,
        i64::MAX,
        i64::MIN + 1,
        i32::MIN as i64,
        i32::MAX as i64,
        u32::MAX as i64,
        1 << 32,
        0x5555_5555_5555_5555,
        0xAAAA_AAAA_AAAA_AAAAu64 as i64,
        12_345_678_901,
        -0x8000,
    ];
    for f in [1.0f64, -1.5, 0.5, -0.0, f64::INFINITY, f64::NEG_INFINITY, i64::MAX as f64, i64::MIN as f64, 1e300, 5e-324, 1e20, -3.75]
    {
        v.push(f.to_bits() as i64);
    }
    v.push(F_NAN);
    v.push(0xFFF8_0000_0000_0001u64 as i64);
    v
}

/// Compiles `m` under every configuration and compares every export on every argument vector with the
/// interpreter (results/traps; memory too when `mem` is set). Float results are NaN-canonicalized when `canon`.
fn check(m: &Module, argsets: &[Vec<i64>], canon: bool, mem: bool) {
    let it = Interp::new(m).unwrap_or_else(|e| panic!("invalid test module: {e}"));
    let k = m.min_mem_log2;
    for opts in configs() {
        let jit = compile_with(m, &opts).unwrap();
        let mut gm = GuardedMem::for_module(m, k);
        let mut im = new_memory(m, k);
        for e in &m.exports {
            let (np, _) = m.func_type(e.func).unwrap();
            // Shapes with an immediate operand only depend on `a`: test them on a reduced set.
            let reduced = e.name.contains("imm") || e.name.starts_with("const");
            for args in argsets.iter().filter(|a| a.len() == np as usize).filter(|a| !reduced || a.last() == Some(&-1)) {
                if mem {
                    gm = GuardedMem::for_module(m, k);
                    im = new_memory(m, k);
                }
                let ri = it.invoke(&mut im, k, &mut NoHost, e.func, args);
                let (rj, _) = run_jit(&jit, m, &mut gm, &mut NoHost, e.func, args, RunOpts::default());
                gm.check_canaries();
                let c = |r: Result<Option<i64>, Trap>| if canon { r.map(|o| o.map(canon_nan)) } else { r };
                assert_eq!(c(ri), c(rj), "export {} args {:x?} opts {:?}", e.name, args, opts);
                if mem {
                    assert!(im == gm.mem(), "memory differs: export {} args {:x?} opts {:?}", e.name, args, opts);
                }
            }
        }
    }
}

fn func(mb: &mut ModuleBuilder, name: &str, params: u8, locals: u32, ops: &[Op]) {
    let mut f = FuncBuilder::new(params, 1);
    f.locals(locals);
    f.ops(ops);
    let idx = mb.add_func(f);
    mb.export(name, idx);
}

/// All shapes for binary operator `op` (two params a, b → op(a, b)).
fn binary_module(op: Op, imms: &[i64]) -> Module {
    use Op::*;
    let mut mb = ModuleBuilder::new();
    // Helper: identity function (forces spills across a call), defined first so its index is known.
    let id = mb.func(1, 1, |f| {
        f.local_get(0);
    });
    func(&mut mb, "mem", 2, 0, &[LocalGet(0), LocalGet(1), op]);
    func(&mut mb, "pin", 2, 0, &[LocalGet(0), LocalGet(1), Drop, Drop, LocalGet(0), LocalGet(1), op]);
    func(&mut mb, "reg", 2, 0, &[LocalGet(0), Const(0), Or, LocalGet(1), Const(0), Or, op]);
    func(&mut mb, "reg_mem", 2, 0, &[LocalGet(0), Const(0), Or, LocalGet(1), op]);
    func(&mut mb, "mem_reg", 2, 0, &[LocalGet(0), LocalGet(1), Const(0), Or, op]);
    func(&mut mb, "same", 2, 0, &[LocalGet(0), LocalGet(0), op, LocalGet(1), Drop]);
    func(&mut mb, "spill_a", 2, 0, &[LocalGet(0), Const(0), Or, LocalGet(1), Call(id), op]);
    func(&mut mb, "call_a", 2, 0, &[LocalGet(0), Call(id), LocalGet(1), Const(0), Or, op]);
    func(&mut mb, "call_b", 2, 0, &[LocalGet(0), Const(0), Xor, LocalGet(1), Call(id), op]);
    // Register pressure: 12 live register values below the operands.
    let mut deep = Vec::new();
    for k in 0..12 {
        deep.extend([LocalGet(0), Const(k), Add]);
    }
    deep.extend([LocalGet(0), Const(0), Or, LocalGet(1), Const(0), Or, op]);
    for _ in 0..12 {
        deep.push(Xor);
    }
    func(&mut mb, "deep", 2, 0, &deep);
    // Same with memory-local operands and pinned operands.
    let mut deep2 = Vec::new();
    for k in 0..9 {
        deep2.extend([LocalGet(1), Const(k), Sub]);
    }
    deep2.extend([LocalGet(0), LocalGet(1), op]);
    for _ in 0..9 {
        deep2.push(Add);
    }
    func(&mut mb, "deep_mem", 2, 0, &deep2);
    // In-place update of a local: x = x op b.
    func(&mut mb, "inplace", 2, 0, &[LocalGet(0), LocalGet(1), op, LocalSet(0), LocalGet(0)]);
    func(
        &mut mb,
        "inplace_pin",
        2,
        0,
        &[LocalGet(0), Drop, LocalGet(0), LocalGet(1), op, LocalSet(0), LocalGet(0)],
    );
    // Old value of x still on the stack when x is updated in place.
    func(&mut mb, "inplace_alias", 2, 0, &[LocalGet(0), LocalGet(0), LocalGet(1), op, LocalSet(0), LocalGet(0), Sub]);
    func(&mut mb, "inplace_self", 2, 0, &[LocalGet(1), Drop, LocalGet(0), LocalGet(0), op, LocalSet(0), LocalGet(0)]);
    // Immediates on either side.
    for (i, &c) in imms.iter().enumerate() {
        func(&mut mb, &format!("imm_r{i}"), 2, 0, &[LocalGet(0), Const(c), op, LocalGet(1), Drop]);
        func(&mut mb, &format!("imm_l{i}"), 2, 0, &[Const(c), LocalGet(1), op, LocalGet(0), Drop]);
        func(
            &mut mb,
            &format!("imm_pin{i}"),
            2,
            0,
            &[LocalGet(0), Drop, LocalGet(0), Const(c), op, LocalGet(0), Drop, LocalGet(1), Drop],
        );
        func(&mut mb, &format!("imm_ip{i}"), 2, 0, &[LocalGet(0), Const(c), op, LocalSet(0), LocalGet(0), LocalGet(1), Drop]);
        func(
            &mut mb,
            &format!("imm_reg{i}"),
            2,
            0,
            &[LocalGet(0), Const(1), Xor, Const(c), op, LocalGet(1), Drop],
        );
    }
    // Comparison consumers (meaningful for comparison ops; harmless for the others).
    func(&mut mb, "brif", 2, 0, &[Block, LocalGet(0), LocalGet(1), op, BrIf(0), Const(5), Return, End, Const(9)]);
    func(
        &mut mb,
        "brif_eqz",
        2,
        0,
        &[Block, LocalGet(0), LocalGet(1), op, Eqz, BrIf(0), Const(5), Return, End, Const(9)],
    );
    func(
        &mut mb,
        "if",
        2,
        1,
        &[LocalGet(0), LocalGet(1), op, If, Const(11), LocalSet(2), Else, Const(22), LocalSet(2), End, LocalGet(2)],
    );
    func(&mut mb, "if_eqz2", 2, 1, &[LocalGet(0), LocalGet(1), op, Eqz, Eqz, If, Const(11), LocalSet(2), End, LocalGet(2)]);
    func(&mut mb, "select", 2, 0, &[Const(100), Const(200), LocalGet(0), LocalGet(1), op, Select]);
    func(
        &mut mb,
        "select_regs",
        2,
        0,
        &[LocalGet(0), Const(1), Add, LocalGet(1), Const(2), Add, LocalGet(0), LocalGet(1), op, Select],
    );
    func(
        &mut mb,
        "select_eqz",
        2,
        0,
        &[LocalGet(1), LocalGet(0), LocalGet(0), LocalGet(1), op, Eqz, Select],
    );
    func(
        &mut mb,
        "select_big",
        2,
        0,
        &[Const(i64::MIN + 3), Const(0x1234_5678_9ABC), LocalGet(0), Const(0), Or, LocalGet(1), op, Select],
    );
    for (i, &c) in imms.iter().enumerate().take(4) {
        func(
            &mut mb,
            &format!("brif_imm{i}"),
            2,
            0,
            &[Block, LocalGet(0), Const(c), op, BrIf(0), Const(5), Return, End, LocalGet(1), Drop, Const(9)],
        );
        func(&mut mb, &format!("select_imm{i}"), 2, 0, &[LocalGet(0), LocalGet(1), Const(c), LocalGet(0), op, Select]);
    }
    mb.build()
}

fn pairs(vals: &[i64]) -> Vec<Vec<i64>> {
    let mut out = Vec::new();
    for &a in vals {
        for &b in vals {
            out.push(vec![a, b]);
        }
    }
    out
}

const IMMS: [i64; 14] = [0, 1, -1, 2, 7, 63, 64, 127, 128, -129, i32::MAX as i64, i32::MIN as i64, 1 << 40, i64::MIN];

fn binary_ops(ops: &[Op], canon: bool) {
    let p = pairs(&values());
    for &op in ops {
        let m = binary_module(op, &IMMS);
        check(&m, &p, canon, false);
    }
}

#[test]
fn int_arith() {
    binary_ops(&[Op::Add, Op::Sub, Op::Mul, Op::And, Op::Or, Op::Xor], false);
}

#[test]
fn int_div() {
    binary_ops(&[Op::DivS, Op::DivU, Op::RemS, Op::RemU], false);
}

#[test]
fn int_shift() {
    binary_ops(&[Op::Shl, Op::ShrS, Op::ShrU, Op::Rotl, Op::Rotr], false);
}

#[test]
fn int_compare() {
    binary_ops(&[Op::Eq, Op::Ne, Op::LtS, Op::LtU, Op::GtS, Op::GtU, Op::LeS, Op::LeU, Op::GeS, Op::GeU], false);
}

#[test]
fn float_ops() {
    binary_ops(&[Op::FAdd, Op::FSub, Op::FMul, Op::FDiv, Op::FEq, Op::FLt, Op::FLe], true);
}

#[test]
fn unary_ops() {
    use Op::*;
    let mut vals = values();
    for k in 0..64 {
        vals.push(1i64 << k);
        vals.push((1i64 << k).wrapping_sub(1));
        vals.push((1i64 << k).wrapping_neg());
    }
    // 0.5 - ulp, ties, 2^52 + 1, the largest f64 below 2^63 and its negation.
    for x in [f64::from_bits(0x3FDF_FFFF_FFFF_FFFF), 2.5, -2.5, 4503599627370497.0, f64::from_bits(0xC3DF_FFFF_FFFF_FFFF), f64::from_bits(0x43DF_FFFF_FFFF_FFFF)] {
        vals.push(x.to_bits() as i64);
    }
    let args: Vec<Vec<i64>> = vals.iter().map(|&v| vec![v]).collect();
    for op in [Clz, Ctz, Popcnt, Eqz, Ext8S, Ext16S, Ext32S, FSqrt, FNeg, FAbs, FFromI, FToI] {
        let mut mb = ModuleBuilder::new();
        let id = mb.func(1, 1, |f| {
            f.local_get(0);
        });
        func(&mut mb, "mem", 1, 0, &[LocalGet(0), op]);
        func(&mut mb, "pin", 1, 0, &[LocalGet(0), Drop, LocalGet(0), op]);
        func(&mut mb, "reg", 1, 0, &[LocalGet(0), Const(0), Or, op]);
        func(&mut mb, "call", 1, 0, &[LocalGet(0), Call(id), op]);
        func(&mut mb, "spilled", 1, 0, &[LocalGet(0), Const(0), Or, Const(1), Call(id), Drop, op]);
        let mut deep = Vec::new();
        for k in 0..12 {
            deep.extend([LocalGet(0), Const(k), Add]);
        }
        deep.extend([LocalGet(0), Const(0), Or, op]);
        for _ in 0..12 {
            deep.push(Xor);
        }
        func(&mut mb, "deep", 1, 0, &deep);
        let mut deep_pin = vec![LocalGet(0), Drop];
        for k in 0..12 {
            deep_pin.extend([LocalGet(0), Const(k), Sub]);
        }
        deep_pin.extend([LocalGet(0), op]);
        for _ in 0..12 {
            deep_pin.push(Add);
        }
        func(&mut mb, "deep_pin", 1, 0, &deep_pin);
        // Constant operand (folded at compile time).
        for (i, &v) in vals.iter().enumerate().step_by(7) {
            func(&mut mb, &format!("const{i}"), 1, 0, &[Const(v), op, LocalGet(0), Drop]);
        }
        let m = mb.build();
        check(&m, &args, matches!(op, FSqrt), false);
    }
}

#[test]
fn select_shapes() {
    use Op::*;
    let mut mb = ModuleBuilder::new();
    func(&mut mb, "s_mem", 3, 0, &[LocalGet(0), LocalGet(1), LocalGet(2), Select]);
    func(&mut mb, "s_pin", 3, 0, &[LocalGet(0), LocalGet(1), LocalGet(2), Drop, Drop, Drop, LocalGet(0), LocalGet(1), LocalGet(2), Select]);
    func(&mut mb, "s_same", 3, 0, &[LocalGet(0), LocalGet(0), LocalGet(2), Select, LocalGet(1), Drop]);
    func(&mut mb, "s_consts", 3, 0, &[Const(-5), Const(1 << 50), LocalGet(2), Select, LocalGet(0), Drop, LocalGet(1), Drop]);
    func(&mut mb, "s_cconst1", 3, 0, &[LocalGet(0), LocalGet(1), Const(3), Select, LocalGet(2), Drop]);
    func(&mut mb, "s_cconst0", 3, 0, &[LocalGet(0), LocalGet(1), Const(0), Select, LocalGet(2), Drop]);
    func(&mut mb, "s_regs", 3, 0, &[LocalGet(0), Const(1), Add, LocalGet(1), Const(2), Add, LocalGet(2), Const(0), Or, Select]);
    let mut deep = Vec::new();
    for k in 0..12 {
        deep.extend([LocalGet(0), Const(k), Add]);
    }
    deep.extend([LocalGet(0), Const(0), Or, LocalGet(1), Const(0), Or, LocalGet(2), Const(0), Or, Select]);
    for _ in 0..12 {
        deep.push(Xor);
    }
    func(&mut mb, "s_deep", 3, 0, &deep);
    // Deep stack: the select's operands sit in spill slots / deep positions (> 32: local refs materialized).
    let mut tall = Vec::new();
    for _ in 0..40 {
        tall.push(LocalGet(0));
    }
    tall.extend([LocalGet(1), LocalGet(2), LocalGet(2), Const(1), And, Select]);
    for _ in 0..40 {
        tall.push(Add);
    }
    func(&mut mb, "s_tall", 3, 0, &tall);
    let m = mb.build();
    let vals = [0i64, 1, -1, 2, i64::MIN, 77];
    let mut args = Vec::new();
    for &a in &vals {
        for &b in &vals {
            for &c in &vals {
                args.push(vec![a, b, c]);
            }
        }
    }
    check(&m, &args, false, false);
}

#[test]
fn locals_aliasing() {
    use Op::*;
    let mut mb = ModuleBuilder::new();
    // old x - new x, with the old value as an unmaterialized reference.
    func(&mut mb, "set_alias", 2, 0, &[LocalGet(0), LocalGet(1), LocalSet(0), LocalGet(0), Sub]);
    func(&mut mb, "tee_alias", 2, 0, &[LocalGet(0), LocalGet(1), LocalTee(0), Sub, LocalGet(0), Mul]);
    func(&mut mb, "tee_self", 2, 0, &[LocalGet(0), LocalTee(0), LocalGet(1), Add]);
    func(&mut mb, "swap", 2, 1, &[LocalGet(0), LocalGet(1), LocalSet(0), LocalSet(1), LocalGet(0), Const(1000), Mul, LocalGet(1), Add]);
    func(
        &mut mb,
        "pinned_swap",
        2,
        1,
        &[
            LocalGet(0), LocalGet(1), Drop, Drop, LocalGet(0), LocalGet(1), Drop, Drop,
            LocalGet(0), LocalGet(1), LocalSet(0), LocalSet(1), LocalGet(0), Const(1000), Mul, LocalGet(1), Add,
        ],
    );
    // Many locals (rep stosq zeroing, high slot offsets) and locals read before written (zero).
    func(&mut mb, "many", 2, 5000, &[LocalGet(4000), LocalGet(1), Add, LocalSet(4999), LocalGet(4999), LocalGet(3), Add, LocalGet(0), Add]);
    // Tall stack of local refs, then a write to the local: refs must keep the old value.
    let mut tall = Vec::new();
    for _ in 0..40 {
        tall.push(LocalGet(0));
    }
    tall.extend([LocalGet(1), LocalSet(0)]);
    for _ in 0..39 {
        tall.push(Xor);
    }
    tall.extend([LocalGet(0), Add]);
    func(&mut mb, "tall_refs", 2, 0, &tall);
    // Loop with a pinned counter and accumulator; branch out with dirty pinned locals.
    func(
        &mut mb,
        "loop_acc",
        2,
        2,
        &[
            LocalGet(1), Const(63), And, LocalSet(2),
            Block, Loop,
            LocalGet(2), Eqz, BrIf(1),
            LocalGet(3), LocalGet(0), Add, Const(3), Rotl, LocalSet(3),
            LocalGet(2), Const(1), Sub, LocalSet(2),
            Br(0),
            End, End,
            LocalGet(3), LocalGet(2), Add,
        ],
    );
    let m = mb.build();
    check(&m, &pairs(&[0, 1, -1, 5, 63, i64::MIN, i64::MAX, 0x1234]), false, false);
}

#[test]
fn br_table_shapes() {
    use Op::*;
    let mut mb = ModuleBuilder::new();
    for (name, n) in [("t0", 0usize), ("t1", 1), ("t3", 3), ("t4", 4), ("t5", 5), ("t17", 17)] {
        let mut f = FuncBuilder::new(1, 1);
        f.locals(1);
        // block x (n+1) ; local.get 0 ; br_table ; end … each end sets a distinct result.
        for _ in 0..=n {
            f.op(Block);
        }
        f.op(LocalGet(0));
        let depths: Vec<u32> = (0..n as u32).map(|i| (i * 7) % (n as u32 + 1)).collect();
        f.br_table(&depths, (n as u32) / 2);
        for k in 0..=n {
            f.op(End);
            f.ops(&[LocalGet(1), Const(10 + k as i64), Add, LocalSet(1)]);
            if k < n {
                f.ops(&[LocalGet(1), Return]);
                // unreachable tail of this level is closed by the next End
            }
        }
        f.op(LocalGet(1));
        let idx = mb.add_func(f);
        mb.export(name, idx);
    }
    // Constant index.
    let mut f = FuncBuilder::new(1, 1);
    f.ops(&[Block, Block, Const(1)]);
    f.br_table(&[0, 1], 0);
    f.ops(&[End, Const(1), Return, End, Const(2)]);
    let idx = mb.add_func(f);
    mb.export("tconst", idx);
    let m = mb.build();
    let args: Vec<Vec<i64>> =
        [-1i64, 0, 1, 2, 3, 4, 5, 6, 16, 17, 18, 100, i64::MIN, i64::MAX, 1 << 32].iter().map(|&v| vec![v]).collect();
    check(&m, &args, false, false);
}

#[test]
fn memory_ops() {
    use Op::*;
    let mut mb = ModuleBuilder::new();
    let data: Vec<u8> = (0..4096u32).map(|i| (i.wrapping_mul(0x9E37_79B9) >> 24) as u8).collect();
    mb.data(0, &data);
    let loads = [Load8U(0), Load8S(0), Load16U(0), Load16S(0), Load32U(0), Load32S(0), Load64(0)];
    let offs = [0u32, 1, 3, 8, 4095, 4096, 4097, (1 << 31) - 1, 1 << 30];
    let mut n = 0;
    for l in loads {
        for &o in &offs {
            let op = match l {
                Load8U(_) => Load8U(o),
                Load8S(_) => Load8S(o),
                Load16U(_) => Load16U(o),
                Load16S(_) => Load16S(o),
                Load32U(_) => Load32U(o),
                Load32S(_) => Load32S(o),
                _ => Load64(o),
            };
            // Write the tail first so that straddling loads see non-zero tail bytes.
            let pre = [MemSize, Const(i64::MIN | 0x0102_0304_0506_0708), Store64(0)];
            let mut v = pre.to_vec();
            v.extend([LocalGet(0), op, LocalGet(1), Drop]);
            func(&mut mb, &format!("ld{n}"), 2, 0, &v);
            let mut v = pre.to_vec();
            v.extend([LocalGet(0), Drop, LocalGet(0), op, LocalGet(1), Drop, LocalGet(0), Drop]);
            func(&mut mb, &format!("ldp{n}"), 2, 0, &v);
            let mut v = pre.to_vec();
            v.extend([LocalGet(0), Const(0), Or, op, LocalGet(1), Drop]);
            func(&mut mb, &format!("ldr{n}"), 2, 0, &v);
            for (j, c) in [0i64, 1, 4095, 4088, 4094, -1, -4, 1 << 33, i64::MIN, 0x7FFF_FFFF].iter().enumerate() {
                let mut v = pre.to_vec();
                v.extend([Const(*c), op, LocalGet(0), Drop, LocalGet(1), Drop]);
                func(&mut mb, &format!("ldc{n}_{j}"), 2, 0, &v);
            }
            n += 1;
        }
    }
    let stores = [Store8(0), Store16(0), Store32(0), Store64(0)];
    for s in stores {
        for &o in &offs {
            let op = match s {
                Store8(_) => Store8(o),
                Store16(_) => Store16(o),
                Store32(_) => Store32(o),
                _ => Store64(o),
            };
            // Store, then read back the whole neighbourhood through the memory comparison.
            func(&mut mb, &format!("st{n}"), 2, 0, &[LocalGet(0), LocalGet(1), op, Const(1)]);
            func(&mut mb, &format!("stp{n}"), 2, 0, &[LocalGet(0), LocalGet(1), Drop, Drop, LocalGet(0), LocalGet(1), op, Const(1)]);
            func(&mut mb, &format!("str{n}"), 2, 0, &[LocalGet(0), Const(0), Or, LocalGet(1), Const(0), Xor, op, Const(1)]);
            func(&mut mb, &format!("stsame{n}"), 2, 0, &[LocalGet(0), LocalGet(0), op, LocalGet(1), Drop, Const(1)]);
            for (j, c) in [0i64, 4095, 4090, -1, 0x7FFF_FFFF].iter().enumerate() {
                func(&mut mb, &format!("stca{n}_{j}"), 2, 0, &[Const(*c), LocalGet(1), op, LocalGet(0), Drop, Const(1)]);
            }
            for (j, c) in [0i64, -1, 0x7F, 0x80, 0xFFFF, -0x8000, i32::MIN as i64, 1 << 40, i64::MIN].iter().enumerate() {
                func(&mut mb, &format!("stcv{n}_{j}"), 2, 0, &[LocalGet(0), Const(*c), op, LocalGet(1), Drop, Const(1)]);
            }
            n += 1;
        }
    }
    func(&mut mb, "size", 2, 0, &[MemSize, LocalGet(0), Drop, LocalGet(1), Drop]);
    let m = mb.build();
    let addrs = [0i64, 1, 7, 4088, 4089, 4095, 4096, 4097, -1, -8, i64::MIN, i64::MAX, 1 << 40, 0x7FFF_FFFF, 0x8000_0000];
    let mut args = Vec::new();
    for &a in &addrs {
        for v in [0x1122_3344_5566_7788i64, -1, 0x80] {
            args.push(vec![a, v]);
        }
    }
    check(&m, &args, false, true);
    // Larger memories than the module minimum (mask > folded-constant limit).
    let it = Interp::new(&m).unwrap();
    let jit = compile(&m).unwrap();
    for k in [13u8, 16] {
        for e in &m.exports {
            for a in &args {
                let mut gm = GuardedMem::for_module(&m, k);
                let mut im = new_memory(&m, k);
                let ri = it.invoke(&mut im, k, &mut NoHost, e.func, a);
                let (rj, _) = run_jit(&jit, &m, &mut gm, &mut NoHost, e.func, a, RunOpts::default());
                gm.check_canaries();
                assert_eq!(ri, rj, "{} {:?} k={k}", e.name, a);
                assert!(im == gm.mem(), "memory differs {} {:?} k={k}", e.name, a);
            }
        }
    }
}

#[test]
fn bulk_ops() {
    use Op::*;
    let mut mb = ModuleBuilder::new();
    let data: Vec<u8> = (0..4096u32).map(|i| (i * 7 + 3) as u8).collect();
    mb.data(0, &data);
    func(&mut mb, "copy", 3, 0, &[LocalGet(0), LocalGet(1), LocalGet(2), MemCopy, Const(1)]);
    func(&mut mb, "fill", 3, 0, &[LocalGet(0), LocalGet(1), LocalGet(2), MemFill, Const(1)]);
    func(&mut mb, "copy_pin", 3, 0, &[LocalGet(0), LocalGet(1), LocalGet(2), Drop, Drop, Drop, LocalGet(2), LocalGet(1), LocalGet(0), MemCopy, Const(1)]);
    func(&mut mb, "fill_regs", 3, 0, &[LocalGet(0), Const(0), Or, LocalGet(1), Const(0), Or, LocalGet(2), Const(0), Or, MemFill, Const(1)]);
    func(&mut mb, "copy_const", 3, 0, &[Const(100), Const(50), Const(200), MemCopy, LocalGet(0), LocalGet(1), LocalGet(2), MemCopy, Const(1)]);
    // Live values across the helper call.
    func(
        &mut mb,
        "copy_live",
        3,
        0,
        &[LocalGet(0), Const(3), Mul, LocalGet(1), Const(5), Mul, LocalGet(0), LocalGet(1), LocalGet(2), MemCopy, Add],
    );
    let m = mb.build();
    let vals = [0i64, 1, 8, 100, 4000, 4095, 4096, 4097, -1, -4096, i64::MIN, i64::MAX, 1 << 32];
    let mut args = Vec::new();
    for &a in &vals {
        for &b in &[0i64, 1, 50, 4095, -1, 0xAB] {
            for &n in &[0i64, 1, 7, 64, 96, 4095, 4096, 4097, -1, i64::MIN, 1 << 33] {
                args.push(vec![a, b, n]);
            }
        }
    }
    check(&m, &args, false, true);
}

#[test]
fn traps() {
    use Op::*;
    let mut mb = ModuleBuilder::new();
    func(&mut mb, "unreachable", 1, 0, &[LocalGet(0), If, Unreachable, End, Const(3)]);
    func(&mut mb, "div0", 1, 0, &[Const(10), LocalGet(0), DivS]);
    func(&mut mb, "div0_const", 1, 0, &[LocalGet(0), Eqz, If, LocalGet(0), Const(0), DivU, Return, End, Const(4)]);
    func(&mut mb, "rem0_live", 1, 0, &[LocalGet(0), Const(2), Add, Const(7), LocalGet(0), RemU, Add]);
    func(&mut mb, "min_div", 1, 0, &[Const(i64::MIN), LocalGet(0), DivS, Const(i64::MIN), LocalGet(0), RemS, Add]);
    let m = mb.build();
    let args: Vec<Vec<i64>> = [0i64, 1, -1, 2, i64::MIN].iter().map(|&v| vec![v]).collect();
    check(&m, &args, false, true);
}

#[test]
fn many_params_and_calls() {
    use Op::*;
    // 255-parameter function, called with mixed operand kinds; and a chain of calls with live values.
    let mut mb = ModuleBuilder::new();
    let mut big = FuncBuilder::new(255, 1);
    big.op(Const(0));
    for i in 0..255u32 {
        big.ops(&[LocalGet(i), Const(i as i64 + 1), Mul, Add]);
    }
    let big_idx = mb.add_func(big);
    let mut caller = FuncBuilder::new(2, 1);
    for i in 0..255i64 {
        match i % 4 {
            0 => caller.op(Const(i * 1_000_000_007)),
            1 => caller.op(LocalGet(0)),
            2 => caller.ops(&[LocalGet(1), Const(i), Xor]),
            _ => caller.op(Const(i64::MIN + i)),
        };
    }
    caller.op(Call(big_idx));
    caller.ops(&[LocalGet(0), Add]);
    let c = mb.add_func(caller);
    mb.export("caller", c);
    let m = mb.build();
    check(&m, &pairs(&[0, 1, -1, 123456789, i64::MIN]), false, false);
}
