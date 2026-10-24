//! JIT-specific end-to-end tests (always on the JIT backend, whatever `K10S_BACKEND` says): preemption of machine
//! code through `VmCtx.stack_limit`, CPU budgets, stack overflow with host calls and preemption near the stack
//! limit, host traps through `trap_request`, calls between JIT services, and the lifetime of executable code across
//! applies.
#![cfg(target_arch = "x86_64")]

mod common;
use common::*;
use k10_ir::Module;
use k10_node::demo::{self, Mb};
use k10_node::http::{Client, Method};
use k10_node::{BackendKind, Node, NodeConfig};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::*};
use std::time::{Duration, Instant};

fn jit(shards: usize) -> NodeConfig {
    NodeConfig { backend: BackendKind::Jit, ..config(shards) }
}

fn status(node: &Node) -> String {
    node.status_json()
}

/// `"code":{"modules":N,"code_bytes":B,"exec_bytes":E,…}` from the status JSON.
fn code_stats(node: &Node) -> (i64, i64, i64) {
    let s = status(node);
    let c = &s[s.find("\"code\":{").expect("code stats")..];
    (stat_from(c, "modules"), stat_from(c, "code_bytes"), stat_from(c, "exec_bytes"))
}

/// Recursion that calls host functions (`kv_get`, `now_ns`) and burns a little CPU at every level, so that host
/// calls and preemption (the interrupt hook, which yields the fiber) happen at every depth up to the stack limit.
/// `handle`: depth = decimal body; responds with the depth reached.
fn deep_host() -> Module {
    let mut m = Mb::new(16);
    m.mb.data(64, b"key");
    let imp = m.imp;
    let rec = m.mb.declare(1, 1);
    m.mb.define_with(rec, |f| {
        f.locals(1);
        f.local_get(0).eqz().if_().const_(0).return_().end();
        f.const_(64).const_(3).const_(2048).const_(8).call(imp.kv_get).drop();
        f.call(imp.now_ns).drop();
        // ~3000 iterations of busy work (a few µs: the descent spans many time slices).
        f.const_(3000).local_set(1);
        f.block().loop_();
        f.local_get(1).eqz().br_if(1);
        f.local_get(1).const_(1).sub().local_set(1);
        f.br(0);
        f.end().end();
        f.local_get(0).const_(1).sub().call(rec).const_(1).add();
    });
    m.finish(
        move |f, i, wd, pd| {
            f.locals(1);
            f.call(i.input_len).local_set(0);
            f.const_(4096).const_(0).local_get(0).call(i.input_read).drop();
            f.const_(4096).local_get(0).call(pd).call(rec).call(wd);
        },
        None,
    )
}

/// Writes 1 MiB to the response N times (N = decimal body): N ≥ 17 exceeds the 16 MiB body limit (`Host(1)`).
fn big_output() -> Module {
    Mb::new(20).finish(
        |f, i, _, pd| {
            f.locals(2);
            f.call(i.input_len).local_set(0);
            f.const_(4096).const_(0).local_get(0).call(i.input_read).drop();
            f.const_(4096).local_get(0).call(pd).local_set(1);
            f.block().loop_();
            f.local_get(1).eqz().br_if(1);
            f.const_(0).const_(1 << 20).call(i.output_write).drop();
            f.local_get(1).const_(1).sub().local_set(1);
            f.br(0);
            f.end().end();
        },
        None,
    )
}

/// `kv_put` with a 2000-byte key (> 1 KiB: `Host(2)`).
fn long_key() -> Module {
    Mb::new(16).finish(
        |f, i, _, _| {
            f.const_(4096).const_(2000).const_(0).const_(1).call(i.kv_put).drop();
        },
        None,
    )
}

#[test]
fn infinite_loops_are_preempted_and_hit_their_cpu_budget() {
    // One shard: two busy loops and an echo share one thread; the loops are machine code with no host calls, so
    // only `stack_limit` safepoints (loop back-edges) can stop them.
    let node = Node::start(jit(1)).unwrap();
    node.apply(bundle(vec![svc("spin", &demo::spin()).cpu(400), svc("short", &demo::spin()).cpu(50), svc("echo", &demo::echo())]))
        .unwrap();
    assert!(status(&node).contains("\"backend\":\"jit\""));
    let addr = node.http_addr().to_string();
    let stop = AtomicBool::new(false);
    let (n, max_us) = (AtomicU64::new(0), AtomicU64::new(0));
    std::thread::scope(|s| {
        let spins: Vec<_> = ["/spin", "/spin", "/short"]
            .into_iter()
            .map(|p| {
                let addr = addr.clone();
                s.spawn(move || {
                    let t = Instant::now();
                    let r = Client::connect(&addr, Duration::from_secs(10)).unwrap().request("GET", p, b"").unwrap();
                    (p, r.status, r.text(), t.elapsed())
                })
            })
            .collect();
        s.spawn(|| {
            let mut c = Client::connect(&addr, Duration::from_secs(10)).unwrap();
            std::thread::sleep(Duration::from_millis(20));
            while !stop.load(Relaxed) {
                let t = Instant::now();
                assert_eq!(c.request("POST", "/echo", b"x").unwrap().text(), "x");
                max_us.fetch_max(t.elapsed().as_micros() as u64, Relaxed);
                n.fetch_add(1, Relaxed);
            }
        });
        for h in spins {
            let (p, st, text, el) = h.join().unwrap();
            assert_eq!(st, 500, "{p}: {text}");
            assert!(text.contains("exceeded its CPU budget"), "{p}: {text}");
            let budget = if p == "/short" { 50 } else { 400 };
            // Wall time ≥ the CPU budget (3 loops share the shard), bounded.
            assert!(el >= Duration::from_millis(budget - 5) && el < Duration::from_secs(10), "{p}: {el:?}");
        }
        stop.store(true, Relaxed);
    });
    let (n, max_us) = (n.load(Relaxed), max_us.load(Relaxed));
    eprintln!("echo next to 3 JIT busy loops: {n} requests, max latency {max_us} µs");
    assert!(n >= 30, "echo starved: {n}");
    assert!(max_us < 100_000, "echo latency {max_us} µs");
    let s = status(&node);
    assert_eq!(svc_stat(&s, "spin", "interrupted"), 2);
    assert_eq!(svc_stat(&s, "short", "interrupted"), 1);
    assert!(stat_from(&s, "yields") > 10, "{s}");
    // The node is healthy and the services are re-created on demand.
    assert_eq!(post(&node, "/echo", b"after").text(), "after");
    node.shutdown();
}

#[test]
fn stack_overflow_traps_cleanly_with_host_calls_and_preemption_at_depth() {
    let node = Node::start(jit(2)).unwrap();
    node.apply(bundle(vec![svc("deep", &demo::deep()), svc("dh", &deep_host()).cpu(60_000), svc("echo", &demo::echo())])).unwrap();
    let mut c = client(&node);
    // TIR guarantees ≥ 1000 frames of a small function; the JIT on a 1 MiB fiber stack gives far more.
    for d in ["1000", "10000"] {
        assert_eq!(c.request("POST", "/deep", d.as_bytes()).unwrap().text(), d);
    }
    for _ in 0..3 {
        let r = c.request("POST", "/deep", b"100000000").unwrap();
        assert_eq!(r.status, 500);
        assert!(r.text().contains("StackOverflow"), "{}", r.text());
        assert_eq!(c.request("POST", "/deep", b"20").unwrap().text(), "20", "a fresh instance after the trap");
    }
    // Host calls and interrupts (yields) at every depth, until the stack limit: a clean trap, no guard-page hit.
    assert_eq!(c.request("POST", "/dh", b"500").unwrap().text(), "500");
    let before = stat_from(&status(&node), "yields");
    let r = c.request("POST", "/dh", b"100000000").unwrap();
    assert_eq!(r.status, 500, "{}", r.text());
    assert!(r.text().contains("StackOverflow"), "{}", r.text());
    let yields = stat_from(&status(&node), "yields") - before;
    eprintln!("deep_host overflow: {yields} yields on the way down");
    assert!(yields >= 5, "{yields}");
    // Several in parallel (distinct fibers on both shards).
    let addr = node.http_addr().to_string();
    std::thread::scope(|s| {
        for _ in 0..6 {
            let addr = addr.clone();
            s.spawn(move || {
                let r = Client::connect(&addr, Duration::from_secs(30)).unwrap().request("POST", "/dh", b"100000000").unwrap();
                assert!(r.status == 500 && r.text().contains("StackOverflow"), "{}", r.text());
            });
        }
    });
    assert_eq!(c.request("POST", "/echo", b"alive").unwrap().text(), "alive");
    node.shutdown();
}

#[test]
fn host_traps_abort_the_invocation() {
    let node = Node::start(jit(1)).unwrap();
    node.apply(bundle(vec![
        svc("bad", &demo::bad_read()).mem(12),
        svc("big", &big_output()).mem(21),
        svc("key", &long_key()),
        svc("m", &demo::memcount(false)),
    ]))
    .unwrap();
    let mut c = client(&node);
    let r = c.request("POST", "/bad", &[1u8; 300]).unwrap();
    assert!(r.status == 500 && r.text().contains("MemOutOfBounds"), "{}", r.text());
    let r = c.request("POST", "/big", b"3").unwrap();
    assert_eq!((r.status, r.body.len()), (200, 3 << 20));
    let r = c.request("POST", "/big", b"17").unwrap();
    assert!(r.status == 500 && r.text().contains("Host(1)"), "{}", r.text());
    let r = c.request("GET", "/key", b"").unwrap();
    assert!(r.status == 500 && r.text().contains("Host(2)"), "{}", r.text());
    // `unreachable` in JIT code, then the instance is replaced.
    assert_eq!(c.request("GET", "/m", b"").unwrap().text(), "1");
    assert_eq!(c.request("GET", "/m/trap", b"").unwrap().status, 500);
    assert_eq!(c.request("GET", "/m", b"").unwrap().text(), "1");
    let s = status(&node);
    assert_eq!(svc_stat(&s, "big", "traps"), 1);
    assert_eq!(svc_stat(&s, "key", "traps"), 1);
    node.shutdown();
}

#[test]
fn calls_between_jit_services() {
    let node = Node::start(jit(2)).unwrap();
    node.apply(bundle(vec![
        svc("front", &demo::caller("back")).calls(&["back"]),
        svc("back", &demo::path_echo()).max(1),
        svc("loop", &demo::call_loop("echo")).calls(&["echo"]),
        svc("echo", &demo::echo()),
    ]))
    .unwrap();
    // Same shard, then cross shard (back's only instance lives on shard 1).
    let r = node.invoke(0, "front", Method::Post, "/a?b", b"x".to_vec());
    assert_eq!((r.status, r.text().as_str()), (200, "method:1 path:/a?b"));
    assert_eq!(node.invoke(1, "back", Method::Get, "/warm", Vec::new()).text(), "method:0 path:/warm");
    for i in 0..20 {
        let r = node.invoke(0, "front", Method::Post, &format!("/p{i}"), Vec::new());
        assert_eq!(r.text(), format!("method:1 path:/p{i}"));
    }
    // 2000 calls from one invocation (the caller's fiber suspends inside the `call` adapter each time).
    assert_eq!(post(&node, "/loop", b"2000").text(), "2000");
    let s = status(&node);
    assert!(stat_from(&s, "calls") >= 2021, "{s}");
    node.shutdown();
}

#[test]
fn executable_code_is_freed_when_old_versions_drain() {
    let node = Node::start(jit(2)).unwrap();
    let v = |cfg: &str, m: &Module| bundle(vec![svc("svc", m).config(cfg), svc("echo", &demo::echo())]);
    node.apply(v("1", &demo::burn())).unwrap();
    let (mods, code, exec) = code_stats(&node);
    assert_eq!(mods, 2);
    assert!(code > 0 && exec >= code && exec % 4096 == 0, "{code} {exec}");
    let echo_only = {
        let e = Node::start(jit(1)).unwrap();
        e.apply(bundle(vec![svc("echo", &demo::echo())])).unwrap();
        let s = code_stats(&e);
        e.shutdown();
        s
    };
    let addr = node.http_addr().to_string();
    let stop = AtomicBool::new(false);
    let bad = AtomicU64::new(0);
    std::thread::scope(|s| {
        for _ in 0..4 {
            s.spawn(|| {
                let mut c = Client::connect(&addr, Duration::from_secs(10)).unwrap();
                while !stop.load(Relaxed) {
                    if c.request("GET", "/svc", b"").unwrap().status != 200 {
                        bad.fetch_add(1, Relaxed);
                    }
                }
            });
        }
        // Alternate between two different modules (burn 1 ms / path echo): each apply compiles a new module while
        // the old version still has requests in flight.
        for i in 0..10 {
            std::thread::sleep(Duration::from_millis(30));
            let m = if i % 2 == 0 { demo::path_echo() } else { demo::burn() };
            node.apply(v(&i.to_string(), &m)).unwrap();
        }
        std::thread::sleep(Duration::from_millis(30));
        stop.store(true, Relaxed);
    });
    assert_eq!(bad.load(Relaxed), 0);
    // Once the retired versions drain, only the current modules remain mapped.
    let t = Instant::now();
    loop {
        let (mods, _, exec) = code_stats(&node);
        let s = status(&node);
        if mods == 2 && stat_from(&s, "slots_used") == svc_stat(&s, "svc", "live") + svc_stat(&s, "echo", "live") {
            let burn = exec - echo_only.2;
            assert!(burn > 0, "{exec} {echo_only:?}");
            break;
        }
        assert!(t.elapsed() < Duration::from_secs(5), "old code not freed: {mods} modules alive\n{s}");
        std::thread::sleep(Duration::from_millis(20));
    }
    // Removing a service frees its code; an unchanged module is reused, not recompiled.
    let prepared = |n: &Node| stat_from(&status(n), "prepared");
    let p0 = prepared(&node);
    node.apply(bundle(vec![svc("echo", &demo::echo())])).unwrap();
    assert_eq!(prepared(&node), p0, "echo's code is reused");
    let t = Instant::now();
    while code_stats(&node).0 != 1 {
        assert!(t.elapsed() < Duration::from_secs(5), "{}", status(&node));
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(code_stats(&node), echo_only);
    node.shutdown();
}
