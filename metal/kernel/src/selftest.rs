//! In-kernel self tests (`k10.selftest` on the command line): memory (above 4 GiB too), page permissions (W^X, NX)
//! and cache attributes, the ACPI tables, coroutines, the one-shot timer, wall-clock resync, fw_cfg files, the
//! admin gate (signed requests), disks (write, flush, read back, a block the host wrote), both engines
//! (interpreter and JIT) through the engine seam — host calls, host traps, stack overflow, time-slice preemption +
//! CPU-budget traps on a coroutine. The kernel then prints `k10-metal selftest: passed` (or `FAILED`) and powers
//! off through ACPI (see metal/test.sh).

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
use crate::{arch, fw_cfg, http, kprintln, runtime, time};

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

pub fn run(nic: bool) -> bool {
    let mut ok = true;
    kprintln!("selftest:");
    ok &= platform(nic);
    ok &= disks();

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

    // Wall clock: a resync from kvmclock steps it by (almost) nothing this early and leaves `now_ns` alone.
    let (m0, w0) = (time::now_ns(), time::unix_ms());
    let resynced = time::resync_wall();
    let (m1, w1) = (time::now_ns(), time::unix_ms());
    check!(
        ok,
        m1 >= m0 && w0 > 1_600_000_000_000 && (!resynced || w1.abs_diff(w0) < 100),
        "time: wall clock {} ms, resync from kvmclock: {}, step {} ms, monotonic clock unaffected",
        w0,
        resynced,
        w1 as i64 - w0 as i64
    );

    // fw_cfg files (metal/test.sh passes `-fw_cfg name=opt/k10s/selftest,string=k10s fw_cfg selftest`).
    check!(ok, fw_cfg::file("opt/k10s/no-such-file", 64) == Ok(None), "fw_cfg: missing file");
    match fw_cfg::file("opt/k10s/selftest", 64) {
        Ok(None) => kprintln!("  info fw_cfg: opt/k10s/selftest not given"),
        r => {
            check!(ok, r == Ok(Some(b"k10s fw_cfg selftest".to_vec())), "fw_cfg: file opt/k10s/selftest read");
            check!(ok, fw_cfg::file("opt/k10s/selftest", 4) == Err(20), "fw_cfg: file larger than the limit refused");
        }
    }

    ok &= admin_gate();

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
        crate::pci::read32(crate::pci::Bdf { seg: 0, bus: 0, dev: 0, func: 0 }, 0);
    }
    let cfg = time::tsc_to_ns(arch::rdtsc() - t0) / 1000;
    kprintln!("  info port I/O exit {} ns, PCI config read {} ns ({})", pio, cfg, if crate::pci::uses_ecam() { "ECAM" } else { "ports" });

    kprintln!("selftest: {}", if ok { "PASS" } else { "FAIL" });
    ok
}

static TEST_STATIC: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(1);

/// ACPI tables, CPUs, page attributes, memory above 4 GiB, the NIC.
fn platform(nic: bool) -> bool {
    use crate::{acpi, percpu};
    let mut ok = true;
    let Some(t) = acpi::get() else {
        check!(ok, false, "acpi: tables read");
        return ok;
    };
    let listed = t.cpus.len().max(1);
    check!(
        ok,
        !t.source.is_empty() && runtime::cpus() <= listed && percpu::online() == runtime::cpus(),
        "cpus: {} listed (from {}), {} online, {}, MAX_CPUS {}",
        listed,
        if t.source.is_empty() { "nowhere" } else { t.source },
        percpu::online(),
        if arch::apic::x2apic() { "x2APIC" } else { "xAPIC" },
        percpu::MAX_CPUS
    );
    let ids_ok = (0..runtime::cpus()).all(|i| {
        let id = percpu::cpu(i).apic_id.load(Ordering::Relaxed);
        t.cpus.is_empty() || t.cpus.iter().any(|c| c.apic_id == id)
    });
    check!(ok, ids_ok, "cpus: every online CPU's APIC id is one the firmware listed");
    if t.source == "acpi" {
        let poweroff = t.s5.is_some() && t.fadt.is_some_and(|f| if t.hw_reduced() { f.sleep_control.valid() } else { f.pm1a_cnt.valid() });
        check!(ok, poweroff || &t.oem == b"FIRECK", "acpi: S5 power off available (\\_S5 {:?}, {})", t.s5, if t.hw_reduced() { "sleep control register" } else { "PM1 control" });
    } else {
        kprintln!("  info acpi: no ACPI tables (CPUs from {}): no power off", if t.source.is_empty() { "nowhere" } else { t.source });
    }
    // Cache attributes: devices uncached, the kernel write-back.
    let text = platform as *const () as u64;
    let mut uc = alloc::vec![("kernel text", text, false)];
    for io in &t.ioapics {
        uc.push(("I/O APIC", io.addr, true));
    }
    if !arch::apic::x2apic() {
        uc.push(("local APIC", t.lapic, true));
    }
    if let Some(h) = t.hpet {
        uc.push(("HPET", h, true));
    }
    for e in &t.ecam {
        uc.push(("ECAM", e.base + ((e.bus_start as u64) << 20), true));
    }
    let bad: Vec<_> = uc.iter().filter(|(_, a, want)| arch::paging::uncached(*a) != Some(*want)).map(|x| x.0).collect();
    check!(ok, bad.is_empty(), "paging: devices mapped uncached, kernel write-back (bad: {:?})", bad);
    // Memory above 4 GiB, the first frame there and the highest one: mapped write-back, read+write, non-executable.
    let top = frames::top();
    let mut high = Vec::new();
    for min in [4 << 30, top.saturating_sub(4 << 20).max(4 << 30)] {
        if let Some(p) = frames::alloc_above(min, 1, frames::Tag::Kernel) {
            high.push(p);
        }
    }
    if high.is_empty() {
        kprintln!("  info paging: no memory above 4 GiB (RAM ends at {} MiB)", top >> 20);
    }
    for &p in &high {
        let w = p as *mut u64;
        unsafe { core::ptr::write_volatile(w, 0x6b31_3073_6869_6768) };
        let back = unsafe { core::ptr::read_volatile(w) };
        let q = arch::paging::query(p);
        check!(
            ok,
            back == 0x6b31_3073_6869_6768 && q == (true, true, !arch::paging::nx_enabled()) && arch::paging::uncached(p) == Some(false),
            "paging: frame {:#x} above 4 GiB read and written (rw-, write-back; RAM ends at {:#x})",
            p,
            top
        );
        frames::free(p, 1, frames::Tag::Kernel);
    }
    check!(ok, nic, "dev: a NIC was found");
    ok
}

/// Every disk: a block the host wrote (block 1, "k10s disk test" if metal/test.sh prepared the image), then
/// write + flush + read back of 8 blocks at block 2 (the host checks them after power off), several requests in
/// flight at once, and an out-of-range request refused.
fn disks() -> bool {
    use crate::dev::{block_sync, BlockError, BlockOp, BlockRequest, DmaRegion, BLOCK};
    let mut ok = true;
    let mut disks = BLOCK.lock();
    for d in disks.iter_mut() {
        let d = &mut **d;
        let bs = d.block_size() as usize;
        let name = d.name();
        let req = |op, lba, blocks, paddr, tag| BlockRequest { op, lba, blocks, paddr, tag };
        let Some(mut buf) = DmaRegion::alloc(16 * bs, frames::Tag::Kernel) else {
            check!(ok, false, "{name}: DMA buffer");
            continue;
        };
        let r = block_sync(d, req(BlockOp::Read, 1, 1, buf.paddr(), 1));
        if r.is_ok() && buf.as_slice().starts_with(b"k10s disk test") {
            check!(ok, true, "{name}: block 1 holds what the host wrote");
        } else {
            check!(ok, r.is_ok(), "{name}: read block 1 ({:?}; no host pattern)", r);
        }
        if r == Err(BlockError::Timeout) {
            // The device may still write there: never give these frames back.
            core::mem::forget(buf);
            continue;
        }
        if !d.read_only() && d.blocks() >= 16 {
            let pattern = |i: usize| (i as u8).wrapping_mul(31) ^ 0x5A;
            let s = buf.as_mut_slice();
            for (i, b) in s[..8 * bs].iter_mut().enumerate() {
                *b = pattern(i);
            }
            s[..20].copy_from_slice(b"written by k10-metal");
            let w = block_sync(d, req(BlockOp::Write, 2, 8, buf.paddr(), 2));
            let f = block_sync(d, req(BlockOp::Flush, 0, 0, 0, 3));
            let Some(back) = DmaRegion::alloc(8 * bs, frames::Tag::Kernel) else { continue };
            let r = block_sync(d, req(BlockOp::Read, 2, 8, back.paddr(), 4));
            let same = back.as_slice()[..8 * bs] == buf.as_slice()[..8 * bs];
            check!(ok, w.is_ok() && f.is_ok() && r.is_ok() && same, "{name}: 8 blocks written, flushed, read back ({:?} {:?} {:?})", w, f, r);
            // Four reads in flight at once, completed in any order.
            let mut tags = Vec::new();
            for k in 0..4u32 {
                if d.submit(0, req(BlockOp::Read, 2 + k as u64, 1, back.paddr() + k as u64 * bs as u64, 10 + k)).is_ok() {
                    tags.push(10 + k);
                }
            }
            let t0 = time::now_ns();
            while !tags.is_empty() && time::now_ns() - t0 < 5_000_000_000 {
                if let Some((tag, Ok(()))) = d.poll(0) {
                    tags.retain(|&t| t != tag);
                }
            }
            check!(ok, tags.is_empty(), "{name}: 4 requests in flight completed");
            if !tags.is_empty() || [w, f, r].contains(&Err(BlockError::Timeout)) {
                core::mem::forget(back);
                core::mem::forget(buf);
                continue;
            }
        }
        let end = d.blocks();
        let past = block_sync(d, req(BlockOp::Read, end, 1, buf.paddr(), 5));
        check!(ok, past == Err(BlockError::Range), "{name}: read past the end refused ({} blocks of {} bytes)", d.blocks(), bs);
        if past == Err(BlockError::Timeout) {
            core::mem::forget(buf);
        }
    }
    if disks.is_empty() {
        kprintln!("  info disks: none");
    }
    ok
}

/// The admin gate with a test key: requests signed for this node's audience pass once, everything else under
/// `/_k10s` but health and audience is refused.
fn admin_gate() -> bool {
    use k10_mesh::admin_auth::{check_body, AdminKey, AdminVerifier};
    let mut ok = true;
    let key = "k10s-selftest-admin-key-0123456789abcdef";
    runtime::admin_init(Some(AdminVerifier::new(AdminKey::parse(key).unwrap(), [0xa5; 16])));
    let (aud, other) = ("a5".repeat(16), "5a".repeat(16));
    let signer = AdminKey::parse(key).unwrap();
    let now = time::unix_ms();
    let gate = |m: &str, target: &[u8], auth: Option<&str>| runtime::admin_gate(m, target, auth.map(str::as_bytes));
    let st = |r: Result<Option<[u8; 32]>, (u16, alloc::string::String)>| r.err().map_or(200, |e| e.0);
    check!(ok, gate("GET", b"/_k10s/audience", None) == Ok(None), "admin: audience endpoint open");
    let elsewhere = signer.authorization(&other, "GET", "/_k10s/status", now, &[6; 16], b"");
    check!(ok, st(gate("GET", b"/_k10s/status", Some(&elsewhere))) == 401, "admin: signed for another audience -> 401");
    let get = signer.authorization(&aud, "GET", "/_k10s/status", now, &[7; 16], b"");
    let empty = k10_mesh::sha256::sha256(b"");
    check!(ok, gate("GET", b"/_k10s/status", Some(&get)) == Ok(Some(empty)), "admin: signed GET status accepted");
    check!(ok, st(gate("GET", b"/_k10s/status", Some(&get))) == 401, "admin: replayed request -> 401");
    check!(ok, st(gate("GET", b"/_k10s/status", None)) == 401, "admin: unsigned status -> 401");
    check!(ok, st(gate("GET", b"/_k10s/metrics", None)) == 401, "admin: unsigned metrics -> 401");
    check!(ok, gate("GET", b"/_k10s/health", None) == Ok(None) && gate("GET", b"/hello/", None) == Ok(None), "admin: health and services open");
    let post = signer.authorization(&aud, "POST", "/_k10s/status", now, &[8; 16], b"");
    check!(ok, st(gate("GET", b"/_k10s/status", Some(&post))) == 401, "admin: signature for another method -> 401");
    let stale = signer.authorization(&aud, "GET", "/_k10s/status", now - 120_000, &[9; 16], b"");
    check!(ok, st(gate("GET", b"/_k10s/status", Some(&stale))) == 401, "admin: stale request -> 401");
    let apply = signer.authorization(&aud, "POST", "/_k10s/apply", now, &[10; 16], b"bundle");
    let body = gate("POST", b"/_k10s/apply", Some(&apply)).ok().flatten();
    check!(
        ok,
        body.is_some_and(|h| check_body(&h, b"bundle").is_ok() && check_body(&h, b"other").is_err()),
        "admin: body checked against its signed hash"
    );
    runtime::admin_init(None);
    let get = signer.authorization(&aud, "GET", "/_k10s/status", now, &[11; 16], b"");
    check!(
        ok,
        st(gate("GET", b"/_k10s/status", Some(&get))) == 403 && st(gate("GET", b"/_k10s/audience", None)) == 403,
        "admin: no admin key -> 403 (audience too)"
    );
    ok
}

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
