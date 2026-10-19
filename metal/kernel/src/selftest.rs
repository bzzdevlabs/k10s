//! In-kernel self tests (`k10.selftest` on the command line): memory, coroutines, the one-shot timer, the
//! interpreter through the engine seam, and time-slice preemption + CPU-budget traps. Exits QEMU through
//! `isa-debug-exit` with success/failure (see metal/test.sh).

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

    // Interpreter through the engine seam: sum 1..=1_000_000.
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
    let m = mb.build();
    let prep = engine::prepare(&m, 0).unwrap();
    let mut slot = slots::alloc(12).unwrap();
    let mut host = TestHost { deadline: u64::MAX, used: 0 };
    let t0 = arch::rdtsc();
    let r = prep.invoke(&mut Scratch::default(), slot.tir_mem(), 12, &mut host, prep.handle);
    let ns = time::tsc_to_ns(arch::rdtsc() - t0);
    check!(ok, r == Ok(Some(500_000_500_000)), "interp: 1e6-iteration loop in {} us ({} ns/iter)", ns / 1000, ns / 1_000_000);

    // Preemption + CPU budget: an infinite loop in a coroutine, 2 ms slices, 20 ms budget.
    let mut mb = ModuleBuilder::new();
    let f = mb.func(0, 0, |f| {
        f.loop_().br(0).end();
    });
    mb.export("handle", f);
    let spin = engine::prepare(&mb.build(), 0).unwrap();
    struct Job<'a> {
        prep: &'a engine::Prepared,
        slot: &'a mut slots::Slot,
        result: Option<Result<Option<i64>, Trap>>,
    }
    extern "C" fn job(arg: usize) -> ! {
        let j = unsafe { &mut *(arg as *mut Job) };
        let mut host = TestHost { deadline: time::ns_to_tsc(20_000_000), used: 0 };
        j.result = Some(j.prep.invoke(&mut Scratch::default(), j.slot.tir_mem(), 12, &mut host, j.prep.handle));
        crate::coro::finish()
    }
    let mut j = Job { prep: &spin, slot: &mut slot, result: None };
    let mut co = Coroutine::new(job, &mut j as *mut Job as usize).unwrap();
    let t0 = time::now_ns();
    let mut slices = 0;
    loop {
        time::PREEMPT.store(false, Ordering::Relaxed);
        let at = arch::rdtsc() + time::ns_to_tsc(crate::runtime::TIME_SLICE_NS);
        time::PREEMPT_AT.store(at, Ordering::Relaxed);
        arch::apic::arm_timer(at);
        let y = co.resume();
        time::PREEMPT_AT.store(u64::MAX, Ordering::Relaxed);
        if y == Yield::Finished {
            break;
        }
        slices += 1;
    }
    let dt = time::now_ns() - t0;
    check!(
        ok,
        j.result == Some(Err(Trap::Interrupted)) && (8..=12).contains(&slices),
        "preemption: infinite loop got {} slices of 2 ms, trapped {:?} after {} ms",
        slices,
        j.result,
        dt / 1_000_000
    );
    drop(slot);

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

/// Host for the tests: no imports, CPU budget enforced at safepoints like the runtime does.
struct TestHost {
    deadline: u64,
    used: u64,
}

impl HostCalls for TestHost {
    fn call(&mut self, _: HostFn, _: &[i64], _: &mut [u8]) -> Result<i64, Trap> {
        Err(Trap::Unreachable)
    }
    fn safepoint(&mut self) -> Result<(), Trap> {
        if !time::PREEMPT.swap(false, Ordering::Relaxed) {
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
