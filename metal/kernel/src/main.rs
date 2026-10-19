//! k10-metal: the k10s node runtime as its own kernel (docs/DESIGN.md §2.9).
//!
//! Workloads are isolated by construction (TIR's masked memory accesses, structured control flow and a closed
//! host API), so the kernel needs no processes, no system calls, no user/kernel boundary and no per-workload page
//! tables: one identity-mapped address space, one core, a timer, a NIC and a console.
//!
//! Boot: PVH entry (boot.rs) → `kmain`: console, GDT/IDT, memory map → frame allocator + heap, TSC clock, local
//! APIC timer, PCI → virtio-net → smoltcp, deployment bundle → services, then the main loop:
//!
//! ```text
//! loop { NIC rx → TCP → HTTP requests → runtime queue → run invocations (2 ms slices) → responses → NIC tx;
//!        idle: arm the one-shot timer for the next deadline, enable RX interrupts, hlt }
//! ```

#![no_std]
#![no_main]

extern crate alloc;

mod arch;
mod boot;
mod coro;
mod http;
mod mem;
mod net;
mod pci;
mod pvh;
mod runtime;
mod selftest;
mod serial;
mod sync;
mod time;
mod virtio_net;

use core::panic::PanicInfo;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use mem::frames;

static TOTAL_RAM: AtomicU64 = AtomicU64::new(0);
static RESERVE: AtomicU64 = AtomicU64::new(256 << 10);
static BOOT_NS: AtomicU64 = AtomicU64::new(0);
static VERBOSE: AtomicBool = AtomicBool::new(false);
/// TSC ticks spent halted (idle) and busy-polling after activity — the rest of uptime is real work.
pub static HALTED_TSC: AtomicU64 = AtomicU64::new(0);
pub static POLLING_TSC: AtomicU64 = AtomicU64::new(0);

/// `k10.verbose` on the command line.
pub fn verbose() -> bool {
    VERBOSE.load(Ordering::Relaxed)
}

/// Bundle baked into the image at build time (`K10_EMBED_BUNDLE`); empty if none.
static EMBEDDED_BUNDLE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/embedded_bundle.bin"));

pub fn total_ram() -> u64 {
    TOTAL_RAM.load(Ordering::Relaxed)
}

/// Memory kept free for the heap: instance slots, coroutine stacks and socket buffers are refused below it.
pub fn reserve_bytes() -> u64 {
    RESERVE.load(Ordering::Relaxed)
}

/// Milliseconds from `kmain` entry to the "ready" line.
pub fn boot_ms() -> f64 {
    BOOT_NS.load(Ordering::Relaxed) as f64 / 1e6
}

#[no_mangle]
extern "C" fn kmain() -> ! {
    let tsc0 = arch::rdtsc();
    serial::init();
    arch::gdt::init();
    arch::idt::init();

    // SAFETY: the pointer handed over by the PVH entry code.
    let bi = unsafe { pvh::parse(boot::start_info_paddr()) };
    let kernel = boot::kernel_image();
    frames::init(&bi, kernel);
    let fs = frames::stats();
    TOTAL_RAM.store(frames::total_ram(&bi), Ordering::Relaxed);
    RESERVE.store((fs.usable / 16).max(256 << 10), Ordering::Relaxed);

    let t_mem = arch::rdtsc();
    let src = time::init(tsc0);
    time::init_steal_time();
    let t_clock = arch::rdtsc();
    arch::apic::init();
    arch::apic::calibrate(time::tsc_hz());
    arch::interrupts_enable();
    let t_apic = arch::rdtsc();

    // The serial console costs ~15 us per character under QEMU (one host write() per byte), so boot is quiet
    // unless `k10.verbose` is given; the ready line carries the essentials.
    let verbose = bi.has_flag("k10.verbose") || bi.has_flag("k10.selftest");
    VERBOSE.store(verbose, Ordering::Relaxed);
    if verbose { kprintln!(
        "k10-metal: {} KiB RAM ({} KiB usable), kernel image {} KiB, TSC {} MHz ({:?}), timer: {}, cmdline \"{}\"",
        total_ram() >> 10,
        fs.usable >> 10,
        (kernel.1 - kernel.0) >> 10,
        time::tsc_hz() / 1_000_000,
        src,
        if arch::apic::uses_tsc_deadline() { "TSC-deadline" } else { "LAPIC one-shot" },
        bi.cmdline()
    ); }

    if bi.has_flag("k10.selftest") {
        let ok = selftest::run();
        arch::qemu_exit(if ok { arch::EXIT_SUCCESS } else { arch::EXIT_FAILURE });
        arch::halt_forever();
    }

    // Network.
    let t_banner = arch::rdtsc();
    let limits = net::Limits::for_ram(total_ram());
    let Some(nic) = virtio_net::VirtioNet::probe(limits.nic_bufs) else {
        kprintln!("k10-metal: no legacy virtio-net device found (use -device virtio-net-pci,disable-modern=on)");
        for d in &pci::enumerate() {
            kprintln!("  pci {:02x}:{:02x}.{} {:04x}:{:04x} class {:02x}.{:02x}", d.bus, d.dev, d.func, d.vendor, d.device, d.class, d.subclass);
        }
        arch::qemu_exit(arch::EXIT_FAILURE);
        arch::halt_forever();
    };
    let t_nic = arch::rdtsc();
    let mac = nic.mac;
    let msix = nic.msix;
    let seed = arch::rdrand().unwrap_or(arch::rdtsc());
    let mut net = net::Net::new(nic, limits, seed);
    let t_net = arch::rdtsc();
    if verbose { kprintln!(
        "net: virtio-net {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} {}, {}/24 gw {}, http :{}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5],
        if msix { "MSI-X" } else { "polled" },
        net::IP,
        net::GATEWAY,
        net::HTTP_PORT
    ); }

    // Deployment: -initrd bundle (PVH module 0), else the embedded one, else wait for POST /_k10s/apply.
    let t_apply0 = arch::rdtsc();
    let mut source = "none";
    if let Some(m) = bi.modules().first().copied() {
        // SAFETY: the module range is reserved in the frame allocator until released below.
        let bytes = unsafe { core::slice::from_raw_parts(m.paddr as *const u8, m.size as usize) };
        match runtime::apply(bytes) {
            Ok(_) => source = "initrd",
            Err(e) => kprintln!("boot bundle (initrd) rejected: {e}"),
        }
        frames::release_module(m.paddr, m.size);
    } else if !EMBEDDED_BUNDLE.is_empty() {
        match runtime::apply(EMBEDDED_BUNDLE) {
            Ok(_) => source = "embedded",
            Err(e) => kprintln!("embedded bundle rejected: {e}"),
        }
    }

    let t_apply = arch::rdtsc();
    let boot_ns = time::now_ns();
    BOOT_NS.store(boot_ns, Ordering::Relaxed);
    let us = |a: u64, b: u64| time::tsc_to_ns(b - a) / 1000;
    if verbose { kprintln!(
        "boot phases (us): cpu+memory {} clock {} apic {} console {} pci+virtio {} tcp/ip {} console {} apply {}",
        us(tsc0, t_mem),
        us(t_mem, t_clock),
        us(t_clock, t_apic),
        us(t_apic, t_banner),
        us(t_banner, t_nic),
        us(t_nic, t_net),
        us(t_net, t_apply0),
        us(t_apply0, t_apply)
    ); }
    let fs = frames::stats();
    let n = runtime::RT.borrow().service_names().count();
    kprintln!(
        "k10-metal ready {:.2}ms vm->kmain={:.1}ms svc={} src={} used={}K img={}K ram={}K",
        boot_ns as f64 / 1e6,
        time::vm_to_kmain_ns() as f64 / 1e6,
        n,
        source,
        (fs.usable - fs.free) >> 10,
        (kernel.1 - kernel.0) >> 10,
        total_ram() >> 10
    );
    if bi.has_flag("k10.exit_on_ready") {
        arch::qemu_exit(arch::EXIT_SUCCESS);
    }

    let spin_ns: u64 = bi.arg("k10.spin_us").and_then(|v| v.parse::<u64>().ok()).unwrap_or(50) * 1000;
    main_loop(&mut net, spin_ns)
}

fn main_loop(net: &mut net::Net, spin_ns: u64) -> ! {
    let mut last_busy = time::now_ns();
    let mut spin_from: Option<u64> = None;
    loop {
        net.poll_ingress();
        net.service(&mut runtime::submit);
        runtime::run_round();
        let replies = runtime::RT.borrow_mut().take_replies();
        for r in replies {
            net.respond(r.conn, r.status, r.content_type, &r.body);
        }
        net.service(&mut runtime::submit);
        net.poll_egress();
        *net::SNAPSHOT.borrow_mut() = net.snapshot();
        runtime::housekeeping();

        let now = time::now_ns();
        let busy = runtime::RT.borrow().has_work() || net.dev.rx.pending() || net.has_work();
        if let Some(t) = spin_from.take() {
            POLLING_TSC.fetch_add(arch::rdtsc() - t, Ordering::Relaxed);
        }
        if busy {
            last_busy = now;
            continue;
        }
        // Stay hot briefly after activity (back-to-back requests skip the interrupt round trip), then sleep.
        if now < last_busy + spin_ns {
            spin_from = Some(arch::rdtsc());
            arch::pause();
            continue;
        }
        let t = arch::rdtsc();
        idle(net);
        HALTED_TSC.fetch_add(arch::rdtsc() - t, Ordering::Relaxed);
        last_busy = time::now_ns();
    }
}

/// Sleeps until a frame arrives or the next timer deadline (TCP timers, request timeouts, housekeeping).
fn idle(net: &mut net::Net) {
    let mut wait_ns: u64 = 1_000_000_000;
    if let Some(d) = net.poll_delay_ns() {
        wait_ns = wait_ns.min(d);
    }
    if let Some(t) = runtime::RT.borrow().next_deadline_ns() {
        wait_ns = wait_ns.min(t.saturating_sub(time::now_ns()));
    }
    if wait_ns == 0 {
        return;
    }
    arch::apic::arm_timer(arch::rdtsc() + time::ns_to_tsc(wait_ns));
    arch::interrupts_disable();
    net.dev.rx_interrupts(true);
    if !net.dev.rx.pending() {
        arch::enable_and_halt(); // returns after the interrupt handler ran
    }
    arch::interrupts_enable();
    net.dev.rx_interrupts(false);
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    unsafe { serial::SERIAL.force_unlock() };
    kprintln!("\n!!! k10-metal PANIC: {}", info);
    if let Some((lo, hi)) = coro::current_stack() {
        kprintln!("    (in coroutine, stack {:#x}..{:#x})", lo, hi);
    }
    arch::qemu_exit(arch::EXIT_FAILURE);
    arch::halt_forever()
}
