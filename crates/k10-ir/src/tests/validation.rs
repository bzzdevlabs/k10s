//! Validation: one negative test per rule, limits accepted at the boundary, ModuleInfo contents.

use crate::builder::{FuncBuilder, ModuleBuilder};
use crate::types::*;
use crate::validate::{validate, BlockInfo, BlockKind, ValidateError, ValidateErrorKind as K};
use Op::*;

fn func(params: u8, results: u8, code: &[Op]) -> Func {
    Func { params, results, locals: 0, code: code.to_vec(), br_tables: vec![] }
}

fn module_with(f: Func) -> Module {
    Module { min_mem_log2: 12, funcs: vec![f], ..Default::default() }
}

/// Validates a single-function module and returns the error kind and pc.
fn err(f: Func) -> (K, Option<u32>) {
    let e: ValidateError = validate(&module_with(f)).expect_err("expected a validation error");
    assert_eq!(e.func, Some(0));
    let _ = format!("{e}"); // Display never panics
    (e.kind, e.pc)
}

fn ok(f: Func) {
    let m = module_with(f);
    if let Err(e) = validate(&m) {
        panic!("unexpected error {e} in {m:?}");
    }
}

fn merr(m: &Module) -> K {
    let e = validate(m).expect_err("expected a validation error");
    assert_eq!(e.func, None);
    e.kind
}

#[test]
fn mem_log2_range() {
    for k in [0, 11, 37, 255] {
        let m = Module { min_mem_log2: k, ..Default::default() };
        assert_eq!(merr(&m), K::MemLog2(k));
    }
    for k in [12, 36] {
        validate(&Module { min_mem_log2: k, ..Default::default() }).unwrap();
    }
}

#[test]
fn import_limits() {
    let mut m = Module { min_mem_log2: 12, ..Default::default() };
    m.imports.push(Import { name: "a".into(), params: 5, results: 1 });
    validate(&m).unwrap();
    m.imports.push(Import { name: "b".into(), params: 6, results: 0 });
    assert_eq!(merr(&m), K::ImportParams { import: 1, params: 6 });
    m.imports[1].params = 0;
    m.imports[1].results = 2;
    assert_eq!(merr(&m), K::ImportResults { import: 1, results: 2 });
}

#[test]
fn func_results() {
    assert_eq!(err(func(0, 2, &[End])), (K::FuncResults(2), None));
}

#[test]
fn locals_limit() {
    let mut f = func(1, 0, &[End]);
    f.locals = MAX_LOCALS - 1;
    ok(f.clone());
    f.locals = MAX_LOCALS;
    assert_eq!(err(f.clone()), (K::TooManyLocals(MAX_LOCALS as u64 + 1), None));
    f.params = 255;
    f.locals = u32::MAX;
    assert_eq!(err(f), (K::TooManyLocals(255 + u32::MAX as u64), None));
}

#[test]
fn code_size_limit() {
    // Const(1000) = 3 bytes, Drop = 1 byte: 262_144 pairs = exactly 1 MiB, plus `end`.
    let mut code = Vec::new();
    for _ in 0..262_143 {
        code.push(Const(1000));
        code.push(Drop);
    }
    code.push(Const(1)); // 2 bytes
    code.push(Drop);
    code.push(End); // 4 * 262_143 + 2 + 1 + 1 = 1 MiB exactly
    let f = func(0, 0, &code);
    assert_eq!(f.encoded_len(), MAX_CODE_BYTES);
    ok(f.clone());
    let mut f2 = f;
    f2.code.insert(0, Nop);
    assert!(matches!(err(f2).0, K::CodeTooLarge(_)));
}

#[test]
fn exports() {
    let mut mb = ModuleBuilder::new();
    let imp = mb.import("h", 0, 0);
    let f = mb.func(0, 0, |_| {});
    mb.export("a", f).export("b", f);
    validate(mb.module()).unwrap();
    mb.export("a", f);
    assert_eq!(merr(mb.module()), K::DuplicateExport { export: 2, first: 0 });

    let mut mb = ModuleBuilder::new();
    let imp2 = mb.import("h", 0, 0);
    assert_eq!(imp, imp2);
    mb.func(0, 0, |_| {});
    mb.export("imp", imp);
    assert_eq!(merr(mb.module()), K::ExportTarget { export: 0, func: 0 });
    let mut m = mb.build();
    m.exports[0].func = 2;
    assert_eq!(merr(&m), K::ExportTarget { export: 0, func: 2 });
}

#[test]
fn data_segments() {
    let mut m = Module { min_mem_log2: 12, ..Default::default() };
    m.data.push(Data { offset: 4096 - 3, bytes: vec![1, 2, 3] });
    m.data.push(Data { offset: 4096, bytes: vec![] });
    validate(&m).unwrap();
    m.data.push(Data { offset: 4096 - 3, bytes: vec![1, 2, 3, 4] });
    assert_eq!(merr(&m), K::DataOutOfRange { segment: 2 });
    m.data[2] = Data { offset: u64::MAX, bytes: vec![1] };
    assert_eq!(merr(&m), K::DataOutOfRange { segment: 2 });
    m.data[2] = Data { offset: 4097, bytes: vec![] };
    assert_eq!(merr(&m), K::DataOutOfRange { segment: 2 });
}

#[test]
fn local_index() {
    let mut f = func(1, 1, &[LocalGet(0), LocalTee(1), End]);
    f.locals = 1;
    ok(f.clone());
    f.locals = 0;
    assert_eq!(err(f), (K::LocalIndex(1), Some(1)));
    assert_eq!(err(func(0, 0, &[Const(1), LocalSet(0), End])), (K::LocalIndex(0), Some(1)));
}

#[test]
fn call_target() {
    let mut m = Module { min_mem_log2: 12, ..Default::default() };
    m.imports.push(Import { name: "h".into(), params: 1, results: 1 });
    m.funcs.push(func(0, 1, &[Const(1), Call(0), End]));
    m.funcs.push(func(0, 1, &[Call(1), End]));
    validate(&m).unwrap();
    m.funcs[1].code[0] = Call(3);
    let e = validate(&m).unwrap_err();
    assert_eq!((e.func, e.pc, e.kind), (Some(1), Some(0), K::CallTarget(3)));
}

#[test]
fn br_table_tables() {
    let mut f = func(0, 0, &[Block, Const(0), BrTable(0), End, End]);
    f.br_tables = vec![vec![0, 0]];
    ok(f.clone());
    f.code[2] = BrTable(1);
    assert_eq!(err(f.clone()), (K::BrTableIndex(1), Some(2)));
    f.code[2] = BrTable(0);
    f.br_tables = vec![vec![]];
    assert_eq!(err(f.clone()), (K::BrTableEmpty(0), Some(2)));
    f.br_tables = vec![vec![0, 1]];
    assert_eq!(err(f), (K::BranchDepth(1), Some(2)));
}

#[test]
fn branch_depth() {
    // The function body is not a label.
    assert_eq!(err(func(0, 0, &[Br(0), End])), (K::BranchDepth(0), Some(0)));
    assert_eq!(err(func(0, 0, &[Const(1), BrIf(0), End])), (K::BranchDepth(0), Some(1)));
    ok(func(0, 0, &[Block, Loop, Br(1), End, End, End]));
    assert_eq!(err(func(0, 0, &[Block, Loop, Br(2), End, End, End])), (K::BranchDepth(2), Some(2)));
}

#[test]
fn memory_offset() {
    ok(func(0, 1, &[Const(0), Load64((1 << 31) - 1), End]));
    assert_eq!(err(func(0, 1, &[Const(0), Load8U(1 << 31), End])), (K::OffsetTooLarge(1 << 31), Some(1)));
    assert_eq!(err(func(0, 0, &[Const(0), Const(0), Store64(u32::MAX), End])), (K::OffsetTooLarge(u32::MAX), Some(2)));
}

#[test]
fn stack_underflow() {
    assert_eq!(err(func(0, 1, &[Const(1), Add, End])), (K::StackUnderflow { needed: 2, height: 1 }, Some(1)));
    assert_eq!(err(func(0, 0, &[Drop, End])), (K::StackUnderflow { needed: 1, height: 0 }, Some(0)));
    assert_eq!(
        err(func(0, 0, &[Const(1), Const(2), MemCopy, End])),
        (K::StackUnderflow { needed: 3, height: 2 }, Some(2))
    );
    let mut m = module_with(func(0, 0, &[Const(1), Call(1), End]));
    m.funcs.push(func(2, 0, &[End]));
    let e = validate(&m).unwrap_err();
    assert_eq!((e.pc, e.kind), (Some(1), K::StackUnderflow { needed: 2, height: 1 }));
}

#[test]
fn stack_discipline_at_control() {
    let h = |expected, height| K::StackHeight { expected, height };
    // block/loop at height 0 only
    assert_eq!(err(func(0, 0, &[Const(1), Block, End, Drop, End])), (h(0, 1), Some(1)));
    assert_eq!(err(func(0, 0, &[Const(1), Loop, End, Drop, End])), (h(0, 1), Some(1)));
    // if consumes exactly the condition
    assert_eq!(err(func(0, 0, &[If, End, End])), (h(1, 0), Some(0)));
    assert_eq!(err(func(0, 0, &[Const(1), Const(1), If, End, Drop, End])), (h(1, 2), Some(2)));
    // else / end of a block at height 0
    assert_eq!(err(func(0, 0, &[Const(1), If, Const(2), Else, End, End])), (h(0, 1), Some(3)));
    assert_eq!(err(func(0, 0, &[Block, Const(2), End, End])), (h(0, 1), Some(2)));
    // br at height 0, br_if / br_table at height 1
    assert_eq!(err(func(0, 0, &[Block, Const(2), Br(0), End, End])), (h(0, 1), Some(2)));
    assert_eq!(err(func(0, 0, &[Block, Const(2), Const(1), BrIf(0), Drop, End, End])), (h(1, 2), Some(3)));
    let mut f = func(0, 0, &[Block, Const(2), Const(1), BrTable(0), End, End]);
    f.br_tables = vec![vec![0]];
    assert_eq!(err(f), (h(1, 2), Some(3)));
    // return / final end with exactly `results`
    assert_eq!(err(func(0, 1, &[Return, End])), (h(1, 0), Some(0)));
    assert_eq!(err(func(0, 0, &[Const(1), Return, End])), (h(0, 1), Some(1)));
    assert_eq!(err(func(0, 1, &[Const(1), Const(2), Return, End])), (h(1, 2), Some(2)));
    assert_eq!(err(func(0, 1, &[End])), (h(1, 0), Some(0)));
    assert_eq!(err(func(0, 0, &[Const(1), End])), (h(0, 1), Some(1)));
    assert_eq!(err(func(0, 1, &[Block, Br(0), End, End])), (h(1, 0), Some(3)));
}

#[test]
fn dead_code_rules() {
    // After an unconditional branch only end/else may follow, and its height check is skipped.
    for term in [Unreachable, Return] {
        assert_eq!(err(func(0, 0, &[Block, term, Nop, End, End])), (K::DeadCode, Some(2)));
    }
    assert_eq!(err(func(0, 0, &[Block, Br(0), Const(1), Drop, End, End])), (K::DeadCode, Some(2)));
    let mut f = func(0, 0, &[Block, Const(0), BrTable(0), Nop, End, End]);
    f.br_tables = vec![vec![0]];
    assert_eq!(err(f), (K::DeadCode, Some(3)));
    // Skipped checks: leftover operands before the end/else that follows.
    ok(func(0, 0, &[Block, Const(1), Const(2), Unreachable, End, End]));
    ok(func(0, 1, &[Const(1), If, Const(5), Return, Else, Const(1), Const(2), Unreachable, End, Const(3), End]));
    ok(func(0, 1, &[Const(1), Return, End])); // final end after return
    ok(func(0, 1, &[Unreachable, End])); // final end after unreachable: results check skipped
    ok(func(0, 0, &[Const(7), Unreachable, End]));
    ok(func(0, 0, &[Loop, Br(0), End, End]));
}

#[test]
fn stack_depth_limit() {
    let mut code: Vec<Op> = (0..MAX_OPERAND_STACK).map(|_| Const(0)).collect();
    code.extend((0..MAX_OPERAND_STACK).map(|_| Drop));
    code.push(End);
    ok(func(0, 0, &code));
    code.insert(0, Const(0));
    code.insert(code.len() - 1, Drop);
    assert_eq!(err(func(0, 0, &code)), (K::StackTooDeep, Some(MAX_OPERAND_STACK as u32)));
}

#[test]
fn nesting_limit() {
    let mut code: Vec<Op> = (0..MAX_NESTING).map(|i| if i % 2 == 0 { Block } else { Loop }).collect();
    code.extend((0..=MAX_NESTING).map(|_| End));
    ok(func(0, 0, &code));
    code.insert(0, Block);
    code.push(End);
    assert_eq!(err(func(0, 0, &code)), (K::NestingTooDeep, Some(MAX_NESTING as u32)));
}

#[test]
fn else_placement() {
    assert_eq!(err(func(0, 0, &[Block, Else, End, End])), (K::ElseWithoutIf, Some(1)));
    assert_eq!(err(func(0, 0, &[Else, End])), (K::ElseWithoutIf, Some(0)));
    assert_eq!(err(func(0, 0, &[Const(1), If, Else, Else, End, End])), (K::ElseWithoutIf, Some(3)));
    assert_eq!(err(func(0, 0, &[Const(1), If, Block, Else, End, End, End])), (K::ElseWithoutIf, Some(3)));
}

#[test]
fn termination() {
    assert_eq!(err(func(0, 0, &[])), (K::MissingEnd, Some(0)));
    assert_eq!(err(func(0, 0, &[Nop])), (K::MissingEnd, Some(1)));
    assert_eq!(err(func(0, 0, &[Block, End])), (K::MissingEnd, Some(2)));
    assert_eq!(err(func(0, 0, &[End, Nop])), (K::CodeAfterEnd, Some(1)));
    assert_eq!(err(func(0, 0, &[End, End])), (K::CodeAfterEnd, Some(1)));
}

#[test]
fn errors_name_the_function() {
    let mut m = Module { min_mem_log2: 12, ..Default::default() };
    m.funcs.push(func(0, 0, &[End]));
    m.funcs.push(func(0, 0, &[Nop, Nop, Add, End]));
    let e = validate(&m).unwrap_err();
    assert_eq!(e.func, Some(1));
    assert_eq!(e.pc, Some(2));
    let s = format!("{e}");
    assert!(s.contains("func 1") && s.contains("instr 2") && s.contains("underflow"), "{s}");
}

#[test]
fn module_info() {
    let mut fb = FuncBuilder::new(2, 1);
    fb.locals(3);
    fb.block() // 0
        .loop_() // 1
        .local_get(0) // 2
        .if_() // 3
        .const_(1) // 4
        .const_(2) // 5
        .add() // 6
        .drop() // 7
        .else_() // 8
        .br(2) // 9
        .end() // 10
        .local_get(1) // 11
        .if_() // 12
        .end() // 13
        .end() // 14
        .end() // 15
        .const_(5); // 16
    let mut m = ModuleBuilder::new();
    let h = m.import("h", 0, 0);
    let f = m.add_func(fb);
    let g = m.func(0, 0, |f| {
        f.call(h);
    });
    m.export("f", f).export("g", g);
    let info = validate(m.module()).unwrap();
    let fi = &info.funcs[0];
    assert_eq!(fi.num_locals, 5);
    assert_eq!(fi.max_stack, 2);
    assert_eq!(fi.max_nesting, 3);
    assert!(fi.leaf);
    assert!(!info.funcs[1].leaf);
    assert_eq!(fi.frame_slots(), 7);
    assert_eq!(fi.code_bytes as usize, m.module().funcs[0].encoded_len());
    assert_eq!(
        fi.blocks,
        vec![
            BlockInfo { kind: BlockKind::Block, start: 0, else_at: None, end: 15 },
            BlockInfo { kind: BlockKind::Loop, start: 1, else_at: None, end: 14 },
            BlockInfo { kind: BlockKind::If, start: 3, else_at: Some(8), end: 10 },
            BlockInfo { kind: BlockKind::If, start: 12, else_at: None, end: 13 },
        ]
    );
    assert_eq!(fi.block_at(12).unwrap().end, 13);
    assert!(fi.block_at(2).is_none());
}

#[test]
fn display_all_kinds() {
    // Every error kind renders a human-readable reason.
    let kinds = [
        K::MemLog2(1),
        K::ImportParams { import: 0, params: 9 },
        K::ImportResults { import: 0, results: 2 },
        K::TooManyFuncs,
        K::FuncResults(2),
        K::TooManyLocals(70000),
        K::CodeTooLarge(2 << 20),
        K::DuplicateExport { export: 1, first: 0 },
        K::ExportTarget { export: 0, func: 0 },
        K::DataOutOfRange { segment: 0 },
        K::LocalIndex(3),
        K::CallTarget(3),
        K::BrTableIndex(0),
        K::BrTableEmpty(0),
        K::BranchDepth(0),
        K::OffsetTooLarge(1 << 31),
        K::StackUnderflow { needed: 2, height: 1 },
        K::StackHeight { expected: 0, height: 1 },
        K::StackTooDeep,
        K::NestingTooDeep,
        K::ElseWithoutIf,
        K::DeadCode,
        K::CodeAfterEnd,
        K::MissingEnd,
    ];
    for k in kinds {
        assert!(!format!("{k}").is_empty());
    }
}
