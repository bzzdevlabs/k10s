//! Cluster mode, multi-process: real `k10s node` children on ephemeral localhost ports.
//!
//! Every node runs with `--probe-ms 100` (faster failure detection than the default 200 ms) and a short leave
//! grace. Conditions are polled with generous timeouts (the machine may be loaded); no fixed sleeps except where a
//! test needs time to pass (load running across an event).
//!
//! The `#[ignore]`d `measure_*` tests print the numbers of docs/NODE.md (run them with `--release -- --ignored
//! --nocapture --test-threads 1`).

mod common;
mod support;

use k10_node::demo;
use k10_node::http::{self, Client};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::*};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

// ------------------------------------------------------------------------------------------------------------------
// Harness
// ------------------------------------------------------------------------------------------------------------------

struct NodeProc {
    child: Child,
    name: String,
    id: String,
    http: String,
    gossip: String,
    data: PathBuf,
    log: Arc<Mutex<String>>,
}

impl NodeProc {
    fn pid(&self) -> i32 {
        self.child.id() as i32
    }

    fn signal(&self, sig: i32) {
        // SAFETY: plain syscall on our own child.
        unsafe { libc::kill(self.pid(), sig) };
    }

    fn get(&self, path: &str) -> std::io::Result<http::ClientResponse> {
        http::request(&self.http, "GET", path, b"", Duration::from_secs(10))
    }

    fn status(&self) -> String {
        self.get("/_k10s/status").map(|r| r.text()).unwrap_or_default()
    }

    fn log(&self) -> String {
        self.log.lock().unwrap().clone()
    }
}

impl Drop for NodeProc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Opts {
    shards: u32,
    probe_ms: Option<u32>,
    extra: Vec<String>,
}

impl Default for Opts {
    fn default() -> Self {
        Opts { shards: 1, probe_ms: Some(100), extra: Vec::new() }
    }
}

fn spawn_node(name: &str, data: &Path, join: Option<&str>, o: &Opts) -> NodeProc {
    let bin = common::k10s_bin();
    let mut args: Vec<String> = [
        "node", "--name", name, "--http", "127.0.0.1:0", "--gossip", "127.0.0.1:0", "--rpc", "127.0.0.1:0", "--leave-grace-ms", "300",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    args.extend(["--shards".into(), o.shards.to_string(), "--data".into(), data.display().to_string()]);
    if let Some(p) = o.probe_ms {
        args.extend(["--probe-ms".into(), p.to_string()]);
    }
    if let Some(j) = join {
        args.extend(["--join".into(), j.to_string()]);
    }
    args.extend(o.extra.iter().cloned());
    support::with_cluster_key(&mut args);
    let mut child = Command::new(bin).args(&args).stderr(Stdio::piped()).stdout(Stdio::null()).spawn().expect("spawn k10s node");
    let log = Arc::new(Mutex::new(String::new()));
    let (tx, rx) = mpsc::channel();
    {
        let log = log.clone();
        let stderr = child.stderr.take().unwrap();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                let Ok(line) = line else { break };
                // "k10s: cluster node ID: gossip G rpc R http H (bound …)"
                if let Some(rest) = line.strip_prefix("k10s: cluster node ") {
                    let w: Vec<&str> = rest.split_whitespace().collect();
                    if w.len() >= 7 {
                        let _ = tx.send((w[0].trim_end_matches(':').to_string(), w[2].to_string(), w[6].to_string()));
                    }
                }
                let mut l = log.lock().unwrap();
                l.push_str(&line);
                l.push('\n');
            }
        });
    }
    let (id, gossip, http) = rx.recv_timeout(Duration::from_secs(20)).unwrap_or_else(|_| {
        panic!("node {name} did not start; log:\n{}", log.lock().unwrap());
    });
    let n = NodeProc { child, name: name.into(), id, http, gossip, data: data.to_path_buf(), log };
    wait_for(&format!("{name} health"), Duration::from_secs(20), || n.get("/_k10s/health").is_ok_and(|r| r.status == 200));
    n
}

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("k10s-cluster-{tag}-{}-{}", std::process::id(), k10_node::sys::mono_ns()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Starts `n` nodes (the first is the seed) and waits until every node sees all of them alive.
fn start_cluster(tag: &str, n: usize, o: &Opts) -> (PathBuf, Vec<NodeProc>) {
    let dir = tmpdir(tag);
    let mut nodes = vec![spawn_node("n1", &dir.join("n1"), None, o)];
    let seed = nodes[0].gossip.clone();
    for i in 2..=n {
        nodes.push(spawn_node(&format!("n{i}"), &dir.join(format!("n{i}")), Some(&seed), o));
    }
    wait_converged(&nodes, n);
    (dir, nodes)
}

fn wait_for(what: &str, timeout: Duration, mut f: impl FnMut() -> bool) -> Duration {
    let t = Instant::now();
    loop {
        if f() {
            return t.elapsed();
        }
        assert!(t.elapsed() < timeout, "timed out after {timeout:?} waiting for: {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn wait_converged(nodes: &[NodeProc], live: usize) {
    for n in nodes {
        wait_for(&format!("{} sees {live} live members", n.name), Duration::from_secs(30), || {
            cluster_num(&n.status(), "live_members") == Some(live as i64)
        });
    }
}

// ----- status parsing (no JSON library in the workspace) ----------------------------------------------------------

fn cluster_part(status: &str) -> &str {
    status.find("\"cluster\":{").map(|i| &status[i..]).unwrap_or("")
}

fn num_after(s: &str, key: &str) -> Option<i64> {
    let k = format!("\"{key}\":");
    let at = s.find(&k)? + k.len();
    let num: String = s[at..].chars().take_while(|c| c.is_ascii_digit() || *c == '-').collect();
    num.parse().ok()
}

fn cluster_num(status: &str, key: &str) -> Option<i64> {
    num_after(cluster_part(status), key)
}

fn applied_version(status: &str) -> Option<i64> {
    let c = cluster_part(status);
    let at = c.find("\"applied\":{")?;
    num_after(&c[at..], "version")
}

/// Service → node ids, from the cluster section of a status document.
fn placement(status: &str) -> BTreeMap<String, Vec<String>> {
    let mut out = BTreeMap::new();
    let c = cluster_part(status);
    let Some(at) = c.find("\"placement\":{") else { return out };
    let body = &c[at + "\"placement\":{".len()..];
    let body = &body[..body.find('}').unwrap_or(0)];
    for part in body.split("],") {
        let Some((name, ids)) = part.split_once(":[") else { continue };
        let ids: Vec<String> =
            ids.trim_end_matches(']').split(',').map(|s| s.trim_matches('"').to_string()).filter(|s| !s.is_empty()).collect();
        out.insert(name.trim_matches('"').to_string(), ids);
    }
    out
}

/// (id, state, incarnation) of every member in a status document.
fn members(status: &str) -> Vec<(String, String, u64)> {
    let c = cluster_part(status);
    let Some(at) = c.find("\"members\":[") else { return vec![] };
    let body = &c[at..];
    let body = &body[..body.find(']').unwrap_or(0)];
    body.split("{\"id\":\"")
        .skip(1)
        .map(|m| {
            let id = m[..m.find('"').unwrap()].to_string();
            let st = m.split("\"state\":\"").nth(1).map(|s| s[..s.find('"').unwrap()].to_string()).unwrap_or_default();
            let inc = num_after(m, "incarnation").unwrap_or(0) as u64;
            (id, st, inc)
        })
        .collect()
}

/// Service → runtime id of the service version a node runs (changes when the node replaces the service).
fn runtime_ids(status: &str) -> BTreeMap<String, i64> {
    let end = status.find("\"cluster\":{").unwrap_or(status.len());
    status[..end]
        .split("{\"name\":\"")
        .skip(1)
        .filter_map(|e| Some((e[..e.find('"')?].to_string(), num_after(e, "rt")?)))
        .collect()
}

fn member_state(status: &str, id: &str) -> Option<String> {
    members(status).into_iter().find(|m| m.0 == id).map(|m| m.1)
}

// ----- deployments ---------------------------------------------------------------------------------------------------

/// Writes modules + a manifest file and applies it with `k10s apply` through `via`.
fn apply(dir: &Path, via: &NodeProc, manifest: &str) -> String {
    let path = dir.join(format!("app-{}.k10s", k10_node::sys::mono_ns()));
    std::fs::write(&path, manifest).unwrap();
    let out = Command::new(common::k10s_bin())
        .args(["apply", path.to_str().unwrap(), "--node", &via.http])
        .output()
        .unwrap();
    let txt = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(out.status.success(), "apply failed: {txt} {}", String::from_utf8_lossy(&out.stderr));
    txt
}

fn write_modules(dir: &Path, n_pairs: usize) {
    std::fs::write(dir.join("echo.k10"), k10_ir::encode(&demo::echo())).unwrap();
    std::fs::write(dir.join("path.k10"), k10_ir::encode(&demo::path_echo())).unwrap();
    std::fs::write(dir.join("config.k10"), k10_ir::encode(&demo::config_echo())).unwrap();
    std::fs::write(dir.join("caller.k10"), k10_ir::encode(&demo::caller("echo"))).unwrap();
    for i in 0..n_pairs {
        std::fs::write(dir.join(format!("c{i}.k10")), k10_ir::encode(&demo::caller(&format!("e{i}")))).unwrap();
    }
}

/// echo, path (3 replicas), cfg (config = `cfg`), chain → echo, and `pairs` caller/callee pairs `c<i>` → `e<i>`.
fn manifest(cfg: &str, pairs: usize) -> String {
    let mut m = String::from(
        "service echo { code = \"echo.k10\" memory = 64KiB }\n\
         service path { code = \"path.k10\" memory = 64KiB replicas = 3 }\n\
         service chain { code = \"caller.k10\" memory = 64KiB calls = [\"echo\"] }\n",
    );
    m.push_str(&format!("service cfg {{ code = \"config.k10\" memory = 64KiB config = \"{cfg}\" }}\n"));
    for i in 0..pairs {
        m.push_str(&format!("service e{i} {{ code = \"echo.k10\" memory = 64KiB }}\n"));
        m.push_str(&format!("service c{i} {{ code = \"c{i}.k10\" memory = 64KiB calls = [\"e{i}\"] }}\n"));
    }
    m
}

fn wait_applied(nodes: &[NodeProc], version: i64, timeout: Duration) -> Duration {
    let t = Instant::now();
    for n in nodes {
        wait_for(&format!("{} applies v{version}", n.name), timeout, || applied_version(&n.status()) >= Some(version));
    }
    t.elapsed()
}

/// Every service answers through every node's ingress.
fn check_all_services(nodes: &[NodeProc], cfg: &str, pairs: usize) {
    for n in nodes {
        let mut c = Client::connect(&n.http, Duration::from_secs(10)).unwrap();
        let r = c.request("GET", "/echo", b"").unwrap();
        assert_eq!((r.status, r.text().as_str()), (200, "hello\n"), "echo via {}", n.name);
        let r = c.request("GET", "/path/x?y=1", b"").unwrap();
        assert_eq!((r.status, r.text().as_str()), (200, "method:0 path:/x?y=1"), "path via {}", n.name);
        let r = c.request("DELETE", "/path/z", b"").unwrap();
        assert_eq!(r.text(), "method:3 path:/z", "methods survive forwarding");
        let r = c.request("POST", "/chain", b"via chain").unwrap();
        assert_eq!((r.status, r.text().as_str()), (200, "via chain"), "chain via {}", n.name);
        let r = c.request("GET", "/cfg", b"").unwrap();
        assert_eq!((r.status, r.text().as_str()), (200, cfg), "cfg via {}", n.name);
        for i in 0..pairs {
            let body = format!("pair {i} via {}", n.name);
            let r = c.request("POST", &format!("/c{i}"), body.as_bytes()).unwrap();
            assert_eq!((r.status, r.text()), (200, body), "c{i} via {}", n.name);
        }
    }
}

/// A closed-loop load generator: `conns` keep-alive clients cycling over `paths` on `targets`.
struct Load {
    stop: Arc<AtomicBool>,
    ok: Arc<AtomicU64>,
    failed: Arc<AtomicU64>,
    errors: Arc<Mutex<Vec<String>>>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl Load {
    fn start(targets: Vec<String>, paths: Vec<String>, conns: usize) -> Load {
        let stop = Arc::new(AtomicBool::new(false));
        let ok = Arc::new(AtomicU64::new(0));
        let failed = Arc::new(AtomicU64::new(0));
        let errors = Arc::new(Mutex::new(Vec::new()));
        let threads = (0..conns)
            .map(|i| {
                let (stop, ok, failed, errors) = (stop.clone(), ok.clone(), failed.clone(), errors.clone());
                let target = targets[i % targets.len()].clone();
                let paths = paths.clone();
                std::thread::spawn(move || {
                    let mut c: Option<Client> = None;
                    let mut k = i;
                    while !stop.load(Relaxed) {
                        let p = &paths[k % paths.len()];
                        k += 1;
                        if c.is_none() {
                            c = Client::connect(&target, Duration::from_secs(10)).ok();
                        }
                        let r = match c.as_mut() {
                            Some(cl) => cl.request("GET", p, b""),
                            None => Err(std::io::Error::other("connect failed")),
                        };
                        match r {
                            Ok(r) if r.status == 200 => {
                                ok.fetch_add(1, Relaxed);
                                if r.close {
                                    c = None;
                                }
                            }
                            other => {
                                failed.fetch_add(1, Relaxed);
                                let e = match other {
                                    Ok(r) => format!("{p} via {target}: {} {}", r.status, r.text().trim()),
                                    Err(e) => format!("{p} via {target}: {e}"),
                                };
                                errors.lock().unwrap().push(e);
                                c = None;
                            }
                        }
                    }
                })
            })
            .collect();
        Load { stop, ok, failed, errors, threads }
    }

    fn stop(self) -> (u64, u64, Vec<String>) {
        self.stop.store(true, Relaxed);
        for t in self.threads {
            let _ = t.join();
        }
        (self.ok.load(Relaxed), self.failed.load(Relaxed), self.errors.lock().unwrap().clone())
    }
}

// ------------------------------------------------------------------------------------------------------------------
// Tests
// ------------------------------------------------------------------------------------------------------------------

/// Join + convergence, apply on one node → every node converges and serves every service through its ingress
/// (local or forwarded, methods preserved), `call` across nodes, replicas on distinct nodes, `k10s status`.
#[test]
fn three_nodes_join_apply_route_and_call() {
    let (dir, nodes) = start_cluster("basic", 3, &Opts::default());
    write_modules(&dir, 6);
    let out = apply(&dir, &nodes[1], &manifest("v1", 6));
    assert!(out.contains("\"version\":1") && out.contains("\"cluster\":true"), "{out}");
    wait_applied(&nodes, 1, Duration::from_secs(30));
    // Every node computed the same placement.
    let places: Vec<_> = nodes.iter().map(|n| placement(&n.status())).collect();
    assert!(places.iter().all(|p| *p == places[0]), "placements differ: {places:?}");
    let p = &places[0];
    assert_eq!(p.len(), 4 + 2 * 6, "{p:?}");
    let ids: Vec<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
    let mut rep = p["path"].clone();
    rep.sort();
    rep.dedup();
    assert_eq!(rep.len(), 3, "3 replicas on 3 distinct nodes: {:?}", p["path"]);
    assert!(p.values().flatten().all(|id| ids.contains(&id.as_str())));
    check_all_services(&nodes, "v1", 6);
    // At least one caller/callee pair lives on different nodes (all 6 co-located has probability 3^-6), and the
    // calls went through the peer protocol.
    let cross = (0..6).filter(|i| p[&format!("c{i}")] != p[&format!("e{i}")]).count();
    assert!(cross > 0, "{p:?}");
    let served: i64 = nodes.iter().map(|n| cluster_num(&n.status(), "served_for_peers").unwrap()).sum();
    assert!(served > 0);
    // The CLI shows the cluster view.
    let out = Command::new(common::k10s_bin()).args(["status", "--node", &nodes[0].http]).output().unwrap();
    let txt = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success());
    assert!(txt.contains("members: 3 live / 3 known") && txt.contains("applied v1") && txt.contains("(this node)"), "{txt}");
    assert!(txt.contains("auth: cluster key id "), "{txt}");
    let out = Command::new(common::k10s_bin()).args(["status", "--json", "--node", &nodes[0].http]).output().unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("\"cluster\":{"));
    // A stale explicit version is refused; re-applying the same content is a no-op.
    let out = apply(&dir, &nodes[2], &manifest("v1", 6));
    assert!(out.contains("\"version\":2"), "version 0 → highest known + 1: {out}");
    wait_applied(&nodes, 2, Duration::from_secs(30));
    for n in &nodes {
        assert!(n.log().contains("fetched desired state") || n.log().contains("announced"), "{}", n.log());
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// `num_after(key)` within the entry of service `name` in a status document.
fn svc_num(status: &str, name: &str, key: &str) -> Option<i64> {
    let at = status.find(&format!("{{\"name\":\"{name}\""))?;
    num_after(&status[at..], key)
}

/// A denied call is answered like an unknown service by the calling node and never forwarded; the same call
/// granted (same code, other service) is.
#[test]
fn denied_call_is_not_forwarded() {
    let (dir, nodes) = start_cluster("deny", 3, &Opts::default());
    write_modules(&dir, 0);
    std::fs::write(dir.join("nosuch.k10"), k10_ir::encode(&demo::caller("nosuch"))).unwrap();
    // `granted` and `denied` run the same code (call `echo`) on every node; `echo` has a single replica.
    let man = "service echo { code = \"echo.k10\" memory = 64KiB }\n\
               service granted { code = \"caller.k10\" memory = 64KiB replicas = 3 calls = [\"echo\"] }\n\
               service denied { code = \"caller.k10\" memory = 64KiB replicas = 3 }\n\
               service wild { code = \"nosuch.k10\" memory = 64KiB replicas = 3 calls = [\"*\"] }\n";
    apply(&dir, &nodes[0], man);
    wait_applied(&nodes, 1, Duration::from_secs(30));
    let p = placement(&nodes[0].status());
    let host = nodes.iter().position(|n| n.id == p["echo"][0]).expect("echo placed");
    let (h, via) = (&nodes[host], &nodes[(host + 1) % 3]);
    assert!(p["denied"].contains(&via.id) && p["granted"].contains(&via.id), "{p:?}");
    let counters = || {
        let (vs, hs) = (via.status(), h.status());
        (cluster_num(&vs, "forwarded"), cluster_num(&hs, "served_for_peers"), svc_num(&hs, "echo", "requests"))
    };
    let before = counters();
    let mut c = Client::connect(&via.http, Duration::from_secs(10)).unwrap();
    for _ in 0..20 {
        let r = c.request("POST", "/denied", b"x").unwrap();
        assert_eq!((r.status, r.text().as_str()), (502, "call error -1"), "denied call");
        // Exactly what a call to a service that exists nowhere gets.
        let r = c.request("POST", "/wild", b"x").unwrap();
        assert_eq!((r.status, r.text().as_str()), (502, "call error -1"), "unknown service");
    }
    assert_eq!(counters(), before, "a denied call must not be forwarded nor reach echo");
    // Control: the granted call crosses the mesh to echo's node.
    for _ in 0..5 {
        let r = c.request("POST", "/granted", b"via mesh").unwrap();
        assert_eq!((r.status, r.text().as_str()), (200, "via mesh"));
    }
    let after = counters();
    assert!(after.0 >= before.0.map(|n| n + 5), "granted calls are forwarded: {before:?} -> {after:?}");
    assert!(after.1 >= before.1.map(|n| n + 5) && after.2 >= before.2.map(|n| n + 5), "{before:?} -> {after:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The Ten examples (compiled by `k10s apply`) on 3 nodes: every service answers through every node's ingress
/// (locally or forwarded, methods and bodies preserved), `chain` → `echo` works wherever the two are placed, and the
/// single-replica `todo` / `counter` keep one state whatever the ingress.
#[test]
fn ten_examples_on_three_nodes() {
    let (dir, nodes) = start_cluster("examples", 3, &Opts::default());
    let out = apply(&dir, &nodes[0], &common::examples_manifest());
    assert!(out.contains("\"version\":1"), "{out}");
    wait_applied(&nodes, 1, Duration::from_secs(30));
    let places: Vec<_> = nodes.iter().map(|n| placement(&n.status())).collect();
    assert!(places.iter().all(|p| *p == places[0]), "placements differ: {places:?}");
    let mut hosts: Vec<&String> = places[0].values().flatten().collect();
    hosts.sort();
    hosts.dedup();
    assert!(hosts.len() >= 2, "8 services on one node out of 3: {:?}", places[0]);
    for n in &nodes {
        common::check_stateless(&n.http);
    }
    let mut last = 0;
    for n in &nodes {
        let id = common::check_todo(&n.http);
        assert!(id > last, "one todo store: ids {last} then {id}");
        last = id;
    }
    let mut want = 1;
    for _ in 0..2 {
        for n in &nodes {
            assert_eq!(common::counter_next(&n.http), want, "via {}", n.name);
            want += 1;
        }
    }
    let forwarded: i64 = nodes.iter().map(|n| cluster_num(&n.status(), "served_for_peers").unwrap()).sum();
    assert!(forwarded > 0);
    let _ = std::fs::remove_dir_all(&dir);
}

/// SIGKILL a node hosting a single-replica service: requests through another node succeed again quickly, the
/// failure is detected and the service re-placed. Then a new version is applied while the node is down; the node
/// restarts with the same data directory (same id, new ports), rejoins and catches up.
#[test]
fn sigkill_failover_then_restart_catches_up() {
    let (dir, mut nodes) = start_cluster("kill", 3, &Opts::default());
    write_modules(&dir, 4);
    apply(&dir, &nodes[0], &manifest("v1", 4));
    wait_applied(&nodes, 1, Duration::from_secs(30));
    check_all_services(&nodes, "v1", 4);
    // A single-replica service hosted on n2 or n3.
    let p = placement(&nodes[0].status());
    let (svc, victim) = p
        .iter()
        .filter(|(_, ids)| ids.len() == 1 && ids[0] != nodes[0].id)
        .map(|(s, ids)| (s.clone(), nodes.iter().position(|n| n.id == ids[0]).unwrap()))
        .next()
        .expect("some single-replica service is not on n1");
    let path = format!("/{svc}");
    let victim_id = nodes[victim].id.clone();
    nodes[victim].signal(libc::SIGKILL);
    let t0 = Instant::now();
    let first = wait_for("first success after SIGKILL", Duration::from_secs(30), || {
        nodes[0].get(&path).is_ok_and(|r| r.status == 200)
    });
    eprintln!("{svc}: first success {:?} after SIGKILL", first);
    let detected = wait_for("failure detected", Duration::from_secs(30), || {
        member_state(&nodes[0].status(), &victim_id).as_deref() == Some("dead")
    });
    let replaced = wait_for("service re-placed", Duration::from_secs(30), || {
        let p = placement(&nodes[0].status());
        p.get(&svc).is_some_and(|ids| !ids.contains(&victim_id) && !ids.is_empty())
    });
    eprintln!("detected dead after {detected:?}, re-placed {replaced:?} later ({:?} total)", t0.elapsed());
    let others: Vec<usize> = (0..3).filter(|&i| i != victim).collect();
    for &i in &others {
        for _ in 0..20 {
            assert_eq!(nodes[i].get(&path).unwrap().status, 200);
        }
    }
    // New version while the victim is down.
    let survivors: Vec<&NodeProc> = others.iter().map(|&i| &nodes[i]).collect();
    apply(&dir, survivors[1], &manifest("v2", 4));
    for n in &survivors {
        wait_for("survivors apply v2", Duration::from_secs(30), || applied_version(&n.status()) == Some(2));
    }
    // Restart with the same data directory.
    let data = nodes[victim].data.clone();
    let name = nodes[victim].name.clone();
    let old_inc = members(&nodes[others[0]].status()).into_iter().find(|m| m.0 == victim_id).unwrap().2;
    let seed = nodes[others[0]].gossip.clone();
    nodes[victim] = spawn_node(&name, &data, Some(&seed), &Opts::default());
    assert_eq!(nodes[victim].id, victim_id, "the node id is persisted in the data directory");
    wait_converged(&nodes, 3);
    wait_applied(&nodes, 2, Duration::from_secs(30));
    let inc = members(&nodes[others[0]].status()).into_iter().find(|m| m.0 == victim_id).unwrap().2;
    assert!(inc > old_inc, "rejoined with a higher incarnation ({inc} > {old_inc})");
    check_all_services(&nodes, "v2", 4);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Graceful leave under load (SIGTERM): peers see `left`, services move, no request fails. Then a new node joins,
/// takes its share of the services and serves everything through its own ingress.
#[test]
fn graceful_leave_and_join_under_load() {
    let (dir, mut nodes) = start_cluster("leave", 4, &Opts::default());
    write_modules(&dir, 6);
    apply(&dir, &nodes[0], &manifest("v1", 6));
    wait_applied(&nodes, 1, Duration::from_secs(30));
    let mut paths: Vec<String> = vec!["/echo".into(), "/path/a".into(), "/chain".into(), "/cfg".into()];
    paths.extend((0..6).map(|i| format!("/c{i}")));
    paths.extend((0..6).map(|i| format!("/e{i}")));
    let load = Load::start(vec![nodes[0].http.clone(), nodes[1].http.clone()], paths.clone(), 4);
    std::thread::sleep(Duration::from_millis(300));
    let before = nodes[0].status();
    let (place_before, rt_before) = (placement(&before), runtime_ids(&before));
    // n4 leaves (SIGTERM) while n1 and n2 receive traffic.
    let leaver = nodes.pop().unwrap();
    let leaver_id = leaver.id.clone();
    let hosted = placement(&nodes[0].status()).values().filter(|ids| ids.contains(&leaver_id)).count();
    leaver.signal(libc::SIGTERM);
    let mut leaver = leaver;
    let st = leaver.child.wait().unwrap();
    assert!(st.success(), "graceful exit: {st:?}\n{}", leaver.log());
    for n in &nodes {
        wait_for("peers see `left`", Duration::from_secs(10), || member_state(&n.status(), &leaver_id).as_deref() == Some("left"));
        wait_for("re-placed without the leaver", Duration::from_secs(10), || {
            placement(&n.status()).values().all(|ids| !ids.contains(&leaver_id))
        });
    }
    std::thread::sleep(Duration::from_millis(300));
    // A new node joins and takes its share.
    let seed = nodes[1].gossip.clone();
    nodes.push(spawn_node("n5", &dir.join("n5"), Some(&seed), &Opts::default()));
    wait_converged(&nodes, 4);
    wait_applied(&nodes, 1, Duration::from_secs(30));
    let newcomer = nodes[3].id.clone();
    for n in &nodes {
        wait_for("placement includes the newcomer", Duration::from_secs(10), || {
            placement(&n.status()).values().any(|ids| ids.contains(&newcomer))
        });
    }
    std::thread::sleep(Duration::from_millis(500));
    let (ok, failed, errors) = load.stop();
    eprintln!("leave + join under load: {ok} ok, {failed} failed; the leaver hosted {hosted} service(s)");
    assert!(ok > 100, "load ran ({ok})");
    assert_eq!(failed, 0, "{errors:?}");
    // Services whose placement (as seen from n1) did not change kept running untouched on n1: same runtime.
    let after = nodes[0].status();
    let (place_after, rt_after) = (placement(&after), runtime_ids(&after));
    let me = &nodes[0].id;
    let mut kept = 0;
    for (svc, b) in &place_before {
        let a = &place_after[svc];
        let same = if b.contains(me) || a.contains(me) {
            b.contains(me) && a.contains(me)
        } else {
            let (mut x, mut y) = (b.clone(), a.clone());
            x.sort();
            y.sort();
            x == y
        };
        if same {
            kept += 1;
            assert_eq!(rt_before[svc], rt_after[svc], "{svc} was replaced although its placement did not change");
        }
    }
    assert!(kept > 0);
    check_all_services(&nodes, "v1", 6);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Five nodes: re-apply new versions (from different nodes) while every ingress is under load; count failed
/// requests (zero expected) and the time until every node serves the new version.
#[test]
fn reapply_under_load_five_nodes() {
    let (dir, nodes) = start_cluster("reapply", 5, &Opts::default());
    write_modules(&dir, 4);
    apply(&dir, &nodes[0], &manifest("v1", 4));
    wait_applied(&nodes, 1, Duration::from_secs(30));
    let paths: Vec<String> =
        ["/echo", "/cfg", "/chain", "/path/q", "/c0", "/c1", "/c2", "/c3", "/e2"].iter().map(|s| s.to_string()).collect();
    let load = Load::start(nodes.iter().map(|n| n.http.clone()).collect(), paths, 10);
    for v in 2..=5 {
        let cfg = format!("v{v}");
        let t = Instant::now();
        apply(&dir, &nodes[v % 5], &manifest(&cfg, 4));
        for n in &nodes {
            wait_for(&format!("{} serves {cfg}", n.name), Duration::from_secs(30), || {
                n.get("/cfg").is_ok_and(|r| r.text() == cfg)
            });
        }
        eprintln!("v{v}: every node serves the new version {:?} after apply", t.elapsed());
        std::thread::sleep(Duration::from_millis(200));
    }
    // Two operators apply different manifests at the same time on two nodes: both may pick the same version; the
    // larger hash wins everywhere.
    let (d1, d2) = (dir.clone(), dir.clone());
    let (h1, h2) = (nodes[1].http.clone(), nodes[3].http.clone());
    let apply_to = |d: PathBuf, h: String, cfg: &'static str| {
        std::thread::spawn(move || {
            let path = d.join(format!("race-{cfg}.k10s"));
            std::fs::write(&path, manifest(cfg, 4)).unwrap();
            let out = Command::new(common::k10s_bin()).args(["apply", path.to_str().unwrap(), "--node", &h]).output().unwrap();
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stdout));
        })
    };
    let (t1, t2) = (apply_to(d1, h1, "race-a"), apply_to(d2, h2, "race-b"));
    t1.join().unwrap();
    t2.join().unwrap();
    let winner = |n: &NodeProc| {
        let st = n.status();
        let c = cluster_part(&st).to_string();
        let at = c.find("\"applied\":{").unwrap();
        let h = c[at..].split("\"hash\":\"").nth(1).unwrap()[..64].to_string();
        (applied_version(&st), h)
    };
    wait_for("all nodes agree on one desired state", Duration::from_secs(30), || {
        let w: Vec<_> = nodes.iter().map(winner).collect();
        w.iter().all(|x| *x == w[0]) && w[0].0 >= Some(6)
    });
    let served: Vec<String> = nodes.iter().map(|n| n.get("/cfg").unwrap().text()).collect();
    assert!(served.iter().all(|c| *c == served[0]) && served[0].starts_with("race-"), "{served:?}");
    std::thread::sleep(Duration::from_millis(200));
    let (ok, failed, errors) = load.stop();
    eprintln!("re-apply under load: {ok} ok, {failed} failed");
    assert!(ok > 100);
    assert_eq!(failed, 0, "{errors:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The node that announced a desired state crashes; a node joining later fetches manifest and blobs from the
/// other members (the origin is tried first and skipped).
#[test]
fn fetch_from_any_member_when_the_origin_is_gone() {
    let (dir, mut nodes) = start_cluster("origin", 3, &Opts::default());
    write_modules(&dir, 2);
    apply(&dir, &nodes[2], &manifest("from-n3", 2));
    wait_applied(&nodes, 1, Duration::from_secs(30));
    let origin = nodes.pop().unwrap();
    let origin_id = origin.id.clone();
    origin.signal(libc::SIGKILL);
    drop(origin);
    wait_for("origin declared dead", Duration::from_secs(30), || {
        member_state(&nodes[0].status(), &origin_id).as_deref() == Some("dead")
    });
    let seed = nodes[0].gossip.clone();
    nodes.push(spawn_node("n4", &dir.join("n4"), Some(&seed), &Opts::default()));
    wait_converged(&nodes, 3);
    wait_applied(&nodes, 1, Duration::from_secs(30));
    assert!(cluster_part(&nodes[2].status()).contains(&format!("\"origin\":\"{origin_id}\"")));
    assert!(cluster_num(&nodes[2].status(), "blobs_fetched").unwrap() >= 3, "{}", nodes[2].status());
    check_all_services(&nodes, "from-n3", 2);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Placement charges reservations: 3 nodes × room for 15 instances (`--mem 1MiB`), 45 services of 64 KiB: each node
/// gets exactly 15 (plain rendezvous hashing would overload some), reserves them, and every service answers.
#[test]
fn placement_respects_memory_reservations() {
    let o = Opts { extra: vec!["--mem".into(), "1MiB".into()], ..Opts::default() };
    let (dir, nodes) = start_cluster("reserve", 3, &o);
    write_modules(&dir, 0);
    let m: String = (0..45).map(|i| format!("service s{i} {{ code = \"echo.k10\" memory = 64KiB }}\n")).collect();
    apply(&dir, &nodes[1], &m);
    wait_applied(&nodes, 1, Duration::from_secs(30));
    let one = 64 * 1024 + 4096;
    let p = placement(&nodes[0].status());
    assert_eq!(p.len(), 45);
    assert!(p.values().all(|ids| ids.len() == 1), "every service placed: {p:?}");
    for n in &nodes {
        let s = n.status();
        assert_eq!(placement(&s), p, "same placement on {}", n.name);
        let here = p.values().filter(|ids| ids[0] == n.id).count() as i64;
        assert_eq!(here, 15, "{} hosts {here} services, its budget holds 15", n.name);
        let mem = &s[s.find("\"memory\":{").unwrap()..];
        assert_eq!(num_after(mem, "mem_reserved"), Some(here * one), "{}: {mem}", n.name);
        assert!(num_after(mem, "mem_reserved") <= num_after(mem, "mem_limit"));
    }
    let mut c = Client::connect(&nodes[2].http, Duration::from_secs(10)).unwrap();
    for i in 0..45 {
        let r = c.request("GET", &format!("/s{i}"), b"").unwrap();
        assert_eq!((r.status, r.text().as_str()), (200, "hello\n"), "s{i}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

// ------------------------------------------------------------------------------------------------------------------
// Measurements (`cargo test --release -p k10s --test cluster -- --ignored --nocapture --test-threads 1 measure_`)
// ------------------------------------------------------------------------------------------------------------------

fn median(v: &mut [f64]) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

/// Runs bench/bin/loadgen; returns (rps, p50 µs, p99 µs, errors).
fn loadgen(addr: &str, path: &str, conns: u32, secs: u32) -> (f64, f64, f64, i64) {
    let bin = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../bench/bin/loadgen");
    let out = Command::new(bin)
        .args(["-addr", addr, "-path", path, "-conns", &conns.to_string(), "-duration", &format!("{secs}s"), "-warmup", "1s"])
        .output()
        .expect("bench/bin/loadgen (see bench/README.md)");
    let j = String::from_utf8_lossy(&out.stdout).into_owned();
    let f = |k: &str| -> f64 {
        let key = format!("\"{k}\": ");
        let at = j.find(&key).unwrap_or_else(|| panic!("{k} missing in {j}")) + key.len();
        j[at..].chars().take_while(|c| c.is_ascii_digit() || *c == '.' || *c == 'e' || *c == '-').collect::<String>().parse().unwrap()
    };
    (f("rps"), f("p50"), f("p99"), f("errors") as i64)
}

fn proc_stat(pid: i32) -> (u64, u64) {
    // (utime + stime in ticks, VmRSS in KiB)
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
    let after = &stat[stat.rfind(')').map_or(0, |i| i + 2)..];
    let f: Vec<&str> = after.split_whitespace().collect();
    let ticks = f.get(11).and_then(|s| s.parse::<u64>().ok()).unwrap_or(0) + f.get(12).and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
    let rss = status
        .lines()
        .find(|l| l.starts_with("VmRSS:"))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    (ticks, rss)
}

/// apply → every node serving the new version (a service replicated on every node, so each node's answer reflects
/// its own apply), and → every node reporting it applied. N = 3, 5, 20 node processes with 1 shard each.
#[test]
#[ignore]
fn measure_apply_convergence() {
    for n in [3usize, 5, 20] {
        let o = Opts { probe_ms: None, ..Default::default() };
        let t = Instant::now();
        let (dir, nodes) = start_cluster(&format!("m-apply{n}"), n, &o);
        let join = t.elapsed();
        write_modules(&dir, 0);
        let m = |cfg: &str| {
            format!(
                "service cfgall {{ code = \"config.k10\" memory = 64KiB replicas = {n} config = \"{cfg}\" }}\n\
                 service echo {{ code = \"echo.k10\" memory = 64KiB }}\nservice path {{ code = \"path.k10\" memory = 64KiB replicas = 2 }}\n"
            )
        };
        apply(&dir, &nodes[0], &m("v1"));
        wait_applied(&nodes, 1, Duration::from_secs(60));
        let (mut serve, mut applied) = (Vec::new(), Vec::new());
        for v in 2..=6 {
            let cfg = format!("v{v}");
            let path = dir.join(format!("m{v}.k10s"));
            std::fs::write(&path, m(&cfg)).unwrap();
            let via = &nodes[v % n];
            let t0 = Instant::now();
            let out = Command::new(common::k10s_bin()).args(["apply", path.to_str().unwrap(), "--node", &via.http]).output().unwrap();
            assert!(out.status.success());
            let mut clients: Vec<Client> = nodes.iter().map(|x| Client::connect(&x.http, Duration::from_secs(10)).unwrap()).collect();
            let mut done = vec![false; n];
            while done.iter().any(|d| !d) {
                for (i, c) in clients.iter_mut().enumerate() {
                    if !done[i] && c.request("GET", "/cfgall", b"").map(|r| r.text() == cfg).unwrap_or(false) {
                        done[i] = true;
                    }
                }
                assert!(t0.elapsed() < Duration::from_secs(60));
            }
            serve.push(ms(t0.elapsed()));
            wait_applied(&nodes, v as i64, Duration::from_secs(60));
            applied.push(ms(t0.elapsed()));
            std::thread::sleep(Duration::from_millis(300));
        }
        let (smax, amax) = (serve.iter().cloned().fold(0.0, f64::max), applied.iter().cloned().fold(0.0, f64::max));
        eprintln!(
            "MEASURE apply N={n}: membership converged {:.0} ms after the first spawn; apply -> all serving: median {:.1} ms (max {:.1}); -> all report applied: median {:.1} ms (max {:.1}); runs {serve:.1?}",
            ms(join),
            median(&mut serve.clone()),
            smax,
            median(&mut applied.clone()),
            amax
        );
        drop(nodes);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Failover after SIGKILL (process crash: the kernel resets its connections) and SIGSTOP (hung machine: nothing
/// answers, no reset), with the default gossip config and a tighter one. A prober sends a fresh-connection GET every
/// 10 ms (in its own thread, 2 s timeout) through a node that does not host the service.
#[test]
#[ignore]
fn measure_failover() {
    for (label, probe, mult) in [("default (probe 200 ms, suspicion x4)", None, None), ("tight (probe 50 ms, suspicion x2)", Some(50u32), Some(2u32))] {
        for sig in [libc::SIGKILL, libc::SIGSTOP] {
            let mut o = Opts { probe_ms: probe, ..Default::default() };
            if let Some(m) = mult {
                o.extra = vec!["--suspicion-mult".into(), m.to_string()];
            }
            let (dir, nodes) = start_cluster("m-fail", 3, &o);
            write_modules(&dir, 0);
            let mut man = String::new();
            for i in 0..12 {
                man.push_str(&format!("service v{i} {{ code = \"echo.k10\" memory = 64KiB }}\n"));
            }
            apply(&dir, &nodes[0], &man);
            wait_applied(&nodes, 1, Duration::from_secs(30));
            let p = placement(&nodes[0].status());
            let (svc, host) = p
                .iter()
                .find(|(_, ids)| ids[0] != nodes[0].id)
                .map(|(s, ids)| (s.clone(), nodes.iter().position(|x| x.id == ids[0]).unwrap()))
                .unwrap();
            let via = nodes[0].http.clone();
            for _ in 0..50 {
                assert_eq!(nodes[0].get(&format!("/{svc}")).unwrap().status, 200);
            }
            let results: Arc<Mutex<Vec<(f64, f64, bool)>>> = Arc::new(Mutex::new(Vec::new()));
            let stop = Arc::new(AtomicBool::new(false));
            let t0 = Instant::now();
            let prober = {
                let (results, stop, via, svc) = (results.clone(), stop.clone(), via.clone(), svc.clone());
                std::thread::spawn(move || {
                    let mut hs = Vec::new();
                    while !stop.load(Relaxed) {
                        let (results, via, svc) = (results.clone(), via.clone(), svc.clone());
                        hs.push(std::thread::spawn(move || {
                            let s = ms(t0.elapsed());
                            let ok = http::request(&via, "GET", &format!("/{svc}"), b"", Duration::from_secs(2)).is_ok_and(|r| r.status == 200);
                            results.lock().unwrap().push((s, ms(t0.elapsed()), ok));
                        }));
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    for h in hs {
                        let _ = h.join();
                    }
                })
            };
            // Plus closed-loop load (8 keep-alive connections) through the same node.
            let load = Load::start(vec![via.clone()], vec![format!("/{svc}")], 8);
            std::thread::sleep(Duration::from_millis(500));
            let kill_at = ms(t0.elapsed());
            nodes[host].signal(sig);
            let hid = nodes[host].id.clone();
            let detect = wait_for("dead", Duration::from_secs(30), || member_state(&nodes[0].status(), &hid).as_deref() == Some("dead"));
            std::thread::sleep(Duration::from_millis(2500));
            stop.store(true, Relaxed);
            prober.join().unwrap();
            let (lok, lfailed, lerr) = load.stop();
            if sig == libc::SIGSTOP {
                nodes[host].signal(libc::SIGKILL);
            }
            let r = results.lock().unwrap().clone();
            let after: Vec<&(f64, f64, bool)> = r.iter().filter(|x| x.0 >= kill_at).collect();
            let first_ok = after.iter().filter(|x| x.2).map(|x| x.1).fold(f64::MAX, f64::min) - kill_at;
            let fails: Vec<(f64, f64)> = r.iter().filter(|x| !x.2).map(|x| (x.0, x.1)).collect();
            // 100 %: every probe *sent* after this point succeeded.
            let all_ok = fails.iter().map(|f| f.0).fold(kill_at, f64::max) - kill_at;
            eprintln!(
                "MEASURE failover {label} {}: first success {:.0} ms after the signal, 100 % of probes sent after {:.0} ms succeed, {} of {} probes failed (last failure answered at +{:.0} ms), member declared dead after {:.0} ms",
                if sig == libc::SIGKILL { "SIGKILL" } else { "SIGSTOP" },
                first_ok,
                all_ok,
                fails.len(),
                r.len(),
                fails.iter().map(|f| f.1).fold(kill_at, f64::max) - kill_at,
                ms(detect)
            );
            eprintln!(
                "MEASURE   closed-loop load (8 conns) during it: {lok} ok, {lfailed} failed ({:?})",
                lerr.iter().take(3).collect::<Vec<_>>()
            );
            drop(nodes);
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}

/// Cross-node `call` vs same-node `call`, and ingress forwarding vs local ingress (bench/bin/loadgen, keep-alive).
#[test]
#[ignore]
fn measure_latency() {
    let spin = std::env::var("K10S_SPIN_US").unwrap_or_default();
    let mut o = Opts { shards: 2, probe_ms: None, ..Default::default() };
    if !spin.is_empty() {
        eprintln!("MEASURE (nodes run with --idle-spin-us {spin})");
        o.extra = vec!["--idle-spin-us".into(), spin];
    }
    let (dir, nodes) = start_cluster("m-lat", 3, &o);
    write_modules(&dir, 12);
    let mut man = String::new();
    for i in 0..12 {
        man.push_str(&format!("service e{i} {{ code = \"echo.k10\" memory = 64KiB }}\n"));
        man.push_str(&format!("service c{i} {{ code = \"c{i}.k10\" memory = 64KiB calls = [\"e{i}\"] }}\n"));
    }
    apply(&dir, &nodes[0], &man);
    wait_applied(&nodes, 1, Duration::from_secs(30));
    let p = placement(&nodes[0].status());
    let host = |s: &str| nodes.iter().position(|x| x.id == p[s][0]).unwrap();
    let same = (0..12).find(|i| host(&format!("c{i}")) == host(&format!("e{i}"))).expect("a co-located pair");
    let cross = (0..12).find(|i| host(&format!("c{i}")) != host(&format!("e{i}"))).expect("a split pair");
    let hs = host(&format!("c{same}"));
    let hc = host(&format!("c{cross}"));
    let e_local = format!("e{same}");
    let e_remote_host = host(&format!("e{cross}"));
    let other = (0..3).find(|&i| i != e_remote_host).unwrap();
    for conns in [1u32, 16] {
        let (r1, p1, q1, x1) = loadgen(&nodes[hs].http, &format!("/{e_local}"), conns, 5);
        let (r2, p2, q2, x2) = loadgen(&nodes[hs].http, &format!("/c{same}"), conns, 5);
        let (r3, p3, q3, x3) = loadgen(&nodes[hc].http, &format!("/c{cross}"), conns, 5);
        eprintln!("MEASURE c={conns} echo, local ingress: {r1:.0} rps p50 {p1} us p99 {q1} us err {x1}");
        eprintln!("MEASURE c={conns} ingress -> caller -> callee, same node: {r2:.0} rps p50 {p2} us p99 {q2} us err {x2}");
        eprintln!("MEASURE c={conns} ingress -> caller -> callee on another node: {r3:.0} rps p50 {p3} us p99 {q3} us err {x3}");
    }
    let e = format!("/e{cross}");
    for conns in [1u32, 16, 64] {
        let (r1, p1, q1, x1) = loadgen(&nodes[e_remote_host].http, &e, conns, 5);
        let (r2, p2, q2, x2) = loadgen(&nodes[other].http, &e, conns, 5);
        eprintln!("MEASURE c={conns} echo via hosting node: {r1:.0} rps p50 {p1} us p99 {q1} us err {x1}");
        eprintln!("MEASURE c={conns} echo via another node (forwarded): {r2:.0} rps p50 {p2} us p99 {q2} us err {x2}");
    }
    drop(nodes);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Idle 5-node cluster: per-node RSS, CPU and gossip traffic over 30 s (no services, then 20 services deployed).
#[test]
#[ignore]
fn measure_idle() {
    let o = Opts { shards: 2, probe_ms: None, ..Default::default() };
    let (dir, nodes) = start_cluster("m-idle", 5, &o);
    write_modules(&dir, 0);
    for phase in ["no services", "20 services"] {
        if phase == "20 services" {
            let mut man = String::new();
            for i in 0..20 {
                man.push_str(&format!("service s{i} {{ code = \"echo.k10\" memory = 64KiB replicas = 2 }}\n"));
            }
            apply(&dir, &nodes[0], &man);
            wait_applied(&nodes, 1, Duration::from_secs(30));
            for n in &nodes {
                for i in 0..20 {
                    assert_eq!(n.get(&format!("/s{i}")).unwrap().status, 200);
                }
            }
        }
        std::thread::sleep(Duration::from_secs(5));
        let secs = 30.0;
        let before: Vec<((u64, u64), i64, i64)> = nodes
            .iter()
            .map(|n| {
                let st = n.status();
                (proc_stat(n.pid()), cluster_num(&st, "gossip_bytes_sent").unwrap(), cluster_num(&st, "gossip_bytes_received").unwrap())
            })
            .collect();
        std::thread::sleep(Duration::from_secs_f64(secs));
        let tick = 100.0; // USER_HZ
        for (i, n) in nodes.iter().enumerate() {
            let st = n.status();
            let ((t1, rss), s1, r1) = (proc_stat(n.pid()), cluster_num(&st, "gossip_bytes_sent").unwrap(), cluster_num(&st, "gossip_bytes_received").unwrap());
            let ((t0, _), s0, r0) = before[i];
            eprintln!(
                "MEASURE idle 5 nodes, {phase}: {} RSS {:.1} MiB, CPU {:.3} % of a core, gossip out {:.0} B/s in {:.0} B/s",
                n.name,
                rss as f64 / 1024.0,
                (t1 - t0) as f64 / tick / secs * 100.0,
                (s1 - s0) as f64 / secs,
                (r1 - r0) as f64 / secs
            );
        }
    }
    drop(nodes);
    let _ = std::fs::remove_dir_all(&dir);
}
