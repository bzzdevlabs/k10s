mod common;

use common::*;
use k10_ir::{Interp, ModuleBuilder, Op, Trap};

struct NoHost;
impl k10_ir::Host for NoHost {
    fn call(&mut self, _: u32, _: &[i64], _: &mut [u8]) -> Result<i64, Trap> {
        Err(Trap::Host(0xDEAD))
    }
}

#[test]
fn add_two() {
    let mut mb = ModuleBuilder::new();
    let f = mb.func(2, 1, |f| {
        f.local_get(0).local_get(1).add();
    });
    mb.export("add", f);
    let m = mb.build();
    let jit = compile(&m).unwrap();
    let mut mem = GuardedMem::for_module(&m, 12);
    let (r, _) = run_jit(&jit, &m, &mut mem, &mut NoHost, f, &[40, 2], RunOpts::default());
    assert_eq!(r, Ok(Some(42)));
}

#[test]
fn fib() {
    let mut mb = ModuleBuilder::new();
    let idx = mb.next_index();
    let f = mb.func(1, 1, |f| {
        f.ops(&[
            Op::LocalGet(0),
            Op::Const(2),
            Op::LtS,
            Op::If,
            Op::LocalGet(0),
            Op::Return,
            Op::End,
            Op::LocalGet(0),
            Op::Const(1),
            Op::Sub,
            Op::Call(idx),
            Op::LocalGet(0),
            Op::Const(2),
            Op::Sub,
            Op::Call(idx),
            Op::Add,
        ]);
    });
    mb.export("fib", f);
    let m = mb.build();
    let jit = compile(&m).unwrap();
    let it = Interp::new(&m).unwrap();
    for n in [0, 1, 2, 10, 20] {
        let mut mem = GuardedMem::for_module(&m, 12);
        let (r, _) = run_jit(&jit, &m, &mut mem, &mut NoHost, f, &[n], RunOpts::default());
        let mut imem = k10_ir::new_memory(&m, 12);
        assert_eq!(r, it.invoke(&mut imem, 12, &mut NoHost, f, &[n]), "fib({n})");
    }
}

/// The no_std path: compile to a blob, load it into caller-provided memory, invoke through `BlobMeta`.
#[test]
fn blob_in_caller_memory() {
    use k10_jit::{CompileOptions, VmCtx};
    let m = k10_ir::gen::gen_module(11, &k10_ir::gen::GenConfig { imports: vec![], ..Default::default() });
    let blob = compile_blob(&m, &CompileOptions::default()).unwrap();
    let len = blob.len().div_ceil(4096) * 4096 + 4096;
    // SAFETY: anonymous mapping, W^X switch before executing, unmapped at the end.
    unsafe {
        let p = libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_PRIVATE | libc::MAP_ANONYMOUS, -1, 0);
        assert_ne!(p, libc::MAP_FAILED);
        // Load at a non-page-aligned (but 16-aligned) address: the code is position independent.
        let base = (p as *mut u8).add(48);
        let dst = std::slice::from_raw_parts_mut(base, len - 48);
        assert!(!blob.write_to(&mut dst[..blob.len() - 1]), "too small must be refused");
        assert!(blob.write_to(dst));
        assert_eq!(libc::mprotect(p, len, libc::PROT_READ | libc::PROT_EXEC), 0);
        let it = Interp::new(&m).unwrap();
        let main = blob.meta().export("main").unwrap();
        let (np, _) = blob.meta().func_type(main).unwrap();
        let args = vec![5i64; np as usize];
        let mut im = k10_ir::new_memory(&m, 12);
        let ri = it.invoke(&mut im, 12, &mut NoHost, main, &args);
        let mut mem = GuardedMem::for_module(&m, 12);
        let mut ctx = VmCtx::new(mem.base(), 4095, TRAMPS.as_ptr(), k10_jit::abi::interrupt_continue, std::ptr::null_mut());
        let rj = blob.meta().invoke(base, &mut ctx, main, &args);
        assert_eq!(ri, rj);
        assert!(im == mem.mem());
        libc::munmap(p, len);
    }
}
