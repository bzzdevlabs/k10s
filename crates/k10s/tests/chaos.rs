//! Multi-node chaos campaign: a seeded, reproducible driver injects random faults while load runs through every
//! live ingress, and checks invariants once each disruption re-stabilizes:
//!
//!  * live nodes see exactly each other as live, and converge on the same desired version and placement;
//!  * every service of the current manifest answers correctly (right identity and version) through every ingress;
//!  * no node crashed on its own, and restarted nodes keep their id;
//!  * 0 errors in the stable verification window.
//!
//! Faults hit every node, the oldest (seed) one more often: SIGKILL (restarted at once or later), SIGSTOP/SIGCONT,
//! graceful leave + rejoin, apply then SIGKILL of the node that took it, leave replaced by a fresh node, joins and
//! re-applies. Nodes start with `--join` of up to 3 live members; a restart reuses the data dir and, when free, the
//! same ports, joining through live members (operational rule), or with its first-run `--join` (static config, none
//! for `c0`) when a member it knew kept its address, sometimes on new ports (then it rejoins via `DATA/peers`).
//!
//! `chaos_short` runs ~90 s; `chaos_long` (`#[ignore]`) ≥ 20 min (`K10S_CHAOS_SECS`). Replay with `K10S_CHAOS_SEED`.

mod support;
use support::*;

use k10_node::http::Client;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::*};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const PROBE_MS: &str = "100";
/// Most nodes (live + down) at any time.
const MAX_NODES: usize = 6;
/// Fewest live nodes a fault may leave.
const MIN_LIVE: usize = 3;

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
    /// Nodes started and not replaced; `child == None` = down (restartable).
    nodes: Vec<NodeProc>,
    /// Node name → rpc address of its last run.
    rpc: BTreeMap<String, String>,
    /// Node name → node id of its first run.
    ids: BTreeMap<String, String>,
    /// Node name → `--join` of its first run (its static config).
    first_join: BTreeMap<String, Option<String>>,
    /// Node name → other live node → (its gossip address, live together since), as of the last converged round.
    together: BTreeMap<String, BTreeMap<String, (String, Instant)>>,
    /// Node name → when it went down.
    died: BTreeMap<String, Instant>,
    next_name: u64,
    version: u64,
    svcs: Vec<Svc>,
    rng: Rng,
}

impl Cluster {
    fn start(tag: &str, n: usize, seed: u64) -> Cluster {
        let dir = tmpdir(tag);
        let mut c = Cluster {
            dir,
            nodes: Vec::new(),
            rpc: BTreeMap::new(),
            ids: BTreeMap::new(),
            first_join: BTreeMap::new(),
            together: BTreeMap::new(),
            died: BTreeMap::new(),
            next_name: 0,
            version: 0,
            svcs: vec![],
            rng: Rng(seed),
        };
        for _ in 0..n {
            c.join_new();
        }
        c.wait_all_live();
        c
    }

    /// `--join` value: up to 3 random live members' gossip addresses, excluding node `except`.
    fn join_list(&mut self, except: Option<usize>) -> Option<String> {
        let mut live: Vec<usize> = self.live_idx().into_iter().filter(|&i| Some(i) != except).collect();
        let mut pick = Vec::new();
        while !live.is_empty() && pick.len() < 3 {
            let k = self.rng.below(live.len() as u64) as usize;
            pick.push(self.nodes[live.swap_remove(k)].gossip.clone());
        }
        (!pick.is_empty()).then(|| pick.join(","))
    }

    /// Starts `name` (data dir `dir/name`) on the given addresses (port 0 = any).
    fn spawn_node(&mut self, name: &str, http: &str, gossip: &str, rpc: &str, join: Option<String>) -> NodeProc {
        let args = base_args(http, gossip, rpc, join.as_deref());
        let n = spawn(name, &self.dir.join(name), &args);
        let st = n.status();
        if let Some(r) = cluster_str(&st, "rpc") {
            self.rpc.insert(name.to_string(), r);
        }
        n
    }

    /// A new node joins through several live members.
    fn join_new(&mut self) {
        let name = format!("c{}", self.next_name);
        self.next_name += 1;
        let join = self.join_list(None);
        let any = "127.0.0.1:0";
        let n = self.spawn_node(&name, any, any, any, join.clone());
        self.ids.insert(name.clone(), n.id.clone());
        self.first_join.insert(name, join);
        self.nodes.push(n);
    }

    /// Restarts down node `i` (same data dir): `--join` of live members (50%), else its static `--join` on the same
    /// ports (30%) or on new ones (20%: nobody can find it, it must rejoin by itself). Static variants need what the
    /// product guarantees for them: a member in its `DATA/peers` (live together > 1.5 s) still at the same address.
    fn restart(&mut self, i: usize) -> &'static str {
        assert!(self.nodes[i].child.is_none());
        let name = self.nodes[i].name.clone();
        let (http, gossip) = (self.nodes[i].http.clone(), self.nodes[i].gossip.clone());
        let rpc = self.rpc.get(&name).cloned().unwrap_or_else(|| "127.0.0.1:0".into());
        let any = "127.0.0.1:0".to_string();
        let roll = if self.has_peer_path(&name) { self.rng.below(10) } else { 0 };
        let (how, join) = match roll {
            0..=4 => ("", self.join_list(Some(i))),
            5..=7 => ("_static", self.first_join.get(&name).cloned().flatten()),
            _ => ("_static_moved", self.first_join.get(&name).cloned().flatten()),
        };
        let keep = how != "_static_moved" && ports_free(&http, &gossip, &rpc);
        let (http, gossip, rpc) = if keep { (http, gossip, rpc) } else { (any.clone(), any.clone(), any) };
        let n = self.spawn_node(&name, &http, &gossip, &rpc, join);
        assert_eq!(Some(&n.id), self.ids.get(&name), "{name} keeps its node id across restarts");
        self.nodes[i] = n;
        how
    }

    /// Some member `name` knew for over 1.5 s before going down is live at the same address.
    fn has_peer_path(&self, name: &str) -> bool {
        let (Some(t), Some(&died)) = (self.together.get(name), self.died.get(name)) else { return false };
        t.iter().any(|(x, (g, since))| {
            died.saturating_duration_since(*since) > Duration::from_millis(1500)
                && self.nodes.iter().any(|n| n.child.is_some() && &n.name == x && &n.gossip == g)
        })
    }

    /// Records which live nodes know each other (after a converged round).
    fn note_converged(&mut self) {
        let now = Instant::now();
        let live: Vec<(String, String)> = self.live_idx().iter().map(|&i| (self.nodes[i].name.clone(), self.nodes[i].gossip.clone())).collect();
        for (d, _) in &live {
            let old = self.together.remove(d).unwrap_or_default();
            let t = live
                .iter()
                .filter(|(x, _)| x != d)
                .map(|(x, g)| (x.clone(), (g.clone(), old.get(x).filter(|(og, _)| og == g).map_or(now, |(_, s)| *s))))
                .collect();
            self.together.insert(d.clone(), t);
        }
    }

    fn wait_all_live(&self) {
        let n = self.live_idx().len();
        for &i in &self.live_idx() {
            let node = &self.nodes[i];
            wait_for(&format!("{} sees {n} live", node.name), Duration::from_secs(40), || live_members(&node.status()) == Some(n as i64));
        }
    }

    /// Manifest `version+1`: `echo` (2-3 replicas) plus 3-7 identity services `sI` (config `sI@vV`, 1-3 replicas).
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

    /// Applies the next manifest through a random live node; returns its index.
    fn apply(&mut self) -> usize {
        self.next_manifest();
        // Version 0 = highest known + 1.
        let live = self.live_idx();
        let via = live[self.rng.below(live.len() as u64) as usize];
        let (st, body) = apply_raw(&self.nodes[via].http, &bundle(0, &self.svcs).encode(), Duration::from_secs(60));
        assert!(st == 200, "apply via {} failed ({st}): {body}", self.nodes[via].name);
        via
    }

    fn live_idx(&self) -> Vec<usize> {
        (0..self.nodes.len()).filter(|&i| self.nodes[i].child.is_some()).collect()
    }

    fn down_idx(&self) -> Vec<usize> {
        (0..self.nodes.len()).filter(|&i| self.nodes[i].child.is_none()).collect()
    }

    /// A live node to fault: the oldest node (in most `--join` lists) one time in three, else any.
    fn victim(&mut self) -> usize {
        let live = self.live_idx();
        if live[0] == 0 && self.rng.below(3) == 0 {
            return 0;
        }
        live[self.rng.below(live.len() as u64) as usize]
    }

    fn sigkill(&mut self, i: usize) {
        self.died.insert(self.nodes[i].name.clone(), Instant::now());
        self.nodes[i].signal(libc::SIGKILL);
        if let Some(mut ch) = self.nodes[i].child.take() {
            let _ = ch.wait();
        }
    }

    /// Current manifest version (ground truth for convergence waits).
    fn target_version(&self) -> i64 {
        self.version as i64
    }
}

fn base_args(http: &str, gossip: &str, rpc: &str, join: Option<&str>) -> Vec<String> {
    let mut a: Vec<String> = ["--http", http, "--gossip", gossip, "--rpc", rpc, "--probe-ms", PROBE_MS, "--leave-grace-ms", "300", "--shards", "2"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    if let Some(j) = join {
        a.extend(["--join".into(), j.to_string()]);
    }
    a
}

/// Nobody bound the addresses of a dead node since.
fn ports_free(http: &str, gossip: &str, rpc: &str) -> bool {
    std::net::TcpListener::bind(http).is_ok() && std::net::UdpSocket::bind(gossip).is_ok() && std::net::TcpListener::bind(rpc).is_ok()
}

fn cluster_part(status: &str) -> &str {
    status.find("\"cluster\":{").map(|c| &status[c..]).unwrap_or("")
}

/// First string field `key` of the cluster section.
fn cluster_str(status: &str, key: &str) -> Option<String> {
    let c = cluster_part(status);
    let k = format!("\"{key}\":\"");
    let at = c.find(&k)? + k.len();
    Some(c[at..at + c[at..].find('"')?].to_string())
}

fn live_members(status: &str) -> Option<i64> {
    num_after(cluster_part(status), "live_members")
}

fn applied_version(status: &str) -> Option<i64> {
    let cl = cluster_part(status);
    let at = cl.find("\"applied\":{")?;
    num_after(&cl[at..], "version")
}

fn placement_map(status: &str) -> BTreeMap<String, Vec<String>> {
    let mut out = BTreeMap::new();
    let c = cluster_part(status);
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

/// Ids of the alive or suspect members of a status document.
fn live_ids(status: &str) -> Vec<String> {
    let c = cluster_part(status);
    let Some(at) = c.find("\"members\":[") else { return vec![] };
    let body = &c[at..];
    let body = &body[..body.find(']').unwrap_or(0)];
    let mut v: Vec<String> = body
        .split("{\"id\":\"")
        .skip(1)
        .filter(|m| m.contains("\"state\":\"alive\"") || m.contains("\"state\":\"suspect\""))
        .map(|m| m[..m.find('"').unwrap_or(0)].to_string())
        .collect();
    v.sort();
    v
}

// ------------------------------------------------------------------------------------------------------------------
// Continuous background load (informational: spans unstable windows)
// ------------------------------------------------------------------------------------------------------------------

struct Load {
    stop: Arc<AtomicBool>,
    targets: Arc<Mutex<Vec<String>>>,
    ok: Arc<AtomicU64>,
    err: Arc<AtomicU64>,
    handles: Vec<std::thread::JoinHandle<()>>,
}

impl Load {
    /// `conns` closed-loop clients over the live ingresses.
    fn start(conns: usize, targets: Vec<String>) -> Load {
        let stop = Arc::new(AtomicBool::new(false));
        let targets = Arc::new(Mutex::new(targets));
        let ok = Arc::new(AtomicU64::new(0));
        let err = Arc::new(AtomicU64::new(0));
        let handles = (0..conns)
            .map(|k| {
                let (stop, targets, ok, err) = (stop.clone(), targets.clone(), ok.clone(), err.clone());
                std::thread::spawn(move || {
                    let mut c: Option<Client> = None;
                    let mut n = k;
                    while !stop.load(Relaxed) {
                        if c.is_none() {
                            let t = {
                                let ts = targets.lock().unwrap();
                                n += 1;
                                ts[n % ts.len()].clone()
                            };
                            c = Client::connect(&t, Duration::from_secs(2)).ok();
                            if c.is_none() {
                                err.fetch_add(1, Relaxed);
                                std::thread::sleep(Duration::from_millis(20));
                                continue;
                            }
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
        Load { stop, targets, ok, err, handles }
    }

    /// Points new connections at the current live ingresses.
    fn retarget(&self, targets: Vec<String>) {
        if !targets.is_empty() {
            *self.targets.lock().unwrap() = targets;
        }
    }

    fn finish(self) -> (u64, u64) {
        self.stop.store(true, Relaxed);
        for h in self.handles {
            let _ = h.join();
        }
        (self.ok.load(Relaxed), self.err.load(Relaxed))
    }
}

// ------------------------------------------------------------------------------------------------------------------
// Invariants
// ------------------------------------------------------------------------------------------------------------------

struct Stats {
    events: BTreeMap<String, u64>,
    verifications: u64,
    violations: Vec<String>,
}

/// Waits until live nodes see exactly each other, applied the current version and agree on placement.
fn wait_converged(c: &Cluster, timeout: Duration) -> Result<(), String> {
    let t = Instant::now();
    let v = c.target_version();
    let live = c.live_idx();
    let mut want: Vec<String> = live.iter().map(|&i| c.nodes[i].id.clone()).collect();
    want.sort();
    loop {
        let statuses: Vec<(usize, String)> = live.iter().map(|&i| (i, c.nodes[i].status())).collect();
        let members_ok = statuses.iter().all(|(_, s)| live_ids(s) == want);
        let all_applied = statuses.iter().all(|(_, s)| applied_version(s) == Some(v));
        let places: Vec<BTreeMap<String, Vec<String>>> = statuses.iter().map(|(_, s)| placement_map(s)).collect();
        let agree = places.windows(2).all(|w| w[0] == w[1]) && !places.is_empty();
        if members_ok && all_applied && agree {
            return Ok(());
        }
        if t.elapsed() > timeout {
            let view: Vec<String> = statuses
                .iter()
                .map(|(i, s)| format!("{}: applied {:?}, live {:?}", c.nodes[*i].name, applied_version(s), live_ids(s)))
                .collect();
            return Err(format!(
                "not converged on v{v} (members {members_ok}, applied {all_applied}, placement {agree}); want live {want:?}; {view:?}"
            ));
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

/// Prints the tail of every node's log.
fn dump_logs(c: &Cluster) {
    for n in &c.nodes {
        let log = n.log();
        let lines: Vec<&str> = log.lines().collect();
        let from = lines.len().saturating_sub(40);
        eprintln!("[chaos] ---- log of {} ({}) ----\n{}", n.name, if n.child.is_some() { "live" } else { "down" }, lines[from..].join("\n"));
    }
}

// ------------------------------------------------------------------------------------------------------------------
// Chaos events
// ------------------------------------------------------------------------------------------------------------------

/// Applies one random disruption; returns its name (`seed_` prefix: it hit the oldest node).
fn inject(c: &mut Cluster) -> String {
    let live = c.live_idx().len();
    let down = c.down_idx();
    // Keep enough nodes up: bring a down node back first.
    if live < MIN_LIVE && !down.is_empty() {
        let i = down[c.rng.below(down.len() as u64) as usize];
        let how = c.restart(i);
        return tag(i, &format!("restart{how}"));
    }
    let can_fault = live > MIN_LIVE || (live == MIN_LIVE && c.rng.below(2) == 0);
    let room = c.nodes.len() < MAX_NODES;
    match c.rng.below(9) {
        1 | 2 if can_fault => {
            // SIGKILL, then restart at once or leave it down for later rounds.
            let i = c.victim();
            c.sigkill(i);
            if c.rng.below(2) == 0 {
                std::thread::sleep(Duration::from_millis(c.rng.below(1500)));
                let how = c.restart(i);
                tag(i, &format!("sigkill_restart{how}"))
            } else {
                tag(i, "sigkill")
            }
        }
        3 if can_fault => {
            let i = c.victim();
            c.nodes[i].signal(libc::SIGSTOP);
            std::thread::sleep(Duration::from_millis(400 + c.rng.below(800)));
            c.nodes[i].signal(libc::SIGCONT);
            tag(i, "sigstop_cont")
        }
        4 if can_fault => {
            // Graceful leave, then the same node rejoins.
            let i = c.victim();
            c.died.insert(c.nodes[i].name.clone(), Instant::now());
            let st = c.nodes[i].terminate(Duration::from_secs(10));
            assert!(st.is_some_and(|s| s.success()), "{}: graceful exit {st:?}\n{}", c.nodes[i].name, c.nodes[i].log());
            std::thread::sleep(Duration::from_millis(c.rng.below(1000)));
            let how = c.restart(i);
            tag(i, &format!("leave_rejoin{how}"))
        }
        5 if can_fault => {
            // Kill the origin right after an apply: others get the state from it once back, or from a fetcher.
            let i = c.apply();
            c.sigkill(i);
            std::thread::sleep(Duration::from_millis(200 + c.rng.below(1500)));
            let how = c.restart(i);
            tag(i, &format!("apply_kill_origin{how}"))
        }
        6 if !down.is_empty() => {
            let i = down[c.rng.below(down.len() as u64) as usize];
            let how = c.restart(i);
            tag(i, &format!("restart{how}"))
        }
        7 if can_fault && down.is_empty() => {
            // Leave for good; a fresh node takes its place.
            let i = c.victim();
            let st = c.nodes[i].terminate(Duration::from_secs(10));
            assert!(st.is_some_and(|s| s.success()), "{}: graceful exit {st:?}\n{}", c.nodes[i].name, c.nodes[i].log());
            let ev = tag(i, "leave_replace");
            c.nodes.remove(i);
            c.join_new();
            ev
        }
        8 if room => {
            c.join_new();
            "join".into()
        }
        _ => {
            c.apply();
            "reapply".into()
        }
    }
}

fn tag(i: usize, ev: &str) -> String {
    if i == 0 {
        format!("seed_{ev}")
    } else {
        ev.to_string()
    }
}

fn run_chaos(tag: &str, duration: Duration, seed: u64) -> Stats {
    eprintln!("[chaos] tag={tag} seed={seed} duration={duration:?}");
    let mut c = Cluster::start(tag, 5, seed);
    c.apply();
    wait_converged(&c, Duration::from_secs(30)).expect("initial convergence");
    c.note_converged();
    let ingresses = |c: &Cluster| c.live_idx().iter().map(|&i| c.nodes[i].http.clone()).collect::<Vec<_>>();
    let load = Load::start(4, ingresses(&c));
    let mut stats = Stats { events: BTreeMap::new(), verifications: 0, violations: Vec::new() };
    let start = Instant::now();
    let mut round = 0u64;
    let mut dumped = false;
    while start.elapsed() < duration {
        round += 1;
        let before = stats.violations.len();
        let ev = inject(&mut c);
        *stats.events.entry(ev.clone()).or_insert(0) += 1;
        load.retarget(ingresses(&c));
        // Let gossip detect kills/leaves and re-place before converging.
        std::thread::sleep(Duration::from_millis(300));
        match wait_converged(&c, Duration::from_secs(40)) {
            Ok(()) => c.note_converged(),
            Err(e) => {
                stats.violations.push(format!("round {round} ({ev}): {e}"));
                // Give it more time once.
                if let Err(e2) = wait_converged(&c, Duration::from_secs(40)) {
                    stats.violations.push(format!("round {round} ({ev}): still not converged: {e2}"));
                    dump_logs(&c);
                    break;
                }
                c.note_converged();
            }
        }
        // Check no live node crashed on its own.
        for n in c.nodes.iter_mut() {
            if let Some(st) = n.exited() {
                stats.violations.push(format!("round {round}: node {} exited unexpectedly: {st:?}", n.name));
            }
        }
        verify(&c, &mut stats);
        if stats.violations.len() > before && !dumped {
            dumped = true;
            dump_logs(&c);
        }
        eprintln!(
            "[chaos] round {round} ({:.0}s): {ev}; nodes={} live={} v{} violations={}",
            start.elapsed().as_secs_f64(),
            c.nodes.len(),
            c.live_idx().len(),
            c.version,
            stats.violations.len()
        );
    }
    let (ok, err) = load.finish();
    eprintln!(
        "[chaos] done: rounds={round} events={:?} verifications={} load ok={ok} err={err} (informational) violations={}",
        stats.events,
        stats.verifications,
        stats.violations.len()
    );
    for v in &stats.violations {
        eprintln!("[chaos] VIOLATION: {v}");
    }
    let _ = std::fs::remove_dir_all(&c.dir);
    stats
}

fn seed_from_env(default: u64) -> u64 {
    std::env::var("K10S_CHAOS_SEED").ok().and_then(|s| s.parse().ok()).unwrap_or(default)
}

fn seed_faults(stats: &Stats) -> u64 {
    stats.events.iter().filter(|(k, _)| k.starts_with("seed_")).map(|(_, n)| n).sum()
}

#[test]
fn chaos_short() {
    let seed = seed_from_env(0x5eed_c0de);
    let stats = run_chaos("chaos-short", Duration::from_secs(75), seed);
    assert!(stats.violations.is_empty(), "invariant violations: {:#?}", stats.violations);
    assert!(stats.verifications >= 3, "too few verification rounds: {}", stats.verifications);
    assert!(seed_faults(&stats) >= 1, "the seed node was never faulted: {:?}", stats.events);
}

#[test]
#[ignore = "long chaos run (>= 20 min); set K10S_CHAOS_SECS"]
fn chaos_long() {
    let secs: u64 = std::env::var("K10S_CHAOS_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(1200);
    let seed = seed_from_env(0x00C0_FFEE_1234);
    let stats = run_chaos("chaos-long", Duration::from_secs(secs), seed);
    assert!(stats.violations.is_empty(), "invariant violations ({} rounds): {:#?}", stats.verifications, stats.violations);
    assert!(seed_faults(&stats) >= 1, "the seed node was never faulted: {:?}", stats.events);
}
