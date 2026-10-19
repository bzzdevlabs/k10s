//! End-to-end programs, calls, host interaction, traps, stack overflow, interrupts, invocation errors.

use super::util::*;
use crate::builder::{FuncBuilder, ModuleBuilder};
use crate::interp::{new_memory, Interp, InterpConfig, InterpStack};
use crate::types::*;
use std::sync::Arc;

fn fib_module() -> Module {
    let mut mb = ModuleBuilder::new();
    let fib = mb.next_index();
    mb.func(1, 1, |f| {
        // if n < 2 { return n } return fib(n-1) + fib(n-2)
        f.local_get(0).const_(2).lt_s().if_().local_get(0).return_().end();
        f.local_get(0).const_(1).sub().call(fib);
        f.local_get(0).const_(2).sub().call(fib);
        f.add();
    });
    mb.export("fib", fib);
    mb.into_module()
}

#[test]
fn recursive_fib() {
    let m = fib_module();
    assert_eq!(run(&m, "fib", &[0]), Ok(Some(0)));
    assert_eq!(run(&m, "fib", &[1]), Ok(Some(1)));
    assert_eq!(run(&m, "fib", &[20]), Ok(Some(6765)));
    assert_eq!(run(&m, "fib", &[25]), Ok(Some(75025)));
    let (o, _) = differential(&m, 12, 0, &[15], u64::MAX, 100);
    assert_eq!(o.result, Ok(Some(610)));
    assert_eq!(o.interrupts, 1973); // one safepoint per call: calls(fib 15) = 2*fib(16) - 1
}

#[test]
fn loop_with_br_table_state_machine() {
    // acc = 0; for i in 0..n { switch i % 5 { 0: acc += i; 1: acc ^= i; 2: acc *= 3; 3: continue; _: acc -= 1 } }
    let mut mb = ModuleBuilder::new();
    let f = mb.func(1, 1, |f| {
        let i = f.new_local();
        let acc = f.new_local();
        f.block() // exit
            .loop_() // continue target
            .local_get(i)
            .local_get(0)
            .ge_s()
            .br_if(1)
            .block() // after-switch
            .block() // default
            .block() // case 3
            .block() // case 2
            .block() // case 1
            .block() // case 0
            .local_get(i)
            .const_(5)
            .rem_s()
            .br_table(&[0, 1, 2, 3], 4)
            .end() // case 0 (inside C1; A is 4 labels out)
            .local_get(acc)
            .local_get(i)
            .add()
            .local_set(acc)
            .br(4)
            .end() // case 1
            .local_get(acc)
            .local_get(i)
            .xor()
            .local_set(acc)
            .br(3)
            .end() // case 2
            .local_get(acc)
            .const_(3)
            .mul()
            .local_set(acc)
            .br(2)
            .end() // case 3: increment and continue
            .local_get(i)
            .const_(1)
            .add()
            .local_set(i)
            .br(2)
            .end() // default
            .local_get(acc)
            .const_(1)
            .sub()
            .local_set(acc)
            .end() // after-switch
            .local_get(i)
            .const_(1)
            .add()
            .local_set(i)
            .br(0)
            .end()
            .end()
            .local_get(acc);
    });
    mb.export("f", f);
    let m = mb.into_module();
    let expect = |n: i64| {
        let mut acc: i64 = 0;
        for i in 0..n {
            match i % 5 {
                0 => acc = acc.wrapping_add(i),
                1 => acc ^= i,
                2 => acc = acc.wrapping_mul(3),
                3 => continue,
                _ => acc = acc.wrapping_sub(1),
            }
        }
        acc
    };
    for n in [0, 1, 2, 5, 17, 100, 1000] {
        assert_eq!(run(&m, "f", &[n]), Ok(Some(expect(n))), "n = {n}");
    }
    differential(&m, 12, 0, &[200], u64::MAX, 10);
}

#[test]
fn sieve_of_eratosthenes() {
    // Counts primes < n using one byte per number in memory.
    let mut mb = ModuleBuilder::new();
    mb.mem_log2(16);
    let f = mb.func(1, 1, |f| {
        let i = f.new_local();
        let j = f.new_local();
        let count = f.new_local();
        // mem.fill(0, 1, n)
        f.const_(0).const_(1).local_get(0).mem_fill();
        f.const_(2).local_set(i);
        f.block().loop_();
        {
            f.local_get(i).local_get(i).mul().local_get(0).ge_s().br_if(1);
            f.local_get(i).load8_u(0).if_();
            {
                f.local_get(i).local_get(i).mul().local_set(j);
                f.block().loop_();
                f.local_get(j).local_get(0).ge_s().br_if(1);
                f.local_get(j).const_(0).store8(0);
                f.local_get(j).local_get(i).add().local_set(j);
                f.br(0).end().end();
            }
            f.end();
            f.local_get(i).const_(1).add().local_set(i).br(0);
        }
        f.end().end();
        // count
        f.const_(2).local_set(i);
        f.block().loop_();
        f.local_get(i).local_get(0).ge_u().br_if(1);
        f.local_get(count).local_get(i).load8_u(0).add().local_set(count);
        f.local_get(i).const_(1).add().local_set(i).br(0);
        f.end().end();
        f.local_get(count);
    });
    mb.export("primes", f);
    let m = mb.into_module();
    assert_eq!(run(&m, "primes", &[100]), Ok(Some(25)));
    assert_eq!(run(&m, "primes", &[10_000]), Ok(Some(1229)));
    assert_eq!(run(&m, "primes", &[65_536]), Ok(Some(6542)));
    assert_eq!(run(&m, "primes", &[65_537]), Err(Trap::MemOutOfBounds)); // fill beyond S
    differential(&m, 16, 0, &[3000], u64::MAX, 10);
}

#[test]
fn insertion_sort_in_memory() {
    // Fills n i64 with an LCG, sorts them (insertion sort), returns a checksum and whether sorted.
    let mut mb = ModuleBuilder::new();
    mb.mem_log2(14);
    let f = mb.func(1, 1, |f| {
        let i = f.new_local();
        let j = f.new_local();
        let x = f.new_local();
        let seed = f.new_local();
        let sum = f.new_local();
        f.const_(12345).local_set(seed);
        f.block().loop_();
        f.local_get(i).local_get(0).ge_s().br_if(1);
        f.local_get(seed).const_(6364136223846793005).mul().const_(1442695040888963407).add().local_tee(seed);
        f.local_set(x);
        f.local_get(i).const_(3).shl().local_get(x).store64(0);
        f.local_get(i).const_(1).add().local_set(i).br(0);
        f.end().end();
        // insertion sort
        f.const_(1).local_set(i);
        f.block().loop_();
        {
            f.local_get(i).local_get(0).ge_s().br_if(1);
            f.local_get(i).const_(3).shl().load64(0).local_set(x);
            f.local_get(i).const_(1).sub().local_set(j);
            f.block().loop_();
            {
                f.local_get(j).const_(0).lt_s().br_if(1);
                f.local_get(j).const_(3).shl().load64(0).local_get(x).le_s().br_if(1);
                // a[j+1] = a[j]  (via mem.copy to exercise it)
                f.local_get(j).const_(3).shl().const_(8).add();
                f.local_get(j).const_(3).shl();
                f.const_(8).mem_copy();
                f.local_get(j).const_(1).sub().local_set(j).br(0);
            }
            f.end().end();
            f.local_get(j).const_(1).add().const_(3).shl().local_get(x).store64(0);
            f.local_get(i).const_(1).add().local_set(i).br(0);
        }
        f.end().end();
        // checksum: sum of a[k] * (k + 1)
        f.const_(0).local_set(i);
        f.block().loop_();
        {
            f.local_get(i).local_get(0).ge_s().br_if(1);
            f.local_get(sum).local_get(i).const_(3).shl().load64(0).local_get(i).const_(1).add().mul().add();
            f.local_set(sum);
            f.local_get(i).const_(1).add().local_set(i).br(0);
        }
        f.end().end();
        f.local_get(sum);
    });
    mb.export("sort", f);
    let m = mb.into_module();
    let expect = |n: usize| {
        let mut seed: i64 = 12345;
        let mut v: Vec<i64> = (0..n)
            .map(|_| {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                seed
            })
            .collect();
        v.sort();
        v.iter().enumerate().fold(0i64, |s, (k, &x)| s.wrapping_add(x.wrapping_mul(k as i64 + 1)))
    };
    for n in [0, 1, 2, 10, 300] {
        assert_eq!(run(&m, "sort", &[n as i64]), Ok(Some(expect(n))), "n = {n}");
    }
    differential(&m, 14, 0, &[60], u64::MAX, 10);
}

#[test]
fn calls_pass_args_and_results() {
    let mut mb = ModuleBuilder::new();
    let h = mb.import("h", 5, 1);
    let h0 = mb.import("h0", 0, 0);
    // g(a, b, c) = a*100 + b*10 + c
    let g = mb.func(3, 1, |f| {
        f.local_get(0).const_(100).mul().local_get(1).const_(10).mul().add().local_get(2).add();
    });
    let v = mb.func(0, 0, |_| {});
    let f = mb.func(1, 1, |f| {
        // keep values on the stack across calls
        f.const_(7).local_get(0).const_(2).const_(3).call(g).call(v).add();
        f.const_(1).const_(2).const_(3).const_(4).const_(5).call(h).add();
        f.call(h0);
    });
    mb.export("f", f);
    let m = mb.into_module();
    struct H(Vec<Vec<i64>>);
    impl Host for H {
        fn call(&mut self, import: u32, args: &[i64], _mem: &mut [u8]) -> Result<i64, Trap> {
            self.0.push(args.to_vec());
            Ok(if import == 0 { args.iter().sum::<i64>() * 1000 } else { 99 })
        }
    }
    let it = Interp::new(&m).unwrap();
    let mut mem = new_memory(&m, 12);
    let mut host = H(vec![]);
    assert_eq!(it.invoke(&mut mem, 12, &mut host, f, &[9]), Ok(Some(7 + 923 + 15000)));
    assert_eq!(host.0, vec![vec![1, 2, 3, 4, 5], vec![]]);
}

#[test]
fn host_traps_and_memory_access() {
    let mut mb = ModuleBuilder::new();
    let h = mb.import("h", 2, 1);
    let f = mb.func(1, 1, |f| {
        f.const_(16).local_get(0).call(h).const_(16).load64(0).add();
    });
    mb.export("f", f);
    let m = mb.into_module();
    struct H;
    impl Host for H {
        fn call(&mut self, _: u32, args: &[i64], mem: &mut [u8]) -> Result<i64, Trap> {
            if args[1] < 0 {
                return Err(Trap::Host(42));
            }
            let a = args[0] as usize;
            mem[a..a + 8].copy_from_slice(&args[1].to_le_bytes());
            Ok(1)
        }
    }
    let it = Interp::new(&m).unwrap();
    let mut mem = new_memory(&m, 12);
    assert_eq!(it.invoke(&mut mem, 12, &mut H, f, &[500]), Ok(Some(501)));
    assert_eq!(it.invoke(&mut mem, 12, &mut H, f, &[-1]), Err(Trap::Host(42)));
}

#[test]
fn unreachable_traps() {
    let m = module1(1, 1, |f| {
        f.local_get(0).if_().unreachable().end().const_(5);
    });
    assert_eq!(run(&m, "f", &[0]), Ok(Some(5)));
    assert_eq!(run(&m, "f", &[1]), Err(Trap::Unreachable));
}

#[test]
fn trap_inside_callee_aborts_everything() {
    let mut mb = ModuleBuilder::new();
    let inner = mb.func(1, 1, |f| {
        f.const_(1).local_get(0).div_u();
    });
    let outer = mb.func(1, 1, |f| {
        f.const_(10).local_get(0).call(inner).add();
    });
    mb.export("f", outer);
    let m = mb.into_module();
    assert_eq!(run(&m, "f", &[1]), Ok(Some(11)));
    assert_eq!(run(&m, "f", &[0]), Err(Trap::DivByZero));
}

fn depth_module() -> Module {
    // rec(n) = n == 0 ? 0 : rec(n - 1) + 1
    let mut mb = ModuleBuilder::new();
    let rec = mb.next_index();
    mb.func(1, 1, |f| {
        f.local_get(0).eqz().if_().const_(0).return_().end();
        f.local_get(0).const_(1).sub().call(rec).const_(1).add();
    });
    mb.export("rec", rec);
    let forever = mb.next_index();
    mb.func(0, 0, |f| {
        f.call(forever);
    });
    mb.export("forever", forever);
    mb.into_module()
}

#[test]
fn stack_overflow_at_exact_depth() {
    let m = depth_module();
    let it = Interp::new(&m).unwrap();
    assert_eq!(it.config().max_call_depth, 10_000);
    let mut mem = new_memory(&m, 12);
    let rec = it.export("rec").unwrap();
    // rec(n) uses n + 1 frames.
    assert_eq!(it.invoke(&mut mem, 12, &mut NoHost, rec, &[9_999]), Ok(Some(9_999)));
    assert_eq!(it.invoke(&mut mem, 12, &mut NoHost, rec, &[10_000]), Err(Trap::StackOverflow));
    let forever = it.export("forever").unwrap();
    assert_eq!(it.invoke(&mut mem, 12, &mut NoHost, forever, &[]), Err(Trap::StackOverflow));
    // Configurable.
    let it = it.with_config(InterpConfig { max_call_depth: 5, ..Default::default() });
    assert_eq!(it.invoke(&mut mem, 12, &mut NoHost, rec, &[4]), Ok(Some(4)));
    assert_eq!(it.invoke(&mut mem, 12, &mut NoHost, rec, &[5]), Err(Trap::StackOverflow));
    let it = it.with_config(InterpConfig { max_call_depth: 0, ..Default::default() });
    assert_eq!(it.invoke(&mut mem, 12, &mut NoHost, rec, &[0]), Err(Trap::StackOverflow));
    // Same limit in the naive oracle.
    let (o, _) = differential(&m, 12, rec, &[300], u64::MAX, 300);
    assert_eq!(o.result, Err(Trap::StackOverflow));
    let (o, _) = differential(&m, 12, rec, &[299], u64::MAX, 300);
    assert_eq!(o.result, Ok(Some(299)));
}

#[test]
fn deep_recursion_does_not_use_the_native_stack() {
    // 10 000 TIR frames on a 128 KiB native stack: only possible with an explicit frame stack.
    let m = depth_module();
    let it = Arc::new(Interp::new(&m).unwrap());
    let it2 = it.clone();
    let r = std::thread::Builder::new()
        .stack_size(128 * 1024)
        .spawn(move || {
            let mut mem = new_memory(&m, 12);
            let rec = it2.export("rec").unwrap();
            (
                it2.invoke(&mut mem, 12, &mut NoHost, rec, &[9_999]),
                it2.invoke(&mut mem, 12, &mut NoHost, it2.export("forever").unwrap(), &[]),
            )
        })
        .unwrap()
        .join()
        .unwrap();
    assert_eq!(r, (Ok(Some(9_999)), Err(Trap::StackOverflow)));
    drop(it);
}

#[test]
fn stack_slot_budget() {
    // A function with 60k locals recursing: the slot budget (4 Mi slots) trips before 10 000 frames.
    let mut mb = ModuleBuilder::new();
    let f = mb.next_index();
    mb.func(0, 0, |b| {
        b.locals(60_000).call(f);
    });
    mb.export("f", f);
    let m = mb.into_module();
    assert_eq!(run(&m, "f", &[]), Err(Trap::StackOverflow));
    let it = Interp::new(&m).unwrap().with_config(InterpConfig { max_stack_slots: 1000, ..Default::default() });
    let mut mem = new_memory(&m, 12);
    assert_eq!(it.invoke(&mut mem, 12, &mut NoHost, f, &[]), Err(Trap::StackOverflow));
}

#[test]
fn locals_are_zeroed_on_every_call() {
    // f(n): l = l + n; if n { l = l + f(n - 1) }; l  — with l a fresh zero local, f(n) = n(n+1)/2.
    // The callee frame overlaps the caller's operand-stack slots, so stale values would show.
    let mut mb = ModuleBuilder::new();
    let f = mb.next_index();
    mb.func(1, 1, |b| {
        let l = b.new_local();
        b.local_get(l).local_get(0).add().local_set(l);
        b.local_get(0).if_().local_get(l).local_get(0).const_(1).sub().call(f).add().local_set(l).end();
        b.local_get(l);
    });
    mb.export("f", f);
    let m = mb.into_module();
    let it = Interp::new(&m).unwrap();
    let mut st = InterpStack::new();
    let mut mem = new_memory(&m, 12);
    for n in [3, 50, 7, 0, 100] {
        assert_eq!(it.invoke_with(&mut st, &mut mem, 12, &mut NoHost, f, &[n]), Ok(Some(n * (n + 1) / 2)));
    }
}

struct Budget {
    left: u64,
    calls: u64,
}

impl Host for Budget {
    fn call(&mut self, _: u32, _: &[i64], _: &mut [u8]) -> Result<i64, Trap> {
        Ok(0)
    }
    fn interrupt(&mut self) -> Result<(), Trap> {
        self.calls += 1;
        if self.left == 0 {
            return Err(Trap::Interrupted);
        }
        self.left -= 1;
        Ok(())
    }
}

fn spin_module() -> Module {
    // spin(n): loop forever if n < 0, else n iterations; returns the count.
    module1(1, 1, |f| {
        let c = f.new_local();
        f.loop_();
        f.local_get(c).const_(1).add().local_set(c);
        f.local_get(0).const_(0).lt_s().local_get(c).local_get(0).lt_s().or().br_if(0);
        f.end();
        f.local_get(c);
    })
}

#[test]
fn interrupt_at_safepoints() {
    let m = spin_module();
    let it = Interp::new(&m).unwrap();
    let mut mem = new_memory(&m, 12);
    // n iterations = 1 entry safepoint + n loop safepoints.
    let mut h = Budget { left: u64::MAX, calls: 0 };
    assert_eq!(it.invoke(&mut mem, 12, &mut h, 0, &[10]), Ok(Some(10)));
    assert_eq!(h.calls, 11);
    // An infinite loop is stopped by the host.
    let mut h = Budget { left: 1000, calls: 0 };
    assert_eq!(it.invoke(&mut mem, 12, &mut h, 0, &[-1]), Err(Trap::Interrupted));
    assert_eq!(h.calls, 1001);
    // Interrupt at function entry.
    let mut h = Budget { left: 0, calls: 0 };
    assert_eq!(it.invoke(&mut mem, 12, &mut h, 0, &[5]), Err(Trap::Interrupted));
    assert_eq!(h.calls, 1);
    // Any trap returned by interrupt() propagates.
    struct Other;
    impl Host for Other {
        fn call(&mut self, _: u32, _: &[i64], _: &mut [u8]) -> Result<i64, Trap> {
            Ok(0)
        }
        fn interrupt(&mut self) -> Result<(), Trap> {
            Err(Trap::Host(9))
        }
    }
    assert_eq!(it.invoke(&mut mem, 12, &mut Other, 0, &[5]), Err(Trap::Host(9)));
}

#[test]
fn safepoint_interval_countdown() {
    let m = spin_module();
    for interval in [1u32, 7, 100, 1024, 5000] {
        let it =
            Interp::new(&m).unwrap().with_config(InterpConfig { safepoint_interval: interval, ..Default::default() });
        let mut mem = new_memory(&m, 12);
        let mut h = Budget { left: u64::MAX, calls: 0 };
        // 10_000 iterations → 10_001 safepoints.
        assert_eq!(it.invoke(&mut mem, 12, &mut h, 0, &[10_000]), Ok(Some(10_000)));
        let eff = interval.clamp(1, 1024) as u64;
        assert_eq!(h.calls, 10_001 / eff, "interval {interval}");
        // Infinite loops are still stopped.
        let mut h = Budget { left: 3, calls: 0 };
        assert_eq!(it.invoke(&mut mem, 12, &mut h, 0, &[-1]), Err(Trap::Interrupted));
        assert_eq!(h.calls, 4);
    }
}

#[test]
fn recursion_interrupts_at_function_entry() {
    let m = depth_module();
    let it = Interp::new(&m).unwrap();
    let mut mem = new_memory(&m, 12);
    let mut h = Budget { left: 50, calls: 0 };
    assert_eq!(it.invoke(&mut mem, 12, &mut h, 0, &[100]), Err(Trap::Interrupted));
    assert_eq!(h.calls, 51);
}

#[test]
fn invocation_errors_do_not_panic() {
    let mut mb = ModuleBuilder::new();
    mb.import("h", 0, 1);
    mb.mem_log2(13);
    let f = mb.func(2, 1, |f| {
        f.local_get(0).local_get(1).add();
    });
    mb.export("f", f);
    let m = mb.into_module();
    let it = Interp::new(&m).unwrap();
    let mut mem = new_memory(&m, 13);
    assert_eq!(it.invoke(&mut mem, 13, &mut NoHost, f, &[1, 2]), Ok(Some(3)));
    // Wrong memory length / size.
    assert_eq!(it.invoke(&mut mem[..8192], 13, &mut NoHost, f, &[1, 2]), Err(Trap::MemOutOfBounds));
    let mut small = new_memory(&m, 12);
    assert_eq!(it.invoke(&mut small, 12, &mut NoHost, f, &[1, 2]), Err(Trap::MemOutOfBounds)); // k < min
    assert_eq!(it.invoke(&mut mem, 14, &mut NoHost, f, &[1, 2]), Err(Trap::MemOutOfBounds));
    assert_eq!(it.invoke(&mut mem, 37, &mut NoHost, f, &[1, 2]), Err(Trap::MemOutOfBounds));
    assert_eq!(it.invoke(&mut mem, 255, &mut NoHost, f, &[1, 2]), Err(Trap::MemOutOfBounds));
    // Not a defined function / wrong arity.
    assert_eq!(it.invoke(&mut mem, 13, &mut NoHost, 0, &[]), Err(Trap::Unreachable));
    assert_eq!(it.invoke(&mut mem, 13, &mut NoHost, 2, &[1, 2]), Err(Trap::Unreachable));
    assert_eq!(it.invoke(&mut mem, 13, &mut NoHost, u32::MAX, &[]), Err(Trap::Unreachable));
    assert_eq!(it.invoke(&mut mem, 13, &mut NoHost, f, &[1]), Err(Trap::Unreachable));
    // Exports and types.
    assert_eq!(it.export("f"), Some(1));
    assert_eq!(it.export("g"), None);
    assert_eq!(it.func_type(0), Some((0, 1)));
    assert_eq!(it.func_type(1), Some((2, 1)));
    assert_eq!(it.func_type(2), None);
    assert_eq!(it.min_mem_log2(), 13);
    let _ = format!("{it:?}");
}

#[test]
fn invalid_module_is_rejected() {
    let m = module1(0, 1, |f| {
        f.add();
    });
    assert!(Interp::new(&m).is_err());
}

#[test]
fn interp_is_send_sync_and_shareable() {
    fn assert_send_sync<T: Send + Sync + 'static>() {}
    assert_send_sync::<Interp>();
    let m = fib_module();
    let it = Arc::new(Interp::new(&m).unwrap());
    let handles: Vec<_> = (0..4)
        .map(|t| {
            let it = it.clone();
            let m = m.clone();
            std::thread::spawn(move || {
                let mut mem = new_memory(&m, 12);
                it.invoke(&mut mem, 12, &mut NoHost, 0, &[15 + t]).unwrap().unwrap()
            })
        })
        .collect();
    let r: Vec<i64> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(r, vec![610, 987, 1597, 2584]);
}

#[test]
fn translation_corner_cases() {
    // Folded `local.get`s must observe values from before later `local.set`/`local.tee` of the same local.
    let cases: Vec<(Vec<Op>, u32, i64)> = {
        use Op::*;
        vec![
            // swap: l0, l1 = l1, l0 ; return l0 * 10 + l1   (args 1, 2 → 21)
            (
                vec![LocalGet(0), LocalGet(1), LocalSet(0), LocalSet(1), LocalGet(0), Const(10), Mul, LocalGet(1), Add],
                0,
                21,
            ),
            // l0 used, then overwritten while still on the stack
            (vec![LocalGet(0), LocalGet(0), Const(5), Add, LocalSet(0), LocalGet(0), Sub], 0, -5),
            // tee chain
            (vec![Const(7), LocalTee(0), LocalTee(1), LocalGet(0), Add, LocalGet(1), Mul], 0, 98),
            // get x; tee x (new); add → old + new
            (vec![LocalGet(0), LocalGet(1), Const(100), Add, LocalTee(0), Add], 0, 103),
            // value from a local survives a call that modifies its own copy
            (vec![LocalGet(0), LocalGet(0), Call(1), Add, LocalGet(0), Add], 1, 1 + 1002 + 1),
            // set to itself, select with folded operands
            (vec![LocalGet(0), LocalSet(0), LocalGet(1), LocalGet(0), LocalGet(0), Select], 0, 2),
            // result retargeted into a local that is also an operand
            (vec![LocalGet(0), LocalGet(1), Add, LocalSet(1), LocalGet(1), LocalGet(1), Mul], 0, 9),
            // retarget blocked by a pending read of the same local below
            (vec![LocalGet(1), LocalGet(0), Const(3), Mul, LocalSet(1), LocalGet(1), Sub], 0, -1),
            // tee then set of the same local with the stack entry still alive
            (vec![Const(4), LocalTee(0), Const(9), LocalSet(0), LocalGet(0), Add], 0, 13),
            // drop of folded operands and materialized ones
            (vec![LocalGet(0), Drop, Const(1), Drop, LocalGet(1)], 0, 2),
        ]
    };
    for (k, (code, extra_callee, expect)) in cases.into_iter().enumerate() {
        let mut mb = ModuleBuilder::new();
        let mut fb = FuncBuilder::new(2, 1);
        fb.ops(&code);
        let f = mb.add_func(fb);
        // callee 1: g(x) = { x = x + 1000; x + 1 } — modifies its own param slot.
        let g = mb.func(1, 1, |b| {
            b.local_get(0).const_(1000).add().local_set(0).local_get(0).const_(1).add();
        });
        assert!(extra_callee == 0 || g == extra_callee);
        mb.export("f", f);
        let m = mb.into_module();
        let (o, _) = differential(&m, 12, f, &[1, 2], u64::MAX, 10);
        assert_eq!(o.result, Ok(Some(expect)), "case {k}");
    }
}
