//! `k10s` — the single CLI/daemon binary.
//!
//! ```text
//! k10s node   [--http ADDR] [--data DIR] [--shards N] [--backend interp] [--name NAME] [--mem SIZE]
//!             [--snapshots fresh|all] [--idle-timeout SECS] [--idle-spin-us N] [--log-stderr] [--pin]
//!             cluster mode (any of): [--gossip ADDR] [--rpc ADDR] [--join HOST:PORT[,…]] [--advertise HOST]
//!             tuning: [--probe-ms N] [--suspicion-mult N] [--leave-grace-ms N]
//! k10s apply  FILE.k10s [--node HOST:PORT]
//! k10s status [--node HOST:PORT] [--json]
//! k10s logs   SERVICE [--node HOST:PORT]
//! ```
//! Argument parsing is hand-written (no dependencies beyond the workspace).

use k10_mesh::manifest::parse_text;
use k10_mesh::sha256::sha256;
use k10_mesh::{Bundle, Manifest};
use k10_node::cluster::{ClusterConfig, ClusterSetup};
use k10_node::http;
use k10_node::{BackendKind, Node, NodeConfig, SnapshotPolicy};
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

const USAGE: &str = "usage:
  k10s node   [--http ADDR] [--data DIR] [--shards N] [--backend interp] [--name NAME] [--mem SIZE]
              [--snapshots fresh|all] [--idle-timeout SECS] [--idle-spin-us N] [--log-stderr] [--pin]
              cluster mode (any of these flags turns it on):
              [--gossip ADDR] (UDP, default 0.0.0.0:7946)  [--rpc ADDR] (TCP, default 0.0.0.0:7947)
              [--join HOST:PORT,...] (gossip addresses of members)  [--advertise HOST] (IP given to peers)
              [--probe-ms N] (gossip probe period, 200)  [--suspicion-mult N] (4)  [--leave-grace-ms N] (1000)
  k10s apply  FILE.k10s [--node HOST:PORT]
  k10s status [--node HOST:PORT] [--json]
  k10s logs   SERVICE [--node HOST:PORT]

The default node address is 127.0.0.1:8080. In a cluster, `apply` and HTTP requests work on any node.";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = args.first() else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };
    let rest = &args[1..];
    let r = match cmd.as_str() {
        "node" => cmd_node(rest),
        "apply" => cmd_apply(rest),
        "status" => cmd_status(rest),
        "logs" => cmd_logs(rest),
        "help" | "--help" | "-h" => {
            println!("{USAGE}");
            Ok(())
        }
        other => Err(format!("unknown command `{other}`\n{USAGE}")),
    };
    match r {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("k10s: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Minimal flag parser: `--key value`, `--key=value`, boolean `--flag`, and positionals.
struct Args {
    flags: Vec<(String, Option<String>)>,
    positional: Vec<String>,
}

impl Args {
    fn parse(args: &[String], booleans: &[&str]) -> Result<Args, String> {
        let mut flags = Vec::new();
        let mut positional = Vec::new();
        let mut i = 0;
        while i < args.len() {
            let a = &args[i];
            if let Some(f) = a.strip_prefix("--") {
                if let Some((k, v)) = f.split_once('=') {
                    flags.push((k.to_string(), Some(v.to_string())));
                } else if booleans.contains(&f) {
                    flags.push((f.to_string(), None));
                } else {
                    let v = args.get(i + 1).ok_or_else(|| format!("--{f} needs a value"))?;
                    flags.push((f.to_string(), Some(v.clone())));
                    i += 1;
                }
            } else {
                positional.push(a.clone());
            }
            i += 1;
        }
        Ok(Args { flags, positional })
    }

    fn get(&self, k: &str) -> Option<&str> {
        self.flags.iter().rev().find(|(n, _)| n == k).and_then(|(_, v)| v.as_deref())
    }

    fn has(&self, k: &str) -> bool {
        self.flags.iter().any(|(n, _)| n == k)
    }

    fn check_known(&self, known: &[&str]) -> Result<(), String> {
        for (k, _) in &self.flags {
            if !known.contains(&k.as_str()) {
                return Err(format!("unknown flag --{k}"));
            }
        }
        Ok(())
    }
}

/// Parses `8GiB`, `512MiB`, `64KiB`, `1G`, or plain bytes.
fn parse_size(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let n: u64 = num.parse().map_err(|_| format!("invalid size `{s}`"))?;
    let mul: u64 = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1 << 10,
        "m" | "mb" | "mib" => 1 << 20,
        "g" | "gb" | "gib" => 1 << 30,
        "t" | "tb" | "tib" => 1 << 40,
        _ => return Err(format!("invalid size unit in `{s}`")),
    };
    n.checked_mul(mul).ok_or_else(|| format!("size `{s}` overflows"))
}

fn cmd_node(args: &[String]) -> Result<(), String> {
    let a = Args::parse(args, &["log-stderr", "pin"])?;
    a.check_known(&[
        "http", "data", "shards", "backend", "name", "mem", "snapshots", "idle-timeout", "idle-spin-us", "log-stderr", "pin",
        "rpc", "gossip", "join", "advertise", "probe-ms", "suspicion-mult", "leave-grace-ms",
    ])?;
    if !a.positional.is_empty() {
        return Err(format!("unexpected argument `{}`", a.positional[0]));
    }
    let mut cfg = NodeConfig::default();
    if let Some(h) = a.get("http") {
        cfg.http = h.parse().map_err(|_| format!("invalid --http address `{h}` (want IP:PORT)"))?;
    }
    cfg.data_dir = a.get("data").map(PathBuf::from);
    if let Some(n) = a.get("shards") {
        cfg.shards = n.parse().ok().filter(|n: &usize| (1..=1024).contains(n)).ok_or("invalid --shards")?;
    }
    match a.get("backend").unwrap_or("interp") {
        "interp" => cfg.backend = BackendKind::Interp,
        "jit" => return Err("--backend jit: the JIT backend (k10-jit) is not available yet; use --backend interp".into()),
        b => return Err(format!("unknown backend `{b}` (interp)")),
    }
    if let Some(n) = a.get("name") {
        cfg.name = n.to_string();
    }
    if let Some(m) = a.get("mem") {
        cfg.mem_limit = parse_size(m)?;
    }
    match a.get("snapshots") {
        None | Some("fresh") => cfg.snapshots = SnapshotPolicy::Fresh,
        Some("all") => cfg.snapshots = SnapshotPolicy::All,
        Some(o) => return Err(format!("invalid --snapshots `{o}` (fresh|all)")),
    }
    if let Some(s) = a.get("idle-timeout") {
        let secs: f64 = s.parse().map_err(|_| "invalid --idle-timeout")?;
        cfg.idle_timeout = Duration::from_secs_f64(secs);
    }
    if let Some(s) = a.get("idle-spin-us") {
        cfg.idle_spin = Duration::from_micros(s.parse().map_err(|_| "invalid --idle-spin-us")?);
    }
    cfg.log_stderr = a.has("log-stderr");
    cfg.pin_threads = a.has("pin");
    let cluster_cfg = cluster_config(&a)?;
    // Block SIGINT/SIGTERM in every thread (inherited by the threads the node spawns) and wait for them here.
    let set = signal_set();
    // SAFETY: plain libc calls on a properly initialized sigset_t.
    unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()) };
    let shards = cfg.shards;
    let data = cfg.data_dir.clone();
    let (node, cluster) = match cluster_cfg {
        None => (Node::start(cfg).map_err(|e| format!("cannot start node: {e}"))?, None),
        Some(cc) => {
            // Cluster mode keeps its own store (manifest + blobs); the single-node bundle file is not used.
            cfg.reload_bundle = false;
            let setup = ClusterSetup::bind(cc, data.as_deref()).map_err(|e| format!("cannot start cluster mode: {e}"))?;
            let node = Node::start_with_router(cfg, setup.router()).map_err(|e| format!("cannot start node: {e}"))?;
            let cluster = setup.start(node.shared().clone()).map_err(|e| format!("cannot start cluster mode: {e}"))?;
            (node, Some(cluster))
        }
    };
    eprintln!(
        "k10s: node `{}` listening on http://{} ({} shards, backend interp, data {})",
        node.shared().cfg.name,
        node.http_addr(),
        shards,
        data.map(|d| d.display().to_string()).unwrap_or_else(|| "none (memory only)".into())
    );
    let mut sig: libc::c_int = 0;
    // SAFETY: `set` is valid; sigwait writes the received signal number.
    unsafe { libc::sigwait(&set, &mut sig) };
    eprintln!("k10s: signal {sig}, shutting down");
    if let Some(c) = &cluster {
        c.leave();
    }
    node.shutdown();
    if let Some(c) = cluster {
        c.stop();
    }
    Ok(())
}

/// `IP:PORT`, or a bare port (all interfaces).
fn parse_bind(flag: &str, v: &str) -> Result<SocketAddr, String> {
    if let Ok(a) = v.parse::<SocketAddr>() {
        return Ok(a);
    }
    if let Ok(p) = v.parse::<u16>() {
        return Ok(SocketAddr::from(([0, 0, 0, 0], p)));
    }
    Err(format!("invalid --{flag} address `{v}` (want IP:PORT or PORT)"))
}

/// Cluster mode is on when any cluster flag is given.
fn cluster_config(a: &Args) -> Result<Option<ClusterConfig>, String> {
    if !["gossip", "rpc", "join", "advertise"].iter().any(|f| a.has(f)) {
        for f in ["probe-ms", "suspicion-mult", "leave-grace-ms"] {
            if a.has(f) {
                return Err(format!("--{f} needs cluster mode (--gossip/--rpc/--join/--advertise)"));
            }
        }
        return Ok(None);
    }
    let mut c = ClusterConfig::default();
    if let Some(v) = a.get("gossip") {
        c.gossip_bind = parse_bind("gossip", v)?;
    }
    if let Some(v) = a.get("rpc") {
        c.rpc_bind = parse_bind("rpc", v)?;
    }
    if let Some(v) = a.get("advertise") {
        c.advertise = Some(match v.parse::<IpAddr>() {
            Ok(ip) => ip,
            Err(_) => (v, 0)
                .to_socket_addrs()
                .ok()
                .and_then(|mut it| it.next())
                .map(|sa| sa.ip())
                .ok_or_else(|| format!("cannot resolve --advertise `{v}`"))?,
        });
    }
    if let Some(v) = a.get("join") {
        for s in v.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            let with_port = if s.parse::<SocketAddr>().is_ok() || s.rsplit_once(':').is_some_and(|(_, p)| p.parse::<u16>().is_ok()) {
                s.to_string()
            } else {
                format!("{s}:7946")
            };
            let addr = with_port
                .to_socket_addrs()
                .ok()
                .and_then(|mut it| it.next())
                .ok_or_else(|| format!("cannot resolve --join address `{s}`"))?;
            c.join.push(addr);
        }
    }
    if let Some(v) = a.get("probe-ms") {
        let ms: u64 = v.parse().ok().filter(|&n| n >= 10).ok_or("invalid --probe-ms (>= 10)")?;
        c.gossip.probe_interval_ms = ms;
        c.gossip.probe_timeout_ms = (ms * 2 / 5).max(5);
        c.gossip.gossip_interval_ms = ms;
    }
    if let Some(v) = a.get("suspicion-mult") {
        c.gossip.suspicion_mult = v.parse().ok().filter(|&n| n >= 1).ok_or("invalid --suspicion-mult (>= 1)")?;
    }
    if let Some(v) = a.get("leave-grace-ms") {
        c.leave_grace = Duration::from_millis(v.parse().map_err(|_| "invalid --leave-grace-ms")?);
    }
    Ok(Some(c))
}

fn signal_set() -> libc::sigset_t {
    // SAFETY: sigemptyset initializes the set; sigaddset only writes into it.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGINT);
        libc::sigaddset(&mut set, libc::SIGTERM);
        set
    }
}

/// Turns a manifest `code = "…"` entry into a TIR blob.
///
/// * `*.k10` — precompiled TIR, checked locally (decode + validate) for early errors.
/// * `*.ten` — Ten sources: HOOK for the Ten compiler (`k10-lang`), not available yet.
fn load_code(path: &Path) -> Result<Vec<u8>, String> {
    match path.extension().and_then(|e| e.to_str()) {
        Some("ten") => compile_ten(path),
        _ => {
            let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
            let m = k10_ir::decode(&bytes).map_err(|e| format!("{}: not a TIR module: {e}", path.display()))?;
            k10_ir::validate(&m).map_err(|e| format!("{}: invalid module: {e}", path.display()))?;
            Ok(bytes)
        }
    }
}

/// HOOK: compile a Ten source file to a TIR blob with `k10-lang` once it exists (`k10_lang::compile(src) ->
/// Result<k10_ir::Module, _>`, then `k10_ir::encode`).
fn compile_ten(path: &Path) -> Result<Vec<u8>, String> {
    Err(format!(
        "{}: compiling Ten sources needs the Ten compiler (k10-lang), which is not wired in yet; precompile to .k10",
        path.display()
    ))
}

fn node_addr(a: &Args) -> String {
    a.get("node").unwrap_or("127.0.0.1:8080").to_string()
}

fn cmd_apply(args: &[String]) -> Result<(), String> {
    let a = Args::parse(args, &[])?;
    a.check_known(&["node"])?;
    let [file] = a.positional.as_slice() else { return Err("usage: k10s apply FILE.k10s [--node HOST:PORT]".into()) };
    let path = Path::new(file);
    let src = std::fs::read_to_string(path).map_err(|e| format!("{file}: {e}"))?;
    let decls = parse_text(&src).map_err(|e| format!("{file}: {e}"))?;
    let base = path.parent().unwrap_or(Path::new("."));
    let mut services = Vec::new();
    let mut blobs: Vec<Vec<u8>> = Vec::new();
    for d in &decls {
        if d.code.is_empty() {
            return Err(format!("{file}: line {}: service `{}` has no `code`", d.line, d.name));
        }
        let blob = load_code(&base.join(&d.code))?;
        services.push(d.to_spec(sha256(&blob)));
        if !blobs.contains(&blob) {
            blobs.push(blob);
        }
    }
    // Version 0: the node assigns current + 1.
    let bundle = Bundle::new(Manifest { version: 0, services }, blobs);
    let bytes = bundle.encode();
    let addr = node_addr(&a);
    let r = http::request(&addr, "POST", "/_k10s/apply", &bytes, Duration::from_secs(60))
        .map_err(|e| format!("cannot reach node {addr}: {e}"))?;
    println!("{}", r.text());
    if r.status == 200 {
        Ok(())
    } else {
        Err(format!("apply failed (HTTP {})", r.status))
    }
}

fn cmd_status(args: &[String]) -> Result<(), String> {
    let a = Args::parse(args, &["json"])?;
    a.check_known(&["node", "json"])?;
    let addr = node_addr(&a);
    if !a.has("json") {
        // Cluster mode: the cluster view (members, desired/applied version, placement, links).
        let r = http::request(&addr, "GET", "/_k10s/cluster", b"", Duration::from_secs(10))
            .map_err(|e| format!("cannot reach node {addr}: {e}"))?;
        if r.status == 200 {
            print!("{}", r.text());
            return Ok(());
        }
    }
    let r = http::request(&addr, "GET", "/_k10s/status", b"", Duration::from_secs(10))
        .map_err(|e| format!("cannot reach node {addr}: {e}"))?;
    println!("{}", r.text());
    if r.status == 200 {
        Ok(())
    } else {
        Err(format!("HTTP {}", r.status))
    }
}

fn cmd_logs(args: &[String]) -> Result<(), String> {
    let a = Args::parse(args, &[])?;
    a.check_known(&["node"])?;
    let [svc] = a.positional.as_slice() else { return Err("usage: k10s logs SERVICE [--node HOST:PORT]".into()) };
    let addr = node_addr(&a);
    let r = http::request(&addr, "GET", &format!("/_k10s/logs/{svc}"), b"", Duration::from_secs(10))
        .map_err(|e| format!("cannot reach node {addr}: {e}"))?;
    print!("{}", r.text());
    if r.status == 200 {
        Ok(())
    } else {
        Err(format!("HTTP {}", r.status))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(parse_size("64KiB").unwrap(), 65536);
        assert_eq!(parse_size("8GiB").unwrap(), 8 << 30);
        assert_eq!(parse_size("1g").unwrap(), 1 << 30);
        assert_eq!(parse_size("123").unwrap(), 123);
        assert!(parse_size("12XB").is_err());
        assert!(parse_size("").is_err());
    }

    #[test]
    fn args() {
        let v: Vec<String> = ["--http", "1.2.3.4:5", "--log-stderr", "--data=/x", "file"].iter().map(|s| s.to_string()).collect();
        let a = Args::parse(&v, &["log-stderr"]).unwrap();
        assert_eq!(a.get("http"), Some("1.2.3.4:5"));
        assert_eq!(a.get("data"), Some("/x"));
        assert!(a.has("log-stderr"));
        assert_eq!(a.positional, vec!["file"]);
        assert!(a.check_known(&["http", "data"]).is_err());
        assert!(Args::parse(&["--http".to_string()], &[]).is_err());
    }
}
