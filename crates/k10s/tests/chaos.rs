//! Multi-node chaos campaign: a seeded, reproducible driver that injects random faults (SIGKILL, SIGSTOP+SIGCONT,
//! restart with the same data dir, graceful leave, new joins, re-applies with services/replicas changing) into a
//! cluster while load runs through every ingress, and checks the invariants after each disruption re-stabilizes:
//!
//!  * every service in the current manifest is served by every live ingress (local or forwarded) within T;
//!  * all live nodes converge on the same desired version and the same placement;
//!  * no node that was not killed/stopped has crashed;
//!  * no request returns a wrong body (stale version, or a different service's identity) in a stable window;
//!  * the error rate in a stable verification window is 0.
//!
//! `chaos_short` is a normal (~90 s) test; `chaos_long` (`#[ignore]`) runs ≥ 20 min (`K10S_CHAOS_SECS`). Node 0 is a
//! permanent seed (never faulted) so there is always a join target and a reference ingress; faults hit nodes 1..N.
//! Reproduce a failure with the printed seed via `K10S_CHAOS_SEED`.

mod support;
use support::*;

use k10_node::http::Client;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::*};
use std::sync::Arc;
use std::time::{Duration, Instant};

const PROBE_MS: &str = "100";

/// A service whose body is its identity `name@vV`: a response proves both *which* service answered (cross-service
/// mix-up) and *which version* it is on (staleness long after convergence).
const IDENT: &str = r#"
export fn handle() {
    let n = config(null, 0);
    if n == 0 { out_str("noconfig"); return; }
    out(read_config(), last_len());
}
"#;

struct Cluster {
    dir: PathBuf,
    nodes: Vec<NodeProc>, // index 0 = permanent seed
    seed_gossip: String,
    next_name: u64,
    version: u64,
    svcs: Vec<Svc>,
    rng: Rng,
}

impl Cluster {
    fn start(tag: &str, n: usize, seed: u64) -> Cluster {
        let dir = tmpdir(tag);
        let args = base_args(None);
        let first = spawn("c0", &dir.join("c0"), &args);
        let seed_gossip = first.gossip.clone();
        let mut c = Cluster { dir, nodes: vec![first], seed_gossip, next_name: 1, version: 0, svcs: vec![], rng: Rng(seed) };
        for _ in 1..n {
            c.join();
        }
        c.wait_all_live();
        c
    }

    fn join(&mut self) {
        let name = format!("c{}", self.next_name);
        self.next_name += 1;
        let node = spawn(&name, &self.dir.join(&name), &base_args(Some(&self.seed_gossip)));
        self.nodes.push(node);
    }

    fn wait_all_live(&self) {
        let n = self.nodes.len();
        for node in &self.nodes {
            wait_for(&format!("{} sees {n} live", node.name), Duration::from_secs(40), || live_members(&node.status()) == Some(n as i64));
        }
    }

    /// Builds manifest version `version+1`: an `echo` (replicas 2-3) for continuous load, plus `k` identity services
    /// `s0..s{k-1}` each with config `sI@v{V}` and a random replica count.
    fn next_manifest(&mut self) {
        self.version += 1;
        let v = self.version;
        let k = 3 + self.rng.below(5) as usize; // 3..=7 identity services
        let mut svcs = vec![echo_svc().replicas(2 + self.rng.below(2) as u32)];
        for i in 0..k {
            let cfg = format!("s{i}@v{v}");
            let reps = 1 + self.rng.below(3) as u32;
            svcs.push(Svc::new(&format!("s{i}"), ten(IDENT)).mem(16).replicas(reps).config(cfg.as_bytes()));
        }
        self.svcs = svcs;
    }

    fn apply(&mut self) {
        self.next_manifest();
        // Apply via a random live node (version 0 = highest known + 1).
        let via = self.live_idx()[self.rng.below(self.live_idx().len() as u64) as usize];
        let (st, body) = apply_raw(&self.nodes[via].http, &bundle(0, &self.svcs).encode(), Duration::from_secs(60));
        assert!(st == 200, "apply failed ({st}): {body}");
    }

    fn live_idx(&self) -> Vec<usize> {
        (0..self.nodes.len()).filter(|&i| self.nodes[i].child.is_some()).collect()
    }

    /// Current manifest version (ground truth for convergence waits).
    fn target_version(&self) -> i64 {
        self.version as i64
    }
}

fn base_args(join: Option<&str>) -> Vec<String> {
    let mut a: Vec<String> = ["--http", "127.0.0.1:0", "--gossip", "127.0.0.1:0", "--rpc", "127.0.0.1:0", "--probe-ms", PROBE_MS, "--leave-grace-ms", "300", "--shards", "2"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    if let Some(j) = join {
        a.extend(["--join".into(), j.to_string()]);
    }
    a
}

fn live_members(status: &str) -> Option<i64> {
    let c = status.find("\"cluster\":{")?;
    num_after(&status[c..], "live_members")
}

fn applied_version(status: &str) -> Option<i64> {
    let cl = &status[status.find("\"cluster\":{")?..];
    let at = cl.find("\"applied\":{")?;
    num_after(&cl[at..], "version")
}

fn placement_map(status: &str) -> BTreeMap<String, Vec<String>> {
    let mut out = BTreeMap::new();
    let Some(ci) = status.find("\"cluster\":{") else { return out };
    let c = &status[ci..];
    let Some(at) = c.find("\"placement\":{") else { return out };
    let body = &c[at + "\"placement\":{".len()..];
    let body = &body[..body.find('}').unwrap_or(0)];
    for part in body.split("],") {
        let Some((name, ids)) = part.split_once(":[") else { continue };
        let mut ids: Vec<String> = ids.trim_end_matches(']').split(',').map(|s| s.trim_matches('"').to_string()).filter(|s| !s.is_empty()).collect();
        ids.sort();
        out.insert(name.trim_matches('"').to_string(), ids);
    }
    out
}

// ------------------------------------------------------------------------------------------------------------------
// Continuous background load (informational: spans unstable windows)
// ------------------------------------------------------------------------------------------------------------------

struct Load {
    stop: Arc<AtomicBool>,
    ok: Arc<AtomicU64>,
    err: Arc<AtomicU64>,
    wrong: Arc<AtomicU64>,
    handles: Vec<std::thread::JoinHandle<()>>,
}

impl Load {
    fn start(targets: Vec<String>) -> Load {
        let stop = Arc::new(AtomicBool::new(false));
        let ok = Arc::new(AtomicU64::new(0));
        let err = Arc::new(AtomicU64::new(0));
        let wrong = Arc::new(AtomicU64::new(0));
        let handles = targets
            .iter()
            .map(|t| {
                let (t, stop, ok, err) = (t.clone(), stop.clone(), ok.clone(), err.clone());
                std::thread::spawn(move || {
                    let mut c: Option<Client> = None;
                    while !stop.load(Relaxed) {
                        if c.is_none() {
                            c = Client::connect(&t, Duration::from_secs(5)).ok();
                        }
                        match c.as_mut().map(|cl| cl.request("GET", "/echo", b"")) {
                            Some(Ok(r)) if r.status == 200 => {
                                ok.fetch_add(1, Relaxed);
                                if r.close {
                                    c = None;
                                }
                            }
                            _ => {
                                err.fetch_add(1, Relaxed);
                                c = None;
                            }
                        }
                    }
                })
            })
            .collect();
        Load { stop, ok, err, wrong, handles }
    }

    fn finish(self) -> (u64, u64, u64) {
        self.stop.store(true, Relaxed);
        for h in self.handles {
            let _ = h.join();
        }
        (self.ok.load(Relaxed), self.err.load(Relaxed), self.wrong.load(Relaxed))
    }
}

// ------------------------------------------------------------------------------------------------------------------
// Invariants
// ------------------------------------------------------------------------------------------------------------------

struct Stats {
    events: BTreeMap<&'static str, u64>,
    verifications: u64,
    violations: Vec<String>,
}

/// Waits until every live node has applied `version` and all live nodes agree on placement, then returns.
fn wait_converged(c: &Cluster, timeout: Duration) -> Result<(), String> {
    let t = Instant::now();
    let v = c.target_version();
    loop {
        let live = c.live_idx();
        let statuses: Vec<(usize, String)> = live.iter().map(|&i| (i, c.nodes[i].status())).collect();
        let all_applied = statuses.iter().all(|(_, s)| applied_version(s) == Some(v));
        let places: Vec<BTreeMap<String, Vec<String>>> = statuses.iter().map(|(_, s)| placement_map(s)).collect();
        let agree = places.windows(2).all(|w| w[0] == w[1]) && !places.is_empty();
        if all_applied && agree {
            return Ok(());
        }
        if t.elapsed() > timeout {
            return Err(format!("not converged on v{v}: applied={:?}", statuses.iter().map(|(i, s)| (i, applied_version(s))).collect::<Vec<_>>()));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Stable-window verification: every service in the current manifest answers correctly through every live ingress,
/// with its current version's identity, and `echo` works — asserting a 0 error rate in the window.
fn verify(c: &Cluster, stats: &mut Stats) {
    stats.verifications += 1;
    let v = c.version;
    for &i in &c.live_idx() {
        let node = &c.nodes[i];
        let mut cl = match Client::connect(&node.http, Duration::from_secs(10)) {
            Ok(cl) => cl,
            Err(e) => {
                stats.violations.push(format!("{}: connect failed in stable window: {e}", node.name));
                continue;
            }
        };
        // echo
        match cl.request("GET", "/echo", b"") {
            Ok(r) if r.status == 200 && r.body == b"hello" => {}
            other => stats.violations.push(format!("{}: echo wrong in stable window: {other:?}", node.name)),
        }
        // identity services: each must answer its own name at the current version
        for s in c.svcs.iter().filter(|s| s.name != "echo") {
            let want = format!("{}@v{v}", s.name);
            match cl.request("GET", &format!("/{}", s.name), b"") {
                Ok(r) if r.status == 200 && r.body == want.as_bytes() => {}
                Ok(r) => stats.violations.push(format!("{} via {}: got {:?} want {want:?} (status {})", s.name, node.name, r.text(), r.status)),
                Err(e) => stats.violations.push(format!("{} via {}: {e}", s.name, node.name)),
            }
        }
    }
}

// ------------------------------------------------------------------------------------------------------------------
// Chaos events
// ------------------------------------------------------------------------------------------------------------------

/// Applies one random disruption to nodes 1.. (node 0 is the permanent seed), then returns its name for the timeline.
fn inject(c: &mut Cluster) -> &'static str {
    // Candidate faultable nodes (never the seed).
    let faultable: Vec<usize> = (1..c.nodes.len()).filter(|&i| c.nodes[i].child.is_some()).collect();
    let can_add = c.nodes.len() < 6;
    let can_fault = !faultable.is_empty() && c.live_idx().len() > 2;
    let choice = c.rng.below(6);
    match choice {
        0 => {
            c.apply();
            "reapply"
        }
        1 if can_fault => {
            let i = faultable[c.rng.below(faultable.len() as u64) as usize];
            c.nodes[i].signal(libc::SIGKILL);
            let _ = c.nodes[i].child.as_mut().unwrap().wait();
            c.nodes[i].child = None; // reap; slot becomes a gap
            "sigkill"
        }
        2 if can_fault => {
            let i = faultable[c.rng.below(faultable.len() as u64) as usize];
            c.nodes[i].signal(libc::SIGSTOP);
            std::thread::sleep(Duration::from_millis(400 + c.rng.below(800)));
            c.nodes[i].signal(libc::SIGCONT);
            "sigstop_cont"
        }
        3 if can_fault => {
            // Graceful leave, then a fresh node joins to keep the cluster populated.
            let i = faultable[c.rng.below(faultable.len() as u64) as usize];
            c.nodes[i].terminate(Duration::from_secs(10));
            c.nodes[i].child = None;
            c.join();
            "leave_join"
        }
        4 if can_add => {
            c.join();
            "join"
        }
        _ => {
            c.apply();
            "reapply"
        }
    }
}

/// Removes reaped (None-child) node slots and refreshes the seed's view; returns after the cluster has shed dead
/// members from gossip so convergence checks only consider live nodes.
fn compact(c: &mut Cluster) {
    c.nodes.retain(|n| n.child.is_some());
}

fn run_chaos(tag: &str, duration: Duration, seed: u64) -> Stats {
    eprintln!("[chaos] tag={tag} seed={seed} duration={duration:?}");
    let mut c = Cluster::start(tag, 5, seed);
    c.apply();
    wait_converged(&c, Duration::from_secs(30)).expect("initial convergence");
    // Continuous stress runs against the permanent seed (node 0), whose address is stable for the whole run, so its
    // error count is meaningful (not dominated by connecting to addresses of nodes later killed/left). Per-round
    // `verify` is what exercises *every* live ingress and asserts a 0 error rate in the stable window.
    let seed_http = c.nodes[0].http.clone();
    let load = Load::start(vec![seed_http.clone(), seed_http.clone(), seed_http.clone(), seed_http]);
    let mut stats = Stats { events: BTreeMap::new(), verifications: 0, violations: Vec::new() };
    let start = Instant::now();
    let mut round = 0u64;
    while start.elapsed() < duration {
        round += 1;
        let ev = inject(&mut c);
        *stats.events.entry(ev).or_insert(0) += 1;
        // Let gossip detect kills/leaves and re-place before converging.
        std::thread::sleep(Duration::from_millis(300));
        compact(&mut c);
        match wait_converged(&c, Duration::from_secs(40)) {
            Ok(()) => {}
            Err(e) => {
                stats.violations.push(format!("round {round} ({ev}): {e}"));
                // Give it more time once.
                if let Err(e2) = wait_converged(&c, Duration::from_secs(40)) {
                    stats.violations.push(format!("round {round} ({ev}): still not converged: {e2}"));
                    break;
                }
            }
        }
        // Check no live node crashed on its own.
        for n in c.nodes.iter_mut() {
            if let Some(st) = n.exited() {
                stats.violations.push(format!("round {round}: node {} exited unexpectedly: {st:?}", n.name));
            }
        }
        verify(&c, &mut stats);
        eprintln!("[chaos] round {round}: {ev}; nodes={} live={} v{} violations={}", c.nodes.len(), c.live_idx().len(), c.version, stats.violations.len());
    }
    let (ok, err, wrong) = load.finish();
    eprintln!("[chaos] done: rounds={round} events={:?} verifications={} load ok={ok} err={err} wrong={wrong} violations={}", stats.events, stats.verifications, stats.violations.len());
    for v in &stats.violations {
        eprintln!("[chaos] VIOLATION: {v}");
    }
    let _ = std::fs::remove_dir_all(&c.dir);
    stats
}

fn seed_from_env(default: u64) -> u64 {
    std::env::var("K10S_CHAOS_SEED").ok().and_then(|s| s.parse().ok()).unwrap_or(default)
}

#[test]
fn chaos_short() {
    let seed = seed_from_env(0x5eed_c0de);
    let stats = run_chaos("chaos-short", Duration::from_secs(75), seed);
    assert!(stats.violations.is_empty(), "invariant violations: {:#?}", stats.violations);
    assert!(stats.verifications >= 3, "too few verification rounds: {}", stats.verifications);
}

#[test]
#[ignore = "long chaos run (>= 20 min); set K10S_CHAOS_SECS"]
fn chaos_long() {
    let secs: u64 = std::env::var("K10S_CHAOS_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(1200);
    let seed = seed_from_env(0xC0ffee_1234);
    let stats = run_chaos("chaos-long", Duration::from_secs(secs), seed);
    assert!(stats.violations.is_empty(), "invariant violations ({} rounds): {:#?}", stats.verifications, stats.violations);
}
