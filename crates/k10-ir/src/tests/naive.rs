//! A deliberately naive transliteration of docs/TIR.md, used as an independent oracle for the prepared
//! interpreter: it walks `Module::code` directly, keeps a real operand stack and label stack per frame, and finds
//! matching `else`/`end` through `FuncInfo::blocks` (which also cross-checks the validator's control info).
//! It shares only the per-op semantic helpers of `crate::sem` (tested separately).

use crate::sem;
use crate::types::*;
use crate::validate::{validate, BlockKind, ModuleInfo};

pub struct Naive<'a> {
    pub m: &'a Module,
    pub info: ModuleInfo,
    pub max_depth: usize,
    /// Number of instructions executed by the last invocation.
    pub steps: u64,
    /// Print every executed instruction (set `NAIVE_TRACE=1` when debugging a mismatch).
    pub trace: bool,
}

struct Label {
    kind: BlockKind,
    start: usize,
    end: usize,
}

struct Frame {
    func: usize,
    pc: usize,
    locals: Vec<i64>,
    stack: Vec<i64>,
    labels: Vec<Label>,
}

impl<'a> Naive<'a> {
    pub fn new(m: &'a Module) -> Self {
        let info = validate(m).expect("naive: module must validate");
        Naive { m, info, max_depth: 10_000, steps: 0, trace: std::env::var_os("NAIVE_TRACE").is_some() }
    }

    fn new_frame(&self, func: usize, args: &[i64]) -> Frame {
        let f = &self.m.funcs[func];
        let mut locals = args.to_vec();
        locals.resize(f.params as usize + f.locals as usize, 0);
        Frame { func, pc: 0, locals, stack: Vec::new(), labels: Vec::new() }
    }

    pub fn invoke(
        &mut self,
        mem: &mut [u8],
        k: u8,
        host: &mut dyn Host,
        func: u32,
        args: &[i64],
    ) -> Result<Option<i64>, Trap> {
        self.steps = 0;
        let size = 1u64 << k;
        assert_eq!(mem.len() as u64, size + TAIL_BYTES as u64);
        let nimp = self.m.imports.len();
        let fi = func as usize - nimp;
        assert_eq!(args.len(), self.m.funcs[fi].params as usize);
        let mut frames: Vec<Frame> = vec![self.new_frame(fi, args)];
        host.interrupt()?; // entry safepoint
        loop {
            let fr = frames.last_mut().unwrap();
            let f = &self.m.funcs[fr.func];
            let finfo = &self.info.funcs[fr.func];
            let op = f.code[fr.pc];
            self.steps += 1;
            if self.trace {
                println!("TRACE f{} pc{} {:?} stack {:?} locals {:?}", fr.func, fr.pc, op, fr.stack, fr.locals);
            }
            let mut next = fr.pc + 1;
            macro_rules! pop {
                () => {
                    fr.stack.pop().expect("naive: stack underflow")
                };
            }
            // Branch to label `d`: returns the new pc.
            let branch = |fr: &mut Frame, d: u32| -> usize {
                let idx = fr.labels.len() - 1 - d as usize;
                let (kind, start, end) = (fr.labels[idx].kind, fr.labels[idx].start, fr.labels[idx].end);
                fr.labels.truncate(idx);
                if kind == BlockKind::Loop {
                    start // re-executes `loop`: safepoint and label push
                } else {
                    end + 1
                }
            };
            let mut ret = false;
            match op {
                Op::Unreachable => return Err(Trap::Unreachable),
                Op::Nop => {}
                Op::Block | Op::Loop => {
                    let b = finfo.block_at(fr.pc as u32).expect("block info");
                    if op == Op::Loop {
                        assert_eq!(b.kind, BlockKind::Loop);
                        host.interrupt()?;
                    }
                    fr.labels.push(Label { kind: b.kind, start: fr.pc, end: b.end as usize });
                }
                Op::If => {
                    let c = pop!();
                    let b = *finfo.block_at(fr.pc as u32).expect("if info");
                    if c != 0 {
                        fr.labels.push(Label { kind: BlockKind::If, start: fr.pc, end: b.end as usize });
                    } else if let Some(e) = b.else_at {
                        fr.labels.push(Label { kind: BlockKind::If, start: fr.pc, end: b.end as usize });
                        next = e as usize + 1;
                    } else {
                        next = b.end as usize + 1;
                    }
                }
                Op::Else => {
                    // End of a then-part reached by fallthrough.
                    let l = fr.labels.pop().unwrap();
                    next = l.end + 1;
                }
                Op::End => {
                    if fr.labels.pop().is_none() {
                        ret = true;
                    }
                }
                Op::Br(d) => next = branch(fr, d),
                Op::BrIf(d) => {
                    if pop!() != 0 {
                        next = branch(fr, d);
                    }
                }
                Op::BrTable(t) => {
                    let i = pop!() as u64;
                    let tab = &f.br_tables[t as usize];
                    let n = tab.len() as u64 - 1;
                    let d = if i < n { tab[i as usize] } else { tab[n as usize] };
                    next = branch(fr, d);
                }
                Op::Return => ret = true,
                Op::Call(idx) => {
                    let (p, r) = self.m.func_type(idx).unwrap();
                    let at = fr.stack.len() - p as usize;
                    let args: Vec<i64> = fr.stack.split_off(at);
                    fr.pc = next;
                    if (idx as usize) < nimp {
                        let v = host.call(idx, &args, mem)?;
                        if r == 1 {
                            fr.stack.push(v);
                        }
                        continue;
                    }
                    if frames.len() >= self.max_depth {
                        return Err(Trap::StackOverflow);
                    }
                    let nf = self.new_frame(idx as usize - nimp, &args);
                    frames.push(nf);
                    host.interrupt()?;
                    continue;
                }
                Op::Drop => {
                    pop!();
                }
                Op::Select => {
                    let c = pop!();
                    let b = pop!();
                    let a = pop!();
                    fr.stack.push(if c != 0 { a } else { b });
                }
                Op::LocalGet(i) => fr.stack.push(fr.locals[i as usize]),
                Op::LocalSet(i) => fr.locals[i as usize] = pop!(),
                Op::LocalTee(i) => fr.locals[i as usize] = *fr.stack.last().unwrap(),
                Op::Const(v) => fr.stack.push(v),
                Op::MemSize => fr.stack.push(size as i64),
                Op::MemCopy => {
                    let n = pop!();
                    let s = pop!();
                    let d = pop!();
                    sem::mem_copy(mem, size, d, s, n)?;
                }
                Op::MemFill => {
                    let n = pop!();
                    let b = pop!();
                    let d = pop!();
                    sem::mem_fill(mem, size, d, b, n)?;
                }
                _ => {
                    if let Some((w, signed, off)) = sem::load_kind(op) {
                        let a = pop!();
                        let e = ((a as u64).wrapping_add(off as u64) % size) as usize;
                        fr.stack.push(sem::load(mem, e, w, signed));
                    } else if let Some((w, off)) = sem::store_kind(op) {
                        let v = pop!();
                        let a = pop!();
                        let e = ((a as u64).wrapping_add(off as u64) % size) as usize;
                        sem::store(mem, e, w, v);
                    } else if let Some(r) = {
                        let len = fr.stack.len();
                        if len >= 2 {
                            sem::eval_binary(op, fr.stack[len - 2], fr.stack[len - 1])
                        } else {
                            None
                        }
                    } {
                        pop!();
                        pop!();
                        fr.stack.push(r?);
                    } else {
                        let a = pop!();
                        fr.stack.push(sem::eval_unary(op, a).expect("naive: unknown op"));
                    }
                }
            }
            if ret {
                let results = f.results as usize;
                let v = if results == 1 { Some(*fr.stack.last().unwrap()) } else { None };
                frames.pop();
                match frames.last_mut() {
                    None => return Ok(v),
                    Some(caller) => {
                        if let Some(v) = v {
                            caller.stack.push(v);
                        }
                    }
                }
            } else {
                frames.last_mut().unwrap().pc = next;
            }
        }
    }
}
