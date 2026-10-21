//! In-kernel self tests (`k10.selftest` on the command line): memory, page permissions (W^X, NX), coroutines, the
//! one-shot timer, both engines (interpreter and JIT) through the engine seam — host calls, host traps, stack
//! overflow, time-slice preemption + CPU-budget traps on a coroutine. Exits QEMU through `isa-debug-exit` with
//! success/failure (see metal/test.sh).

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;

use k10_ir::{ModuleBuilder, Trap};


use crate::coro::{Coroutine, Yield};
use crate::mem::{frames, slots};
use crate::runtime::engine::{self, HostCalls, Scratch};
use crate::runtime::host::HostFn;
use crate::{arch, http, kprintln, time};

macro_rules! check {
    ($ok:ident, $cond:expr, $($msg:tt)*) => {
        if $cond {
            kprintln!("  ok   {}", format_args!($($msg)*));
        } else {
            kprintln!("  FAIL {}", format_args!($($msg)*));
            $ok = false;
        }
    };
}

pub fn run() -> bool {
    let mut ok = true;
    kprintln!("selftest:");

    // Frames: alignment, contiguity, accounting.
    let before = frames::free_bytes();
    let a = frames::alloc(16, 16, frames::Tag::Kernel);
    let b = frames::alloc(1, 1, frames::Tag::Kernel);
    check!(ok, a.is_some_and(|p| p % (16 * 4096) == 0), "frames: 64 KiB-aligned run of 16 frames at {:#x}", a.unwrap_or(0));
    frames::free(a.unwrap(), 16, frames::Tag::Kernel);
    frames::free(b.unwrap(), 1, frames::Tag::Kernel);
    check!(ok, frames::free_bytes() == before, "frames: free count restored");

    // Heap: many sizes, large allocations, realloc.
    let mut v: Vec<Box<[u8]>> = Vec::new();
    for i in 0..2000usize {
        let n = (i * 37) % 5000 + 1;
        v.push(vec![(i & 0xff) as u8; n].into_boxed_slice());
    }
    let good = v.iter().enumerate().all(|(i, b)| b.iter().all(|&x| x == (i & 0xff) as u8));
    drop(v);
    let mut m = BTreeMap::new();
    for i in 0..10_000u32 {
        m.insert(i, i * 2);
    }
    let mut grow = Vec::new();
    for i in 0..300_000u32 {
        grow.push(i);
    }
    check!(ok, good && m[&777] == 1554 && grow[299_999] == 299_999, "heap: 2000 mixed blocks, BTreeMap, 1.2 MB Vec");
    drop(m);
    drop(grow);

    // Slots.
    let s = slots::alloc(16);
    check!(ok, s.as_ref().is_some_and(|s| s.footprint() == 65536 + 4096), "slots: 64 KiB + tail");
    let base = s.as_ref().map(|s| s.base() as usize);
    drop(s);
    let s2 = slots::alloc(16);
    check!(ok, s2.as_ref().map(|s| s.base() as usize) == base, "slots: freed slot reused from cache");
    let conf = crate::runtime::host_conformance();
    check!(ok, conf.is_ok(), "host ABI conformance (k10_host::conformance): {:?}", conf);
    drop(s2);

    // Coroutines: ping-pong.
    extern "C" fn counter(arg: usize) -> ! {
        let n = unsafe { &mut *(arg as *mut u64) };
        for _ in 0..1000 {
            *n += 1;
            crate::coro::yield_now(Yield::Preempted);
        }
        crate::coro::finish()
    }
    let mut n: u64 = 0;
    let mut co = Coroutine::new(counter, &mut n as *mut u64 as usize).unwrap();
    let t0 = arch::rdtsc();
    let mut switches = 0;
    while co.resume() != Yield::Finished {
        switches += 1;
    }
    let ns = time::tsc_to_ns(arch::rdtsc() - t0);
    check!(ok, n == 1000 && switches == 1000, "coroutines: 1000 yields, {} ns per round trip", ns / 1001);

    // One-shot timer.
    let irqs = time::TIMER_IRQS.load(Ordering::Relaxed);
    let t0 = time::now_ns();
    arch::apic::arm_timer(arch::rdtsc() + time::ns_to_tsc(1_000_000));
    while time::TIMER_IRQS.load(Ordering::Relaxed) == irqs {
        arch::enable_and_halt();
    }
    let dt = time::now_ns() - t0;
    check!(ok, (900_000..5_000_000).contains(&dt), "timer: 1 ms one-shot fired after {} us", dt / 1000);

    // HTTP parsing.
    let req = b"POST /echo/x?y=1 HTTP/1.1\r\nHost: a\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello";
    let parsed = match http::parse_head(req) {
        http::Parse::Head(h) => h.method == http::Method::Post && h.target == b"/echo/x?y=1" && h.content_length == 5 && !h.keep_alive,
        _ => false,
    };
    check!(ok, parsed, "http: request head");

    // Page permissions: kernel text read+execute, data read+write+NX, null page unmapped.
    let nx = arch::paging::nx_enabled();
    let text = arch::paging::query(run as *const () as u64);
    let data = arch::paging::query(&raw const TEST_STATIC as u64);
    let heap_page = arch::paging::query(Box::into_raw(Box::new(0u64)) as u64);
    check!(
        ok,
        text == (true, false, true) && data == (true, true, !nx) && heap_page == (true, true, !nx) && !arch::paging::query(0).0,
        "paging: NX {}, kernel text r-x, data rw-, heap rw-, page 0 unmapped",
        if nx { "on" } else { "off (CPU lacks it)" }
    );

    let jit_was = engine::jit_enabled();
    for jit in [false, true] {
        engine::set_jit(jit);
        ok &= engines(jit);
    }
    engine::set_jit(jit_was);

    // Cost of a port-I/O exit (QEMU userspace device) and of an MMIO-free in-kernel exit (rdmsr of the TSC
    // deadline is handled in KVM): informs how much the console and PCI probing cost at boot.
    let t0 = arch::rdtsc();
    for _ in 0..1000 {
        arch::inb(0x3F8 + 5);
    }
    let pio = time::tsc_to_ns(arch::rdtsc() - t0) / 1000;
    let t0 = arch::rdtsc();
    for _ in 0..1000 {
        crate::pci::read32(0, 0, 0, 0);
    }
    let cfg = time::tsc_to_ns(arch::rdtsc() - t0) / 1000;
    kprintln!("  info port I/O exit {} ns, PCI config read {} ns", pio, cfg);

    kprintln!("selftest: {}", if ok { "PASS" } else { "FAIL" });
    ok
}

static TEST_STATIC: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(1);

/// The engine-level tests, run once per engine.
fn engines(jit: bool) -> bool {
    let mut ok = true;
    let name = if jit { "jit" } else { "interp" };

    // Sum 1..=1_000_000.
    let mut mb = ModuleBuilder::new();
    let f = mb.func(0, 1, |f| {
        let (i, acc) = (f.new_local(), f.new_local());
        f.const_(1_000_000).local_set(i);
        f.block().loop_();
        f.local_get(i).eqz().br_if(1);
        f.local_get(acc).local_get(i).add().local_set(acc);
        f.local_get(i).const_(1).sub().local_set(i);
        f.br(0).end().end();
        f.local_get(acc);
    });
    mb.export("handle", f);
    let prep = engine::prepare(mb.build(), 0).unwrap();
    check!(ok, prep.engine_name() == name, "{name}: module prepared with the {} engine", prep.engine_name());
    if let engine::Code::Jit(j) = &prep.code {
        let page = j.code_addr();
        check!(
            ok,
            arch::paging::query(page) == (true, false, true),
            "{name}: code page {:#x} sealed r-x ({} bytes of machine code, compiled in {} us)",
            page,
            j.code_bytes(),
            j.compile_ns / 1000
        );
    }
    let mut slot = slots::alloc(12).unwrap();
    let mut host = TestHost { deadline: u64::MAX, used: 0, calls: 0 };
    let t0 = arch::rdtsc();
    let r = prep.invoke(&mut Scratch::default(), slot.tir_mem(), 12, &mut host, prep.handle);
    let ns = time::tsc_to_ns(arch::rdtsc() - t0);
    check!(ok, r == Ok(Some(500_000_500_000)), "{name}: 1e6-iteration loop in {} us ({:.2} ns/iter)", ns / 1000, ns as f64 / 1e6);
    drop(prep);

    // Host calls (and a host trap) through the engine's host interface: now_ns() + 1, then random() traps.
    let mut mb = ModuleBuilder::new();
    let now = mb.import("now_ns", 0, 1);
    let rnd = mb.import("random", 0, 1);
    let out = mb.import("output_write", 2, 1);
    let f = mb.func(0, 1, |f| {
        f.const_(64).const_(5).call(out).drop();
        f.call(now).const_(1).add();
    });
    let g = mb.func(0, 1, |f| {
        f.call(rnd);
    });
    mb.export("handle", f).export("trapper", g);
    let prep = engine::prepare(mb.build(), 0).unwrap();
    let r = prep.invoke(&mut Scratch::default(), slot.tir_mem(), 12, &mut host, prep.handle);
    let trapper = prep.export("trapper").unwrap();
    let t = prep.invoke(&mut Scratch::default(), slot.tir_mem(), 12, &mut host, trapper);
    check!(
        ok,
        r == Ok(Some(43)) && host.calls == 3 && t == Err(Trap::Host(7)),
        "{name}: host calls -> {:?} ({} calls), host trap -> {:?}",
        r,
        host.calls,
        t
    );
    drop(prep);

    // Unbounded recursion on a coroutine stack: StackOverflow after ≥ 1000 frames (docs/TIR.md §6), stack canary
    // intact (checked by resume). Each level stores its depth at address 0.
    let mut mb = ModuleBuilder::new();
    let me = mb.next_index();
    let f = mb.func(1, 1, |f| {
        f.const_(0).local_get(0).store64(0);
        f.local_get(0).const_(1).add().call(me);
    });
    let h = mb.func(0, 1, |f| {
        f.const_(0).call(f_idx(me));
    });
    mb.export("handle", h);
    let deep = engine::prepare(mb.build(), 0).unwrap();
    let _ = f;
    struct Deep<'a> {
        prep: &'a engine::Prepared,
        slot: &'a mut slots::Slot,
        result: Option<Result<Option<i64>, Trap>>,
    }
    extern "C" fn deep_job(arg: usize) -> ! {
        let j = unsafe { &mut *(arg as *mut Deep) };
        let mut host = TestHost { deadline: u64::MAX, used: 0, calls: 0 };
        j.result = Some(j.prep.invoke(&mut Scratch::default(), j.slot.tir_mem(), 12, &mut host, j.prep.handle));
        crate::coro::finish()
    }
    let mut j = Deep { prep: &deep, slot: &mut slot, result: None };
    let mut co = Coroutine::new(deep_job, &mut j as *mut Deep as usize).unwrap();
    while co.resume() != Yield::Finished {}
    let depth = i64::from_le_bytes(j.slot.tir_mem()[..8].try_into().unwrap());
    check!(
        ok,
        j.result == Some(Err(Trap::StackOverflow)) && depth >= 1000,
        "{name}: unbounded recursion -> {:?} at depth {} ({} KiB coroutine stack)",
        j.result,
        depth,
        crate::coro::stack_size() >> 10
    );
    drop(deep);

    // Preemption + CPU budget: an infinite loop in a coroutine, 2 ms slices, 20 ms budget.
    let mut mb = ModuleBuilder::new();
    let f = mb.func(0, 0, |f| {
        f.loop_().br(0).end();
    });
    mb.export("handle", f);
    let spin = engine::prepare(mb.build(), 0).unwrap();
    struct Job<'a> {
        prep: &'a engine::Prepared,
        slot: &'a mut slots::Slot,
        result: Option<Result<Option<i64>, Trap>>,
    }
    extern "C" fn job(arg: usize) -> ! {
        let j = unsafe { &mut *(arg as *mut Job) };
        let mut host = TestHost { deadline: time::ns_to_tsc(20_000_000), used: 0, calls: 0 };
        j.result = Some(j.prep.invoke(&mut Scratch::default(), j.slot.tir_mem(), 12, &mut host, j.prep.handle));
        crate::coro::finish()
    }
    let mut j = Job { prep: &spin, slot: &mut slot, result: None };
    let mut co = Coroutine::new(job, &mut j as *mut Job as usize).unwrap();
    let t0 = time::now_ns();
    let mut slices = 0;
    loop {
        let at = arch::rdtsc() + time::ns_to_tsc(crate::runtime::TIME_SLICE_NS);
        time::set_preempt_at(at);
        arch::apic::arm_timer(at);
        let y = co.resume();
        time::set_preempt_at(u64::MAX);
        if y == Yield::Finished {
            break;
        }
        slices += 1;
    }
    let dt = time::now_ns() - t0;
    check!(
        ok,
        j.result == Some(Err(Trap::Interrupted)) && (8..=12).contains(&slices),
        "{name}: preemption: infinite loop got {} slices of 2 ms, trapped {:?} after {} ms",
        slices,
        j.result,
        dt / 1_000_000
    );
    drop(slot);
    drop(spin);
    ok
}

/// Identity (keeps the recursive test readable: `h` calls the recursive function by index).
fn f_idx(i: u32) -> u32 {
    i
}

/// Host for the tests: `now_ns` → 42, `output_write` → its length, `random` traps `Host(7)`; CPU budget enforced
/// at safepoints like the runtime does.
struct TestHost {
    deadline: u64,
    used: u64,
    calls: u32,
}

impl HostCalls for TestHost {
    fn call(&mut self, f: HostFn, args: &[i64], mem: &mut [u8]) -> Result<i64, Trap> {
        self.calls += 1;
        match f {
            HostFn::NowNs if args.is_empty() => Ok(42),
            HostFn::OutputWrite if args.len() == 2 && mem.len() == 4096 + 8 => Ok(args[1]),
            HostFn::Random => Err(Trap::Host(7)),
            _ => Err(Trap::Unreachable),
        }
    }
    fn safepoint(&mut self) -> Result<(), Trap> {
        if !time::take_preempt() {
            return Ok(());
        }
        self.used += time::ns_to_tsc(crate::runtime::TIME_SLICE_NS);
        if self.used >= self.deadline {
            return Err(Trap::Interrupted);
        }
        crate::coro::yield_now(Yield::Preempted);
        Ok(())
    }
}
