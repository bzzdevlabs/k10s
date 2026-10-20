//! `k10s` — the single CLI/daemon binary.
//!
//! ```text
//! k10s node   [--http ADDR] [--data DIR] [--shards N] [--backend jit|interp] [--name NAME] [--mem SIZE]
//!             [--snapshots fresh|all] [--idle-timeout SECS] [--idle-spin-us N] [--log-stderr] [--pin]
//!             admin API: [--admin ADDR] [--admin-token-file FILE]
//!             cluster mode (any of): [--gossip ADDR] [--rpc ADDR] [--join HOST:PORT[,…]] [--advertise HOST]
//!             cluster auth: --cluster-key-file FILE | --insecure-unauthenticated-cluster; [--gossip-max-skew SECS]
//!             tuning: [--probe-ms N] [--suspicion-mult N] [--leave-grace-ms N]
//! k10s keygen [-o FILE]
//! k10s apply  FILE.k10s [--node HOST:PORT] [--admin-token-file FILE]
//! k10s build  FILE.ten [-o FILE.k10]
//! k10s run    FILE.ten|FILE.k10 [--path /p?q=1] [--method GET|POST|PUT|DELETE] [--body STR|@FILE]
//!             [--backend jit|interp] [--memory SIZE] [--config STR] [--cpu-ms N]
//! k10s status [--node HOST:PORT] [--json] [--admin-token-file FILE]
//! k10s logs   SERVICE [--node HOST:PORT] [--admin-token-file FILE]
//! ```
//! Argument parsing is hand-written (no dependencies beyond the workspace). The default backend is `K10S_BACKEND`
//! (`jit` | `interp`) if set, else the JIT.

use k10_mesh::manifest::parse_text;
use k10_mesh::secure::ClusterKey;
use k10_mesh::sha256::sha256;
use k10_mesh::{Bundle, Manifest, ServiceSpec};
use k10_node::cluster::{ClusterConfig, ClusterSetup};
use k10_node::http;
use k10_node::admin::AdminToken;
use k10_node::{BackendKind, Node, NodeConfig, SnapshotPolicy};
use std::io::Write;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

const USAGE: &str = "usage:
  k10s node   [--http ADDR] [--data DIR] [--shards N] [--backend jit|interp] [--name NAME] [--mem SIZE]
              [--kv-max SIZE] (per-service KV quota, default 256MiB, 0 = unlimited)
              [--conn-buffer-max SIZE] (bytes buffered in HTTP connections node-wide, default max(--mem/8, 64MiB))
              [--snapshots fresh|all] [--idle-timeout SECS] [--idle-spin-us N] [--log-stderr] [--pin]
              admin API (/_k10s/; health and metrics are always open):
              [--admin-token-file FILE] (bearer token required; without it only loopback clients may use it)
              [--admin ADDR] (own listener for the admin API; the ingress then serves only /_k10s/health)
              cluster mode (any of these flags turns it on):
              [--gossip ADDR] (UDP, default 0.0.0.0:7946)  [--rpc ADDR] (TCP, default 0.0.0.0:7947)
              [--join HOST:PORT,...] (gossip addresses of members)  [--advertise HOST] (IP given to peers)
              --cluster-key-file FILE (required: same key on every node, see keygen)
                or --insecure-unauthenticated-cluster (no authentication: trusted networks only)
              [--gossip-max-skew SECS] (clock skew allowed on gossip, 300; 0 = off)
              [--probe-ms N] (gossip probe period, 200)  [--suspicion-mult N] (4)  [--leave-grace-ms N] (1000)
  k10s keygen [-o FILE]                           (32 random bytes as hex: a cluster key or an admin token;
                                                   FILE is created with mode 0600, never overwritten)
  k10s apply  FILE.k10s [--node HOST:PORT]        (code = \"x.ten\" sources are compiled, \"x.k10\" sent as is)
  k10s build  FILE.ten [-o FILE.k10]              (compile Ten to TIR)
  k10s run    FILE.ten|FILE.k10 [--path /p?q=1] [--method GET|POST|PUT|DELETE] [--body STR|@FILE]
              [--backend jit|interp] [--memory SIZE] [--config STR] [--cpu-ms N]
              (runs one request in-process, no node needed: body on stdout, status and logs on stderr)
  k10s status [--node HOST:PORT] [--json]
  k10s logs   SERVICE [--node HOST:PORT]
  apply, status and logs send the admin token from --admin-token-file FILE, else $K10S_ADMIN_TOKEN_FILE, else
  $K10S_ADMIN_TOKEN.

The default node address is 127.0.0.1:8080. In a cluster, `apply` and HTTP requests work on any node.
The default backend is $K10S_BACKEND (jit|interp) if set, else jit.";

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
        "build" => cmd_build(rest),
        "run" => cmd_run(rest),
        "status" => cmd_status(rest),
        "logs" => cmd_logs(rest),
        "keygen" => cmd_keygen(rest),
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
            // `-o VALUE` (single-letter short flags take a value).
            let short = a.strip_prefix('-').filter(|f| f.len() == 1 && f != &"-");
            if let Some(f) = a.strip_prefix("--").or(short) {
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

/// Reads a secret file (trimmed); warns when other users may read it.
fn read_secret(flag: &str, path: &str) -> Result<String, String> {
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::metadata(path).map_err(|e| format!("--{flag} {path}: {e}"))?;
    if meta.permissions().mode() & 0o077 != 0 {
        eprintln!("k10s: warning: {path} is readable by other users (chmod 600 it)");
    }
    let s = std::fs::read_to_string(path).map_err(|e| format!("--{flag} {path}: {e}"))?;
    let s = s.trim().to_string();
    if s.is_empty() {
        return Err(format!("--{flag} {path}: empty"));
    }
    Ok(s)
}

fn cmd_keygen(args: &[String]) -> Result<(), String> {
    let a = Args::parse(args, &[])?;
    a.check_known(&["o", "output"])?;
    if !a.positional.is_empty() {
        return Err("usage: k10s keygen [-o FILE]".into());
    }
    let mut key = [0u8; 32];
    k10_node::sys::fill_random(&mut key).map_err(|e| format!("getrandom: {e}"))?;
    let line = format!("{}\n", k10_mesh::sha256::to_hex(&key));
    match a.get("o").or(a.get("output")) {
        None => print!("{line}"),
        Some(f) => {
            use std::os::unix::fs::OpenOptionsExt;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(f)
                .map_err(|e| format!("{f}: {e}"))?;
            file.write_all(line.as_bytes()).map_err(|e| format!("{f}: {e}"))?;
            eprintln!("k10s: wrote a new key to {f} (mode 0600)");
        }
    }
    Ok(())
}

fn cmd_node(args: &[String]) -> Result<(), String> {
    let a = Args::parse(args, &["log-stderr", "pin", "insecure-unauthenticated-cluster"])?;
    a.check_known(&[
        "http", "data", "shards", "backend", "name", "mem", "kv-max", "conn-buffer-max", "snapshots", "idle-timeout", "idle-spin-us", "log-stderr",
        "pin", "rpc", "gossip", "join", "advertise", "probe-ms", "suspicion-mult", "leave-grace-ms", "admin", "admin-token-file",
        "cluster-key-file", "insecure-unauthenticated-cluster", "gossip-max-skew",
    ])?;
    if !a.positional.is_empty() {
        return Err(format!("unexpected argument `{}`", a.positional[0]));
    }
    let mut cfg = NodeConfig::default();
    if let Some(h) = a.get("http") {
        cfg.http = h.parse().map_err(|_| format!("invalid --http address `{h}` (want IP:PORT)"))?;
    }
    if let Some(h) = a.get("admin") {
        cfg.admin_listen = Some(h.parse().map_err(|_| format!("invalid --admin address `{h}` (want IP:PORT)"))?);
    }
    if let Some(f) = a.get("admin-token-file") {
        cfg.admin_token = Some(AdminToken::new(read_secret("admin-token-file", f)?.as_bytes()));
    }
    cfg.data_dir = a.get("data").map(PathBuf::from);
    if let Some(n) = a.get("shards") {
        cfg.shards = n.parse().ok().filter(|n: &usize| (1..=1024).contains(n)).ok_or("invalid --shards")?;
    }
    cfg.backend = backend_flag(&a)?;
    if let Some(n) = a.get("name") {
        cfg.name = n.to_string();
    }
    if let Some(m) = a.get("mem") {
        cfg.mem_limit = parse_size(m)?;
    }
    if let Some(m) = a.get("kv-max") {
        cfg.kv.max_bytes = parse_size(m)?;
    }
    if let Some(m) = a.get("conn-buffer-max") {
        cfg.conn_buffer_max = parse_size(m)?.max(1);
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
    let backend = cfg.backend;
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
    let admin_where = match node.shared().admin_addr.get() {
        Some(a) => format!("on http://{a}"),
        None => "on the ingress".into(),
    };
    let admin_auth = if node.shared().cfg.admin_token.is_some() { "bearer token required" } else { "loopback clients only" };
    eprintln!("k10s: admin API {admin_where}: {admin_auth} (health and metrics open)");
    eprintln!(
        "k10s: node `{}` listening on http://{} ({} shards, backend {}, data {})",
        node.shared().cfg.name,
        node.http_addr(),
        shards,
        backend.name(),
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
        for f in ["probe-ms", "suspicion-mult", "leave-grace-ms", "cluster-key-file", "insecure-unauthenticated-cluster", "gossip-max-skew"] {
            if a.has(f) {
                return Err(format!("--{f} needs cluster mode (--gossip/--rpc/--join/--advertise)"));
            }
        }
        return Ok(None);
    }
    let mut c = ClusterConfig::default();
    match (a.get("cluster-key-file"), a.has("insecure-unauthenticated-cluster")) {
        (Some(_), true) => return Err("--cluster-key-file and --insecure-unauthenticated-cluster exclude each other".into()),
        (Some(f), false) => c.key = Some(ClusterKey::parse_hex(&read_secret("cluster-key-file", f)?).map_err(|e| format!("--cluster-key-file {f}: {e}"))?),
        (None, true) => c.insecure = true,
        (None, false) => {
            return Err("cluster mode needs --cluster-key-file FILE (create it once with `k10s keygen -o FILE` and copy it to \
                        every node), or --insecure-unauthenticated-cluster where every host that can reach the gossip and rpc \
                        ports is trusted"
                .into())
        }
    }
    if let Some(v) = a.get("gossip-max-skew") {
        c.gossip_max_skew = Duration::from_secs(v.parse().map_err(|_| "invalid --gossip-max-skew (seconds)")?);
    }
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

/// `--backend jit|interp`, else [`BackendKind::from_env`].
fn backend_flag(a: &Args) -> Result<BackendKind, String> {
    match a.get("backend") {
        None => match std::env::var("K10S_BACKEND") {
            Ok(v) if BackendKind::parse(v.trim()).is_none() => Err(format!("K10S_BACKEND=`{v}`: unknown backend (jit|interp)")),
            _ => Ok(BackendKind::from_env()),
        },
        Some(b) => BackendKind::parse(b).ok_or_else(|| format!("unknown backend `{b}` (jit|interp)")),
    }
}

/// Turns a manifest `code = "…"` entry (or a `run` argument) into a TIR blob.
///
/// * `*.ten` — Ten sources, compiled with `k10-lang` (diagnostics are printed to stderr).
/// * anything else — precompiled TIR (`*.k10`), checked locally (decode + validate) for early errors.
fn load_code(path: &Path) -> Result<Vec<u8>, String> {
    match path.extension().and_then(|e| e.to_str()) {
        Some("ten") => compile_ten(path).map(|m| k10_ir::encode(&m)),
        _ => {
            let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
            let m = k10_ir::decode(&bytes).map_err(|e| format!("{}: not a TIR module: {e}", path.display()))?;
            k10_ir::validate(&m).map_err(|e| format!("{}: invalid module: {e}", path.display()))?;
            Ok(bytes)
        }
    }
}

/// Compiles a Ten file (and its includes). On errors the rendered diagnostics go to stderr and the error is a
/// one-line summary.
fn compile_ten(path: &Path) -> Result<k10_ir::Module, String> {
    let mut sources = k10_lang::Sources::new();
    k10_lang::compile_file_with_sources(path, &mut sources).map_err(|diags| {
        eprint!("{}", k10_lang::render_diagnostics(&diags, &sources));
        let n = diags.len();
        format!("could not compile `{}` ({n} error{})", path.display(), if n == 1 { "" } else { "s" })
    })
}

fn cmd_build(args: &[String]) -> Result<(), String> {
    let a = Args::parse(args, &[])?;
    a.check_known(&["o", "output"])?;
    let [file] = a.positional.as_slice() else { return Err("usage: k10s build FILE.ten [-o FILE.k10]".into()) };
    let path = Path::new(file);
    let out = match a.get("o").or(a.get("output")) {
        Some(o) => PathBuf::from(o),
        None => path.with_extension("k10"),
    };
    if out == path {
        return Err(format!("refusing to overwrite the source `{file}` (use -o)"));
    }
    let m = compile_ten(path)?;
    let bytes = k10_ir::encode(&m);
    std::fs::write(&out, &bytes).map_err(|e| format!("{}: {e}", out.display()))?;
    let exports: Vec<&str> = m.exports.iter().map(|e| e.name.as_str()).collect();
    let imports: Vec<&str> = m.imports.iter().map(|i| i.name.as_str()).collect();
    eprintln!(
        "k10s: wrote {} ({} bytes: {} functions, exports [{}], imports [{}], min memory {})",
        out.display(),
        bytes.len(),
        m.funcs.len(),
        exports.join(", "),
        imports.join(", "),
        human_size(1u64 << m.min_mem_log2)
    );
    Ok(())
}

fn human_size(n: u64) -> String {
    match n {
        n if n >= 1 << 30 && n % (1 << 30) == 0 => format!("{}GiB", n >> 30),
        n if n >= 1 << 20 && n % (1 << 20) == 0 => format!("{}MiB", n >> 20),
        n if n >= 1 << 10 && n % (1 << 10) == 0 => format!("{}KiB", n >> 10),
        n => format!("{n}B"),
    }
}

/// `k10s run`: one request against a module, in-process (a one-shard node on a loopback ephemeral port, memory-only
/// KV, service logs echoed to stderr). The response body goes to stdout; the status line to stderr. Fails (exit 1)
/// when the invocation trapped or could not run.
fn cmd_run(args: &[String]) -> Result<(), String> {
    let a = Args::parse(args, &[])?;
    a.check_known(&["path", "method", "body", "backend", "memory", "config", "cpu-ms"])?;
    let usage = "usage: k10s run FILE.ten|FILE.k10 [--path /p?q=1] [--method GET|POST|PUT|DELETE] [--body STR|@FILE] \
                 [--backend jit|interp] [--memory SIZE] [--config STR] [--cpu-ms N]";
    let [file] = a.positional.as_slice() else { return Err(usage.into()) };
    let blob = load_code(Path::new(file))?;
    let m = k10_ir::decode(&blob).map_err(|e| format!("{file}: {e}"))?;
    let method = match a.get("method").unwrap_or("GET").to_ascii_uppercase().as_str() {
        "GET" => http::Method::Get,
        "POST" => http::Method::Post,
        "PUT" => http::Method::Put,
        "DELETE" => http::Method::Delete,
        "HEAD" => http::Method::Head,
        _ => http::Method::Other,
    };
    let body = match a.get("body") {
        None => Vec::new(),
        Some(b) => match b.strip_prefix('@') {
            Some(f) => std::fs::read(f).map_err(|e| format!("--body @{f}: {e}"))?,
            None => b.as_bytes().to_vec(),
        },
    };
    let path = a.get("path").unwrap_or("");
    if !path.is_empty() && !path.starts_with('/') && !path.starts_with('?') {
        return Err(format!("--path `{path}` must start with `/` or `?` (it is what follows /<service> in a URL)"));
    }
    let mem_log2 = match a.get("memory") {
        Some(s) => {
            let n = parse_size(s)?;
            if !n.is_power_of_two() {
                return Err(format!("--memory {s}: must be a power of two"));
            }
            n.trailing_zeros() as u8
        }
        None => m.min_mem_log2.max(20),
    };
    let cpu_ms = match a.get("cpu-ms") {
        Some(v) => v.parse().ok().filter(|&n: &u32| n >= 1).ok_or("invalid --cpu-ms")?,
        None => 60_000,
    };
    let spec = ServiceSpec {
        name: "run".into(),
        code_hash: sha256(&blob),
        replicas: 1,
        mem_log2,
        max_instances: 1,
        cpu_ms,
        fresh: false,
        config: a.get("config").unwrap_or("").as_bytes().to_vec(),
    };
    let cfg = NodeConfig {
        http: "127.0.0.1:0".parse().unwrap(),
        shards: 1,
        backend: backend_flag(&a)?,
        data_dir: None,
        log_stderr: true,
        request_timeout: Duration::from_millis(cpu_ms as u64 + 30_000),
        ..NodeConfig::default()
    };
    let node = Node::start(cfg).map_err(|e| format!("cannot start the runtime: {e}"))?;
    let r = node.apply(Bundle::new(Manifest { version: 0, services: vec![spec] }, vec![blob]));
    if let Err(e) = r {
        node.shutdown();
        return Err(format!("{file}: {e}"));
    }
    let t = Instant::now();
    let res = node.invoke(0, "run", method, path, body);
    let took = t.elapsed();
    node.shutdown();
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(&res.body);
    let _ = out.flush();
    eprintln!("k10s: HTTP {} ({} bytes, {:.3} ms)", res.http_status(), res.body.len(), took.as_secs_f64() * 1e3);
    match res.error {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

fn node_addr(a: &Args) -> String {
    a.get("node").unwrap_or("127.0.0.1:8080").to_string()
}

/// The admin token to send: `--admin-token-file`, else `$K10S_ADMIN_TOKEN_FILE`, else `$K10S_ADMIN_TOKEN`.
fn client_token(a: &Args) -> Result<Option<String>, String> {
    if let Some(f) = a.get("admin-token-file") {
        return read_secret("admin-token-file", f).map(Some);
    }
    if let Some(f) = std::env::var_os("K10S_ADMIN_TOKEN_FILE").filter(|f| !f.is_empty()) {
        return read_secret("admin-token-file ($K10S_ADMIN_TOKEN_FILE)", &f.to_string_lossy()).map(Some);
    }
    Ok(std::env::var("K10S_ADMIN_TOKEN").ok().map(|t| t.trim().to_string()).filter(|t| !t.is_empty()))
}

/// A request to the admin API with the configured token; 401/403 get a hint.
fn admin_request(a: &Args, method: &str, path: &str, body: &[u8], timeout: Duration) -> Result<http::ClientResponse, String> {
    let addr = node_addr(a);
    let token = client_token(a)?;
    let r = http::request_auth(&addr, method, path, body, timeout, token.as_deref()).map_err(|e| format!("cannot reach node {addr}: {e}"))?;
    if r.status == 401 || r.status == 403 {
        return Err(format!(
            "node {addr} refused: {} (HTTP {}); pass the node's admin token with --admin-token-file FILE or $K10S_ADMIN_TOKEN_FILE",
            r.text().trim(),
            r.status
        ));
    }
    Ok(r)
}

fn cmd_apply(args: &[String]) -> Result<(), String> {
    let a = Args::parse(args, &[])?;
    a.check_known(&["node", "admin-token-file"])?;
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
    let r = admin_request(&a, "POST", "/_k10s/apply", &bytes, Duration::from_secs(60))?;
    println!("{}", r.text());
    if r.status == 200 {
        Ok(())
    } else {
        Err(format!("apply failed (HTTP {})", r.status))
    }
}

fn cmd_status(args: &[String]) -> Result<(), String> {
    let a = Args::parse(args, &["json"])?;
    a.check_known(&["node", "json", "admin-token-file"])?;
    if !a.has("json") {
        // Cluster mode: the cluster view (members, desired/applied version, placement, links).
        let r = admin_request(&a, "GET", "/_k10s/cluster", b"", Duration::from_secs(10))?;
        if r.status == 200 {
            print!("{}", r.text());
            return Ok(());
        }
    }
    let r = admin_request(&a, "GET", "/_k10s/status", b"", Duration::from_secs(10))?;
    println!("{}", r.text());
    if r.status == 200 {
        Ok(())
    } else {
        Err(format!("HTTP {}", r.status))
    }
}

fn cmd_logs(args: &[String]) -> Result<(), String> {
    let a = Args::parse(args, &[])?;
    a.check_known(&["node", "admin-token-file"])?;
    let [svc] = a.positional.as_slice() else { return Err("usage: k10s logs SERVICE [--node HOST:PORT]".into()) };
    let r = admin_request(&a, "GET", &format!("/_k10s/logs/{svc}"), b"", Duration::from_secs(10))?;
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
