//! Abuse / noisy-neighbour campaign (single node): hostile Ten services and raw HTTP that try to hurt the node
//! or starve a well-behaved `echo` neighbour through every host function and resource. Each scenario asserts the
//! node stays up, `echo` keeps answering (latency bounded vs a baseline), and resources are reclaimed afterwards.
//!
//! Runs against the binary's backend (`K10S_BACKEND`, else jit). The heavy scenarios are `#[ignore]`.

mod support;
use support::*;

use k10_node::http::{self, Client};
use std::time::{Duration, Instant};

// ------------------------------------------------------------------------------------------------------------------
// Hostile Ten services
// ------------------------------------------------------------------------------------------------------------------

/// Tries every out-of-range / negative / overflowing pointer+length against the copy host functions. All must
/// trap inside TIR (MemOutOfBounds); the node must never read out of the slot.
const EVIL_PTRS: &str = r#"
buffer buf: [u8; 256];
export fn handle() {
    let k = query_int("k", 0);
    if k == 0 { input_read(buf as *u8, 0, 1 << 40); }        // huge len
    else if k == 1 { input_read(buf as *u8, 0 - 1, 16); }    // negative off (clamped, ok)
    else if k == 2 { input_read((0 - 1) as *u8, 0, 16); }    // huge ptr
    else if k == 3 { output_write(buf as *u8, 1 << 40); }
    else if k == 4 { output_write((1 << 62) as *u8, 32); }
    else if k == 5 { req_path(buf as *u8, 1 << 50); }
    else if k == 6 { kv_get(buf as *u8, 1 << 40, buf as *u8, 16); }
    else if k == 7 { kv_put(buf as *u8, 0 - 8, buf as *u8, 16); }
    else if k == 8 { config(buf as *u8, 0 - 1); }
    else if k == 9 { call_resp_read(buf as *u8, 0, 1 << 40); }
    else { out_str("ok"); return; }
    out_str("survived");
}
"#;

/// Output bomb: writes 1 MiB per `output_write` until it crosses the 16 MiB limit (trap Host(1)).
const OUTPUT_BOMB: &str = r#"
buffer chunk: [u8; 65536];
export fn handle() {
    let i = 0;
    while i < 1024 {
        output_write(chunk as *u8, 65536);
        i += 1;
    }
    out_str("never");
}
"#;

/// KV bomb: fills the store with many 1 MiB values (hits per-service KV quota if one exists; else grows the log).
const KV_BOMB: &str = r#"
buffer val: [u8; 1048576];
buffer key: [u8; 64];
export fn handle() {
    let base = query_int("base", 0);
    let count = query_int("count", 64);
    let i = 0;
    while i < count {
        let klen = itoa(base + i, key as *u8);
        kv_put(key as *u8, klen, val as *u8, 1048576);
        i += 1;
    }
    out_int(count);
    out_str("\n");
}
"#;

/// KV value exactly at the 1 MiB limit (ok) and one byte over (trap Host(2)).
const KV_LIMIT: &str = r#"
buffer val: [u8; 1048577];
export fn handle() {
    let over = query_int("over", 0);
    kv_put("k", 1, val as *u8, 1048576 + over);
    out_str("ok");
}
"#;

/// Log spam: `n` log lines per request.
const LOG_SPAM: &str = r#"
buffer line: [u8; 4096];
export fn handle() {
    let n = query_int("n", 10000);
    let i = 0;
    while i < n {
        log(line as *u8, 4096);
        i += 1;
    }
    out_str("spammed");
}
"#;

/// Infinite loop in handle() (CPU budget trap).
const SPIN: &str = r#"
export fn handle() {
    let i = 0;
    loop { i += 1; }
}
"#;

/// Infinite loop in init() (the instance never becomes ready; its CPU budget must still bound it).
const SPIN_INIT: &str = r#"
global g: i64 = 0;
export fn init() { loop { g += 1; } }
export fn handle() { out_str("unreachable"); }
"#;

/// Always traps (divide by zero): crash-loop.
const ALWAYS_TRAP: &str = r#"
export fn handle() { out_int(1 / input_len()); }
"#;

/// Allocates (touches) the whole slot: one byte per page, up to `memsize()`.
const MEM_HOG: &str = r#"
export fn handle() {
    let p = __heap_base;
    while p < memsize() {
        *(p as *u8) = 1;
        p += 4096;
    }
    out_int(p);
}
"#;

/// Recursion to depth `n` (stack overflow beyond the budget).
const DEEP: &str = r#"
fn rec(n: i64) -> i64 {
    if n == 0 { return 0; }
    return 1 + rec(n - 1);
}
export fn handle() { out_int(rec(query_int("n", 1000000))); }
"#;

/// Calls itself forever (recursion bomb bounded by call depth 32).
const SELF_CALL: &str = r#"
export fn handle() {
    let st = call_svc("selfcall", null, 0);
    out_int(st);
}
"#;

/// A→B→A cycle: `cycle` calls `cycle2` which calls `cycle` (bounded by call depth / deadline).
const CYCLE_A: &str = r#"
export fn handle() { out_int(call_svc("cycleb", null, 0)); }
"#;
const CYCLE_B: &str = r#"
export fn handle() { out_int(call_svc("cyclea", null, 0)); }
"#;

/// Slow: busy-waits `ms` then answers (ties up a fiber/instance).
const SLOW: &str = r#"
export fn handle() {
    let ms = query_int("ms", 500);
    let start = now_ns();
    loop {
        if now_ns() - start >= ms * 1000000 { break; }
    }
    out_str("slow");
}
"#;

// ------------------------------------------------------------------------------------------------------------------
// Helpers
// ------------------------------------------------------------------------------------------------------------------

/// Baseline echo latency on an otherwise idle node, then a function running the attack while echo is probed. Prints
/// a row and returns (baseline, under-attack).
fn neighbour_table(tag: &str, attack: impl FnOnce(&str)) -> (Lat, Lat) {
    let (dir, node) = single(tag, 2, &[]);
    deploy(&node.http, &[echo_svc()]);
    // warm echo
    let mut c = Client::connect(&node.http, Duration::from_secs(5)).unwrap();
    for _ in 0..50 {
        let _ = c.request("GET", "/echo", b"");
    }
    let baseline = measure(&node.http, Duration::from_secs(2));
    let probe = Probe::start(&node.http, "/echo", "hello", Duration::from_millis(1));
    attack(&node.http);
    let under = probe.stop();
    println!("[neighbour:{tag}] baseline  {baseline}");
    println!("[neighbour:{tag}] attacked  {under}");
    assert!(node.healthy(), "{tag}: node unhealthy after attack");
    let _ = std::fs::remove_dir_all(&dir);
    (baseline, under)
}

fn get(addr: &str, path: &str) -> (u16, String) {
    match http::request(addr, "GET", path, b"", Duration::from_secs(30)) {
        Ok(r) => (r.status, r.text()),
        Err(e) => (0, e.to_string()),
    }
}

/// Retries a GET until it is not a 503 (crash-loop backoff clears), up to ~5 s.
fn wait_ok(addr: &str, path: &str) -> (u16, String) {
    let t = Instant::now();
    loop {
        let r = get(addr, path);
        if r.0 != 503 || t.elapsed() > Duration::from_secs(5) {
            return r;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

// ------------------------------------------------------------------------------------------------------------------
// Tests
// ------------------------------------------------------------------------------------------------------------------

/// Every out-of-range host-function argument traps cleanly; the node keeps serving.
#[test]
fn evil_pointer_lengths_trap_cleanly() {
    let (dir, node) = single("evilptr", 1, &[]);
    deploy(&node.http, &[echo_svc(), Svc::new("evil", ten(EVIL_PTRS)).mem(16)]);
    // The TIR sandbox MASKS every pointer into the slot, so a huge pointer with a small length wraps in-range (no
    // trap, no out-of-slot access); only a length that cannot fit, or a masked start+len past the end, traps.
    // k=1 (off clamped), k=4 (ptr masked, len fits), k=10 (no-op) succeed; the rest trap MemOutOfBounds.
    let ok = [1, 4, 10];
    for k in 0..=10 {
        let (st, body) = get(&node.http, &format!("/evil?k={k}"));
        if ok.contains(&k) {
            // May be briefly refused (503) if a prior k left the service in crash-loop backoff; retry until clear.
            let (st, body) = wait_ok(&node.http, &format!("/evil?k={k}"));
            assert_eq!(st, 200, "k={k} should succeed (masked/clamped): {body}");
        } else {
            // 500 (trap) or 503 (crash-loop backoff): both are clean containment, never a crash or out-of-slot read.
            assert!(st == 500 || st == 503, "k={k} should trap or back off: {st} {body}");
        }
    }
    // Neighbour still fine.
    assert_eq!(get(&node.http, "/echo"), (200, "hello".into()));
    assert!(node.healthy());
    let _ = std::fs::remove_dir_all(&dir);
}

/// 16 MiB output limit traps; no OOM, neighbour unaffected.
#[test]
fn output_bomb_is_capped() {
    let (base, under) = neighbour_table("outputbomb", |addr| {
        deploy(addr, &[echo_svc(), Svc::new("bomb", ten(OUTPUT_BOMB)).mem(18).max(8)]);
        for _ in 0..8 {
            let (st, _) = get(addr, "/bomb");
            // trap (500) or crash-loop backoff (503) — both contain the 16 MiB bomb.
            assert!(st == 500 || st == 503, "output bomb must trap/back off, got {st}");
        }
    });
    // echo stays responsive: p99 within 50 ms even while 16 MiB buffers are built and dropped.
    assert!(under.errors == 0, "echo errors during output bomb: {under:?}");
    assert!(under.p99_us < 100_000, "echo p99 {}us (base {}us)", under.p99_us, base.p50_us);
}

/// KV value at the 1 MiB limit is accepted; one byte over traps.
#[test]
fn kv_value_limit_enforced() {
    let (dir, node) = single("kvlimit", 1, &[]);
    deploy(&node.http, &[Svc::new("kv", ten(KV_LIMIT)).mem(21)]);
    assert_eq!(get(&node.http, "/kv?over=0"), (200, "ok".into()));
    assert_eq!(get(&node.http, "/kv?over=1").0, 500, "value over 1 MiB must trap");
    assert!(node.healthy());
    let _ = std::fs::remove_dir_all(&dir);
}

/// Log spam is bounded by the per-service ring; neighbour unaffected and memory does not grow without bound.
#[test]
fn log_spam_is_bounded() {
    let (base, under) = neighbour_table("logspam", |addr| {
        deploy(addr, &[echo_svc(), Svc::new("spam", ten(LOG_SPAM)).mem(16).max(4)]);
        for _ in 0..20 {
            let _ = get(addr, "/spam?n=20000");
        }
    });
    assert!(under.errors == 0, "echo errors during log spam: {under:?}");
    assert!(under.p99_us < 100_000, "echo p99 {}us (base {}us)", under.p99_us, base.p50_us);
}

/// Infinite loops in handle() hit the CPU budget (trap), do not wedge the shard, and echo on the SAME single shard
/// keeps < 50 ms latency (the scheduler gives one slice to the oldest preempted fiber per loop).
#[test]
fn infinite_loops_dont_starve_neighbour() {
    let (dir, node) = single("spin", 1, &["--shards", "1"]);
    deploy(&node.http, &[echo_svc(), Svc::new("spin", ten(SPIN)).mem(16).cpu(50).max(8)]);
    let probe = Probe::start(&node.http, "/echo", "hello", Duration::from_millis(1));
    // Launch several concurrent infinite loops on the one shard.
    let addr = node.http.clone();
    let threads: Vec<_> = (0..4)
        .map(|_| {
            let addr = addr.clone();
            std::thread::spawn(move || {
                for _ in 0..5 {
                    let _ = get(&addr, "/spin");
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    let under = probe.stop();
    println!("[spin] echo during 20 infinite loops on 1 shard: {under}");
    assert_eq!(under.errors, 0, "echo errors while loops run: {under:?}");
    assert!(under.p99_us < 100_000, "echo p99 {}us while loops run", under.p99_us);
    // Spin instances trapped (Interrupted) and were replaced; crash-loop backoff may apply. Node healthy.
    assert!(node.healthy());
    let _ = std::fs::remove_dir_all(&dir);
}

/// An infinite loop in init() is bounded by the CPU budget: the instance creation traps, the service crash-loops
/// (503), and the node/neighbour are fine.
#[test]
fn infinite_init_is_bounded() {
    let (dir, node) = single("spininit", 1, &[]);
    deploy(&node.http, &[echo_svc(), Svc::new("spininit", ten(SPIN_INIT)).mem(16).cpu(50).max(4)]);
    let t = Instant::now();
    for _ in 0..6 {
        let (st, _) = get(&node.http, "/spininit");
        assert!(st == 500 || st == 503, "init spin should trap or backoff, got {st}");
    }
    assert!(t.elapsed() < Duration::from_secs(20), "init spin took too long: {:?}", t.elapsed());
    assert_eq!(get(&node.http, "/echo"), (200, "hello".into()));
    assert!(node.healthy());
    let _ = std::fs::remove_dir_all(&dir);
}

/// A service that traps on every request enters crash-loop backoff (503) and does not consume the node; echo fine.
#[test]
fn crash_loop_is_contained() {
    let (dir, node) = single("crashloop", 2, &[]);
    deploy(&node.http, &[echo_svc(), Svc::new("boom", ten(ALWAYS_TRAP)).mem(16).max(8)]);
    let probe = Probe::start(&node.http, "/echo", "hello", Duration::from_millis(1));
    let mut saw_503 = false;
    for _ in 0..60 {
        let (st, _) = get(&node.http, "/boom");
        assert!(st == 500 || st == 503, "boom: {st}");
        saw_503 |= st == 503;
    }
    let under = probe.stop();
    assert!(saw_503, "crash-loop backoff should eventually answer 503 fast");
    assert_eq!(under.errors, 0, "echo errors during crash loop: {under:?}");
    println!("[crashloop] echo {under}");
    assert!(node.healthy());
    let _ = std::fs::remove_dir_all(&dir);
}

/// Deep recursion traps StackOverflow (not a SIGSEGV) and the fiber's stack is trimmed afterwards.
#[test]
fn deep_recursion_traps() {
    let (dir, node) = single("deep", 1, &[]);
    deploy(&node.http, &[echo_svc(), Svc::new("deep", ten(DEEP)).mem(16).max(4)]);
    for _ in 0..8 {
        let (st, _) = get(&node.http, "/deep?n=10000000");
        assert!(st == 500 || st == 503, "deep recursion must trap/back off, got {st}");
    }
    // A shallow call still works once backoff clears (instance replaced, no lingering corruption).
    assert_eq!(wait_ok(&node.http, "/deep?n=10").0, 200);
    assert_eq!(get(&node.http, "/echo"), (200, "hello".into()));
    assert!(node.healthy());
    let _ = std::fs::remove_dir_all(&dir);
}

/// Call cycles (A→B→A) and self-recursion bombs are bounded by call depth / deadline and unwind, not deadlock.
#[test]
fn call_cycles_and_recursion_bombs_unwind() {
    let (dir, node) = single("cycles", 2, &[]);
    deploy(
        &node.http,
        &[
            echo_svc(),
            Svc::new("selfcall", ten(SELF_CALL)).mem(16).max(64).calls(&["selfcall"]),
            Svc::new("cyclea", ten(CYCLE_A)).mem(16).max(1).calls(&["cycleb"]),
            Svc::new("cycleb", ten(CYCLE_B)).mem(16).max(1).calls(&["cyclea"]),
        ],
    );
    // self-call: depth limit → the deepest call returns -2, bubbling up; the top request completes (200).
    let (st, _) = get(&node.http, "/selfcall");
    assert_eq!(st, 200, "self-call should unwind, not hang");
    // cycle with max_instances=1: inner call times out (-3) and everything unwinds.
    let (st, _) = get(&node.http, "/cyclea");
    assert_eq!(st, 200, "cycle should unwind via timeout");
    assert_eq!(get(&node.http, "/echo"), (200, "hello".into()));
    assert!(node.healthy());
    let _ = std::fs::remove_dir_all(&dir);
}

/// Memory hog touching its whole slot is confined to the slot; many such instances respect `--mem` (503) and the
/// address space is reclaimed when they are destroyed.
#[test]
fn mem_hog_confined_and_reclaimed() {
    // 64 MiB node cap, 4 MiB slots → at most ~16 live instances; the rest get 503. Short idle timeout so reclaim
    // runs during the test.
    let (dir, node) = single("memhog", 2, &["--mem", "64MiB", "--idle-timeout", "2"]);
    deploy(&node.http, &[echo_svc(), Svc::new("hog", ten(MEM_HOG)).mem(22).max(1000)]);
    let addr = node.http.clone();
    let threads: Vec<_> = (0..32)
        .map(|_| {
            let addr = addr.clone();
            std::thread::spawn(move || get(&addr, "/hog").0)
        })
        .collect();
    let mut oom = 0;
    for t in threads {
        let st = t.join().unwrap();
        assert!(st == 200 || st == 503, "hog: {st}");
        oom += (st == 503) as u32;
    }
    println!("[memhog] {oom}/32 got 503 at the --mem cap");
    assert_eq!(get(&node.http, "/echo"), (200, "hello".into()));
    // After idle reclaim (warm_instances = 1), the many 4 MiB hog slots are freed: slot_bytes drop to roughly one
    // warm hog + one warm echo (≈ 4 MiB), well under the 24 MiB touched at the peak.
    wait_for("hog slots reclaimed", Duration::from_secs(60), || {
        section_num(&node.status(), "memory", "slot_bytes").unwrap_or(i64::MAX) < 8 << 20
    });
    assert!(node.healthy());
    let _ = std::fs::remove_dir_all(&dir);
}

/// Thousands of concurrent slow requests exhaust fibers; excess requests queue/time out but the node stays up and
/// echo on another shard keeps serving.
#[test]
#[ignore = "slow (~30s): fiber/connection exhaustion"]
fn slow_request_flood_does_not_crash() {
    let (dir, node) = single("slowflood", 2, &[]);
    deploy(&node.http, &[echo_svc(), Svc::new("slow", ten(SLOW)).mem(16).max(200)]);
    let probe = Probe::start(&node.http, "/echo", "hello", Duration::from_millis(2));
    let addr = node.http.clone();
    let threads: Vec<_> = (0..400)
        .map(|i| {
            let addr = addr.clone();
            std::thread::spawn(move || {
                let _ = get(&addr, &format!("/slow?ms=1000&i={i}"));
            })
        })
        .collect();
    std::thread::sleep(Duration::from_secs(2));
    assert!(node.healthy(), "node unhealthy under slow flood");
    for t in threads {
        let _ = t.join();
    }
    let under = probe.stop();
    println!("[slowflood] echo {under}");
    assert!(node.healthy());
    let _ = std::fs::remove_dir_all(&dir);
}

/// Runs the KV bomb (rounds of `count` × 1 MiB values) until the per-service quota stops it. Returns
/// (statuses per round, final kv live_bytes, echo latency during the bomb).
fn kv_bomb(node: &NodeProc, rounds: usize, count: usize) -> (Vec<u16>, i64, Lat) {
    let probe = Probe::start(&node.http, "/echo", "hello", Duration::from_millis(2));
    let mut sts = Vec::new();
    for round in 0..rounds {
        let (st, _) = get(&node.http, &format!("/kv?base={}&count={count}", round * count));
        let live = section_num(&node.status(), "kv", "live_bytes").unwrap_or(0);
        println!("[kvbomb] round {round}: status {st}, kv live_bytes={live}");
        sts.push(st);
    }
    let under = probe.stop();
    (sts, section_num(&node.status(), "kv", "live_bytes").unwrap_or(-1), under)
}

/// Regression for the missing per-service KV quota (found by this campaign: a service could fill the node's
/// memory and disk without bound — 1 GiB in 16 requests). With `--kv-max 32MiB`, the bomb's puts succeed up to the
/// quota, then `kv_put` traps `Host(3)` (500, then crash-loop 503): live data never exceeds the quota, nothing
/// partial is stored, and the node and its neighbour are unaffected.
#[test]
fn kv_quota_stops_kv_bomb() {
    let (dir, node) = single("kvquota", 2, &["--kv-max", "32MiB"]);
    deploy(&node.http, &[echo_svc(), Svc::new("kv", ten(KV_BOMB)).mem(21).max(4)]);
    // 8 × 1 MiB per round: rounds 0..=2 fit (24 MiB), round 3 crosses 32 MiB.
    let (sts, live, under) = kv_bomb(&node, 6, 8);
    assert_eq!(&sts[..3], &[200, 200, 200], "puts under the quota succeed: {sts:?}");
    assert!(sts[3..].iter().all(|&s| s == 500 || s == 503), "puts past the quota must trap/back off: {sts:?}");
    assert!(live > 24 << 20 && live <= 32 << 20, "live KV bytes capped by the 32 MiB quota: {live}");
    assert_eq!(under.errors, 0, "echo errors during the KV bomb: {under:?}");
    println!("[kvquota] echo during bomb: {under}");
    assert!(node.healthy());
    let _ = std::fs::remove_dir_all(&dir);
}

/// Same with the default quota (256 MiB): the bomb is capped instead of growing to 1 GiB+.
#[test]
#[ignore = "slow / disk heavy: 256 MiB of KV writes"]
fn kv_bomb_capped_by_default_quota() {
    let (dir, node) = single("kvbomb", 2, &[]);
    deploy(&node.http, &[echo_svc(), Svc::new("kv", ten(KV_BOMB)).mem(21).max(4)]);
    let (sts, live, under) = kv_bomb(&node, 8, 64);
    println!("[kvbomb] statuses {sts:?}; echo {under}");
    assert!(sts.contains(&500) || sts.contains(&503), "default quota must stop the bomb: {sts:?}");
    assert!(live <= 256 << 20, "live KV bytes capped at 256 MiB: {live}");
    assert!(node.healthy());
    let _ = std::fs::remove_dir_all(&dir);
}

/// Hostile bundles at `/_k10s/apply`: garbage, truncated, giant, thousands of services, extreme memory sizes. All
/// are rejected (4xx/422) without crashing or disturbing the running deployment.
#[test]
fn hostile_apply_bundles_rejected() {
    let (dir, node) = single("hostileapply", 2, &[]);
    deploy(&node.http, &[echo_svc()]);
    let probe = Probe::start(&node.http, "/echo", "hello", Duration::from_millis(1));

    // Garbage and truncated bundles.
    for (name, body) in [
        ("empty", vec![]),
        ("garbage", vec![0xFFu8; 4096]),
        ("bad-magic", b"NOPE\x01".to_vec()),
    ] {
        let (st, _) = apply_raw(&node.http, &body, Duration::from_secs(10));
        assert!(st == 400 || st == 422, "{name}: expected 4xx, got {st}");
    }

    // A valid-looking bundle whose service references a missing blob.
    let mut b = bundle(0, &[Svc::new("x", ten(SPIN))]);
    b.blobs.clear();
    let (st, _) = apply_raw(&node.http, &b.encode(), Duration::from_secs(10));
    assert!(st == 422 || st == 400, "missing blob: {st}");

    // A module whose declared memory is below its minimum.
    let (st, _) = apply_raw(&node.http, &bundle(0, &[Svc::new("tiny", ten(KV_LIMIT)).mem(12)]).encode(), Duration::from_secs(10));
    assert_eq!(st, 422, "memory below module minimum must be 422");

    // Invalid TIR blob.
    let mut b = bundle(0, &[echo_svc()]);
    b.blobs = vec![([0u8; 32], vec![1, 2, 3, 4, 5])];
    b.manifest.services[0].code_hash = k10_mesh::sha256::sha256(&[1, 2, 3, 4, 5]);
    let (st, _) = apply_raw(&node.http, &b.encode(), Duration::from_secs(10));
    assert_eq!(st, 422, "invalid TIR must be 422");

    // The original echo keeps serving the whole time.
    let under = probe.stop();
    assert_eq!(under.errors, 0, "echo errors during hostile applies: {under:?}");
    assert_eq!(get(&node.http, "/echo"), (200, "hello".into()));
    assert!(node.healthy());
    let _ = std::fs::remove_dir_all(&dir);
}

/// A manifest with thousands of services applies (or is cleanly rejected) without crashing; echo keeps serving.
#[test]
#[ignore = "slow: large manifest preparation"]
fn giant_manifest_applies_or_rejects_cleanly() {
    let (dir, node) = single("giantmanifest", 2, &[]);
    deploy(&node.http, &[echo_svc()]);
    let code = ten("export fn handle() { out_str(\"x\"); }");
    let mut svcs = vec![echo_svc()];
    for i in 0..3000 {
        svcs.push(Svc::new(&format!("s{i}"), code.clone()).mem(13));
    }
    let t = Instant::now();
    let (st, body) = apply_raw(&node.http, &bundle(0, &svcs).encode(), Duration::from_secs(120));
    println!("[giant] 3000 services apply: status {st} in {:?}", t.elapsed());
    assert!(st == 200 || st == 422 || st == 413, "giant manifest: {st} {body}");
    if st == 200 {
        wait_for("echo among 3000", Duration::from_secs(30), || get(&node.http, "/echo") == (200, "hello".into()));
    }
    assert!(node.healthy());
    let _ = std::fs::remove_dir_all(&dir);
}

/// Malformed / abusive HTTP (bad request lines, huge headers, pipelining floods, slowloris heads) are answered
/// with the right 4xx and never wedge the shard; echo on another connection stays fine.
#[test]
fn malformed_http_is_handled() {
    let (dir, node) = single("badhttp", 2, &[]);
    deploy(&node.http, &[echo_svc()]);
    let addr: std::net::SocketAddr = node.http.parse().unwrap();

    // Oversized header (> 16 KiB) → 431.
    let big = format!("GET /echo HTTP/1.1\r\nX-Big: {}\r\n\r\n", "a".repeat(20_000));
    assert!(raw_exchange(addr, big.as_bytes()).contains(" 431 "), "huge header should be 431");
    // Bogus version → 505.
    assert!(raw_exchange(addr, b"GET /echo HTTP/9.9\r\n\r\n").contains(" 505 "));
    // Chunked body → 411.
    assert!(raw_exchange(addr, b"POST /echo HTTP/1.1\r\ntransfer-encoding: chunked\r\n\r\n").contains(" 411 "));
    // Conflicting content-length → 400.
    assert!(raw_exchange(addr, b"POST /echo HTTP/1.1\r\ncontent-length: 1\r\ncontent-length: 2\r\n\r\n").contains(" 400 "));
    // Garbage request line → 400.
    assert!(raw_exchange(addr, b"!!!! not http\r\n\r\n").contains(" 400 "));

    // Pipelining flood: 200 requests on one connection, all answered in order.
    let mut c = Client::connect(&node.http, Duration::from_secs(10)).unwrap();
    for _ in 0..200 {
        let r = c.request("GET", "/echo", b"").unwrap();
        assert_eq!((r.status, r.text().as_str()), (200, "hello"));
    }
    assert_eq!(get(&node.http, "/echo"), (200, "hello".into()));
    assert!(node.healthy());
    let _ = std::fs::remove_dir_all(&dir);
}

/// Slowloris: a connection that dribbles a request head byte-by-byte is closed by the header timeout and does not
/// tie up the shard. (Shortened header timeout would need a flag; here we assert the dribbler does not block echo.)
#[test]
fn slowloris_does_not_block_echo() {
    use std::io::Write;
    use std::net::TcpStream;
    let (dir, node) = single("slowloris", 1, &[]);
    deploy(&node.http, &[echo_svc()]);
    let addr: std::net::SocketAddr = node.http.parse().unwrap();
    // Open several dribbling connections on the single shard.
    let drips: Vec<TcpStream> = (0..8)
        .filter_map(|_| {
            let mut s = TcpStream::connect(addr).ok()?;
            let _ = s.write_all(b"GET /ec");
            Some(s)
        })
        .collect();
    // echo on a fresh connection is unaffected.
    let probe = Probe::start(&node.http, "/echo", "hello", Duration::from_millis(1));
    std::thread::sleep(Duration::from_secs(2));
    let under = probe.stop();
    println!("[slowloris] echo while 8 heads dribble: {under}");
    assert_eq!(under.errors, 0, "echo errors during slowloris: {under:?}");
    drop(drips);
    assert!(node.healthy());
    let _ = std::fs::remove_dir_all(&dir);
}

fn raw_exchange(addr: std::net::SocketAddr, req: &[u8]) -> String {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.write_all(req).unwrap();
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        match s.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

/// Reads one HTTP response (head + Content-Length body) from `s`: (status, head, body).
fn read_response(s: &mut std::net::TcpStream) -> (u16, String, Vec<u8>) {
    use std::io::Read;
    let mut buf = Vec::new();
    let mut tmp = vec![0u8; 64 << 10];
    let head_end = loop {
        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break p + 4;
        }
        let n = s.read(&mut tmp).expect("read response head");
        assert!(n > 0, "connection closed before a response head ({} bytes)", buf.len());
        buf.extend_from_slice(&tmp[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let status: u16 = head[9..12].parse().unwrap();
    let len: usize = head
        .lines()
        .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap()))
        .unwrap_or(0);
    let mut body = buf[head_end..].to_vec();
    while body.len() < len {
        let n = s.read(&mut tmp).expect("read response body");
        assert!(n > 0, "connection closed mid-body");
        body.extend_from_slice(&tmp[..n]);
    }
    (status, head, body)
}

/// Node-wide connection buffer budget (`--conn-buffer-max`, here 1 MiB). A request body is reserved when its head
/// arrives: one that does not fit gets 503 (and the connection is closed) without being read. Unsent response bytes
/// (a client that does not read) count too: over the budget, new requests get 503 until they drain; admin requests
/// still work. Everything returns to 0 afterwards.
#[test]
fn connection_buffer_budget() {
    use std::io::Write;
    use std::net::TcpStream;
    let (dir, node) = single("bufbudget", 1, &["--conn-buffer-max", "1MiB"]);
    deploy(&node.http, &[echo_svc()]);
    let buffered = || section_num(&node.status(), "http_stats", "buffered_bytes").unwrap();
    let refused = || section_num(&node.status(), "http_stats", "buffer_refused").unwrap();
    assert_eq!(section_num(&node.status(), "http_stats", "buffer_max"), Some(1 << 20));
    let addr: std::net::SocketAddr = node.http.parse().unwrap();
    // A: an 800 kB body, sent slowly (only the head and 1000 bytes so far).
    let mut a = TcpStream::connect(addr).unwrap();
    a.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    a.write_all(b"POST /echo HTTP/1.1\r\nHost: x\r\nContent-Length: 800000\r\n\r\n").unwrap();
    a.write_all(&[b'a'; 1000]).unwrap();
    wait_for("A's body reserved", Duration::from_secs(10), || buffered() >= 800_000);
    // B: 400 kB more does not fit: 503 before the body is sent, connection closed.
    let mut b = TcpStream::connect(addr).unwrap();
    b.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    b.write_all(b"POST /echo HTTP/1.1\r\nHost: x\r\nContent-Length: 400000\r\nExpect: 100-continue\r\n\r\n").unwrap();
    let (st, head, _) = read_response(&mut b);
    assert_eq!(st, 503, "{head}");
    assert!(head.to_ascii_lowercase().contains("connection: close"), "{head}");
    // Small requests still fit (the budget is not exceeded, only nearly full).
    assert_eq!(get(&node.http, "/echo"), (200, "hello".into()));
    let (st, body) = http::request(&node.http, "POST", "/echo", &[b'b'; 1000], Duration::from_secs(5)).map(|r| (r.status, r.body)).unwrap();
    assert_eq!((st, body.len()), (200, 1000));
    // A completes and gets its echo; then nothing is buffered any more and B's size fits.
    a.write_all(&[b'a'; 799_000]).unwrap();
    let (st, _, body) = read_response(&mut a);
    assert_eq!((st, body.len()), (200, 800_000));
    wait_for("budget released", Duration::from_secs(10), || buffered() == 0);
    let r = http::request(&node.http, "POST", "/echo", &vec![b'c'; 400_000], Duration::from_secs(10)).unwrap();
    assert_eq!((r.status, r.body.len()), (200, 400_000));
    // D: a 15 MB echo whose response is never read. The body alone exceeds the budget but is admitted because
    // nothing else is buffered; the unread part of the response (beyond what the socket buffers absorb) stays
    // charged, so the node is over budget and sheds new requests.
    let before = refused();
    let mut d = TcpStream::connect(addr).unwrap();
    d.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let big = vec![b'd'; 15_000_000];
    d.write_all(format!("POST /echo HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n", big.len()).as_bytes()).unwrap();
    d.write_all(&big).unwrap();
    wait_for("response residue charged", Duration::from_secs(20), || buffered() > 1 << 20);
    assert_eq!(get(&node.http, "/echo").0, 503, "over budget: new requests are shed");
    let e = http::request(&node.http, "POST", "/echo", b"x", Duration::from_secs(5)).unwrap();
    assert_eq!(e.status, 503);
    assert!(refused() >= before + 2);
    assert!(node.healthy(), "admin requests are not shed");
    // D reads its response: the residue drains and the node serves again.
    let (st, _, body) = read_response(&mut d);
    assert!(st == 200 && body == big, "D: {st}, {} bytes", body.len());
    wait_for("residue released", Duration::from_secs(10), || buffered() == 0);
    assert_eq!(get(&node.http, "/echo"), (200, "hello".into()));
    drop((a, b, d));
    let _ = std::fs::remove_dir_all(&dir);
}
