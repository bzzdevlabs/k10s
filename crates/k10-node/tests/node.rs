//! End-to-end tests: a node started in-process on an ephemeral port, driven over HTTP (and the programmatic API
//! where a test needs to pin work to a shard).

mod common;
use common::*;
use k10_node::http::{Client, Method};
use k10_node::{demo, SnapshotPolicy};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::*};
use std::time::{Duration, Instant};

fn status(node: &k10_node::Node) -> String {
    node.status_json()
}

#[test]
fn call_same_shard_and_cross_shard() {
    let node = start(config(2));
    node.apply(bundle(vec![
        svc("a", &demo::caller("b")).calls(&["b"]),
        svc("b", &demo::path_echo()).max(1),
        svc("c", &demo::caller("e")).calls(&["e"]),
        svc("e", &demo::echo()),
    ]))
    .unwrap();
    // Same shard: c (shard 0) calls e, which gets an instance on shard 0.
    let r = node.invoke(0, "c", Method::Post, "/x", b"ping".to_vec());
    assert_eq!((r.status, r.text().as_str()), (200, "ping"));
    let s = status(&node);
    assert_eq!(stat_from(&s, "cross_shard_replies"), 0, "{s}");
    // Pin b's only instance (max_instances = 1) to shard 1, then call it from shard 0: the request is queued on
    // b's node-wide queue, shard 1 is poked, runs it and replies through shard 0's inbox.
    let r = node.invoke(1, "b", Method::Get, "/warm", Vec::new());
    assert_eq!(r.text(), "method:0 path:/warm");
    for i in 0..20 {
        let r = node.invoke(0, "a", Method::Post, &format!("/p{i}"), b"x".to_vec());
        assert_eq!((r.status, r.text()), (200, format!("method:1 path:/p{i}")));
    }
    let s = status(&node);
    assert!(stat_from(&s, "cross_shard_replies") >= 20, "{s}");
    assert_eq!(svc_stat(&s, "b", "created"), 1);
    // Over HTTP too.
    let r = post(&node, "/a/over/http?q=1", b"body");
    assert_eq!((r.status, r.text().as_str()), (200, "method:1 path:/over/http?q=1"));
    // Unknown target → 502 with code -1.
    node.apply(bundle(vec![svc("u", &demo::caller("nosuch")).calls(&["*"])])).unwrap();
    let r = get(&node, "/u");
    assert_eq!((r.status, r.text().as_str()), (502, "call error -1"));
    node.shutdown();
}

/// `GET /<prober>/<target>` with body `n` (see `demo::call_probe`) → (status, response length, ns per call).
fn probe(node: &k10_node::Node, prober: &str, target: &str, n: u32) -> (i64, i64, f64) {
    let mut c = client(node);
    let r = c.request("POST", &format!("/{prober}/{target}"), n.to_string().as_bytes()).unwrap();
    assert_eq!(r.status, 200, "{prober} -> {target}: {}", r.text());
    let t = r.text();
    let v: Vec<i64> = t.split(' ').map(|x| x.parse().unwrap()).collect();
    (v[0], v[1], v[2] as f64 / n as f64)
}

/// `calls` allowlist (both backends): granted calls work; any other call gets exactly what an unknown service gets.
#[test]
fn call_allowlist_denied_call_is_indistinguishable_from_unknown() {
    let node = start(config(1));
    node.apply(bundle(vec![
        svc("echo", &demo::echo()),
        svc("other", &demo::echo()),
        svc("probe", &demo::call_probe()).calls(&["echo"]),
        svc("none", &demo::call_probe()),
        svc("me", &demo::call_probe()).calls(&["me"]),
        svc("any", &demo::call_probe()).calls(&["*"]),
    ]))
    .unwrap();
    let st = |p: &str, t: &str| {
        let (s, l, _) = probe(&node, p, t, 1);
        (s, l)
    };
    // Granted (echo answers "hello\n").
    assert_eq!(st("probe", "echo"), (200, 6));
    assert_eq!(st("probe", "echo/sub/path"), (200, 6));
    assert_eq!(st("any", "other"), (200, 6), "the wildcard reaches every service");
    // A truly unknown service (only a wildcard caller gets that far) and every denied call look the same.
    let unknown = st("any", "nosuch");
    assert_eq!(unknown, (-1, 0));
    for (p, t) in [("probe", "other"), ("probe", "probe"), ("probe", "none"), ("probe", "nosuch"), ("probe", "echo2")] {
        assert_eq!(st(p, t), unknown, "{p} -> {t}");
    }
    for t in ["echo", "none", "nosuch"] {
        assert_eq!(st("none", t), unknown, "no `calls`: nothing, not even itself (none -> {t})");
    }
    // Self-calls need an entry: `me` lists itself (its inner call to echo is then denied, hence status 200).
    let mut c = client(&node);
    let r = c.request("POST", "/me/me/echo", b"1").unwrap();
    assert!(r.text().starts_with("200 "), "{}", r.text());
    // Denied calls reached no service.
    let s = status(&node);
    assert_eq!(svc_stat(&s, "echo", "requests"), 2);
    assert_eq!(svc_stat(&s, "other", "requests"), 1);
    // Same timing class, denied existing vs. missing name: interleaved rounds of 10 000 calls, best of 7; the loose
    // bound absorbs machine noise (identical paths; a forwarded or executed call is orders of magnitude slower).
    let cases = [("probe", "other"), ("probe", "no-such-service"), ("any", "nosuch"), ("probe", "echo")];
    let mut best = [f64::MAX; 4];
    for _ in 0..7 {
        for (k, (p, t)) in cases.iter().enumerate() {
            best[k] = best[k].min(probe(&node, p, t, 10_000).2);
        }
    }
    let [exists, missing, wild, granted] = best;
    eprintln!("ns/call: denied {exists:.0} (exists) {missing:.0} (missing), wildcard unknown {wild:.0}, granted {granted:.0}");
    assert!(exists < 2.5 * missing && missing < 2.5 * exists, "denied timings differ: {exists:.0} vs {missing:.0} ns");
    node.shutdown();
}

/// A callee that is not in the manifest is refused at apply time (typos are caught, nothing changes).
#[test]
fn apply_rejects_calls_to_services_not_in_the_manifest() {
    let node = start(config(1));
    node.apply(bundle(vec![svc("echo", &demo::echo())])).unwrap();
    let typo = bundle(vec![svc("echo", &demo::echo()), svc("c", &demo::caller("echo")).calls(&["ehco"])]);
    let e = node.apply(typo).unwrap_err();
    assert!(e.contains("calls `ehco`"), "{e}");
    // Over HTTP the bundle does not even decode (the decoder checks callees too).
    let b = bundle(vec![svc("c", &demo::caller("echo")).calls(&["echo"])]).encode();
    let mut c = client(&node);
    let r = admin(&mut c, "POST", "/_k10s/apply", &b).unwrap();
    assert_eq!(r.status, 400, "{}", r.text());
    assert_eq!(get(&node, "/echo").status, 200, "the running state is unchanged");
    assert_eq!(get(&node, "/c").status, 404);
    node.shutdown();
}

/// A service whose module imports `call` but whose manifest grants no `calls` (e.g. written before `calls`) is reported.
#[test]
fn apply_warns_about_callers_without_calls() {
    let node = start(config(1));
    let mut mb = k10_ir::ModuleBuilder::new();
    let f = mb.func(0, 0, |_| {});
    mb.export("handle", f);
    let plain = mb.build();
    let mk = || bundle(vec![svc("p", &plain), svc("c", &demo::caller("p")), svc("g", &demo::caller("p")).calls(&["p"])]);
    let r = node.apply(mk()).unwrap();
    assert_eq!(r.warnings.len(), 1, "{:?}", r.warnings);
    assert!(r.warnings[0].starts_with("service `c` imports `call`"), "{:?}", r.warnings);
    assert!(r.to_json().contains("\"warnings\":[\"service `c` imports `call`"));
    let s = status(&node);
    assert!(s.contains("\"warnings\":[\"service `c` imports `call`"), "{s}");
    assert!(s.contains("\"name\":\"g\"") && s.contains("\"may_call\":[\"p\"]"), "{s}");
    assert_eq!(node.apply(mk()).unwrap().warnings.len(), 1, "a no-op re-apply still reports it");
    node.shutdown();
}

#[test]
fn kv_and_bundle_persist_across_restart() {
    let dir = tmpdir("persist");
    let mk = || NodeConfig { data_dir: Some(dir.clone()), ..config(2) };
    use k10_node::NodeConfig;
    {
        let node = start(mk());
        node.apply(bundle(vec![svc("kv", &demo::kv()), svc("counter", &demo::counter())])).unwrap();
        let mut c = client(&node);
        assert_eq!(c.request("POST", "/kv/foo", b"bar").unwrap().status, 200);
        assert_eq!(c.request("PUT", "/kv/gone", b"x").unwrap().status, 200);
        assert_eq!(c.request("DELETE", "/kv/gone", b"").unwrap().status, 200);
        assert_eq!(c.request("DELETE", "/kv/gone", b"").unwrap().status, 404);
        assert_eq!(c.request("GET", "/kv/foo", b"").unwrap().text(), "bar");
        assert_eq!(c.request("GET", "/kv/missing", b"").unwrap().status, 404);
        for i in 1..=3 {
            assert_eq!(c.request("GET", "/counter", b"").unwrap().text(), i.to_string());
        }
        node.shutdown();
    }
    let node = start(mk());
    // The bundle was reloaded from the data directory; the KV log was replayed.
    let mut c = client(&node);
    assert_eq!(c.request("GET", "/kv/foo", b"").unwrap().text(), "bar");
    assert_eq!(c.request("GET", "/kv/gone", b"").unwrap().status, 404);
    assert_eq!(c.request("GET", "/counter", b"").unwrap().text(), "4");
    node.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

/// The same with a raw store (`--store`): KV namespaces and the bundle live in the store's segmented log.
#[test]
fn kv_and_bundle_persist_in_a_raw_store() {
    use k10_node::kv::{FileDisk, KvConfig, KvStore};
    use k10_node::NodeConfig;
    let dir = tmpdir("persist-raw");
    let path = dir.join("store");
    std::fs::File::create(&path).unwrap().set_len(64 << 20).unwrap();
    let mk = || {
        let f = std::fs::OpenOptions::new().read(true).write(true).open(&path).unwrap();
        let kv = KvStore::open_raw(Box::new(FileDisk::new(f, "test").unwrap()), "test", KvConfig::default()).unwrap();
        NodeConfig { store: Some(std::sync::Arc::new(kv)), ..config(2) }
    };
    {
        let node = start(mk());
        node.apply(bundle(vec![svc("kv", &demo::kv()), svc("counter", &demo::counter())])).unwrap();
        let mut c = client(&node);
        assert_eq!(c.request("POST", "/kv/foo", b"bar").unwrap().status, 200);
        for i in 1..=3 {
            assert_eq!(c.request("GET", "/counter", b"").unwrap().text(), i.to_string());
        }
        node.shutdown();
    }
    let node = start(mk());
    let mut c = client(&node);
    assert_eq!(c.request("GET", "/kv/foo", b"").unwrap().text(), "bar");
    assert_eq!(c.request("GET", "/counter", b"").unwrap().text(), "4");
    node.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn trap_fails_request_and_replaces_instance() {
    let node = start(config(1));
    node.apply(bundle(vec![svc("m", &demo::memcount(false)), svc("bad", &demo::bad_read()).mem(12), svc("deep", &demo::deep())]))
        .unwrap();
    let mut c = client(&node);
    assert_eq!(c.request("GET", "/m", b"").unwrap().text(), "1");
    assert_eq!(c.request("GET", "/m", b"").unwrap().text(), "2");
    let r = c.request("GET", "/m/trap", b"").unwrap();
    assert_eq!(r.status, 500);
    assert!(r.text().contains("Unreachable"), "{}", r.text());
    // A new instance: memory state starts over.
    assert_eq!(c.request("GET", "/m", b"").unwrap().text(), "1");
    let s = status(&node);
    assert_eq!(svc_stat(&s, "m", "traps"), 1);
    assert_eq!(svc_stat(&s, "m", "created"), 2);
    assert_eq!(svc_stat(&s, "m", "live"), 1);
    // Host range check trap.
    let r = c.request("POST", "/bad", &[1u8; 300]).unwrap();
    assert_eq!(r.status, 500);
    assert!(r.text().contains("MemOutOfBounds"), "{}", r.text());
    // Stack overflow is a trap too; moderate recursion is fine.
    assert_eq!(c.request("POST", "/deep", b"500").unwrap().text(), "500");
    let r = c.request("POST", "/deep", b"100000").unwrap();
    assert_eq!(r.status, 500);
    assert!(r.text().contains("StackOverflow"), "{}", r.text());
    assert_eq!(c.request("POST", "/deep", b"20").unwrap().text(), "20");
    // The trap is in the service log.
    let logs = admin(&mut c, "GET", "/_k10s/logs/m", b"").unwrap().text();
    assert!(logs.contains("trapped: Unreachable"), "{logs}");
    node.shutdown();
}

#[test]
fn crash_loop_backoff() {
    let node = start(config(1));
    node.apply(bundle(vec![svc("m", &demo::memcount(false))])).unwrap();
    let mut c = client(&node);
    for _ in 0..3 {
        assert_eq!(c.request("GET", "/m/trap", b"").unwrap().status, 500);
    }
    // 3 consecutive traps: instance creation is backed off (100 ms) and nothing is alive → 503.
    let r = c.request("GET", "/m", b"").unwrap();
    assert_eq!(r.status, 503, "{}", r.text());
    assert!(r.text().contains("crash-looping"));
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(c.request("GET", "/m", b"").unwrap().text(), "1");
    node.shutdown();
}

#[test]
fn cpu_budget_trap_and_fairness() {
    // One shard: the busy loops and the well-behaved service share the same thread.
    let node = start(config(1));
    node.apply(bundle(vec![svc("spin", &demo::spin()).cpu(300), svc("echo", &demo::echo())])).unwrap();
    let addr = node.http_addr().to_string();
    let stop = AtomicBool::new(false);
    let echo_n = AtomicU64::new(0);
    let echo_max_us = AtomicU64::new(0);
    std::thread::scope(|s| {
        let spins: Vec<_> = (0..3)
            .map(|_| {
                let addr = addr.clone();
                s.spawn(move || {
                    let t = Instant::now();
                    let r = Client::connect(&addr, Duration::from_secs(10)).unwrap().request("GET", "/spin", b"").unwrap();
                    (r.status, r.text(), t.elapsed())
                })
            })
            .collect();
        s.spawn(|| {
            let mut c = Client::connect(&addr, Duration::from_secs(10)).unwrap();
            std::thread::sleep(Duration::from_millis(30));
            while !stop.load(Relaxed) {
                let t = Instant::now();
                let r = c.request("GET", "/echo", b"").unwrap();
                assert_eq!(r.status, 200);
                echo_max_us.fetch_max(t.elapsed().as_micros() as u64, Relaxed);
                echo_n.fetch_add(1, Relaxed);
            }
        });
        for h in spins {
            let (st, text, el) = h.join().unwrap();
            assert_eq!(st, 500, "{text}");
            assert!(text.contains("CPU budget"), "{text}");
            assert!(el >= Duration::from_millis(290) && el < Duration::from_secs(5), "{el:?}");
        }
        stop.store(true, Relaxed);
    });
    let n = echo_n.load(Relaxed);
    let max = echo_max_us.load(Relaxed);
    eprintln!("echo while 3 busy loops ran: {n} requests, max latency {max} µs");
    assert!(n >= 30, "echo starved: only {n} requests");
    assert!(max < 100_000, "echo latency too high: {max} µs");
    let s = status(&node);
    assert_eq!(svc_stat(&s, "spin", "interrupted"), 3);
    assert!(stat_from(&s, "preempt_requests") > 10, "{s}");
    node.shutdown();
}

#[test]
fn call_cycle_times_out_instead_of_deadlocking() {
    let node = start(k10_node::NodeConfig { call_timeout: Duration::from_millis(500), ..config(2) });
    node.apply(bundle(vec![
        svc("a", &demo::caller("b")).calls(&["b"]).max(1),
        svc("b", &demo::caller("a")).calls(&["a"]).max(1),
        svc("echo", &demo::echo()),
    ]))
    .unwrap();
    let t = Instant::now();
    let r = get(&node, "/a");
    let el = t.elapsed();
    assert_eq!(r.status, 502, "{}", r.text());
    assert!(r.text().contains("-3"), "{}", r.text());
    assert!(el >= Duration::from_millis(450) && el < Duration::from_secs(3), "{el:?}");
    // The node is healthy afterwards (and the cycle is still answered, not stuck).
    assert_eq!(get(&node, "/echo").status, 200);
    assert_eq!(get(&node, "/a").status, 502);
    assert!(stat_from(&status(&node), "timeouts") >= 2);
    node.shutdown();
}

#[test]
fn max_instances_respected_and_requests_queue() {
    let node = start(config(2));
    node.apply(bundle(vec![svc("burn", &demo::burn()).config("100").max(2)])).unwrap();
    let addr = node.http_addr().to_string();
    let t = Instant::now();
    std::thread::scope(|s| {
        let hs: Vec<_> = (0..8)
            .map(|_| {
                let addr = addr.clone();
                s.spawn(move || Client::connect(&addr, Duration::from_secs(10)).unwrap().request("GET", "/burn", b"").unwrap())
            })
            .collect();
        for h in hs {
            let r = h.join().unwrap();
            assert_eq!((r.status, r.text().as_str()), (200, "done"));
        }
    });
    let el = t.elapsed();
    let s = status(&node);
    assert_eq!(svc_stat(&s, "burn", "peak"), 2, "{s}");
    assert!(svc_stat(&s, "burn", "created") <= 2);
    assert!(svc_stat(&s, "burn", "queued") >= 6);
    assert!(el >= Duration::from_millis(390), "8 × 100 ms on 2 instances took {el:?}");
    node.shutdown();
}

#[test]
fn idle_instances_are_reclaimed_down_to_one() {
    let node = start(k10_node::NodeConfig { idle_timeout: Duration::from_millis(300), ..config(2) });
    node.apply(bundle(vec![svc("burn", &demo::burn()).config("50")])).unwrap();
    let addr = node.http_addr().to_string();
    std::thread::scope(|s| {
        for _ in 0..6 {
            let addr = addr.clone();
            s.spawn(move || {
                let r = Client::connect(&addr, Duration::from_secs(10)).unwrap().request("GET", "/burn", b"").unwrap();
                assert_eq!(r.status, 200);
            });
        }
    });
    let s = status(&node);
    assert!(svc_stat(&s, "burn", "live") >= 2, "{s}");
    let t = Instant::now();
    loop {
        let s = status(&node);
        if svc_stat(&s, "burn", "live") == 1 {
            assert!(svc_stat(&s, "burn", "reclaimed") >= 1);
            break;
        }
        assert!(t.elapsed() < Duration::from_secs(5), "not reclaimed: {s}");
        std::thread::sleep(Duration::from_millis(50));
    }
    // The warm instance still serves.
    assert_eq!(get(&node, "/burn").status, 200);
    node.shutdown();
}

#[test]
fn apply_new_versions_under_load_without_errors() {
    let node = start(config(2));
    node.apply(bundle(vec![svc("svc", &demo::config_echo()).config("v1")])).unwrap();
    let addr = node.http_addr().to_string();
    let stop = AtomicBool::new(false);
    let ok = AtomicU64::new(0);
    let bad = AtomicU64::new(0);
    let seen_v3 = AtomicU64::new(0);
    std::thread::scope(|s| {
        for _ in 0..8 {
            s.spawn(|| {
                let mut c = Client::connect(&addr, Duration::from_secs(10)).unwrap();
                while !stop.load(Relaxed) {
                    let r = c.request("GET", "/svc/x", b"").unwrap();
                    if r.status == 200 {
                        ok.fetch_add(1, Relaxed);
                        if r.text().starts_with("method:") {
                            seen_v3.fetch_add(1, Relaxed);
                        }
                    } else {
                        eprintln!("bad response {} {}", r.status, r.text());
                        bad.fetch_add(1, Relaxed);
                    }
                }
            });
        }
        std::thread::sleep(Duration::from_millis(300));
        let r = node.apply(bundle(vec![svc("svc", &demo::config_echo()).config("v2")])).unwrap();
        assert_eq!(r.updated, vec!["svc"]);
        std::thread::sleep(Duration::from_millis(300));
        // A different module.
        let r = node.apply(bundle(vec![svc("svc", &demo::path_echo())])).unwrap();
        assert_eq!(r.updated, vec!["svc"]);
        std::thread::sleep(Duration::from_millis(300));
        stop.store(true, Relaxed);
    });
    assert_eq!(bad.load(Relaxed), 0);
    assert!(ok.load(Relaxed) > 100);
    assert!(seen_v3.load(Relaxed) > 0);
    assert_eq!(get(&node, "/svc/y").text(), "method:0 path:/y");
    // Old versions drained: only the current version's instances remain.
    let t = Instant::now();
    loop {
        let s = status(&node);
        let live = svc_stat(&s, "svc", "live");
        let slots = stat_from(&s, "slots_used");
        if slots == live {
            break;
        }
        assert!(t.elapsed() < Duration::from_secs(3), "old instances not drained: {s}");
        std::thread::sleep(Duration::from_millis(20));
    }
    // Removing a service.
    node.apply(bundle(vec![svc("echo", &demo::echo())])).unwrap();
    assert_eq!(get(&node, "/svc").status, 404);
    node.shutdown();
}

#[test]
fn fresh_services_and_init() {
    let node = start(config(2));
    node.apply(bundle(vec![
        svc("plain", &demo::memcount(false)),
        svc("fresh", &demo::memcount(false)).fresh(),
        svc("initp", &demo::memcount(true)),
        svc("initf", &demo::memcount(true)).fresh(),
    ]))
    .unwrap();
    let mut c = client(&node);
    for i in 1..=3 {
        assert_eq!(c.request("GET", "/plain", b"").unwrap().text(), i.to_string());
        assert_eq!(c.request("GET", "/fresh", b"").unwrap().text(), "1");
        assert_eq!(c.request("GET", "/initp", b"").unwrap().text(), (1000 + i).to_string());
        assert_eq!(c.request("GET", "/initf", b"").unwrap().text(), "1001");
    }
    // Concurrent requests on the fresh service (several instances mapped from the snapshot).
    let addr = node.http_addr().to_string();
    std::thread::scope(|s| {
        for _ in 0..8 {
            let addr = addr.clone();
            s.spawn(move || {
                let mut c = Client::connect(&addr, Duration::from_secs(10)).unwrap();
                for _ in 0..20 {
                    assert_eq!(c.request("GET", "/initf", b"").unwrap().text(), "1001");
                }
            });
        }
    });
    let s = status(&node);
    assert!(s.contains("\"snapshot\":{\"extent\""), "{s}");
    node.shutdown();
}

#[test]
fn snapshot_policy_all() {
    let node = start(k10_node::NodeConfig { snapshots: SnapshotPolicy::All, snapshot_map_min: 0, ..config(2) });
    node.apply(bundle(vec![svc("initp", &demo::memcount(true)), svc("dense", &demo::dense(8))])).unwrap();
    let addr = node.http_addr().to_string();
    std::thread::scope(|s| {
        for _ in 0..6 {
            let addr = addr.clone();
            s.spawn(move || {
                let mut c = Client::connect(&addr, Duration::from_secs(10)).unwrap();
                for _ in 0..10 {
                    let v: i64 = c.request("GET", "/initp", b"").unwrap().text().parse().unwrap();
                    assert!(v > 1000);
                    assert_eq!(c.request("GET", "/dense", b"").unwrap().text(), "192");
                }
            });
        }
    });
    let n = node.prewarm(0, "dense", 50);
    assert_eq!(n, 50);
    let s = status(&node);
    assert!(stat_from(&s, "snapshot_mapped") >= 50, "{s}");
    node.shutdown();
}

#[test]
fn admin_endpoints() {
    let node = start(config(2));
    node.apply(bundle(vec![svc("logger", &demo::logger()), svc("cfg", &demo::config_echo()).config("a=b")])).unwrap();
    let mut c = client(&node);
    assert_eq!(c.request("POST", "/logger", b"first line").unwrap().text(), "logged");
    assert_eq!(c.request("POST", "/logger", b"second line\n").unwrap().text(), "logged");
    let logs = admin(&mut c, "GET", "/_k10s/logs/logger", b"").unwrap();
    assert_eq!(logs.status, 200);
    assert!(logs.text().contains(" first line\n") && logs.text().contains(" second line\n"), "{}", logs.text());
    assert_eq!(admin(&mut c, "GET", "/_k10s/logs/nosuch", b"").unwrap().status, 404);
    assert_eq!(c.request("GET", "/cfg", b"").unwrap().text(), "a=b");
    let m = admin(&mut c, "GET", "/_k10s/metrics", b"").unwrap().text();
    assert!(m.contains("k10s_service_requests_total{service=\"logger\"} 2"), "{m}");
    assert!(m.contains("k10s_rss_bytes"));
    let st = admin(&mut c, "GET", "/_k10s/status", b"").unwrap().text();
    assert!(st.starts_with('{') && st.ends_with('}'));
    assert!(st.contains("\"name\":\"cfg\""));
    assert_eq!(admin(&mut c, "GET", "/_k10s/nope", b"").unwrap().status, 404);
    assert_eq!(admin(&mut c, "GET", "/_k10s/apply", b"").unwrap().status, 405);
    assert_eq!(c.request("GET", "/_k10s/health", b"").unwrap().text(), "ok\n");
    // Unsigned (metrics too: it is open only on a dedicated --admin listener) or signed for another body.
    for p in ["/_k10s/status", "/_k10s/metrics", "/_k10s/logs/logger"] {
        assert_eq!(client(&node).request("GET", p, b"").unwrap().status, 401, "{p}");
    }
    let key = k10_mesh::admin_auth::AdminKey::parse(ADMIN_KEY).unwrap();
    let mut nonce = [7u8; 16];
    nonce[0] = 1;
    let aud = client(&node).audience().unwrap();
    let auth = key.authorization(&aud, "POST", "/_k10s/apply", k10_node::sys::wall_ms(), &nonce, b"signed");
    let mut s = TcpStream::connect(node.http_addr()).unwrap();
    write!(s, "POST /_k10s/apply HTTP/1.1\r\nauthorization: {auth}\r\ncontent-length: 6\r\n\r\nforged").unwrap();
    let mut resp = String::new();
    s.read_to_string(&mut resp).unwrap();
    assert!(resp.starts_with("HTTP/1.1 401") && resp.contains("body does not match"), "{resp}");
    // apply over HTTP: garbage, then a real bundle, then a stale version.
    let r = admin(&mut c, "POST", "/_k10s/apply", b"garbage").unwrap();
    assert_eq!(r.status, 400, "{}", r.text());
    let b = bundle(vec![svc("echo", &demo::echo())]);
    let r = admin(&mut c, "POST", "/_k10s/apply", &b.encode()).unwrap();
    assert_eq!(r.status, 200, "{}", r.text());
    assert!(r.text().contains("\"added\":[\"echo\"]"), "{}", r.text());
    assert!(r.text().contains("\"removed\":[\"cfg\",\"logger\"]"), "{}", r.text());
    let mut stale = bundle(vec![svc("echo", &demo::echo())]);
    stale.manifest.version = 1;
    let r = admin(&mut c, "POST", "/_k10s/apply", &stale.encode()).unwrap();
    assert_eq!(r.status, 409, "{}", r.text());
    // Invalid module.
    let mut bad = bundle(vec![svc("echo", &demo::echo())]);
    bad.blobs[0].1 = b"K10M\x01\x0cjunk".to_vec();
    bad.manifest.services[0].code_hash = k10_mesh::sha256::sha256(&bad.blobs[0].1);
    bad.blobs[0].0 = bad.manifest.services[0].code_hash;
    let r = admin(&mut c, "POST", "/_k10s/apply", &bad.encode()).unwrap();
    assert_eq!(r.status, 422, "{}", r.text());
    assert_eq!(c.request("GET", "/echo", b"").unwrap().text(), "hello\n");
    node.shutdown();
}

#[test]
fn status_codes_and_big_bodies() {
    let node = start(config(2));
    node.apply(bundle(vec![svc("st", &demo::status_echo()), svc("echo", &demo::echo()).mem(21)])).unwrap();
    let mut c = client(&node);
    assert_eq!(c.request("POST", "/st", b"201").unwrap().status, 201);
    assert_eq!(c.request("POST", "/st", b"404").unwrap().status, 404);
    assert_eq!(c.request("POST", "/st", b"42").unwrap().status, 500);
    let big: Vec<u8> = (0..1_000_000u32).map(|i| (i % 251) as u8).collect();
    let r = c.request("POST", "/echo", &big).unwrap();
    assert_eq!(r.status, 200);
    assert!(r.body == big);
    // Over the 16 MiB limit: 413 and the connection is closed.
    let mut s = TcpStream::connect(node.http_addr()).unwrap();
    s.write_all(b"POST /echo HTTP/1.1\r\ncontent-length: 20000000\r\n\r\n").unwrap();
    let mut out = String::new();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.read_to_string(&mut out).unwrap();
    assert!(out.starts_with("HTTP/1.1 413"), "{out}");
    node.shutdown();
}

fn raw_exchange(addr: std::net::SocketAddr, req: &[u8]) -> String {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.write_all(req).unwrap();
    let mut out = Vec::new();
    let _ = s.read_to_end(&mut out);
    String::from_utf8_lossy(&out).into_owned()
}

#[test]
fn http_protocol_edges() {
    let node = start(k10_node::NodeConfig { header_timeout: Duration::from_secs(1), ..config(2) });
    node.apply(bundle(vec![svc("echo", &demo::echo()), svc("path", &demo::path_echo())])).unwrap();
    let addr = node.http_addr();
    // Pipelining: three requests in one write, answered in order; the last one closes.
    let out = raw_exchange(
        addr,
        b"GET /path/1 HTTP/1.1\r\n\r\nPOST /echo HTTP/1.1\r\nContent-Length: 3\r\n\r\nabcGET /path/3 HTTP/1.1\r\nConnection: close\r\n\r\n",
    );
    let p1 = out.find("path:/1").unwrap();
    let p2 = out.find("abc").unwrap();
    let p3 = out.find("path:/3").unwrap();
    assert!(p1 < p2 && p2 < p3, "{out}");
    assert_eq!(out.matches("HTTP/1.1 200 OK").count(), 3);
    // Chunked bodies are refused.
    let out = raw_exchange(addr, b"POST /echo HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n");
    assert!(out.starts_with("HTTP/1.1 411"), "{out}");
    assert_eq!(out.matches("HTTP/1.1").count(), 1);
    // Malformed request line.
    assert!(raw_exchange(addr, b"BLAH\r\n\r\n").starts_with("HTTP/1.1 400"));
    // HTTP/1.0 closes after the response; HEAD has no body.
    let out = raw_exchange(addr, b"GET /echo HTTP/1.0\r\n\r\n");
    assert!(out.ends_with("hello\n") && out.contains("connection: close"), "{out}");
    let out = raw_exchange(addr, b"HEAD /echo HTTP/1.1\r\nConnection: close\r\n\r\n");
    assert!(out.contains("content-length: 6") && !out.contains("hello"), "{out}");
    // Partial input, byte by byte.
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_nodelay(true).unwrap();
    for b in b"POST /echo HTTP/1.1\r\nContent-Length: 5\r\n\r\nhello".iter() {
        s.write_all(&[*b]).unwrap();
    }
    let mut c = Client::connect(&addr.to_string(), Duration::from_secs(5)).unwrap();
    let _ = c.request("GET", "/echo", b"").unwrap();
    let mut buf = [0u8; 256];
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let n = s.read(&mut buf).unwrap();
    assert!(String::from_utf8_lossy(&buf[..n]).ends_with("\r\n\r\nhello"));
    // Expect: 100-continue.
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.write_all(b"POST /echo HTTP/1.1\r\nContent-Length: 4\r\nExpect: 100-continue\r\n\r\n").unwrap();
    let n = s.read(&mut buf).unwrap();
    assert!(String::from_utf8_lossy(&buf[..n]).starts_with("HTTP/1.1 100 Continue"));
    s.write_all(b"body").unwrap();
    let n = s.read(&mut buf).unwrap();
    assert!(String::from_utf8_lossy(&buf[..n]).ends_with("body"));
    // Slowloris: an incomplete head is cut off after the header timeout.
    let mut s = TcpStream::connect(addr).unwrap();
    s.write_all(b"GET /echo HTTP/1.1\r\nX-Slow: ").unwrap();
    let t = Instant::now();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut rest = Vec::new();
    let _ = s.read_to_end(&mut rest);
    assert!(t.elapsed() >= Duration::from_millis(900) && t.elapsed() < Duration::from_secs(4), "{:?}", t.elapsed());
    // Many keep-alive requests on one connection.
    let mut c = Client::connect(&addr.to_string(), Duration::from_secs(5)).unwrap();
    for i in 0..1000 {
        let body = format!("req{i}");
        assert_eq!(c.request("POST", "/echo", body.as_bytes()).unwrap().text(), body);
    }
    node.shutdown();
}

#[test]
fn shutdown_drains_in_flight_requests() {
    let node = start(config(2));
    node.apply(bundle(vec![svc("burn", &demo::burn()).config("300")])).unwrap();
    let addr = node.http_addr().to_string();
    let h = std::thread::spawn(move || Client::connect(&addr, Duration::from_secs(10)).unwrap().request("GET", "/burn", b""));
    std::thread::sleep(Duration::from_millis(100));
    let t = Instant::now();
    node.shutdown();
    let r = h.join().unwrap().unwrap();
    assert_eq!((r.status, r.text().as_str(), r.close), (200, "done", true));
    assert!(t.elapsed() >= Duration::from_millis(150) && t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
}

#[test]
fn pipelined_requests_respect_backpressure() {
    let node = start(config(1));
    node.apply(bundle(vec![svc("echo", &demo::echo()).mem(21)])).unwrap();
    // 200 pipelined 64 KiB echoes written before reading anything: the node must not buffer all responses.
    let mut s = TcpStream::connect(node.http_addr()).unwrap();
    let body = vec![b'x'; 64 << 10];
    let mut req = format!("POST /echo HTTP/1.1\r\ncontent-length: {}\r\n\r\n", body.len()).into_bytes();
    req.extend_from_slice(&body);
    let writer = {
        let mut s = s.try_clone().unwrap();
        std::thread::spawn(move || {
            for _ in 0..200 {
                if s.write_all(&req).is_err() {
                    break;
                }
            }
        })
    };
    std::thread::sleep(Duration::from_millis(300));
    let st = status(&node);
    assert!(stat_from(&st, "rss_bytes") < 200 << 20, "{st}");
    let mut total = 0usize;
    let mut buf = vec![0u8; 1 << 20];
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let want = 200 * (body.len() + 40);
    while total < want - 200 * 40 {
        let n = s.read(&mut buf).unwrap();
        assert!(n > 0);
        total += n;
    }
    writer.join().unwrap();
    node.shutdown();
}

#[test]
fn stress_calls_through_small_pools_across_shards_with_reapply() {
    let node = start(k10_node::NodeConfig { call_timeout: Duration::from_secs(10), ..config(4) });
    let mk = |cfg: &str| {
        let front = svc("front", &demo::caller("back")).calls(&["back"]).max(8);
        bundle(vec![front, svc("back", &demo::burn()).config(cfg).max(3)])
    };
    node.apply(mk("1")).unwrap();
    let addr = node.http_addr().to_string();
    let ok = AtomicU64::new(0);
    std::thread::scope(|s| {
        for _ in 0..32 {
            s.spawn(|| {
                let mut c = Client::connect(&addr, Duration::from_secs(30)).unwrap();
                for _ in 0..30 {
                    let r = c.request("POST", "/front", b"").unwrap();
                    assert_eq!((r.status, r.text().as_str()), (200, "done"));
                    ok.fetch_add(1, Relaxed);
                }
            });
        }
        // Re-apply new versions of `back` while requests are queued on it.
        for v in ["2", "1", "2"] {
            std::thread::sleep(Duration::from_millis(80));
            node.apply(mk(v)).unwrap();
        }
    });
    assert_eq!(ok.load(Relaxed), 32 * 30);
    let s = status(&node);
    assert!(svc_stat(&s, "back", "peak") <= 3, "{s}");
    assert!(stat_from(&s, "queued_served") > 0, "{s}");
    // Everything drains back to idle.
    let t = Instant::now();
    while stat_from(&status(&node), "fibers_busy") != 0 {
        assert!(t.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(10));
    }
    node.shutdown();
}

#[test]
fn many_idle_keepalive_connections_stay_cheap() {
    let node = start(config(2));
    node.apply(bundle(vec![svc("echo", &demo::echo())])).unwrap();
    let addr = node.http_addr().to_string();
    let _ = get(&node, "/echo");
    let before = stat_from(&status(&node), "rss_bytes");
    let mut clients: Vec<Client> = Vec::new();
    for _ in 0..1000 {
        let mut c = Client::connect(&addr, Duration::from_secs(10)).unwrap();
        assert_eq!(c.request("GET", "/echo", b"").unwrap().status, 200);
        clients.push(c);
    }
    let s = status(&node);
    assert_eq!(stat_from(&s, "connections_open"), 1000);
    let grown = stat_from(&s, "rss_bytes") - before;
    assert!(grown < 8 << 20, "1000 idle connections cost {grown} bytes of RSS");
    // They all still work.
    for c in clients.iter_mut().step_by(97) {
        assert_eq!(c.request("POST", "/echo", b"again").unwrap().text(), "again");
    }
    node.shutdown();
}

/// The cluster hook: a service placed on another node is handed to the installed router.
#[test]
fn remote_placement_goes_through_the_router() {
    use k10_node::router::{Completion, RemoteRouter};
    use k10_node::service::Placement;
    struct Fake;
    impl RemoteRouter for Fake {
        fn forward(&self, service: &str, nodes: &[k10_mesh::NodeId], req: k10_node::host::Request, _d: u64, done: Completion) {
            let body = format!("{service}@{}:{}", nodes.len(), String::from_utf8_lossy(&req.path));
            std::thread::spawn(move || done.complete(203, body.into_bytes()));
        }
    }
    let node = k10_node::Node::start_with_router(config(2), std::sync::Arc::new(Fake)).unwrap();
    let b = bundle(vec![svc("far", &demo::echo()), svc("near", &demo::caller("far")).calls(&["far"])]);
    node.shared()
        .apply_with(b, false, &|s| if s.name == "far" { Placement::Remote { nodes: vec![k10_mesh::NodeId(7)] } } else { Placement::Local })
        .unwrap();
    let r = get(&node, "/far/x");
    assert_eq!((r.status, r.text().as_str()), (203, "far@1:/x"));
    let r = get(&node, "/near/y");
    assert_eq!((r.status, r.text().as_str()), (203, "far@1:/y"));
    node.shutdown();
}

/// Cluster-mode apply: `(version, hash)` ordering like gossip's desired-state pointer, and placement-only updates that
/// replace exactly the services whose placement changed.
#[test]
fn apply_cluster_orders_by_version_and_hash_and_updates_placement_only() {
    use k10_node::service::Placement;
    let node = start(config(1));
    let sh = node.shared();
    let rt = |name: &str| sh.current_table().get(name).map(|s| s.id).unwrap();
    let mk = |version: u64, greeting: &str| {
        let mut b = bundle(vec![svc("a", &demo::echo()), svc("b", &demo::config_echo()).config(greeting)]);
        b.manifest.version = version;
        b
    };
    let local = |_: &k10_mesh::ServiceSpec| Placement::Local;
    assert!(sh.apply_cluster(mk(0, "x"), &local).is_err(), "cluster applies carry a version");
    let r = sh.apply_cluster(mk(3, "x"), &local).unwrap();
    assert_eq!(r.added, vec!["a", "b"]);
    let (a0, b0) = (rt("a"), rt("b"));
    // Same manifest, same placement: nothing replaced.
    let r = sh.apply_cluster(mk(3, "x"), &local).unwrap();
    assert!(r.updated.is_empty() && r.added.is_empty());
    assert_eq!((rt("a"), rt("b")), (a0, b0));
    // Same manifest, `b` moves away: only `b` is replaced, and it is routed through the (default) router.
    let far = |s: &k10_mesh::ServiceSpec| {
        if s.name == "b" { Placement::Remote { nodes: vec![k10_mesh::NodeId(9)] } } else { Placement::Local }
    };
    let r = sh.apply_cluster(mk(3, "x"), &far).unwrap();
    assert_eq!(r.updated, vec!["b"]);
    assert_eq!(rt("a"), a0);
    assert_ne!(rt("b"), b0);
    assert_eq!(get(&node, "/b").status, 503, "single-node router: remote services are unavailable");
    assert_eq!(get(&node, "/a").text(), "hello\n");
    // Lower version: stale. Same version, another manifest: the larger hash wins.
    assert!(sh.apply_cluster(mk(2, "x"), &far).unwrap_err().starts_with("stale"));
    let (p, q) = (mk(4, "p"), mk(4, "q"));
    let (lo, hi) = if p.manifest.hash() < q.manifest.hash() { (p, q) } else { (q, p) };
    let hi_cfg = String::from_utf8(hi.manifest.services[1].config.clone()).unwrap();
    sh.apply_cluster(lo.clone(), &local).unwrap();
    sh.apply_cluster(hi, &local).unwrap();
    assert!(sh.apply_cluster(lo, &local).unwrap_err().starts_with("stale"));
    assert_eq!(get(&node, "/b").text(), hi_cfg);
    node.shutdown();
}

// ------------------------------------------------------------------------------------------------------------------
// Memory reservations (`k10_mesh::memory::MemBudget`)
// ------------------------------------------------------------------------------------------------------------------

/// A field of the node-wide `"memory"` status object.
fn mem_stat(s: &str, key: &str) -> i64 {
    stat_from(&s[s.find("\"memory\":{").expect("memory section")..], key)
}

/// Memory of one `2^log2` instance (slot + tail page).
fn inst_bytes(log2: u8) -> u64 {
    (1u64 << log2) + 4096
}

/// `n` concurrent GETs of `path`; returns (status, body) of each.
fn concurrent_gets(node: &k10_node::Node, path: &str, n: usize) -> Vec<(u16, String)> {
    let addr = node.http_addr().to_string();
    std::thread::scope(|s| {
        let hs: Vec<_> = (0..n)
            .map(|_| {
                let (addr, path) = (addr.clone(), path.to_string());
                s.spawn(move || {
                    let r = Client::connect(&addr, Duration::from_secs(10)).unwrap().request("GET", &path, b"").unwrap();
                    (r.status, r.text())
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    })
}

/// A bursting hog cannot take another service's reservation (before reservations, `echo` then got 503).
#[test]
fn memory_reservations_keep_services_serving_while_a_hog_bursts() {
    let hog = inst_bytes(20);
    let limit = 12 * hog; // exactly 12 hog instances
    let node = start(k10_node::NodeConfig { mem_limit: limit, idle_timeout: Duration::from_secs(3600), ..config(2) });
    node.apply(bundle(vec![svc("hog", &demo::burn()).config("100").mem(20).max(64), svc("echo", &demo::echo())])).unwrap();
    let s = status(&node);
    assert_eq!(mem_stat(&s, "mem_reserved") as u64, hog + inst_bytes(16), "{s}");
    assert_eq!(mem_stat(&s, "mem_shared_limit") as u64, limit - hog - inst_bytes(16));
    assert_eq!(svc_stat(&s, "echo", "mem_reserved") as u64, inst_bytes(16));
    let echo_during_burst = std::thread::scope(|sc| {
        let burst = sc.spawn(|| concurrent_gets(&node, "/hog", 40));
        // Wait until the hog holds its reservation and the whole shared pool.
        let t = Instant::now();
        while svc_stat(&status(&node), "hog", "live") < 11 {
            assert!(t.elapsed() < Duration::from_secs(10), "hog did not burst: {}", status(&node));
            std::thread::sleep(Duration::from_millis(2));
        }
        let r = get(&node, "/echo");
        let all = burst.join().unwrap();
        // Requests beyond the hog's memory wait for one of its instances.
        assert!(all.iter().all(|(st, b)| *st == 200 && b == "done"), "{all:?}");
        (r.status, r.text())
    });
    assert_eq!(echo_during_burst, (200, "hello\n".to_string()));
    let s = status(&node);
    assert_eq!(svc_stat(&s, "hog", "created"), 11, "1 reserved + 10 from the shared pool: {s}");
    assert!(mem_stat(&s, "mem_refused") > 0);
    assert!(mem_stat(&s, "mem_committed") as u64 <= limit);
    assert_eq!(svc_stat(&s, "hog", "mem_shared") as u64, 10 * hog);
    let metrics = k10_node::admin::metrics(node.shared());
    assert!(metrics.contains(&format!("k10s_memory_reserved_bytes {}", hog + inst_bytes(16))), "{metrics}");
    assert!(metrics.contains(&format!("k10s_service_memory_reserved_bytes{{service=\"echo\"}} {}", inst_bytes(16))));
    node.shutdown();
}

/// An apply whose reservations exceed `--mem` is rejected (422); the node keeps serving what it had.
#[test]
fn over_committed_apply_is_rejected() {
    let node = start(k10_node::NodeConfig { mem_limit: 1 << 20, ..config(1) });
    let many = |n: usize| bundle((0..n).map(|i| svc(&format!("s{i}"), &demo::echo())).collect());
    // 15 × (64 KiB + 4 KiB) = 1020 KiB fit in 1 MiB; 16 do not.
    node.apply(many(15)).unwrap();
    let err = node.apply(many(16)).unwrap_err();
    assert!(err.contains("memory reservations do not fit") && err.contains("--mem"), "{err}");
    let r = admin(&mut client(&node), "POST", "/_k10s/apply", &many(16).encode()).unwrap();
    assert_eq!(r.status, 422, "{}", r.text());
    assert!(r.text().contains("16 service(s)"), "{}", r.text());
    let s = status(&node);
    assert_eq!(stat_from(&s[s.find("\"manifest\":{").unwrap()..], "version"), 1, "{s}");
    assert_eq!(mem_stat(&s, "mem_reserved") as u64, 15 * inst_bytes(16));
    assert_eq!(get(&node, "/s14").text(), "hello\n");
    assert_eq!(get(&node, "/s15").status, 404);
    // Cluster mode never refuses: everything is installed over-committed (one instance each, no burst).
    let mut b = many(16);
    b.manifest.version = 7;
    node.shared().apply_cluster(b, &|_| k10_node::service::Placement::Local).unwrap();
    let s = status(&node);
    assert_eq!(mem_stat(&s, "mem_reserved") as u64, 16 * inst_bytes(16));
    assert_eq!(mem_stat(&s, "mem_overcommit") as u64, 16 * inst_bytes(16) - (1 << 20));
    for i in 0..16 {
        assert_eq!(get(&node, &format!("/s{i}")).text(), "hello\n");
    }
    node.shutdown();
}

/// A persisted bundle that no longer fits (smaller `--mem`) is reloaded over-committed and reported, not dropped;
/// an unreadable one is reported in status and metrics.
#[test]
fn reload_that_does_not_fit_runs_degraded_and_reload_errors_are_visible() {
    let dir = tmpdir("reload-degraded");
    let cfg = |mem: u64| k10_node::NodeConfig { data_dir: Some(dir.clone()), mem_limit: mem, ..config(1) };
    let many = bundle((0..16).map(|i| svc(&format!("s{i}"), &demo::echo())).collect());
    {
        let node = start(cfg(64 << 20));
        node.apply(many).unwrap();
        node.shutdown();
    }
    let node = start(cfg(1 << 20));
    let s = status(&node);
    assert_eq!(mem_stat(&s, "mem_overcommit") as u64, 16 * inst_bytes(16) - (1 << 20), "{s}");
    assert!(s.contains("\"reload_error\":null"), "{s}");
    for i in 0..16 {
        assert_eq!(get(&node, &format!("/s{i}")).text(), "hello\n");
    }
    let m = k10_node::admin::metrics(node.shared());
    assert!(m.contains(&format!("k10s_memory_overcommit_bytes {}", 16 * inst_bytes(16) - (1 << 20))), "{m}");
    // A fitting apply ends the degraded state.
    node.apply(bundle((0..8).map(|i| svc(&format!("s{i}"), &demo::echo())).collect())).unwrap();
    assert_eq!(mem_stat(&status(&node), "mem_overcommit"), 0);
    node.shutdown();
    std::fs::write(dir.join("bundle.k10b"), b"garbage").unwrap();
    let node = start(cfg(1 << 20));
    let s = status(&node);
    assert!(s.contains("\"reload_error\":\"invalid"), "{s}");
    assert!(k10_node::admin::metrics(node.shared()).contains("k10s_reload_failed 1"));
    node.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

/// A request queued for memory never waits on nothing: when the last instance goes and no memory is left for a
/// replacement, its waiters fail at once (503), not at their deadline.
#[test]
fn waiters_fail_fast_when_the_last_instance_goes_without_memory() {
    use k10_node::service::Placement;
    let (hog, s_bytes) = (inst_bytes(20), inst_bytes(16));
    // The hog's reservation, 2 hog bursts and one instance of `s` (placed elsewhere: no reservation).
    let node = start(k10_node::NodeConfig { mem_limit: 3 * hog + s_bytes, ..config(2) });
    let sh = node.shared().clone();
    let mk = |v: u64, with_e: bool| {
        let mut svcs = vec![svc("hog", &demo::burn()).config("1500").mem(20).cpu(10_000), svc("s", &demo::burn()).cpu(10_000)];
        if with_e {
            svcs.push(svc("e", &demo::echo()));
        }
        let mut b = bundle(svcs);
        b.manifest.version = v;
        b
    };
    let place = |s: &k10_mesh::ServiceSpec| {
        if s.name == "s" { Placement::Remote { nodes: vec![k10_mesh::NodeId(9)] } } else { Placement::Local }
    };
    sh.apply_cluster(mk(1, false), &place).unwrap();
    let serve = |body: &str| {
        let (tx, rx) = std::sync::mpsc::channel();
        let req = k10_node::host::Request { method: 0, path: Vec::new(), body: body.as_bytes().to_vec() };
        let deadline = k10_node::sys::mono_ns() + 30_000_000_000;
        sh.serve_local("s".into(), req, deadline, Box::new(move |r| tx.send((r.status, Instant::now())).unwrap()));
        rx
    };
    let wait = |what: &str, f: &dyn Fn(&str) -> bool| {
        let t = Instant::now();
        while !f(&status(&node)) {
            assert!(t.elapsed() < Duration::from_secs(10), "{what}: {}", status(&node));
            std::thread::sleep(Duration::from_millis(2));
        }
    };
    std::thread::scope(|sc| {
        let hogs = sc.spawn(|| concurrent_gets(&node, "/hog", 3));
        wait("hog holds its reservation and 2 bursts", &|s| svc_stat(s, "hog", "live") == 3);
        let x1 = serve("400");
        wait("s holds the last of the pool", &|s| svc_stat(s, "s", "live") == 1);
        let x2 = serve("10");
        wait("x2 queued for memory", &|s| svc_stat(s, "s", "queue") == 1);
        // A new reservation over-commits the node: `s`'s burst instance goes when x1 ends, with no replacement.
        sh.apply_cluster(mk(2, true), &place).unwrap();
        let (st1, t1) = x1.recv().unwrap();
        let (st2, t2) = x2.recv_timeout(Duration::from_secs(5)).expect("the waiter failed at once, not at its deadline");
        assert_eq!((st1, st2), (200, -2));
        assert!(t2.duration_since(t1) < Duration::from_secs(1));
        assert!(hogs.join().unwrap().iter().all(|(st, _)| *st == 200));
    });
    assert_eq!(get(&node, "/e").text(), "hello\n");
    node.shutdown();
}

/// Reservations added while burst instances fill the budget serve at once; burst instances are given back.
#[test]
fn reservations_added_while_the_pool_is_full_take_effect_at_once() {
    let hog = inst_bytes(20);
    let limit = 12 * hog;
    let node = start(k10_node::NodeConfig { mem_limit: limit, idle_timeout: Duration::from_secs(3600), ..config(2) });
    let hog_svc = || svc("hog", &demo::burn()).config("50").mem(20).max(64);
    node.apply(bundle(vec![hog_svc()])).unwrap();
    let all = concurrent_gets(&node, "/hog", 24);
    assert!(all.iter().all(|(st, _)| *st == 200), "{all:?}");
    let s = status(&node);
    assert_eq!(svc_stat(&s, "hog", "live"), 12, "{s}");
    assert_eq!(mem_stat(&s, "mem_committed") as u64, limit);
    // `echo`'s reservation does not fit next to 12 idle hog instances: it serves anyway…
    node.apply(bundle(vec![hog_svc(), svc("echo", &demo::echo())])).unwrap();
    assert_eq!(get(&node, "/echo").text(), "hello\n");
    // …and idle burst instances are destroyed until the node is within budget.
    let t = Instant::now();
    loop {
        let s = status(&node);
        if mem_stat(&s, "mem_committed") as u64 <= limit {
            assert_eq!(svc_stat(&s, "hog", "live"), 11, "{s}");
            break;
        }
        assert!(t.elapsed() < Duration::from_secs(5), "still over-committed: {s}");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(get(&node, "/hog").text(), "done");
    node.shutdown();
}

/// 32-bit address space: a service larger than the largest slot (2^28) is refused at apply instead of wrapping its
/// size; the largest slot serves.
#[cfg(target_pointer_width = "32")]
#[test]
fn memory_beyond_the_largest_slot_is_refused() {
    let node = start(config(1));
    let max = k10_node::slot::MAX_SLOT_LOG2;
    let e = node.apply(bundle(vec![svc("big", &demo::echo()).mem(max + 1)])).unwrap_err();
    assert!(e.contains("larger than the largest slot"), "{e}");
    node.apply(bundle(vec![svc("big", &demo::echo()).mem(max)])).unwrap();
    assert_eq!(get(&node, "/big").status, 200);
    node.shutdown();
}
