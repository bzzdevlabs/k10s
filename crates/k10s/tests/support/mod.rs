//! Shared harness of the reliability suites (`abuse.rs`, `chaos.rs`, `soak.rs`): real `k10s node` processes,
//! bundles built in-process (Ten sources compiled with `k10-lang`, raw TIR from `k10_ir::builder`), a latency
//! probe, `/_k10s/status` field extraction (no JSON library in the workspace) and `/proc` sampling.
//!
//! `K10S_BIN` overrides the node binary (long runs use a copy, so rebuilding the workspace does not swap the
//! binary under a running chaos/soak test).
#![allow(dead_code)]

use k10_mesh::{Bundle, Manifest, ServiceSpec};
use k10_node::http::{self, Client};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering::*};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

pub fn bin() -> PathBuf {
    std::env::var_os("K10S_BIN").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_k10s")))
}

pub fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("k10s-rel-{tag}-{}-{}", std::process::id(), k10_node::sys::mono_ns()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A cluster key file shared by every test node of this process (fresh random key, mode 0600).
pub fn key_file() -> &'static Path {
    static F: OnceLock<PathBuf> = OnceLock::new();
    F.get_or_init(|| {
        use std::os::unix::fs::OpenOptionsExt;
        use std::io::Write;
        let p = std::env::temp_dir().join(format!("k10s-test-key-{}-{}", std::process::id(), k10_node::sys::mono_ns()));
        let mut k = [0u8; 32];
        k10_node::sys::fill_random(&mut k).unwrap();
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&p).unwrap();
        writeln!(f, "{}", k10_mesh::sha256::to_hex(&k)).unwrap();
        p
    })
}

/// The cluster key of [`key_file`].
pub fn cluster_key() -> k10_mesh::secure::ClusterKey {
    k10_mesh::secure::ClusterKey::parse_hex(&std::fs::read_to_string(key_file()).unwrap()).unwrap()
}

/// Cluster-mode arguments get the test cluster key unless they choose their own authentication.
pub fn with_cluster_key(args: &mut Vec<String>) {
    let cluster = args.iter().any(|a| ["--gossip", "--rpc", "--join", "--advertise"].contains(&a.as_str()));
    let chosen = args.iter().any(|a| a == "--cluster-key-file" || a == "--insecure-unauthenticated-cluster");
    if cluster && !chosen {
        args.extend(["--cluster-key-file".into(), key_file().display().to_string()]);
    }
}

/// Handshakes a raw peer connection as member `id` with the test key and sends its `Hello`.
pub fn peer_handshake(s: &mut std::net::TcpStream, id: u64) -> k10_mesh::secure::SecureLink {
    use k10_mesh::secure::{SecureLink, AUTH_FRAME_LEN};
    use k10_mesh::wire::{FrameDecoder, Msg};
    use std::io::{Read, Write};
    let mut nonce = [0u8; 32];
    k10_node::sys::fill_random(&mut nonce).unwrap();
    let mut sec = SecureLink::new(&cluster_key(), k10_mesh::NodeId(id), nonce);
    let mut out = Vec::new();
    sec.start(&mut out);
    s.write_all(&out).unwrap();
    out.clear();
    let mut dec = FrameDecoder::new(AUTH_FRAME_LEN);
    let mut buf = [0u8; 256];
    let payload = loop {
        if let Some(f) = dec.next().unwrap() {
            break f.payload.to_vec();
        }
        let n = s.read(&mut buf).unwrap();
        assert!(n > 0, "node closed the link during the handshake");
        dec.feed(&buf[..n]);
    };
    sec.on_auth(&payload, None, &mut out).unwrap();
    let a: k10_mesh::Addr = "127.0.0.1:1".parse().unwrap();
    let me = k10_mesh::Member::new(k10_mesh::NodeId(id), a, a, a, k10_mesh::Capacity { mem_bytes: 1 << 30, cores: 1 });
    sec.push(&Msg::Hello { member: me }, 0, &mut out).unwrap();
    sec.seal_pending(&mut out);
    s.write_all(&out).unwrap();
    sec
}

pub fn wait_for(what: &str, timeout: Duration, mut f: impl FnMut() -> bool) -> Duration {
    let t = Instant::now();
    loop {
        if f() {
            return t.elapsed();
        }
        assert!(t.elapsed() < timeout, "timed out after {timeout:?} waiting for: {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

// ------------------------------------------------------------------------------------------------------------------
// Node processes
// ------------------------------------------------------------------------------------------------------------------

pub struct NodeProc {
    pub child: Option<Child>,
    pub name: String,
    /// Cluster node id (empty in single-node mode).
    pub id: String,
    pub http: String,
    pub gossip: String,
    pub data: PathBuf,
    pub args: Vec<String>,
    pub log: Arc<Mutex<String>>,
}

impl NodeProc {
    pub fn pid(&self) -> i32 {
        self.child.as_ref().map(|c| c.id() as i32).unwrap_or(0)
    }

    pub fn signal(&self, sig: i32) {
        if self.child.is_some() {
            // SAFETY: plain syscall on our own child.
            unsafe { libc::kill(self.pid(), sig) };
        }
    }

    /// The process is still running (not exited, not a zombie we have reaped). Returns the exit status otherwise.
    pub fn exited(&mut self) -> Option<std::process::ExitStatus> {
        self.child.as_mut().and_then(|c| c.try_wait().ok().flatten())
    }

    pub fn kill(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }

    /// SIGTERM and wait (graceful shutdown / leave). Returns the exit status.
    pub fn terminate(&mut self, timeout: Duration) -> Option<std::process::ExitStatus> {
        let mut c = self.child.take()?;
        // SAFETY: plain syscall on our own child.
        unsafe { libc::kill(c.id() as i32, libc::SIGTERM) };
        let t = Instant::now();
        loop {
            if let Ok(Some(st)) = c.try_wait() {
                return Some(st);
            }
            if t.elapsed() > timeout {
                let _ = c.kill();
                let _ = c.wait();
                return None;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    pub fn get(&self, path: &str) -> std::io::Result<http::ClientResponse> {
        http::request(&self.http, "GET", path, b"", Duration::from_secs(10))
    }

    pub fn status(&self) -> String {
        self.get("/_k10s/status").map(|r| r.text()).unwrap_or_default()
    }

    pub fn healthy(&self) -> bool {
        http::request(&self.http, "GET", "/_k10s/health", b"", Duration::from_secs(5)).is_ok_and(|r| r.status == 200)
    }

    pub fn log(&self) -> String {
        self.log.lock().unwrap().clone()
    }

    pub fn proc_sample(&self) -> Option<ProcSample> {
        ProcSample::read(self.pid())
    }
}

impl Drop for NodeProc {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Spawns `k10s node` with `args` (after `node`). Single-node mode unless `args` contain cluster flags. The log
/// (stderr) is kept in memory, bounded to its last 4 MiB.
pub fn spawn(name: &str, data: &Path, args: &[String]) -> NodeProc {
    let mut all: Vec<String> = vec!["node".into(), "--name".into(), name.into(), "--data".into(), data.display().to_string()];
    all.extend(args.iter().cloned());
    with_cluster_key(&mut all);
    let mut child = Command::new(bin()).args(&all).stderr(Stdio::piped()).stdout(Stdio::null()).spawn().expect("spawn k10s node");
    let log = Arc::new(Mutex::new(String::new()));
    let (tx, rx) = mpsc::channel::<(String, String, String)>();
    {
        let log = log.clone();
        let stderr = child.stderr.take().unwrap();
        std::thread::spawn(move || {
            let mut id = String::new();
            let mut gossip = String::new();
            for line in BufReader::new(stderr).lines() {
                let Ok(line) = line else { break };
                // "k10s: cluster node ID: gossip G rpc R http H (bound …)"
                if let Some(rest) = line.strip_prefix("k10s: cluster node ") {
                    let w: Vec<&str> = rest.split_whitespace().collect();
                    if w.len() >= 3 {
                        id = w[0].trim_end_matches(':').to_string();
                        gossip = w[2].to_string();
                    }
                }
                // "k10s: node `n` listening on http://ADDR (…)"
                if let Some(at) = line.find("listening on http://") {
                    let addr = line[at + "listening on http://".len()..].split_whitespace().next().unwrap_or("").to_string();
                    let _ = tx.send((addr, id.clone(), gossip.clone()));
                }
                let mut l = log.lock().unwrap();
                if l.len() > 4 << 20 {
                    let cut = l.len() - (2 << 20);
                    let cut = (cut..l.len()).find(|&i| l.is_char_boundary(i)).unwrap_or(l.len());
                    l.drain(..cut);
                }
                l.push_str(&line);
                l.push('\n');
            }
        });
    }
    let (http, id, gossip) = match rx.recv_timeout(Duration::from_secs(30)) {
        Ok(v) => v,
        Err(_) => {
            let _ = child.kill();
            panic!("node {name} did not start; log:\n{}", log.lock().unwrap());
        }
    };
    let n = NodeProc { child: Some(child), name: name.into(), id, http, gossip, data: data.to_path_buf(), args: args.to_vec(), log };
    wait_for(&format!("{name} health"), Duration::from_secs(20), || n.healthy());
    n
}

/// A single node on an ephemeral port with `shards` shards (backend: `K10S_BACKEND`, else jit).
pub fn single(tag: &str, shards: usize, extra: &[&str]) -> (PathBuf, NodeProc) {
    let dir = tmpdir(tag);
    let mut args: Vec<String> = vec!["--http".into(), "127.0.0.1:0".into(), "--shards".into(), shards.to_string()];
    args.extend(extra.iter().map(|s| s.to_string()));
    let n = spawn(tag, &dir.join("data"), &args);
    (dir, n)
}

// ------------------------------------------------------------------------------------------------------------------
// Bundles
// ------------------------------------------------------------------------------------------------------------------

/// Compiles Ten source to an encoded TIR blob (panics with the diagnostics).
pub fn ten(src: &str) -> Vec<u8> {
    let mut s = k10_lang::Sources::default();
    match k10_lang::compile_with_sources(src, None, &mut s) {
        Ok(m) => k10_ir::encode(&m),
        Err(d) => panic!("Ten compile error:\n{}\n{src}", k10_lang::render_diagnostics(&d, &s)),
    }
}

pub fn ten_file(path: &Path) -> Vec<u8> {
    k10_ir::encode(&k10_lang::compile_file(path).unwrap_or_else(|d| panic!("{}: {d:?}", path.display())))
}

#[derive(Clone, Debug)]
pub struct Svc {
    pub name: String,
    pub code: Vec<u8>,
    pub mem_log2: u8,
    pub max_instances: u32,
    pub cpu_ms: u32,
    pub fresh: bool,
    pub replicas: u32,
    pub config: Vec<u8>,
    pub calls: k10_mesh::Calls,
}

impl Svc {
    pub fn new(name: &str, code: Vec<u8>) -> Svc {
        Svc {
            name: name.into(),
            code,
            mem_log2: 16,
            max_instances: 64,
            cpu_ms: 1000,
            fresh: false,
            replicas: 1,
            config: vec![],
            calls: Default::default(),
        }
    }
    /// The services this one may `call` (manifest allowlist).
    pub fn calls(mut self, names: &[&str]) -> Svc {
        self.calls = k10_mesh::Calls::from_list(names).expect("valid callees");
        self
    }
    pub fn mem(mut self, log2: u8) -> Svc {
        self.mem_log2 = log2;
        self
    }
    pub fn max(mut self, n: u32) -> Svc {
        self.max_instances = n;
        self
    }
    pub fn cpu(mut self, ms: u32) -> Svc {
        self.cpu_ms = ms;
        self
    }
    pub fn fresh(mut self) -> Svc {
        self.fresh = true;
        self
    }
    pub fn replicas(mut self, n: u32) -> Svc {
        self.replicas = n;
        self
    }
    pub fn config(mut self, c: &[u8]) -> Svc {
        self.config = c.to_vec();
        self
    }
}

pub fn bundle(version: u64, svcs: &[Svc]) -> Bundle {
    let mut services: Vec<ServiceSpec> = svcs
        .iter()
        .map(|s| ServiceSpec {
            name: s.name.clone(),
            code_hash: k10_mesh::sha256::sha256(&s.code),
            replicas: s.replicas,
            mem_log2: s.mem_log2,
            max_instances: s.max_instances,
            cpu_ms: s.cpu_ms,
            fresh: s.fresh,
            config: s.config.clone(),
            calls: s.calls.clone(),
        })
        .collect();
    services.sort_by(|a, b| a.name.cmp(&b.name));
    let mut blobs: Vec<Vec<u8>> = svcs.iter().map(|s| s.code.clone()).collect();
    blobs.sort();
    blobs.dedup();
    Bundle::new(Manifest { version, services }, blobs)
}

/// `POST /_k10s/apply` of raw bytes; returns (status, body).
pub fn apply_raw(addr: &str, bytes: &[u8], timeout: Duration) -> (u16, String) {
    match http::request(addr, "POST", "/_k10s/apply", bytes, timeout) {
        Ok(r) => (r.status, r.text()),
        Err(e) => (0, format!("transport error: {e}")),
    }
}

/// Applies `svcs` (version 0 = current + 1) and asserts success.
pub fn deploy(addr: &str, svcs: &[Svc]) -> String {
    let (st, body) = apply_raw(addr, &bundle(0, svcs).encode(), Duration::from_secs(120));
    assert_eq!(st, 200, "apply failed: {body}");
    body
}

/// The well-behaved neighbour every abuse scenario watches.
pub const ECHO_TEN: &str = include_str!("../../../../examples/echo.ten");

pub fn echo_svc() -> Svc {
    Svc::new("echo", ten(ECHO_TEN)).mem(16)
}

// ------------------------------------------------------------------------------------------------------------------
// Latency probe
// ------------------------------------------------------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
pub struct Lat {
    pub n: usize,
    pub errors: usize,
    pub p50_us: u64,
    pub p99_us: u64,
    pub max_us: u64,
    pub first_error: Option<String>,
}

impl std::fmt::Display for Lat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "n={} err={} p50={}us p99={}us max={}us", self.n, self.errors, self.p50_us, self.p99_us, self.max_us)
    }
}

pub fn lat_of(mut v: Vec<u64>, errors: usize, first_error: Option<String>) -> Lat {
    v.sort_unstable();
    let pick = |q: f64| if v.is_empty() { 0 } else { v[((v.len() - 1) as f64 * q).round() as usize] };
    Lat { n: v.len(), errors, p50_us: pick(0.5), p99_us: pick(0.99), max_us: v.last().copied().unwrap_or(0), first_error }
}

/// Closed-loop GETs of `path` on one keep-alive connection, `pause` between requests; every answer must be
/// `200 expect`.
pub struct Probe {
    stop: Arc<AtomicBool>,
    h: Option<std::thread::JoinHandle<Lat>>,
}

impl Probe {
    pub fn start(addr: &str, path: &str, expect: &'static str, pause: Duration) -> Probe {
        let stop = Arc::new(AtomicBool::new(false));
        let (addr, path, st) = (addr.to_string(), path.to_string(), stop.clone());
        let h = std::thread::spawn(move || {
            let mut lats = Vec::new();
            let mut errors = 0;
            let mut first = None;
            let mut c: Option<Client> = None;
            while !st.load(Relaxed) {
                let t = Instant::now();
                if c.is_none() {
                    c = Client::connect(&addr, Duration::from_secs(10)).ok();
                }
                let r = match c.as_mut() {
                    Some(cl) => cl.request("GET", &path, b""),
                    None => Err(std::io::Error::other("connect failed")),
                };
                let us = t.elapsed().as_micros() as u64;
                match r {
                    Ok(r) if r.status == 200 && r.body == expect.as_bytes() => {
                        lats.push(us);
                        if r.close {
                            c = None;
                        }
                    }
                    other => {
                        errors += 1;
                        if first.is_none() {
                            first = Some(match other {
                                Ok(r) => format!("{} {:?}", r.status, r.text()),
                                Err(e) => e.to_string(),
                            });
                        }
                        c = None;
                    }
                }
                if !pause.is_zero() {
                    std::thread::sleep(pause);
                }
            }
            lat_of(lats, errors, first)
        });
        Probe { stop, h: Some(h) }
    }

    pub fn stop(mut self) -> Lat {
        self.stop.store(true, Relaxed);
        self.h.take().unwrap().join().unwrap()
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        self.stop.store(true, Relaxed);
        if let Some(h) = self.h.take() {
            let _ = h.join();
        }
    }
}

/// Measures the probe for `d`.
pub fn measure(addr: &str, d: Duration) -> Lat {
    let p = Probe::start(addr, "/echo", "hello", Duration::from_millis(1));
    std::thread::sleep(d);
    p.stop()
}

// ------------------------------------------------------------------------------------------------------------------
// Status parsing
// ------------------------------------------------------------------------------------------------------------------

pub fn num_after(s: &str, key: &str) -> Option<i64> {
    let k = format!("\"{key}\":");
    let at = s.find(&k)? + k.len();
    let num: String = s[at..].chars().take_while(|c| c.is_ascii_digit() || *c == '-').collect();
    num.parse().ok()
}

/// A field of the `"memory"` / `"exec"` / `"kv"` / `"code"` / `"http_stats"` object of a status document.
pub fn section_num(status: &str, section: &str, key: &str) -> Option<i64> {
    let at = status.find(&format!("\"{section}\":{{"))?;
    let s = &status[at..];
    let end = s.find('}').unwrap_or(s.len());
    num_after(&s[..end], key)
}

/// The JSON object of service `name` (current version) in a status document.
pub fn svc_part<'a>(status: &'a str, name: &str) -> Option<&'a str> {
    let at = status.find(&format!("{{\"name\":\"{name}\""))?;
    let s = &status[at..];
    let end = s.find("\"retired\":").map(|e| e + 20).unwrap_or(s.len());
    Some(&s[..end.min(s.len())])
}

pub fn svc_num(status: &str, name: &str, key: &str) -> Option<i64> {
    num_after(svc_part(status, name)?, key)
}

// ------------------------------------------------------------------------------------------------------------------
// /proc sampling
// ------------------------------------------------------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
pub struct ProcSample {
    pub rss_kb: u64,
    pub threads: u64,
    pub fds: u64,
    pub vmas: u64,
    /// Executable mappings without a backing file (JIT code).
    pub exec_anon: u64,
}

impl ProcSample {
    pub fn read(pid: i32) -> Option<ProcSample> {
        if pid <= 0 {
            return None;
        }
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
        let field = |k: &str| {
            status
                .lines()
                .find(|l| l.starts_with(k))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|v| v.parse().ok())
                .unwrap_or(0)
        };
        let maps = std::fs::read_to_string(format!("/proc/{pid}/maps")).ok()?;
        let mut vmas = 0;
        let mut exec_anon = 0;
        for l in maps.lines() {
            vmas += 1;
            let w: Vec<&str> = l.split_whitespace().collect();
            if w.len() >= 2 && w[1].contains('x') && (w.len() < 6 || w[5].starts_with("[anon")) {
                exec_anon += 1;
            }
        }
        let fds = std::fs::read_dir(format!("/proc/{pid}/fd")).ok()?.count() as u64;
        Some(ProcSample { rss_kb: field("VmRSS:"), threads: field("Threads:"), fds, vmas, exec_anon })
    }
}

/// Least-squares slope of `ys` over `xs` (per unit of x) and the fit's R².
pub fn slope(xs: &[f64], ys: &[f64]) -> (f64, f64) {
    let n = xs.len() as f64;
    if xs.len() < 2 {
        return (0.0, 0.0);
    }
    let mx = xs.iter().sum::<f64>() / n;
    let my = ys.iter().sum::<f64>() / n;
    let sxy: f64 = xs.iter().zip(ys).map(|(x, y)| (x - mx) * (y - my)).sum();
    let sxx: f64 = xs.iter().map(|x| (x - mx) * (x - mx)).sum();
    let syy: f64 = ys.iter().map(|y| (y - my) * (y - my)).sum();
    if sxx == 0.0 {
        return (0.0, 0.0);
    }
    let b = sxy / sxx;
    let r2 = if syy == 0.0 { 1.0 } else { (sxy * sxy) / (sxx * syy) };
    (b, r2)
}

/// Deterministic PRNG (splitmix64) for seeded drivers.
#[derive(Clone)]
pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next() % n
        }
    }
    pub fn chance(&mut self, p: f64) -> bool {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64 <= p
    }
}
