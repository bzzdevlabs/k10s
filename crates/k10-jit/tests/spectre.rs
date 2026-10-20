//! Speculation hardening (docs/JIT-ABI.md): exact `br_table`, thunk and host-call sequences per [`IndirectBranch`]
//! mode, and every mode running like the interpreter.

mod common;
use common::*;
use k10_ir::{new_memory, FuncBuilder, Host, Interp, Module, ModuleBuilder, Op, Trap};
use k10_jit::x64::{Alu, Asm, Cond, Mem, Reg, Size, RM};
use k10_jit::{compile_blob, compile_with, CompileOptions, IndirectBranch};
use Op::*;
use Reg::*;

const MODES: [IndirectBranch; 3] = [IndirectBranch::Jump, IndirectBranch::Lfence, IndirectBranch::Retpoline];
/// Allocatable registers (never r11, rsp, r13-r15).
const ALLOCATABLE: [Reg; 11] = [Rsi, Rdi, R8, R9, R10, Rbx, Rbp, R12, Rax, Rcx, Rdx];
use common::RETPOLINE_THUNK as THUNK;

fn opts(indirect: IndirectBranch, br_tree_max: u16) -> CompileOptions {
    CompileOptions { indirect, br_tree_max, ..CompileOptions::default() }
}

fn asm(f: impl FnOnce(&mut Asm)) -> Vec<u8> {
    let mut a = Asm::new();
    f(&mut a);
    a.finish().unwrap()
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn count(hay: &[u8], needle: &[u8]) -> usize {
    hay.windows(needle.len()).filter(|w| *w == needle).count()
}

/// Adds `sw(i)`: `br_table` over `n` cases returning `7k + 1`, default -5.
fn add_switch(mb: &mut ModuleBuilder, n: usize) -> u32 {
    let mut f = FuncBuilder::new(1, 1);
    f.op(Block);
    for _ in 0..n {
        f.op(Block);
    }
    f.op(LocalGet(0));
    let depths: Vec<u32> = (0..n as u32).collect();
    f.br_table(&depths, n as u32);
    for k in 0..n {
        f.ops(&[End, Const(7 * k as i64 + 1), Return]);
    }
    f.ops(&[End, Const(-5)]);
    mb.add_func(f)
}

fn switch_module(n: usize) -> (Module, u32) {
    let mut mb = ModuleBuilder::new();
    let f = add_switch(&mut mb, n);
    mb.export("sw", f);
    (mb.build(), f)
}

/// Jump-table sequence of docs/JIT-ABI.md in every mode: clamp, table load, the mode's jump, `n + 1` entries.
#[test]
fn jump_table_sequence() {
    for n in [5usize, 16, 300] {
        let (m, _) = switch_module(n);
        for mode in MODES {
            let blob = compile_blob(&m, &opts(mode, 0)).unwrap();
            let code = blob.code();
            let (r, at) = ALLOCATABLE
                .iter()
                .find_map(|&r| {
                    let clamp = asm(|a| {
                        a.mov_ri(R11, n as i64);
                        a.alu(Alu::Cmp, r, RM::R(R11));
                        a.cmov(Cond::Ae, r, RM::R(R11));
                    });
                    find(code, &clamp).map(|p| (r, p + clamp.len()))
                })
                .unwrap_or_else(|| panic!("n={n} {mode:?}: no branch-free clamp"));
            assert_eq!(&code[at..at + 3], &[0x4C, 0x8D, 0x1D], "lea r11, [rip + table]");
            let table = (at + 7) as i64 + i32::from_le_bytes(code[at + 3..at + 7].try_into().unwrap()) as i64;
            let load = asm(|a| a.movsxd(r, RM::M(Mem::bi(R11, r, 2, 0))));
            let at = at + 7;
            assert_eq!(&code[at..at + load.len()], &load[..], "movsxd r, [r11 + 4r]");
            let at = at + load.len();
            let jump = match mode {
                IndirectBranch::Jump => asm(|a| {
                    a.alu(Alu::Add, r, RM::R(R11));
                    a.jmp_rm(RM::R(r));
                }),
                IndirectBranch::Lfence => asm(|a| {
                    a.alu(Alu::Add, R11, RM::R(r));
                    a.lfence();
                    a.jmp_rm(RM::R(R11));
                }),
                IndirectBranch::Retpoline => asm(|a| a.alu(Alu::Add, R11, RM::R(r))),
            };
            assert_eq!(&code[at..at + jump.len()], &jump[..], "n={n} {mode:?}: jump");
            let mut end = at + jump.len();
            if mode == IndirectBranch::Retpoline {
                let thunk = find(code, &THUNK).expect("retpoline thunk") as i64;
                let target = match code[end] {
                    0xEB => end as i64 + 2 + code[end + 1] as i8 as i64,
                    0xE9 => end as i64 + 5 + i32::from_le_bytes(code[end + 1..end + 5].try_into().unwrap()) as i64,
                    b => panic!("expected jmp to the thunk, found {b:#x}"),
                };
                assert_eq!(target, thunk, "br_table jumps through the retpoline thunk");
                end += if code[end] == 0xEB { 2 } else { 5 };
            }
            // Then the 4-aligned table: n + 1 entries, all targets in this function.
            assert_eq!(table as usize, end.next_multiple_of(4));
            for k in 0..=n {
                let e = table as usize + 4 * k;
                let t = table + i32::from_le_bytes(code[e..e + 4].try_into().unwrap()) as i64;
                assert!(t > table + 4 * n as i64 && (t as usize) < code.len(), "entry {k} -> {t:#x}");
            }
        }
    }
}

/// One thunk in retpoline mode only; small tables become compare trees (no table `lea`).
#[test]
fn thunk_and_trees() {
    let (m, _) = switch_module(16);
    for mode in MODES {
        let code = compile_blob(&m, &opts(mode, 0)).unwrap().code().to_vec();
        assert_eq!(count(&code, &THUNK), (mode == IndirectBranch::Retpoline) as usize, "{mode:?}");
    }
    for n in [1usize, 2, 3, 4, 7] {
        let (m, _) = switch_module(n);
        let code = compile_blob(&m, &opts(IndirectBranch::Jump, k10_jit::DEFAULT_BR_TREE_MAX)).unwrap().code().to_vec();
        assert_eq!(count(&code, &[0x4C, 0x8D, 0x1D]), 0, "n={n}: compare tree expected, found a table load");
        let code = compile_blob(&m, &opts(IndirectBranch::Jump, 0)).unwrap().code().to_vec();
        assert_eq!(count(&code, &[0x4C, 0x8D, 0x1D]), 1, "n={n}: br_tree_max 0 forces a jump table");
    }
}

/// Host-call sequence per mode (docs/JIT-ABI.md).
#[test]
fn host_call_sequences() {
    let mut mb = ModuleBuilder::new();
    let _h0 = mb.import("h0", 1, 1);
    let h1 = mb.import("h1", 1, 1);
    let f = mb.func(1, 1, |f| {
        f.ops(&[LocalGet(0), Call(h1)]);
    });
    mb.export("f", f);
    let m = mb.build();
    for mode in MODES {
        let code = compile_blob(&m, &opts(mode, 0)).unwrap().code().to_vec();
        let seq = asm(|a| {
            a.load(Size::S64, Rax, Mem::b(R14, k10_jit::abi::OFF_HOST_FNS));
            match mode {
                IndirectBranch::Jump => a.call_rm(RM::M(Mem::b(Rax, 8))),
                IndirectBranch::Lfence => {
                    a.load(Size::S64, R11, Mem::b(Rax, 8));
                    a.lfence();
                    a.call_rm(RM::R(R11));
                }
                IndirectBranch::Retpoline => a.load(Size::S64, R11, Mem::b(Rax, 8)),
            }
        });
        let p = find(&code, &seq).unwrap_or_else(|| panic!("{mode:?}: host call sequence not found"));
        if mode == IndirectBranch::Retpoline {
            let at = p + seq.len();
            assert_eq!(code[at], 0xE8, "call thunk");
            let target = at as i64 + 5 + i32::from_le_bytes(code[at + 1..at + 5].try_into().unwrap()) as i64;
            assert_eq!(target, find(&code, &THUNK).unwrap() as i64);
        }
    }
}

#[test]
fn indirect_branch_names() {
    for mode in MODES {
        assert_eq!(IndirectBranch::parse(mode.name()), Some(mode));
    }
    assert_eq!(IndirectBranch::parse("retpolines"), None);
    assert_eq!(IndirectBranch::default(), IndirectBranch::Jump);
    assert_eq!(CompileOptions::default().indirect, IndirectBranch::Jump);
}

struct H;
impl Host for H {
    fn call(&mut self, _: u32, args: &[i64], _: &mut [u8]) -> Result<i64, Trap> {
        Ok(args[0].wrapping_mul(3).wrapping_add(1))
    }
}

/// Host calls, helpers, both `br_table` lowerings and interrupts in every mode, against the interpreter.
#[test]
fn all_modes_run_like_the_interpreter() {
    let mut mb = ModuleBuilder::new();
    let h = mb.import("h", 1, 1);
    let big = add_switch(&mut mb, 40);
    let small = add_switch(&mut mb, 3);
    let mut f = FuncBuilder::new(1, 1);
    f.locals(2); // 1 = acc, 2 = i
    #[rustfmt::skip]
    f.ops(&[
        Block, Loop,
            LocalGet(2), Const(40), GeS, BrIf(1),
            LocalGet(1), LocalGet(0), LocalGet(2), Add, Call(big), Call(h), Add,
            LocalGet(0), LocalGet(2), Xor, Call(small), Add, LocalSet(1),
            LocalGet(2), Const(1), Add, LocalSet(2),
            Br(0),
        End, End,
        Const(0), LocalGet(1), Const(64), MemFill,
        Const(128), Const(0), Const(64), MemCopy,
        LocalGet(1), Const(130), Load64(0), Add,
    ]);
    let main = mb.add_func(f);
    mb.export("main", main);
    let m = mb.build();
    let it = Interp::new(&m).unwrap();
    for mode in MODES {
        for tree_max in [0u16, k10_jit::DEFAULT_BR_TREE_MAX, u16::MAX] {
            let jit = compile_with(&m, &opts(mode, tree_max)).unwrap();
            for x in [-100i64, -41, -1, 0, 1, 2, 3, 17, 39, 40, 41, 1000, i64::MAX - 50, i64::MIN] {
                let mut im = new_memory(&m, 12);
                let want = it.invoke(&mut im, 12, &mut H, main, &[x]);
                for preempt_on_call in [false, true] {
                    let mut gm = GuardedMem::for_module(&m, 12);
                    let (got, interrupts) = run_jit(&jit, &m, &mut gm, &mut H, main, &[x], RunOpts { preempt_on_call });
                    assert_eq!(got, want, "{mode:?} tree_max {tree_max} x={x}");
                    assert_eq!(gm.mem(), &im[..gm.mem().len()], "{mode:?}: memory");
                    gm.check_canaries();
                    assert_eq!(interrupts > 0, preempt_on_call, "{mode:?}: interrupt hook calls");
                }
            }
        }
    }
}
