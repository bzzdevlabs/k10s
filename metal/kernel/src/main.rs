//! k10-metal: the k10s node runtime as its own kernel (docs/DESIGN.md §2.9).
//!
//! Workloads are isolated by construction (TIR's masked memory accesses, structured control flow and a closed
//! host API), so the kernel needs no processes, no system calls, no user/kernel boundary and no per-workload page
//! tables: one identity-mapped address space (W^X, NX), one or more CPUs, a timer, a NIC and a console.
//!
//! Boot: PVH entry (boot.rs) → `kmain`: per-CPU state, console, GDT/IDT, memory map → frame allocator + heap, the
//! identity map (all RAM; devices uncached), ACPI tables (acpi.rs), console from SPCR, TSC clock, local APIC
//! (x2APIC or xAPIC) and I/O APICs, application processors from the MADT (smp.rs), PCI (ECAM) and virtio-mmio →
//! NIC and disks (dev/, virtio/) → smoltcp with DHCP or a static address, cluster layer (optional), deployment
//! bundle → services (JIT-compiled), then CPU 0's main loop (the other CPUs run `runtime::worker_main`):
//!
//! ```text
//! loop { NIC rx → TCP → HTTP requests → dispatch to a CPU → gossip / peer links → run this CPU's invocations
//!        (2 ms slices) → responses (from every CPU) → NIC tx;
//!        idle: arm the one-shot timer for the next deadline, enable RX interrupts, hlt }
//! ```

#![no_std]
#![no_main]

extern crate alloc;

mod acpi;
mod arch;
mod boot;
mod bootcfg;
mod cluster;
mod console;
mod coro;
mod dev;
mod efi;
mod fw_cfg;
mod http;
mod mem;
mod net;
mod pci;
mod percpu;
mod power;
mod pvh;
mod runtime;
mod selftest;
mod osupdate;
mod storage;
mod serial;
mod smp;
mod sntp;
mod sync;
mod time;
mod virtio;
mod watchdog;

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

/// Clock reading (`time::now_ns`) of the "ready" line; 0 before.
pub fn boot_ns() -> u64 {
    BOOT_NS.load(Ordering::Relaxed)
}

/// Stops the boot on a configuration error (then reboots after `k10.panic` seconds, see power.rs).
pub fn fatal(msg: core::fmt::Arguments) -> ! {
    kprintln!("k10-metal: {msg}");
    power::fatal_stop()
}

/// Boot argument `name` parsed as `T` (`None` if absent); an invalid value stops the boot.
fn knob<T: core::str::FromStr>(bi: &pvh::BootInfo, name: &str, want: &str) -> Option<T> {
    bi.arg(name).map(|v| v.parse().unwrap_or_else(|_| fatal(format_args!("{name}={v}: want {want}"))))
}

#[no_mangle]
extern "C" fn kmain() -> ! {
    let tsc0 = arch::rdtsc();
    // Per-CPU state first: locks (and so the console) need the CPU index.
    percpu::init_cpu(0);
    serial::init();
    arch::gdt::init();
    arch::idt::init();

    // SAFETY: the pointer handed over by the PVH entry code.
    let bi = unsafe { pvh::parse(boot::start_info_paddr()) };
    power::set_panic_reboot(knob(&bi, "k10.panic", "seconds (0 = halt)"));
    // The serial console costs ~15 us per character under QEMU (one host write() per byte), so boot is quiet
    // unless `k10.verbose` is given; the ready line carries the essentials.
    let verbose = bi.has_flag("k10.verbose") || bi.has_flag("k10.selftest");
    VERBOSE.store(verbose, Ordering::Relaxed);
    acpi::early();
    let kernel = boot::kernel_image();
    let t_parse = arch::rdtsc();
    let memmap = mem::memmap::MemoryMap::from_entries(bi.memmap().iter().map(|r| (r.addr, r.size, r.kind)));
    frames::init(&memmap, bi.modules(), kernel);
    arch::paging::init(&memmap);
    efi::init(&bi);
    TOTAL_RAM.store(frames::total_ram(&memmap), Ordering::Relaxed);
    let t_paging = arch::rdtsc();
    let have_acpi = acpi::init(bi.rsdp);
    frames::add_low_memory(&memmap, acpi::ebda_range());
    let fs = frames::stats();
    RESERVE.store((fs.usable / 16).max(256 << 10), Ordering::Relaxed);
    let serial_arg = console::configure(bi.arg("k10.console"))
        .unwrap_or_else(|e| fatal(format_args!("k10.console={}: {e}", bi.arg("k10.console").unwrap_or(""))));
    match serial::choose(serial_arg, acpi::get().and_then(|t| t.spcr)) {
        Ok(Some((port, baud, fifo))) => serial::set_port(port, baud, fifo),
        Ok(None) => {}
        Err(e) => fatal(format_args!("k10.console={}: {e}", bi.arg("k10.console").unwrap_or(""))),
    }
    if let Some((_, m)) = bi.module(k10_bootproto::MOD_FRAMEBUFFER) {
        // SAFETY: the framebuffer module is never released.
        if let Some((w, h, cols, rows, cw, ch)) = console::fb::init(unsafe { m.bytes() }) {
            kprintln!("console: framebuffer {w}x{h}, {cols}x{rows} characters of {cw}x{ch} pixels");
        }
    }
    if let Some(dst) = console::udp::target() {
        kprintln!("console: UDP to {dst} from port {} once the network is up", console::udp::LOCAL_PORT);
    }
    if memmap.dropped > 0 {
        kprintln!("k10-metal: memory map: {} region(s) beyond {} ignored", memmap.dropped, mem::memmap::MAX_REGIONS);
    }

    let t_mem = arch::rdtsc();
    let src = time::init(tsc0);
    time::init_steal_time();
    let t_clock = arch::rdtsc();
    let tables = acpi::get();
    let wide_ids = tables.is_some_and(|t| t.cpus.iter().any(|c| c.apic_id > 254));
    arch::apic::init(bi.arg("k10.apic") == Some("xapic"), wide_ids);
    arch::ioapic::init(tables.map_or(&[][..], |t| &t.ioapics));
    arch::apic::calibrate(time::tsc_hz());
    percpu::this().apic_id.store(arch::apic::id(), Ordering::Relaxed);
    percpu::this().online.store(true, Ordering::Release);
    arch::interrupts_enable();
    let t_apic = arch::rdtsc();
    if verbose {
        if have_acpi {
            acpi::describe();
        } else {
            kprintln!("acpi: no RSDP and no MP table: one CPU, no ACPI power off");
        }
    }
    // Execution engine: the JIT unless `k10.engine=interp`. JIT frames live on the coroutine stacks.
    let jit = match bi.arg("k10.engine") {
        None | Some("jit") => true,
        Some("interp") => false,
        Some(v) => fatal(format_args!("k10.engine={v}: want jit|interp")),
    };
    runtime::engine::set_jit(jit);
    // Side-channel knobs (SECURITY.md): JIT indirect-branch form, guest clock resolution (its key needs RDRAND).
    let indirect = match bi.arg("k10.jit_indirect") {
        None => k10_jit::IndirectBranch::Jump,
        Some(v) => k10_jit::IndirectBranch::parse(v)
            .unwrap_or_else(|| fatal(format_args!("k10.jit_indirect={v}: want jump|lfence|retpoline"))),
    };
    runtime::jit::configure(indirect);
    let clock_res_ns = knob::<u64>(&bi, "k10.timer_res_us", "microseconds")
        .map_or(k10_host::clock::DEFAULT_RES_NS, |us| us.saturating_mul(1000));
    let mut clock_key = [0u8; 16];
    if clock_res_ns != 0 {
        for half in clock_key.chunks_mut(8) {
            let r = arch::rdrand().unwrap_or_else(|| {
                fatal(format_args!("the guest clock key needs RDRAND (use -cpu host), or k10.timer_res_us=0"))
            });
            half.copy_from_slice(&r.to_le_bytes());
        }
    }
    time::set_guest_clock(k10_host::clock::GuestClock::new(clock_res_ns, clock_key));
    if verbose {
        kprintln!("k10-metal: jit indirect branches: {}, guest clock resolution: {} ns", indirect.name(), clock_res_ns);
    }
    coro::set_stack_size(if jit { coro::STACK_SIZE_JIT } else { coro::STACK_SIZE_INTERP });
    // Application processors (`-smp N`; `k10.smp=N` limits how many are used).
    let ncpu = smp::start_aps(knob(&bi, "k10.smp", "a CPU count").unwrap_or(percpu::MAX_CPUS));
    runtime::set_cpus(ncpu);
    if verbose { kprintln!(
        "k10-metal {}: {} KiB RAM ({} KiB usable), kernel image {} KiB, TSC {} MHz ({:?}), {}, timer: {}, NX {}, engine {}, cmdline \"{}\"",
        env!("K10S_VERSION_FULL"),
        total_ram() >> 10,
        fs.usable >> 10,
        (kernel.1 - kernel.0) >> 10,
        time::tsc_hz() / 1_000_000,
        src,
        if arch::apic::x2apic() { "x2APIC" } else { "xAPIC" },
        if arch::apic::uses_tsc_deadline() { "TSC-deadline" } else { "LAPIC one-shot" },
        if arch::paging::nx_enabled() { "on" } else { "off" },
        if jit { "jit" } else { "interp" },
        bi.cmdline()
    ); }

    // PCI (ECAM windows from the MCFG) and the devices: the first NIC, every disk.
    pci::init(tables.map_or(&[][..], |t| &t.ecam), bi.arg("k10.pci") == Some("full"));
    let t_banner = arch::rdtsc();
    let limits = net::Limits::for_ram(total_ram());
    let nic = dev::probe(bi.cmdline(), limits.nic_bufs);
    let t_nic = arch::rdtsc();
    if verbose {
        for d in dev::BLOCK.lock().iter() {
            kprintln!(
                "disk: {}: {} blocks of {} bytes{}, {} queue(s), up to {} blocks per request",
                d.name(),
                d.blocks(),
                d.block_size(),
                if d.read_only() { ", read-only" } else { "" },
                d.queues(),
                d.max_blocks()
            );
        }
    }

    watchdog::init(bi.arg("k10.watchdog"), tables.and_then(|t| t.wdat.as_deref()))
        .unwrap_or_else(|e| fatal(format_args!("k10.watchdog={}: {e}", bi.arg("k10.watchdog").unwrap_or(""))));
    power::init_button();
    // Test modes: `k10.selftest` runs the self tests; `=watchdog` hangs with the watchdog armed; `=panic` panics
    // once the UDP console has sent a datagram (its panic path).
    let test_mode = bi.arg("k10.selftest");
    if test_mode == Some("watchdog") {
        watchdog::selftest_hang();
    }
    if bi.has_flag("k10.selftest") && test_mode != Some("panic") {
        let ok = selftest::run(nic.is_some());
        kprintln!("k10-metal selftest: {}", if ok { "passed" } else { "FAILED" });
        power::off();
    }
    // K10S-DATA: the KV namespaces come back before any service runs (the partition k10boot read, if several).
    let booted = bi.module(k10_bootproto::MOD_IMAGE).and_then(|(_, m)| k10_bootproto::BootedImage::decode(unsafe { m.bytes() }));
    storage::init(bi.arg("k10.data"), bi.arg("k10.grow") != Some("0"), booted.map(|b| b.data_partition).filter(|g| *g != [0; 16]));
    osupdate::init(&bi);

    if bi.from_loader() {
        let mut names = alloc::string::String::new();
        for m in bi.modules() {
            names.push_str(if names.is_empty() { "" } else { ", " });
            names.push_str(m.name());
        }
        kprintln!("k10-metal: boot loader modules: {names}; ACPI RSDP at {:#x}", bi.rsdp);
    }

    // Admin API: requests signed with the admin key (k10s.conf admin-token-file, or fw_cfg `opt/k10s/admin_key`
    // under QEMU's -kernel) for this boot's random audience; without a key only /_k10s/health.
    use k10_mesh::admin_auth::{AdminKey, AdminVerifier, AUDIENCE_LEN};
    let admin_key =
        bootcfg::secret(&bi, bootcfg::Secret::Admin, AdminKey::parse).unwrap_or_else(|e| fatal(format_args!("{e}")));
    runtime::admin_init(admin_key.map(|key| {
        let mut audience = [0u8; AUDIENCE_LEN];
        if !arch::rdrand_bytes(&mut audience) {
            fatal(format_args!("an admin key needs RDRAND for the admin audience (use -cpu host)"));
        }
        AdminVerifier::new(key, audience)
    }));

    // Network.
    let ip = net::IpConfig::parse(bi.arg("k10.ip")).unwrap_or_else(|e| fatal(format_args!("k10.ip={}: {e}", bi.arg("k10.ip").unwrap_or(""))));
    let Some(nic) = nic else {
        kprintln!("k10-metal: no supported NIC (virtio-net on PCI, or virtio_mmio.device= on the command line)");
        for d in pci::devices() {
            kprintln!("  pci {:04x}:{:02x}:{:02x}.{} {:04x}:{:04x} class {:02x}.{:02x}", d.seg, d.bus, d.dev, d.func, d.vendor, d.device, d.class, d.subclass);
        }
        fatal(format_args!("no network"));
    };
    let mac = nic.mac();
    let (nic_name, irq, pairs) = (nic.name(), nic.has_interrupts(), nic.queue_pairs());
    let seed = arch::rdrand().unwrap_or(arch::rdtsc());
    let mut net = net::Net::new(nic, limits, seed, ip);
    console::udp::set_nic(&mut net.dev);
    if let Some(v) = bi.arg("k10.dns") {
        let servers = v.split(',').map(|a| a.trim().parse()).collect::<Result<_, _>>();
        net.set_dns(servers.unwrap_or_else(|_| fatal(format_args!("k10.dns={v}: want A.B.C.D,…"))));
    }
    sntp::init(bi.arg("k10.ntp"));
    // Instance memory: `k10.mem`, else what is free now less four heap reserves (code, KV and buffers grow there).
    let mem = bi.arg("k10.mem").and_then(|v| v.parse().ok());
    runtime::set_mem_limit(mem.unwrap_or_else(|| frames::free_bytes().saturating_sub(4 * reserve_bytes())));
    // DHCP: the DISCOVER goes out now; the lease arrives while the bundle is applied. A cluster member waits for it
    // before it starts (it sends from, and may advertise, that address); a single node before the ready line.
    let cluster_cfg = cluster::config(&bi);
    if ip == net::IpConfig::Dhcp {
        net.poll_ingress();
        net.poll_egress();
        if cluster_cfg.is_some() {
            wait_for_address(&mut net, 30_000_000_000);
        }
    }
    if test_mode == Some("panic") {
        selftest_panic(&mut net);
    }
    let t_net = arch::rdtsc();
    if verbose { kprintln!(
        "net: {} {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}, {} queue pair(s), {}, {} ({}), http :{}",
        nic_name,
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5],
        pairs,
        if irq { "interrupts" } else { "polled" },
        net.ipv4().map_or(alloc::string::String::from("no address yet"), |a| alloc::format!("{a}")),
        if ip == net::IpConfig::Dhcp { "DHCP" } else { "static" },
        net::HTTP_PORT
    ); }

    // Cluster mode (k10.join= / k10.gossip= / k10.cluster): gossip + peer links; the desired state comes from the
    // cluster (a boot bundle is applied like an operator apply: installed, then announced).
    // A member that advertises the interface's address needs one: it waits for the lease as long as it takes.
    let follows_address = cluster_cfg.as_ref().is_some_and(cluster::advertises_interface);
    while follows_address && net.ipv4().is_none() {
        kprintln!("net: the cluster member advertises the interface's address and has none yet; waiting for DHCP");
        wait_for_address(&mut net, 30_000_000_000);
    }
    let advertised = net.ipv4();
    let clustered = match cluster_cfg {
        Some(cfg) => cluster::start(cfg, &mut net),
        None => false,
    };
    bootcfg::wipe_unused_keys(&bi);
    let apply_boot = |bytes: &[u8]| -> Result<alloc::string::String, alloc::string::String> {
        if clustered {
            match cluster::operator_apply(bytes) {
                Some((200, r)) => Ok(r),
                Some((_, e)) => Err(e),
                None => Err("cluster not running".into()),
            }
        } else {
            runtime::apply(bytes)
        }
    };

    // Deployment: a single node first installs the bundle K10S-DATA kept from its last apply; then the loader's
    // bundle (k10s.bundle, or QEMU's -initrd), else the embedded one, goes on top as an apply would (an older one is
    // refused), else the node waits for POST /_k10s/apply.
    let t_apply0 = arch::rdtsc();
    let mut source = if clustered { "cluster" } else { "none" };
    if let Some(kept) = storage::file(runtime::BUNDLE_FILE).filter(|_| !clustered) {
        match runtime::reload(&kept) {
            Ok(_) => source = "data",
            Err(e) => kprintln!("K10S-DATA's {} not installed: {e}", runtime::BUNDLE_FILE),
        }
    }
    if let Some(m) = bootcfg::bundle(&bi) {
        // SAFETY: the module range is reserved in the frame allocator until released below.
        let bytes = unsafe { core::slice::from_raw_parts(m.paddr as *const u8, m.size as usize) };
        match apply_boot(bytes) {
            Ok(r) if r.contains("\"unchanged\":true") => {}
            Ok(_) => source = if m.name().is_empty() { "initrd" } else { "loader" },
            Err(e) => kprintln!("boot bundle ({}) rejected: {e}", if m.name().is_empty() { "initrd" } else { m.name() }),
        }
        frames::release_module(m.paddr, m.size);
    } else if !EMBEDDED_BUNDLE.is_empty() {
        match apply_boot(EMBEDDED_BUNDLE) {
            Ok(r) if r.contains("\"unchanged\":true") => {}
            Ok(_) => source = "embedded",
            Err(e) => kprintln!("embedded bundle rejected: {e}"),
        }
    }

    if ip == net::IpConfig::Dhcp && net.ipv4().is_none() {
        wait_for_address(&mut net, 30_000_000_000);
    }
    let t_apply = arch::rdtsc();
    let boot_ns = time::now_ns();
    BOOT_NS.store(boot_ns, Ordering::Relaxed);
    let us = |a: u64, b: u64| time::tsc_to_ns(b - a) / 1000;
    if verbose { kprintln!(
        "boot phases (us): cpu+boot info {} memory {} acpi {} clock {} apic {} console+cpus {} pci+devices {} tcp/ip {} console {} apply+address {}",
        us(tsc0, t_parse),
        us(t_parse, t_paging),
        us(t_paging, t_mem),
        us(t_mem, t_clock),
        us(t_clock, t_apic),
        us(t_apic, t_banner),
        us(t_banner, t_nic),
        us(t_nic, t_net),
        us(t_net, t_apply0),
        us(t_apply0, t_apply)
    ); }
    let fs = frames::stats();
    let n = runtime::service_count();
    kprintln!(
        "k10-metal ready {:.2}ms vm->kmain={:.1}ms svc={} src={} engine={} cpus={} used={}K img={}K ram={}K ip={}",
        boot_ns as f64 / 1e6,
        time::vm_to_kmain_ns() as f64 / 1e6,
        n,
        source,
        if jit { "jit" } else { "interp" },
        ncpu,
        (fs.usable - fs.free) >> 10,
        (kernel.1 - kernel.0) >> 10,
        total_ram() >> 10,
        net.ipv4().map_or(alloc::string::String::from("none"), |a| alloc::format!("{}", a.address()))
    );
    if bi.has_flag("k10.exit_on_ready") {
        power::off();
    }

    let spin_ns = knob::<u64>(&bi, "k10.spin_us", "microseconds").unwrap_or(50).saturating_mul(1000);
    main_loop(&mut net, spin_ns, follows_address.then_some(advertised).flatten())
}

/// Runs the network until the interface has an address (DHCP), at most `timeout_ns`.
fn wait_for_address(net: &mut net::Net, timeout_ns: u64) {
    let end = time::now_ns() + timeout_ns;
    while net.ipv4().is_none() && time::now_ns() < end {
        net.poll_ingress();
        net.poll_console();
        net.poll_egress();
        if net.ipv4().is_some() {
            break;
        }
        let wait = net.poll_delay_ns().unwrap_or(10_000_000).min(10_000_000).max(10_000);
        arch::apic::arm_timer(arch::rdtsc() + time::ns_to_tsc(wait));
        arch::interrupts_disable();
        net.dev.rx_interrupts(true);
        if !net.rx_pending() {
            arch::enable_and_halt();
        }
        arch::interrupts_enable();
        net.dev.rx_interrupts(false);
    }
    if net.ipv4().is_none() {
        kprintln!("net: no DHCP lease after {} s; still trying", timeout_ns / 1_000_000_000);
    }
}

/// `k10.selftest=panic`: waits for an address and for the UDP console's first datagram, then panics.
fn selftest_panic(net: &mut net::Net) -> ! {
    wait_for_address(net, 30_000_000_000);
    kprintln!("selftest: the UDP console's first datagram");
    let end = time::now_ns() + 10_000_000_000;
    while !console::udp::sent_one() && time::now_ns() < end {
        net.poll_ingress();
        net.poll_console();
        net.poll_egress();
        arch::pause();
    }
    panic!("k10.selftest=panic: a panic reported over the UDP console");
}

/// After a power request: time to answer it and send the cluster leave.
const POWER_GRACE_NS: u64 = 300_000_000;

/// `advertised`: the address the cluster member advertises, when it is the interface's (a lease that changes it
/// reboots the node, as Tier 1 restarts its node, so peers learn the new one).
fn main_loop(net: &mut net::Net, spin_ns: u64, advertised: Option<smoltcp::wire::Ipv4Cidr>) -> ! {
    watchdog::arm();
    let mut last_busy = time::now_ns();
    let mut spin_from: Option<u64> = None;
    let mut leaving = false;
    let mut nmis = 0;
    loop {
        let n = arch::idt::NMIS.load(Ordering::Relaxed);
        if n != nmis {
            kprintln!("k10-metal: {} NMI(s) of unknown origin (port 0x61 = {:#04x})", n - nmis, arch::idt::NMI_REASON.load(Ordering::Relaxed));
            nmis = n;
        }
        if let (Some(old), Some(new)) = (advertised, net.ipv4()) {
            if old.address() != new.address() && power::requested().is_none() {
                kprintln!("net: address changed from {} to {}: rebooting to advertise the new one", old.address(), new.address());
                power::request(true);
            }
        }
        if let Some((reboot, at)) = power::requested() {
            if let Some(b) = power::take_button_press() {
                kprintln!("power: button pressed ({b}): leaving the cluster, then powering off");
            }
            if !core::mem::replace(&mut leaving, true) {
                cluster::leave();
            }
            if time::now_ns() > at + POWER_GRACE_NS {
                storage::shutdown();
                if reboot {
                    power::reboot();
                }
                power::off();
            }
        }
        time::resync_wall_if_due();
        net.poll_ingress();
        sntp::poll(net);
        net.service(&mut runtime::submit);
        cluster::poll(net);
        runtime::run_cpu0();
        // Forwards created by invocations (`call` of remote services), answers to peers' calls.
        cluster::poll(net);
        let replies = runtime::take_replies();
        for r in replies {
            net.respond(r.conn, r.status, r.content_type, &r.body);
        }
        net.service(&mut runtime::submit);
        net.poll_console();
        net.poll_egress();
        watchdog::pet();
        *net::SNAPSHOT.borrow_mut() = net.snapshot();
        runtime::housekeeping();
        storage::poll();
        efi::poll_health(net::SNAPSHOT.borrow().rx_packets);

        let now = time::now_ns();
        let busy = runtime::has_work() || net.rx_pending() || net.has_work() || cluster::has_work(net) || storage::busy() || leaving;
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
    if let Some(t) = runtime::next_deadline_ns() {
        wait_ns = wait_ns.min(t.saturating_sub(time::now_ns()));
    }
    if let Some(t) = cluster::next_deadline_ns() {
        wait_ns = wait_ns.min(t.saturating_sub(time::now_ns()));
    }
    if let Some(t) = storage::next_deadline_ns() {
        wait_ns = wait_ns.min(t.saturating_sub(time::now_ns()));
    }
    if let Some(t) = sntp::next_deadline_ns() {
        wait_ns = wait_ns.min(t.saturating_sub(time::now_ns()));
    }
    // A NIC without interrupts is polled every millisecond.
    if !net.dev.has_interrupts() {
        wait_ns = wait_ns.min(1_000_000);
    }
    if wait_ns == 0 {
        return;
    }
    arch::apic::arm_timer(arch::rdtsc() + time::ns_to_tsc(wait_ns));
    arch::interrupts_disable();
    // Other CPUs post replies and forwards, then wake us with an IPI if we are idle (see runtime::worker_main).
    let me = percpu::this();
    runtime::note_halting();
    me.idle.store(true, Ordering::SeqCst);
    net.dev.rx_interrupts(true);
    if !net.rx_pending() && !runtime::has_work() {
        arch::enable_and_halt(); // returns after the interrupt handler ran
    }
    me.idle.store(false, Ordering::SeqCst);
    arch::interrupts_enable();
    net.dev.rx_interrupts(false);
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    unsafe {
        serial::SERIAL.force_unlock();
        console::force_unlock();
    }
    kprintln!("\n!!! k10-metal PANIC: {}", info);
    if let Some((lo, hi)) = coro::current_stack() {
        kprintln!("    (in coroutine, stack {:#x}..{:#x})", lo, hi);
    }
    power::fatal_stop()
}
