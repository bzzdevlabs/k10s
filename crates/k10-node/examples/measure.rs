//! Runtime measurements (run in release):
//!
//! ```text
//! cargo run --release -p k10-node --example measure -- prims
//! cargo run --release -p k10-node --example measure -- invoke
//! cargo run --release -p k10-node --example measure -- create
//! cargo run --release -p k10-node --example measure -- density copy 10000
//! cargo run --release -p k10-node --example measure -- density map 100000      (copy | snapcopy | map)
//! cargo run --release -p k10-node --example measure -- idle 100
//! ```
use k10_mesh::{sha256::sha256, Bundle, Manifest, ServiceSpec};
use k10_node::coro::{Coro, Stack};
use k10_node::http::Method;
use k10_node::{demo, sys, Node, NodeConfig, SnapshotPolicy};
use std::ffi::c_void;
use std::time::{Duration, Instant};

fn spec(name: &str, code: &[u8], mem_log2: u8, max: u32) -> ServiceSpec {
    ServiceSpec {
        name: name.into(),
        code_hash: sha256(code),
        replicas: 1,
        mem_log2,
        max_instances: max,
        cpu_ms: 10_000,
        fresh: false,
        config: Vec::new(),
        calls: Default::default(),
    }
}

fn bundle(svcs: &[(&str, k10_ir::Module, u8, u32)]) -> Bundle {
    let mut specs = Vec::new();
    let mut blobs: Vec<Vec<u8>> = Vec::new();
    for (n, m, l, max) in svcs {
        let code = k10_ir::encode(m);
        specs.push(spec(n, &code, *l, *max));
        if !blobs.contains(&code) {
            blobs.push(code);
        }
    }
    Bundle::new(Manifest { version: 0, services: specs, ..Default::default() }, blobs)
}

/// Lets `caller` call `callees` (its `calls` allowlist in the manifest).
fn grant(mut b: Bundle, caller: &str, callees: &[&str]) -> Bundle {
    let s = b.manifest.services.iter_mut().find(|s| s.name == caller).expect("caller in the bundle");
    s.calls = k10_mesh::Calls::from_list(callees).expect("valid callees");
    b
}

fn node(shards: usize, snapshots: SnapshotPolicy) -> Node {
    node_map(shards, snapshots, 0)
}

fn node_map(shards: usize, snapshots: SnapshotPolicy, snapshot_map_min: usize) -> Node {
    Node::start(NodeConfig {
        http: "127.0.0.1:0".parse().unwrap(),
        shards,
        snapshots,
        snapshot_map_min,
        idle_timeout: Duration::from_secs(3600),
        max_fibers: 1024,
        max_calls_per_invocation: u32::MAX,
        ..NodeConfig::default()
    })
    .unwrap()
}

fn prims() {
    // Coroutine round trip (resume + suspend).
    struct P {
        c: *mut Coro,
    }
    extern "C" fn entry(arg: *mut c_void) -> ! {
        let p = arg as *const P;
        loop {
            unsafe { Coro::suspend((*p).c) };
        }
    }
    let mut p = Box::new(P { c: std::ptr::null_mut() });
    let pp: *mut P = &mut *p;
    let c = Box::into_raw(Box::new(Coro::new(Stack::new(64 << 10).unwrap(), entry, pp as *mut c_void)));
    p.c = c;
    let n = 10_000_000;
    let t = Instant::now();
    for _ in 0..n {
        unsafe { Coro::resume(c) };
    }
    println!("coroutine resume+suspend round trip: {:.1} ns", t.elapsed().as_nanos() as f64 / n as f64);
    unsafe { drop(Box::from_raw(c)) };

    // Raw interpreter invocation of an empty function.
    struct H;
    impl k10_ir::Host for H {
        fn call(&mut self, _: u32, _: &[i64], _: &mut [u8]) -> Result<i64, k10_ir::Trap> {
            Ok(0)
        }
    }
    let m = demo::empty();
    let it = k10_ir::Interp::new(&m).unwrap();
    let f = it.export("handle").unwrap();
    let mut mem = k10_ir::new_memory(&m, 12);
    let mut st = k10_ir::InterpStack::new();
    let n = 10_000_000;
    let t = Instant::now();
    for _ in 0..n {
        it.invoke_with(&mut st, &mut mem, 12, &mut H, f, &[]).unwrap();
    }
    println!("interp invoke (empty handle, no scheduler): {:.1} ns", t.elapsed().as_nanos() as f64 / n as f64);

    // Slot alloc + free.
    let stats = std::sync::Arc::new(k10_node::slot::MemStats::default());
    let mut pools = k10_node::slot::SlotPools::new(stats);
    let n = 1_000_000;
    let t = Instant::now();
    for _ in 0..n {
        let mut s = pools.alloc(16).unwrap();
        s.full()[100] = 1; // one page touched
        pools.free(s);
    }
    println!("slot alloc + touch 1 page + free (madvise): {:.1} ns", t.elapsed().as_nanos() as f64 / n as f64);
}

fn invoke() {
    let nd = node(2, SnapshotPolicy::Fresh);
    let b = bundle(&[
        ("empty", demo::empty(), 12, 64),
        ("empty1", demo::empty(), 12, 1),
        ("cl-local", demo::call_loop("empty"), 16, 64),
        ("cl-cross", demo::call_loop("empty1"), 16, 64),
        ("echo", demo::echo(), 16, 64),
    ]);
    nd.apply(grant(grant(b, "cl-local", &["empty"]), "cl-cross", &["empty1"])).unwrap();
    nd.bench_invoke(0, "empty", 100_000);
    let n = 2_000_000;
    let ns = nd.bench_invoke(0, "empty", n);
    println!("invocation overhead (empty handle via scheduler, in-shard loop): {:.0} ns", ns as f64 / n as f64);
    let ns = nd.bench_invoke(0, "echo", n);
    println!("invocation (echo GET → \"hello\"): {:.0} ns", ns as f64 / n as f64);
    // External round trip through the inbox (another thread waits on a condvar).
    let n = 20_000;
    let t = Instant::now();
    for _ in 0..n {
        nd.invoke(0, "empty", Method::Get, "", Vec::new());
    }
    println!("programmatic invoke from another thread (inbox + eventfd + condvar): {:.1} µs", t.elapsed().as_micros() as f64 / n as f64);
    // Same-shard call latency.
    nd.invoke(0, "cl-local", Method::Post, "", b"1000".to_vec());
    let calls = 1_000_000;
    let t = Instant::now();
    let r = nd.invoke(0, "cl-local", Method::Post, "", calls.to_string().into_bytes());
    let el = t.elapsed();
    assert_eq!(r.text(), calls.to_string());
    println!("local call, same shard (suspend caller, run callee, resume): {:.0} ns", el.as_nanos() as f64 / calls as f64);
    // Cross-shard: pin empty1's only instance on shard 1, call it from shard 0.
    nd.invoke(1, "empty1", Method::Get, "", Vec::new());
    let calls = 50_000;
    nd.invoke(0, "cl-cross", Method::Post, "", b"1000".to_vec());
    let t = Instant::now();
    let r = nd.invoke(0, "cl-cross", Method::Post, "", calls.to_string().into_bytes());
    let el = t.elapsed();
    assert_eq!(r.text(), calls.to_string(), "{r:?}");
    println!("local call, cross shard (queue + inbox/eventfd both ways): {:.2} µs", el.as_nanos() as f64 / calls as f64 / 1000.0);
    nd.shutdown();
    // Same with a 50 µs idle spin (no eventfd wake-ups while shards are active).
    let nd = Node::start(NodeConfig {
        http: "127.0.0.1:0".parse().unwrap(),
        shards: 2,
        max_calls_per_invocation: u32::MAX,
        idle_spin: Duration::from_micros(50),
        ..NodeConfig::default()
    })
    .unwrap();
    let b = bundle(&[("empty1", demo::empty(), 12, 1), ("cl-cross", demo::call_loop("empty1"), 16, 64)]);
    nd.apply(grant(b, "cl-cross", &["empty1"])).unwrap();
    nd.invoke(1, "empty1", Method::Get, "", Vec::new());
    nd.invoke(0, "cl-cross", Method::Post, "", b"1000".to_vec());
    let t = Instant::now();
    let r = nd.invoke(0, "cl-cross", Method::Post, "", calls.to_string().into_bytes());
    let el = t.elapsed();
    assert_eq!(r.text(), calls.to_string(), "{r:?}");
    println!("local call, cross shard, idle_spin = 50 µs: {:.2} µs", el.as_nanos() as f64 / calls as f64 / 1000.0);
    nd.shutdown();
}

/// Invocation throughput when every shard runs the same service vs. its own service (shared-state contention).
fn scale() {
    let shards = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    let nd = node(shards, SnapshotPolicy::Fresh);
    let mut svcs = vec![("empty".to_string(), demo::empty(), 12u8, 64u32)];
    for i in 0..shards {
        svcs.push((format!("empty-{i}"), demo::empty(), 12, 64));
    }
    let specs: Vec<(&str, k10_ir::Module, u8, u32)> = svcs.iter().map(|(n, m, l, x)| (n.as_str(), m.clone(), *l, *x)).collect();
    nd.apply(bundle(&specs)).unwrap();
    for same in [true, false] {
        let n = 500_000;
        let t = Instant::now();
        std::thread::scope(|s| {
            for i in 0..shards {
                let nd = &nd;
                let name = if same { "empty".to_string() } else { format!("empty-{i}") };
                s.spawn(move || nd.bench_invoke(i, &name, n));
            }
        });
        let el = t.elapsed();
        println!(
            "{shards} shards, {} service: {:.2} M invocations/s total ({:.0} ns per invocation per shard)",
            if same { "one shared" } else { "one per shard" },
            (n * shards) as f64 / el.as_secs_f64() / 1e6,
            el.as_nanos() as f64 / n as f64
        );
    }
    nd.shutdown();
}

fn create() {
    let m = usize::MAX;
    for (label, pol, map_min, module) in [
        ("empty module, fresh slot", SnapshotPolicy::Fresh, m, demo::empty()),
        ("dense(4 KiB init), data + init()", SnapshotPolicy::Fresh, m, demo::dense(4)),
        ("dense(4 KiB init), snapshot copied", SnapshotPolicy::All, m, demo::dense(4)),
        ("dense(4 KiB init), snapshot mapped CoW", SnapshotPolicy::All, 0, demo::dense(4)),
        ("dense(56 KiB init), data + init()", SnapshotPolicy::Fresh, m, demo::dense(56)),
        ("dense(56 KiB init), snapshot copied", SnapshotPolicy::All, m, demo::dense(56)),
        ("dense(56 KiB init), snapshot mapped CoW", SnapshotPolicy::All, 0, demo::dense(56)),
    ] {
        let nd = node_map(1, pol, map_min);
        nd.apply(bundle(&[("svc", module, 16, 1_000_000)])).unwrap();
        nd.prewarm(0, "svc", 10);
        let n = 20_000;
        let t = Instant::now();
        let got = nd.prewarm(0, "svc", n);
        let el = t.elapsed();
        assert_eq!(got, n);
        println!("instance creation, {label}: {:.2} µs", el.as_nanos() as f64 / n as f64 / 1000.0);
        nd.shutdown();
    }
}

fn density(mode: &str, n: usize) {
    let (pol, map_min) = match mode {
        "map" => (SnapshotPolicy::All, 0),
        "snapcopy" => (SnapshotPolicy::All, usize::MAX),
        _ => (SnapshotPolicy::Fresh, usize::MAX),
    };
    let shards = 2;
    let nd = node_map(shards, pol, map_min);
    nd.apply(bundle(&[("dense", demo::dense(4), 16, 1_000_000)])).unwrap();
    // One instance first (creates the snapshot in snapshot mode), then measure.
    nd.invoke(0, "dense", Method::Get, "", Vec::new());
    std::thread::sleep(Duration::from_millis(200));
    let (rss0, pss0) = sys::rss_pss_bytes();
    let t = Instant::now();
    let mut got = 0;
    std::thread::scope(|s| {
        let hs: Vec<_> = (0..shards).map(|i| {
            let nd = &nd;
            s.spawn(move || nd.prewarm(i, "dense", n / shards))
        }).collect();
        for h in hs {
            got += h.join().unwrap();
        }
    });
    let el = t.elapsed();
    std::thread::sleep(Duration::from_millis(200));
    let (rss1, pss1) = sys::rss_pss_bytes();
    let st = nd.status_json();
    let mapped = st.split("\"snapshot_mapped\":").nth(1).and_then(|s| s.split(',').next()).unwrap_or("?").to_string();
    println!(
        "density mode={mode} instances={got} (64 KiB slot, init dirties 4 KiB): create {:.2} µs/inst, RSS {:.1} → {:.1} MiB \
         ({:.0} B/inst), PSS {:.1} → {:.1} MiB ({:.0} B/inst), snapshot-mapped {mapped}",
        el.as_nanos() as f64 / got as f64 / 1000.0,
        rss0 as f64 / 1048576.0,
        rss1 as f64 / 1048576.0,
        (rss1 - rss0) as f64 / got as f64,
        pss0 as f64 / 1048576.0,
        pss1 as f64 / 1048576.0,
        (pss1 as f64 - pss0 as f64) / got as f64,
    );
    // Instances serve requests.
    let r = nd.invoke(1, "dense", Method::Get, "", Vec::new());
    assert_eq!(r.text(), "192");
    nd.shutdown();
}

fn idle(services: usize) {
    let nd = node(std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1), SnapshotPolicy::Fresh);
    let measure = |label: &str| {
        std::thread::sleep(Duration::from_secs(2));
        let c0 = sys::process_cpu_ns();
        let t = Instant::now();
        std::thread::sleep(Duration::from_secs(10));
        let cpu = sys::process_cpu_ns() - c0;
        let (rss, pss) = sys::rss_pss_bytes();
        println!(
            "idle {label}: CPU {:.3}% of one core, RSS {:.1} MiB, PSS {:.1} MiB",
            cpu as f64 / t.elapsed().as_nanos() as f64 * 100.0,
            rss as f64 / 1048576.0,
            pss as f64 / 1048576.0
        );
    };
    measure("0 services");
    let mods: Vec<(String, k10_ir::Module)> = (0..services).map(|i| (format!("svc-{i}"), demo::echo())).collect();
    let specs: Vec<(&str, k10_ir::Module, u8, u32)> = mods.iter().map(|(n, m)| (n.as_str(), m.clone(), 16, 64)).collect();
    nd.apply(bundle(&specs)).unwrap();
    measure(&format!("{services} services deployed (no instances)"));
    for (n, _) in &mods {
        nd.invoke(0, n, Method::Get, "", Vec::new());
    }
    measure(&format!("{services} services, 1 warm instance each"));
    nd.shutdown();
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(|s| s.as_str()).unwrap_or("all") {
        "prims" => prims(),
        "invoke" => invoke(),
        "create" => create(),
        "scale" => scale(),
        "density" => density(args.get(1).map(|s| s.as_str()).unwrap_or("copy"), args.get(2).and_then(|s| s.parse().ok()).unwrap_or(10_000)),
        "idle" => idle(args.get(1).and_then(|s| s.parse().ok()).unwrap_or(100)),
        _ => {
            prims();
            invoke();
            create();
        }
    }
}
