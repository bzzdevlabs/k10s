//! Shared test helpers: hosts, one-shot runners, differential runner.

use super::naive::Naive;
use crate::builder::{FuncBuilder, ModuleBuilder};
use crate::interp::{new_memory, Interp, InterpConfig};
use crate::types::*;

/// A host with no imports: any call is a test failure surfaced as `Host(0xDEAD)`.
pub struct NoHost;

impl Host for NoHost {
    fn call(&mut self, _import: u32, _args: &[i64], _mem: &mut [u8]) -> Result<i64, Trap> {
        Err(Trap::Host(0xDEAD))
    }
}

pub use crate::gen::DiffHost as TestHost;

pub fn mix(mut h: u64, v: u64) -> u64 {
    h ^= v;
    h = h.wrapping_mul(0x100_0000_01B3);
    h ^ (h >> 29)
}

/// Runs export `name` of `m` with the prepared interpreter on a fresh minimal memory.
pub fn run(m: &Module, name: &str, args: &[i64]) -> Result<Option<i64>, Trap> {
    let it = Interp::new(m).unwrap_or_else(|e| panic!("validation failed: {e}"));
    let k = m.min_mem_log2;
    let mut mem = new_memory(m, k);
    it.invoke(&mut mem, k, &mut NoHost, it.export(name).expect("export"), args)
}

/// Outcome of one invocation, for comparison.
#[derive(Debug, PartialEq, Eq)]
pub struct Outcome {
    pub result: Result<Option<i64>, Trap>,
    pub mem_hash: u64,
    pub log: Vec<(u32, Vec<i64>)>,
    pub interrupts: u64,
}

pub fn hash_bytes(b: &[u8]) -> u64 {
    let mut h = 0xCBF2_9CE4_8422_2325u64;
    for c in b.chunks(8) {
        let mut x = [0u8; 8];
        x[..c.len()].copy_from_slice(c);
        h = mix(h, u64::from_le_bytes(x));
    }
    h
}

/// Runs `func` with both the prepared and the naive interpreter (same memory size, same fuel) and asserts that
/// result, final memory, host-call log and interrupt count are identical. Returns the outcome and the number
/// of TIR instructions the naive interpreter executed.
pub fn differential(m: &Module, k: u8, func: u32, args: &[i64], fuel: u64, max_depth: u32) -> (Outcome, u64) {
    let info = crate::validate::validate(m).expect("valid");
    let max_frame = info.funcs.iter().map(|f| f.frame_slots() as usize).max().unwrap_or(0);
    let cfg = InterpConfig {
        max_call_depth: max_depth,
        // Make the depth limit the only stack limit, so both interpreters overflow at the same call.
        max_stack_slots: max_frame * (max_depth as usize + 1),
        safepoint_interval: 1,
    };
    let it = Interp::from_validated(m, &info).with_config(cfg);
    let mut mem_a = new_memory(m, k);
    let mut host_a = TestHost::new(fuel);
    let ra = it.invoke(&mut mem_a, k, &mut host_a, func, args);

    let mut nv = Naive::new(m);
    nv.max_depth = max_depth as usize;
    let mut mem_b = new_memory(m, k);
    let mut host_b = TestHost::new(fuel);
    let rb = nv.invoke(&mut mem_b, k, &mut host_b, func, args);

    let a = Outcome { result: ra, mem_hash: hash_bytes(&mem_a), log: host_a.log, interrupts: host_a.interrupts };
    let b = Outcome { result: rb, mem_hash: hash_bytes(&mem_b), log: host_b.log, interrupts: host_b.interrupts };
    if a != b {
        let first_diff = mem_a.iter().zip(&mem_b).position(|(x, y)| x != y);
        if a.log != b.log {
            let i = a.log.iter().zip(&b.log).position(|(x, y)| x != y);
            if let Some(i) = i {
                eprintln!("first host-log diff at call {i}: prepared {:?} vs naive {:?}", a.log[i], b.log[i]);
            }
        }
        panic!(
            "prepared vs naive mismatch (func {func}, args {args:?}, k {k}):\n prepared: {:?} interrupts {} log {}\n naive:    {:?} interrupts {} log {}\n first memory diff at {first_diff:?}\nmodule: {m:#?}",
            a.result,
            a.interrupts,
            a.log.len(),
            b.result,
            b.interrupts,
            b.log.len()
        );
    }
    (a, nv.steps)
}

/// Module with one exported function `f` built by `body`.
pub fn module1(params: u8, results: u8, body: impl FnOnce(&mut FuncBuilder)) -> Module {
    let mut mb = ModuleBuilder::new();
    let f = mb.func(params, results, body);
    mb.export("f", f);
    mb.into_module()
}

/// Evaluates `op` on `(a, b)` through a compiled function, checking prepared and naive agree.
pub fn bin(op: Op, a: i64, b: i64) -> Result<i64, Trap> {
    // Two shapes: operands from locals (folded) and from constants (materialized).
    let m1 = module1(2, 1, |f| {
        f.local_get(0).local_get(1).op(op);
    });
    let m2 = module1(0, 1, |f| {
        f.const_(a).const_(b).op(op);
    });
    let r1 = differential(&m1, 12, 0, &[a, b], 100, 10).0.result;
    let r2 = differential(&m2, 12, 0, &[], 100, 10).0.result;
    assert_eq!(r1, r2, "{op:?}({a}, {b})");
    r1.map(|v| v.unwrap())
}

/// Evaluates unary `op` on `a`.
pub fn un(op: Op, a: i64) -> i64 {
    let m1 = module1(1, 1, |f| {
        f.local_get(0).op(op);
    });
    let m2 = module1(0, 1, |f| {
        f.const_(a).op(op).local_set(0).local_get(0);
        // (extra local for the set above)
        f.locals(1);
    });
    let r1 = differential(&m1, 12, 0, &[a], 100, 10).0.result;
    let r2 = differential(&m2, 12, 0, &[], 100, 10).0.result;
    assert_eq!(r1, r2, "{op:?}({a})");
    r1.unwrap().unwrap()
}

pub fn fbits(x: f64) -> i64 {
    x.to_bits() as i64
}

pub fn fval(x: i64) -> f64 {
    f64::from_bits(x as u64)
}
