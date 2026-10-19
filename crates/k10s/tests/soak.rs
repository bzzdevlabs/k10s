//! Soak / leak campaign: a single node and a 3-node cluster under steady mixed load (echo, chain, compute, counter,
//! todo) with periodic re-applies, sampling RSS, fds, threads, VMAs, executable (JIT) mappings, KV log sizes and
//! fiber/instance counts every 10 s. It fits a linear trend to each series and asserts nothing grows without bound;
//! the samples and a summary are written under `bench/results/soak/`.
//!
//! `soak_short_single` / `soak_short_cluster` are normal (~2 min) tests; `soak_long_*` (`#[ignore]`) run ≥ 30 min
//! (`K10S_SOAK_SECS`).

mod support;
use support::*;

use k10_node::http::{self, Client};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::*};
use std::sync::Arc;
use std::time::{Duration, Instant};

const SAMPLE_EVERY: Duration = Duration::from_secs(10);
const REAPPLY_EVERY: Duration = Duration::from_secs(30);

/// Where samples and the summary go: `bench/results/soak/` for the long runs (or any run with `K10S_SOAK_OUT=1`);
/// ordinary short test runs write into their own temp dir so `cargo test` does not touch the repository tree.
fn results_dir(tag: &str, tmp: &Path) -> PathBuf {
    if tag.contains("long") || std::env::var_os("K10S_SOAK_OUT").is_some() {
        let d = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../bench/results/soak");
        std::fs::create_dir_all(&d).ok();
        d
    } else {
        tmp.to_path_buf()
    }
}

// The example services as precompiled Svc specs (compiled from examples/*.ten).
fn example_svcs(variant: u64) -> Vec<Svc> {
    let ex = |f: &str| ten_file(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples").join(f));
    let mut v = vec![
        echo_svc().replicas(3),
        Svc::new("chain", ex("chain.ten")).mem(16).config(b"echo").replicas(2),
        Svc::new("compute", ex("compute.ten")).mem(16).cpu(60000).replicas(2),
        Svc::new("counter", ex("counter.ten")).mem(16).replicas(1),
        Svc::new("todo", ex("todo.ten")).mem(20).replicas(1),
    ];
    // A churned service toggled on re-apply, to exercise retirement + code unmap without unbounded growth.
    if variant % 2 == 0 {
        v.push(Svc::new("hello", ex("hello.ten")).mem(16).replicas(2));
    } else {
        v.push(Svc::new("hello2", ex("hello.ten")).mem(16).replicas(1));
    }
    v
}

#[derive(Default, Clone)]
struct Series {
    t: Vec<f64>,
    rss_kb: Vec<f64>,
    fds: Vec<f64>,
    threads: Vec<f64>,
    vmas: Vec<f64>,
    exec_anon: Vec<f64>,
    kv_log: Vec<f64>,
    instances: Vec<f64>,
    fibers: Vec<f64>,
}

impl Series {
    fn push(&mut self, t: f64, p: &ProcSample, kv_log: i64, instances: i64, fibers: i64) {
        self.t.push(t);
        self.rss_kb.push(p.rss_kb as f64);
        self.fds.push(p.fds as f64);
        self.threads.push(p.threads as f64);
        self.vmas.push(p.vmas as f64);
        self.exec_anon.push(p.exec_anon as f64);
        self.kv_log.push(kv_log as f64);
        self.instances.push(instances as f64);
        self.fibers.push(fibers as f64);
    }

    fn write_csv(&self, path: &Path) {
        let mut f = std::fs::File::create(path).unwrap();
        writeln!(f, "t_s,rss_kb,fds,threads,vmas,exec_anon,kv_log_bytes,instances,fibers").unwrap();
        for i in 0..self.t.len() {
            writeln!(
                f,
                "{:.0},{:.0},{:.0},{:.0},{:.0},{:.0},{:.0},{:.0},{:.0}",
                self.t[i], self.rss_kb[i], self.fds[i], self.threads[i], self.vmas[i], self.exec_anon[i], self.kv_log[i], self.instances[i], self.fibers[i]
            )
            .unwrap();
        }
    }
}

/// Mixed closed-loop load over `targets`, cycling the example endpoints. Returns (ok, err).
struct Mix {
    stop: Arc<AtomicBool>,
    ok: Arc<AtomicU64>,
    err: Arc<AtomicU64>,
    /// Error breakdown: "<step> <status|error kind>" → count (reported in the summary).
    errs: Arc<std::sync::Mutex<std::collections::BTreeMap<String, u64>>>,
    handles: Vec<std::thread::JoinHandle<()>>,
}

fn kind(r: &Option<std::io::Result<k10_node::http::ClientResponse>>) -> String {
    match r {
        None => "no-connection".into(),
        Some(Ok(r)) => format!("{}", r.status),
        Some(Err(e)) => format!("{:?}", e.kind()),
    }
}

impl Mix {
    fn start(targets: &[String], conns_per: usize) -> Mix {
        let stop = Arc::new(AtomicBool::new(false));
        let ok = Arc::new(AtomicU64::new(0));
        let err = Arc::new(AtomicU64::new(0));
        let errs = Arc::new(std::sync::Mutex::new(std::collections::BTreeMap::new()));
        let mut handles = Vec::new();
        for t in targets {
            for k in 0..conns_per {
                let (t, stop, ok, err, errs) = (t.clone(), stop.clone(), ok.clone(), err.clone(), errs.clone());
                handles.push(std::thread::spawn(move || {
                    // GET endpoints plus a bounded to-do lifecycle (step 4: POST then DELETE the created id). The
                    // to-do load is create+delete so its live set (and its single growing "index" value) stays
                    // small: a create-only loop would grow the example's index until it exhausts the 1 MiB instance
                    // heap and traps — the example's own scaling limit, not a node leak. This churns the KV log
                    // (puts + dels) so compaction is exercised instead.
                    let gets = ["/echo", "/chain", "/compute?n=50", "/counter"];
                    let mut c: Option<Client> = None;
                    let mut i = k;
                    while !stop.load(Relaxed) {
                        let step = i % 5;
                        i += 1;
                        if c.is_none() {
                            c = Client::connect(&t, Duration::from_secs(5)).ok();
                        }
                        // Ok(()) or Err(label) for this iteration.
                        let outcome: Result<(), String> = if step < 4 {
                            let r = c.as_mut().map(|cl| cl.request("GET", gets[step], b""));
                            match &r {
                                Some(Ok(resp)) if resp.status < 500 => {
                                    if resp.close {
                                        c = None;
                                    }
                                    Ok(())
                                }
                                _ => Err(format!("GET {} {}", gets[step], kind(&r))),
                            }
                        } else {
                            let r = c.as_mut().map(|cl| cl.request("POST", "/todo", b"soak item"));
                            match &r {
                                Some(Ok(resp)) if resp.status == 201 => {
                                    let id = resp.text().strip_prefix("{\"id\":").and_then(|s| s.split(',').next()).and_then(|n| n.parse::<i64>().ok());
                                    if resp.close {
                                        c = None;
                                    }
                                    match id {
                                        None => Err(format!("POST /todo unparsable {:?}", resp.text())),
                                        Some(id) => {
                                            let d = c.as_mut().map(|cl| cl.request("DELETE", &format!("/todo/{id}"), b""));
                                            match &d {
                                                Some(Ok(resp)) if resp.status < 500 => {
                                                    if resp.close {
                                                        c = None;
                                                    }
                                                    Ok(())
                                                }
                                                _ => Err(format!("DELETE /todo {}", kind(&d))),
                                            }
                                        }
                                    }
                                }
                                _ => Err(format!("POST /todo {}", kind(&r))),
                            }
                        };
                        match outcome {
                            Ok(()) => {
                                ok.fetch_add(1, Relaxed);
                            }
                            Err(label) => {
                                err.fetch_add(1, Relaxed);
                                *errs.lock().unwrap().entry(label).or_insert(0) += 1;
                                c = None;
                            }
                        }
                    }
                }));
            }
        }
        Mix { stop, ok, err, errs, handles }
    }

    /// (ok, err, error breakdown sorted by count).
    fn finish(self) -> (u64, u64, Vec<(String, u64)>) {
        self.stop.store(true, Relaxed);
        for h in self.handles {
            let _ = h.join();
        }
        let mut b: Vec<(String, u64)> = self.errs.lock().unwrap().iter().map(|(k, v)| (k.clone(), *v)).collect();
        b.sort_by(|a, b| b.1.cmp(&a.1));
        (self.ok.load(Relaxed), self.err.load(Relaxed), b)
    }
}

/// Fits a slope (per sample) and flags unbounded growth. Uses the second-half vs first-half mean to be robust to an
/// initial warm-up ramp (physical pages faulting in, pools filling): a leak keeps rising in the second half.
struct Trend {
    name: String,
    slope_per_min: f64,
    first_half: f64,
    second_half: f64,
    min: f64,
    max: f64,
}

fn trend(name: &str, t: &[f64], y: &[f64]) -> Trend {
    let (b, _r2) = slope(t, y);
    let half = y.len() / 2;
    let mean = |s: &[f64]| if s.is_empty() { 0.0 } else { s.iter().sum::<f64>() / s.len() as f64 };
    Trend {
        name: name.to_string(),
        slope_per_min: b * 60.0,
        first_half: mean(&y[..half]),
        second_half: mean(&y[half..]),
        min: y.iter().cloned().fold(f64::INFINITY, f64::min),
        max: y.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
    }
}

fn summarize(series: &Series, label: &str, out: &mut String) -> Vec<Trend> {
    use std::fmt::Write;
    let mk = |n: &str, y: &[f64]| trend(n, &series.t, y);
    let trends = vec![
        mk("rss_kb", &series.rss_kb),
        mk("fds", &series.fds),
        mk("threads", &series.threads),
        mk("vmas", &series.vmas),
        mk("exec_anon", &series.exec_anon),
        mk("kv_log_bytes", &series.kv_log),
        mk("instances", &series.instances),
        mk("fibers", &series.fibers),
    ];
    let _ = writeln!(out, "## {label} ({} samples)", series.t.len());
    let _ = writeln!(out, "{:<14} {:>12} {:>12} {:>14} {:>12}", "metric", "min", "max", "slope/min", "2nd/1st mean");
    for tr in &trends {
        let ratio = if tr.first_half == 0.0 { 1.0 } else { tr.second_half / tr.first_half };
        let _ = writeln!(out, "{:<14} {:>12.0} {:>12.0} {:>14.2} {:>12.2}", tr.name, tr.min, tr.max, tr.slope_per_min, ratio);
    }
    trends
}

/// Asserts no unbounded growth. fds/threads must be flat; VMAs / exec mappings / KV log / instances / fibers must
/// plateau (second-half mean not meaningfully above the first-half, allowing a warm-up ramp); RSS may ramp then
/// plateau. Thresholds are lenient enough for a short noisy run but catch a real leak.
fn assert_no_leak(trends: &[Trend], violations: &mut Vec<String>) {
    for tr in trends {
        let grew = tr.second_half - tr.first_half;
        let ratio = if tr.first_half <= 0.0 { 1.0 } else { tr.second_half / tr.first_half };
        match tr.name.as_str() {
            "fds" | "threads" => {
                if tr.max - tr.min > 24.0 {
                    violations.push(format!("{} not flat: min {} max {} (slope/min {:.2})", tr.name, tr.min, tr.max, tr.slope_per_min));
                }
            }
            "exec_anon" => {
                // JIT executable mappings: re-applies unmap old code, so this must oscillate, not grow.
                if grew > 8.0 && ratio > 1.5 {
                    violations.push(format!("exec mappings growing: 1st {:.0} 2nd {:.0}", tr.first_half, tr.second_half));
                }
            }
            "vmas" => {
                if ratio > 1.6 && grew > 200.0 {
                    violations.push(format!("VMAs growing: 1st {:.0} 2nd {:.0}", tr.first_half, tr.second_half));
                }
            }
            "kv_log_bytes" => {
                // counter overwrites one key; todo creates+deletes — compaction must cap the log well under 256 MiB.
                if tr.max > 256.0 * 1024.0 * 1024.0 {
                    violations.push(format!("KV log exceeded 256 MiB: max {:.0}", tr.max));
                }
            }
            "rss_kb" => {
                if ratio > 2.0 && grew > 256.0 * 1024.0 {
                    violations.push(format!("RSS still climbing: 1st {:.0} KB 2nd {:.0} KB", tr.first_half, tr.second_half));
                }
            }
            "instances" | "fibers" => {
                if ratio > 3.0 && grew > 2000.0 {
                    violations.push(format!("{} growing: 1st {:.0} 2nd {:.0}", tr.name, tr.first_half, tr.second_half));
                }
            }
            _ => {}
        }
    }
}

fn status_metric(status: &str, section: &str, key: &str) -> i64 {
    section_num(status, section, key).unwrap_or(0)
}

fn run_soak(tag: &str, cluster_nodes: usize, duration: Duration) -> Vec<String> {
    eprintln!("[soak] tag={tag} nodes={cluster_nodes} duration={duration:?}");
    let dir = tmpdir(tag);
    // Nodes.
    let mut nodes: Vec<NodeProc> = Vec::new();
    if cluster_nodes <= 1 {
        nodes.push(spawn(tag, &dir.join("n0"), &["--http", "127.0.0.1:0", "--shards", "2", "--idle-timeout", "5"].iter().map(|s| s.to_string()).collect::<Vec<_>>()));
    } else {
        let base: Vec<String> = ["--http", "127.0.0.1:0", "--gossip", "127.0.0.1:0", "--rpc", "127.0.0.1:0", "--probe-ms", "150", "--shards", "2", "--idle-timeout", "5"].iter().map(|s| s.to_string()).collect();
        nodes.push(spawn("n0", &dir.join("n0"), &base));
        let seed = nodes[0].gossip.clone();
        for i in 1..cluster_nodes {
            let mut a = base.clone();
            a.extend(["--join".into(), seed.clone()]);
            nodes.push(spawn(&format!("n{i}"), &dir.join(format!("n{i}")), &a));
        }
        for n in &nodes {
            wait_for(&format!("{} converges", n.name), Duration::from_secs(40), || {
                n.status().find("\"cluster\":{").is_some_and(|i| num_after(&n.status()[i..], "live_members") == Some(cluster_nodes as i64))
            });
        }
    }
    let mut variant = 0u64;
    deploy(&nodes[0].http, &example_svcs(variant));
    // Wait until echo answers on every ingress.
    for n in &nodes {
        wait_for(&format!("{} serves echo", n.name), Duration::from_secs(40), || {
            http::request(&n.http, "GET", "/echo", b"", Duration::from_secs(5)).is_ok_and(|r| r.status == 200)
        });
    }

    let targets: Vec<String> = nodes.iter().map(|n| n.http.clone()).collect();
    let mix = Mix::start(&targets, 4);

    let mut series = Series::default();
    let start = Instant::now();
    let mut last_reapply = Instant::now();
    let pid0 = nodes[0].pid();
    while start.elapsed() < duration {
        std::thread::sleep(SAMPLE_EVERY);
        if last_reapply.elapsed() >= REAPPLY_EVERY {
            last_reapply = Instant::now();
            variant += 1;
            let (st, _b) = apply_raw(&nodes[0].http, &bundle(0, &example_svcs(variant)).encode(), Duration::from_secs(60));
            if st != 200 {
                eprintln!("[soak] re-apply status {st}");
            }
        }
        let Some(p) = ProcSample::read(pid0) else { continue };
        let s = nodes[0].status();
        let kv_log = status_metric(&s, "kv", "log_bytes");
        let instances = status_metric(&s, "exec", "instances");
        let fibers = status_metric(&s, "exec", "fibers");
        let t = start.elapsed().as_secs_f64();
        series.push(t, &p, kv_log, instances, fibers);
        eprintln!("[soak] t={t:.0}s rss={}MiB fds={} threads={} vmas={} exec_anon={} kv_log={}KiB inst={} fib={}", p.rss_kb / 1024, p.fds, p.threads, p.vmas, p.exec_anon, kv_log / 1024, instances, fibers);
    }
    let (ok, err, breakdown) = mix.finish();

    // Write CSV + summary.
    let ts = k10_node::sys::mono_ns();
    let rd = results_dir(tag, &dir);
    let csv = rd.join(format!("{tag}-{ts}.csv"));
    series.write_csv(&csv);
    let mut summary = String::new();
    use std::fmt::Write;
    let _ = writeln!(summary, "# k10s soak: {tag} ({} nodes), {:.0}s, load ok={ok} err={err}", cluster_nodes, duration.as_secs_f64());
    let _ = writeln!(summary, "# backend: {}", std::env::var("K10S_BACKEND").unwrap_or_else(|_| "jit".into()));
    if !breakdown.is_empty() {
        let _ = writeln!(summary, "# load errors by kind: {}", breakdown.iter().take(8).map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(", "));
    }
    let trends = summarize(&series, tag, &mut summary);
    let mut violations = Vec::new();
    assert_no_leak(&trends, &mut violations);
    if ok == 0 {
        violations.push("no successful requests".into());
    }
    let _ = writeln!(summary, "\nviolations: {}", if violations.is_empty() { "none".into() } else { violations.join("; ") });
    std::fs::write(rd.join(format!("{tag}-{ts}.md")), &summary).unwrap();
    eprintln!("{summary}");
    eprintln!("[soak] wrote {}", csv.display());

    let _ = std::fs::remove_dir_all(&dir);
    violations
}

#[test]
fn soak_short_single() {
    let v = run_soak("single-short", 1, Duration::from_secs(120));
    assert!(v.is_empty(), "leak/soak violations: {v:#?}");
}

#[test]
fn soak_short_cluster() {
    let v = run_soak("cluster3-short", 3, Duration::from_secs(120));
    assert!(v.is_empty(), "leak/soak violations: {v:#?}");
}

#[test]
#[ignore = "long soak (>= 30 min); set K10S_SOAK_SECS"]
fn soak_long_single() {
    let secs: u64 = std::env::var("K10S_SOAK_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(1800);
    let v = run_soak("single-long", 1, Duration::from_secs(secs));
    assert!(v.is_empty(), "leak/soak violations: {v:#?}");
}

#[test]
#[ignore = "long soak (>= 30 min); set K10S_SOAK_SECS"]
fn soak_long_cluster() {
    let secs: u64 = std::env::var("K10S_SOAK_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(1800);
    let v = run_soak("cluster3-long", 3, Duration::from_secs(secs));
    assert!(v.is_empty(), "leak/soak violations: {v:#?}");
}
