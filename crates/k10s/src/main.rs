//! `k10s` — the single CLI/daemon binary.
//!
//! ```text
//! k10s node   [--http ADDR] [--data DIR] [--shards N] [--backend jit|interp] [--name NAME] [--mem SIZE]
//!             [--snapshots fresh|all] [--idle-timeout SECS] [--idle-spin-us N] [--log-stderr] [--pin]
//!             [--timer-res-us N] [--jit-indirect jump|lfence|retpoline]
//!             [--sandbox on|seccomp-only|log|off] [--user USER[:GROUP]]
//!             admin API: [--admin ADDR] [--admin-token-file FILE]
//!             cluster mode (any of): [--gossip ADDR] [--rpc ADDR] [--join HOST:PORT[,…]] [--advertise HOST]
//!             cluster auth: --cluster-key-file FILE [--cluster-next-key-file FILE] | --insecure-unauthenticated-cluster
//!                           [--gossip-max-skew SECS]
//!             tuning: [--probe-ms N] [--suspicion-mult N] [--leave-grace-ms N]
//!             disk images (docs/OS.md): [--os-commit-grace SECS], and `--os-agent-fd N` from init
//! k10s keygen [-o FILE]
//! k10s image  pubkey KEY | sign --key KEY --arch A --profile P --version V IN OUT | verify FILE [--trust PUB,…]
//! k10s apply  FILE.k10s [--node HOST:PORT] [--admin-token-file FILE]
//! k10s build  FILE.ten [-o FILE.k10]
//! k10s run    FILE.ten|FILE.k10 [--path /p?q=1] [--method GET|POST|PUT|DELETE] [--body STR|@FILE]
//!             [--backend jit|interp] [--memory SIZE] [--config STR] [--cpu-ms N]
//! k10s status [--node HOST:PORT] [--json] [--admin-token-file FILE]
//! k10s logs   SERVICE [--node HOST:PORT] [--admin-token-file FILE]
//! k10s version                       (also `--version`, `-V`)
//! ```
//! Argument parsing is hand-written (no dependencies beyond the workspace). The default backend is `K10S_BACKEND`
//! (`jit` | `interp`) if set, else the JIT.

use k10_mesh::manifest::{parse_document, OsImage, OsPlan};
use k10_mesh::osimage;
use k10_mesh::admin_auth::AdminKey;
use k10_mesh::secure::{ClusterKey, ClusterKeys};
use k10_mesh::sha256::sha256;
use k10_mesh::{Bundle, Manifest, ServiceSpec};
use k10_node::cluster::{os, ClusterConfig, ClusterSetup};
use k10_node::http;
use k10_node::router::{NoRemote, RemoteRouter};
use k10_node::sandbox;
use k10_node::{BackendKind, Node, NodeConfig, SnapshotPolicy};
use std::io::Write;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::process::ExitCode;
use std::time::{Duration, Instant};

const USAGE: &str = "usage:
  k10s node   [--http ADDR] [--data DIR] [--shards N] [--backend jit|interp] [--name NAME] [--mem SIZE]
              [--kv-max SIZE] (per-service KV quota, default 256MiB, 0 = unlimited)
              [--conn-buffer-max SIZE] (bytes buffered in HTTP connections node-wide, default max(--mem/8, 64MiB))
              [--snapshots fresh|all] [--idle-timeout SECS] [--idle-spin-us N] [--log-stderr] [--pin]
              [--timer-res-us N] (guest clock resolution, default 100; 0 = full, trusted tenants only)
              [--jit-indirect jump|lfence|retpoline] (Spectre v2 hardening of JIT code, default jump)
              [--sandbox on|seccomp-only|log|off] (process sandbox: Landlock + seccomp, default on or $K10S_SANDBOX)
              [--user USER[:GROUP]] (started as root: run as USER once the listeners are bound)
              admin API (/_k10s/): requests signed with the admin key; health is open, metrics too on --admin
              [--admin-token-file FILE] (the admin key; default DATA/admin.key, or admin.key in the state
                directory without --data, generated on first start)
              [--admin ADDR] (own listener for the admin API; the ingress then serves only /_k10s/health)
              cluster mode (any of these flags turns it on):
              [--gossip ADDR] (UDP, default 0.0.0.0:7946)  [--rpc ADDR] (TCP, default 0.0.0.0:7947)
              [--join HOST:PORT,...] (gossip addresses of members)  [--advertise HOST] (IP given to peers)
              --cluster-key-file FILE (required: same key on every node, see keygen)
                or --insecure-unauthenticated-cluster (no authentication: trusted networks only)
              [--cluster-next-key-file FILE] (key rotation: also accepted, see docs/NODE.md)
              [--gossip-max-skew SECS] (clock skew allowed on gossip, default 300, at least 1)
              [--probe-ms N] (gossip probe period, 200)  [--suspicion-mult N] (4)  [--leave-grace-ms N] (1000)
              [--os-commit-grace SECS] (disk images: a trial boot of a new OS image serves this long before it
                commits, 30; docs/OS.md)
  k10s keygen [-o FILE]                           (32 random bytes as hex: a cluster key or an admin key;
                                                   FILE is created with mode 0600, never overwritten)
  k10s image pubkey KEY                           (the public key of a signing key made by keygen)
  k10s image sign --key KEY --arch ARCH --profile PROFILE --version V IN OUT
                                                  (signs an OS image for Tier 1 updates, a UKI or a Pi boot
                                                   slot, docs/OS.md)
  k10s image verify FILE [--trust PUBKEY,...]     (checks an OS image; trusted: the k10s release key and PUBKEY)
  k10s image payload IN OUT                       (the UKI or Pi slot of an OS image without its signature: to
                                                   sign for Secure Boot, then again with image sign)
  k10s apply  FILE.k10s [--node HOST:PORT]        (code = \"x.ten\" sources are compiled, \"x.k10\" sent as is;
                                                   os { images = [\"x.efi\"] } rolls out OS images)
  k10s build  FILE.ten [-o FILE.k10]              (compile Ten to TIR)
  k10s run    FILE.ten|FILE.k10 [--path /p?q=1] [--method GET|POST|PUT|DELETE] [--body STR|@FILE]
              [--backend jit|interp] [--memory SIZE] [--config STR] [--cpu-ms N]
              (runs one request in-process, no node needed: body on stdout, status and logs on stderr)
  k10s status [--node HOST:PORT] [--json]
  k10s logs   SERVICE [--node HOST:PORT]
  k10s version                                    (also --version, -V)
  apply, status and logs sign with the admin key from --admin-token-file FILE, else $K10S_ADMIN_TOKEN_FILE, else
  admin.key in the state directory ($K10S_HOME, else $XDG_STATE_HOME/k10s, else ~/.local/state/k10s).

The default node address is 127.0.0.1:8080. In a cluster, `apply` and HTTP requests work on any node.
The default backend is $K10S_BACKEND (jit|interp) if set, else jit.";

/// Build version, e.g. `0.1.1-dev.45+1a2b3c4` (scripts/version.sh via build.rs).
const VERSION: &str = env!("K10S_VERSION_FULL");

/// `k10s node` flags that take no value.
const NODE_BOOLEANS: &[&str] = &["log-stderr", "pin", "insecure-unauthenticated-cluster"];

mod pid1;

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().collect();
    if pid1::is_init(argv.first().map_or("", String::as_str)) {
        pid1::main();
    }
    if let Some(module) = pid1::modprobe_request(&argv) {
        return pid1::modprobe(module);
    }
    let args = &argv[1.min(argv.len())..];
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
        "version" | "--version" | "-V" => {
            println!("k10s {VERSION}");
            Ok(())
        }
        "keygen" => cmd_keygen(rest),
        "image" => cmd_image(rest),
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
                    if booleans.contains(&k) {
                        return Err(format!("--{k} takes no value (got `{v}`)"));
                    }
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

/// A secret file's contents, wiped on drop.
struct Secret(Vec<u8>);

impl Drop for Secret {
    fn drop(&mut self) {
        k10_mesh::crypto::zeroize(&mut self.0);
    }
}

impl Secret {
    fn text(&self) -> &str {
        std::str::from_utf8(&self.0).unwrap_or("").trim()
    }
}

/// Reads a secret file; warns when other users may read it.
fn read_secret(flag: &str, path: &Path) -> Result<Secret, String> {
    use std::io::Read;
    use std::os::unix::fs::PermissionsExt;
    let err = |e: &dyn std::fmt::Display| format!("--{flag} {}: {e}", path.display());
    let mut f = std::fs::File::open(path).map_err(|e| err(&e))?;
    let meta = f.metadata().map_err(|e| err(&e))?;
    if meta.permissions().mode() & 0o077 != 0 {
        eprintln!("k10s: warning: {} is readable by other users (chmod 600 it)", path.display());
    }
    if meta.len() > 4096 {
        return Err(err(&"larger than 4 KiB: not a key"));
    }
    // Sized up front: no reallocation leaves copies behind.
    let mut s = Secret(Vec::with_capacity(meta.len() as usize + 1));
    f.read_to_end(&mut s.0).map_err(|e| err(&e))?;
    if std::str::from_utf8(&s.0).is_err() {
        return Err(err(&"not text"));
    }
    if s.text().is_empty() {
        return Err(err(&"empty"));
    }
    Ok(s)
}

fn load_admin_key(flag: &str, path: &Path) -> Result<AdminKey, String> {
    AdminKey::parse(read_secret(flag, path)?.text()).map_err(|e| format!("--{flag} {}: {e}", path.display()))
}

fn load_cluster_key(flag: &str, path: &str) -> Result<ClusterKey, String> {
    ClusterKey::parse_hex(read_secret(flag, Path::new(path))?.text()).map_err(|e| format!("--{flag} {path}: {e}"))
}

/// 32 random bytes as hex.
fn new_key() -> Result<Secret, String> {
    let mut key = [0u8; 32];
    k10_node::sys::fill_random(&mut key).map_err(|e| format!("getrandom: {e}"))?;
    let line = Secret(format!("{}\n", k10_mesh::sha256::to_hex(&key)).into_bytes());
    k10_mesh::crypto::zeroize(&mut key);
    Ok(line)
}

/// Creates `path` (mode 0600, owned by `owner` when given) holding a new key; fails if it exists.
fn write_new_key(path: &Path, owner: Option<&sandbox::User>) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let line = new_key().map_err(std::io::Error::other)?;
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?;
    if let Some(u) = owner {
        std::os::unix::fs::fchown(&f, Some(u.uid), Some(u.gid))?;
    }
    f.write_all(&line.0)
}

/// `$K10S_HOME`, else `$XDG_STATE_HOME/k10s`, else `~/.local/state/k10s`.
fn state_dir() -> Option<PathBuf> {
    let env = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    env("K10S_HOME").or_else(|| env("XDG_STATE_HOME").map(|d| d.join("k10s"))).or_else(|| env("HOME").map(|h| h.join(".local/state/k10s")))
}

/// The node's admin key: `--admin-token-file`, else `admin.key` in the data (or state) directory, created if missing.
/// Started as root with `--user`, the data directory and a created key belong to that user.
fn node_admin_key(a: &Args, user: Option<&sandbox::User>) -> Result<(AdminKey, PathBuf), String> {
    if let Some(f) = a.get("admin-token-file") {
        return Ok((load_admin_key("admin-token-file", Path::new(f))?, PathBuf::from(f)));
    }
    // SAFETY: geteuid has no preconditions.
    let owner = user.filter(|_| unsafe { libc::geteuid() } == 0);
    let dir = match (a.get("data"), owner) {
        (Some(d), Some(u)) => {
            let d = PathBuf::from(d);
            u.own_dir(&d)?;
            d
        }
        (Some(d), None) => PathBuf::from(d),
        (None, _) => state_dir().ok_or("no --admin-token-file, --data or $HOME: nowhere to keep the admin key")?,
    };
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let path = dir.join("admin.key");
    let key_owner = owner.filter(|_| a.has("data"));
    match write_new_key(&path, key_owner) {
        Ok(()) => eprintln!("k10s: generated the admin key {}", path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(format!("{}: {e}", path.display())),
    }
    Ok((load_admin_key("admin-token-file", &path)?, path))
}

fn cmd_keygen(args: &[String]) -> Result<(), String> {
    let a = Args::parse(args, &[])?;
    a.check_known(&["o", "output"])?;
    if !a.positional.is_empty() {
        return Err("usage: k10s keygen [-o FILE]".into());
    }
    match a.get("o").or(a.get("output")) {
        None => print!("{}", new_key()?.text()),
        Some(f) => {
            write_new_key(Path::new(f), None).map_err(|e| format!("{f}: {e}"))?;
            eprintln!("k10s: wrote a new key to {f} (mode 0600)");
        }
    }
    Ok(())
}

fn cmd_image(args: &[String]) -> Result<(), String> {
    let usage = "usage: k10s image pubkey KEY | sign --key KEY --arch ARCH --profile PROFILE --version V IN OUT | verify FILE [--trust PUBKEY,...] | payload IN OUT";
    let Some((sub, rest)) = args.split_first() else { return Err(usage.into()) };
    let a = Args::parse(rest, &[])?;
    let seed = |path: &str| -> Result<[u8; 32], String> {
        let s = read_secret("key", Path::new(path))?;
        let k = os::parse_hex32(s.text().trim()).ok_or_else(|| format!("--key {path}: not 64 hex digits (make one with `k10s keygen -o FILE`)"))?;
        Ok(k)
    };
    match sub.as_str() {
        "pubkey" => {
            a.check_known(&[])?;
            let [key] = a.positional.as_slice() else { return Err(usage.into()) };
            let mut s = seed(key)?;
            println!("{}", k10_mesh::sha256::to_hex(&k10_mesh::crypto::ed25519::public_key(&s)));
            k10_mesh::crypto::zeroize(&mut s);
        }
        "sign" => {
            a.check_known(&["key", "arch", "profile", "version"])?;
            let ([input, output], Some(key)) = (a.positional.as_slice(), a.get("key")) else { return Err(usage.into()) };
            let need = |k: &str| a.get(k).map(str::to_string).ok_or_else(|| format!("missing --{k}"));
            let meta = osimage::ImageMeta { arch: need("arch")?, profile: need("profile")?, version: need("version")? };
            let payload = std::fs::read(input).map_err(|e| format!("{input}: {e}"))?;
            let mut s = seed(key)?;
            let signed = osimage::sign(&payload, &meta, &s);
            k10_mesh::crypto::zeroize(&mut s);
            let signed = signed.map_err(|e| format!("{input}: {e}"))?;
            std::fs::write(output, &signed).map_err(|e| format!("{output}: {e}"))?;
            eprintln!("k10s: {output}: {}-{} {} image, sha256 {}", meta.arch, meta.profile, meta.version, k10_mesh::sha256::to_hex(&sha256(&signed)));
        }
        "verify" => {
            a.check_known(&["trust"])?;
            let [file] = a.positional.as_slice() else { return Err(usage.into()) };
            let mut trusted = pid1::release_keys();
            for k in a.get("trust").into_iter().flat_map(|t| t.split(',')).filter(|k| !k.is_empty()) {
                trusted.push(os::parse_hex32(k).ok_or_else(|| format!("--trust: `{k}` is not a public key (64 hex digits)"))?);
            }
            let blob = std::fs::read(file).map_err(|e| format!("{file}: {e}"))?;
            let info = osimage::verify(&blob, &trusted).map_err(|e| format!("{file}: {e}"))?;
            println!(
                "{file}: platform {}-{}, version {}, signed by {}, sha256 {}{}",
                info.meta.arch,
                info.meta.profile,
                info.meta.version,
                k10_mesh::sha256::to_hex(&info.key),
                k10_mesh::sha256::to_hex(&sha256(&blob)),
                if osimage::authenticode_signed(&blob) { ", signed for Secure Boot (Authenticode)" } else { "" }
            );
        }
        "payload" => {
            a.check_known(&[])?;
            let [input, output] = a.positional.as_slice() else { return Err(usage.into()) };
            let blob = std::fs::read(input).map_err(|e| format!("{input}: {e}"))?;
            let payload = osimage::payload(&blob).map_err(|e| format!("{input}: {e}"))?;
            std::fs::write(output, &payload).map_err(|e| format!("{output}: {e}"))?;
            eprintln!("k10s: {output}: the {}-byte payload of {input}", payload.len());
        }
        _ => return Err(usage.into()),
    }
    Ok(())
}

fn cmd_node(args: &[String]) -> Result<(), String> {
    let a = Args::parse(args, NODE_BOOLEANS)?;
    a.check_known(&[
        "http", "data", "shards", "backend", "name", "mem", "kv-max", "conn-buffer-max", "snapshots", "idle-timeout", "idle-spin-us", "log-stderr",
        "pin", "rpc", "gossip", "join", "advertise", "probe-ms", "suspicion-mult", "leave-grace-ms",
        "timer-res-us", "jit-indirect", "admin", "admin-token-file", "cluster-key-file", "cluster-next-key-file",
        "insecure-unauthenticated-cluster", "gossip-max-skew", "sandbox", "user", "os-agent-fd", "os-commit-grace",
    ])?;
    // Process sandbox (k10-node/src/sandbox): early() here, enter() between binding the sockets and starting threads.
    let sandbox_mode = match a.get("sandbox") {
        None => sandbox::Mode::from_env()?,
        Some(m) => sandbox::Mode::parse(m).ok_or_else(|| format!("invalid --sandbox `{m}` (on|seccomp-only|log|off)"))?,
    };
    let os_fd = a.get("os-agent-fd").map(|fd| fd.parse::<i32>().map_err(|_| format!("invalid --os-agent-fd `{fd}`"))).transpose()?;
    if sandbox_mode != sandbox::Mode::Off {
        sandbox::early(os_fd).map_err(|e| format!("sandbox: {e} (use --sandbox off to run without it)"))?;
    }
    let user = a.get("user").map(sandbox::User::resolve).transpose()?;
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
    let (admin_key, admin_key_path) = node_admin_key(&a, user.as_ref())?;
    cfg.admin_key = Some(admin_key);
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
    cfg.version = VERSION.into();
    cfg.pin_threads = a.has("pin");
    if let Some(s) = a.get("timer-res-us") {
        cfg.timer_res = Duration::from_micros(s.parse().map_err(|_| "invalid --timer-res-us")?);
    }
    if let Some(s) = a.get("jit-indirect") {
        cfg.jit_indirect =
            k10_node::IndirectBranch::parse(s).ok_or_else(|| format!("invalid --jit-indirect `{s}` (jump|lfence|retpoline)"))?;
    }
    let mut cluster_cfg = cluster_config(&a)?;
    // Tier 1 (docs/OS.md, "Updates"): init's update channel and the running image, in the gossip record from the start.
    let os_link = match os_fd {
        None => None,
        Some(fd) => {
            let mut link = os::OsLink::from_fd(fd).map_err(|e| format!("--os-agent-fd {fd}: {e}"))?;
            let st = link.first_status(Duration::from_secs(10)).map_err(|e| format!("--os-agent-fd {fd}: {e}"))?;
            if let Some(cc) = cluster_cfg.as_mut() {
                cc.os = Some(st.info());
            }
            Some((link, st))
        }
    };
    let os_grace = match a.get("os-commit-grace") {
        None => Duration::from_secs(30),
        Some(s) => Duration::from_secs(s.parse().map_err(|_| format!("invalid --os-commit-grace `{s}` (seconds)"))?),
    };
    // Block SIGINT/SIGTERM/SIGHUP in every thread (inherited by the threads the node spawns) and wait for them here.
    let set = signal_set();
    // SAFETY: plain libc calls on a properly initialized sigset_t.
    unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()) };
    let shards = cfg.shards;
    let backend = cfg.backend;
    let data = cfg.data_dir.clone();
    let policy = sandbox::Policy {
        data_dir: data.clone(),
        write_dirs: std::env::var_os("K10_JIT_DUMP").filter(|d| !d.is_empty()).map(PathBuf::from).into_iter().collect(),
    };
    // Phase 1 binds the sockets (secrets are read above); phase 2, inside the sandbox, opens the stores, reloads the
    // bundle and starts the threads.
    let cluster_bound = match cluster_cfg {
        None => None,
        Some(cc) => {
            // Cluster mode keeps its own store (manifest + blobs); the single-node bundle file is not used.
            cfg.reload_bundle = false;
            Some(ClusterSetup::bind(cc).map_err(|e| format!("cannot start cluster mode: {e}"))?)
        }
    };
    let bound = Node::bind(cfg).map_err(|e| format!("cannot start node: {e}"))?;
    let sandbox_line = sandbox::enter(sandbox_mode, user.as_ref(), &policy).map_err(|e| format!("sandbox: {e}"))?;
    let setup = match cluster_bound {
        None => None,
        Some(b) => Some(b.open(data.as_deref()).map_err(|e| format!("cannot start cluster mode: {e}"))?),
    };
    let router = setup.as_ref().map_or_else(|| Arc::new(NoRemote) as Arc<dyn RemoteRouter>, ClusterSetup::router);
    let node = bound.start(router).map_err(|e| format!("cannot start node: {e}"))?;
    let cluster = match setup {
        None => None,
        Some(s) => Some(s.start(node.shared().clone()).map_err(|e| format!("cannot start cluster mode: {e}"))?),
    };
    eprintln!("{sandbox_line}");
    if let Some((link, st)) = os_link {
        os::spawn(link, st, cluster.as_ref().map(|c| c.shared().clone()), os_grace).map_err(|e| format!("os agent: {e}"))?;
    }
    let admin_where = match node.shared().admin_addr.get() {
        Some(a) => format!("on http://{a}"),
        None => "on the ingress".into(),
    };
    eprintln!("k10s: admin API {admin_where}: requests signed with the key in {}", admin_key_path.display());
    eprintln!(
        "k10s {VERSION}: node `{}` listening on http://{} ({} shards, backend {}, data {})",
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
        for f in [
            "probe-ms", "suspicion-mult", "leave-grace-ms", "cluster-key-file", "cluster-next-key-file",
            "insecure-unauthenticated-cluster", "gossip-max-skew",
        ] {
            if a.has(f) {
                return Err(format!("--{f} needs cluster mode (--gossip/--rpc/--join/--advertise)"));
            }
        }
        return Ok(None);
    }
    let mut c = ClusterConfig::default();
    match (a.get("cluster-key-file"), a.has("insecure-unauthenticated-cluster")) {
        (Some(_), true) => return Err("--cluster-key-file and --insecure-unauthenticated-cluster exclude each other".into()),
        (Some(f), false) => {
            let next = a.get("cluster-next-key-file").map(|n| load_cluster_key("cluster-next-key-file", n)).transpose()?;
            let keys = ClusterKeys::new(load_cluster_key("cluster-key-file", f)?, next);
            c.keys = Some(keys.map_err(|e| format!("--cluster-next-key-file: {e}"))?);
        }
        (None, true) if a.has("cluster-next-key-file") => {
            return Err("--cluster-next-key-file needs --cluster-key-file".into());
        }
        (None, true) => c.insecure = true,
        (None, false) => {
            return Err("cluster mode needs --cluster-key-file FILE (create it once with `k10s keygen -o FILE` and copy it to \
                        every node), or --insecure-unauthenticated-cluster where every host that can reach the gossip and rpc \
                        ports is trusted"
                .into())
        }
    }
    if let Some(v) = a.get("gossip-max-skew") {
        let secs: u64 = v.parse().ok().filter(|s| *s >= 1).ok_or("invalid --gossip-max-skew (seconds, at least 1)")?;
        c.gossip_max_skew = Duration::from_secs(secs);
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
        libc::sigaddset(&mut set, libc::SIGHUP);
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
        // The only service: nothing to call.
        calls: Default::default(),
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
    let r = node.apply(Bundle::new(Manifest { version: 0, services: vec![spec], ..Default::default() }, vec![blob]));
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

/// The admin key to sign with: `--admin-token-file`, else `$K10S_ADMIN_TOKEN_FILE`, else the state directory's.
fn client_key(a: &Args) -> Result<AdminKey, String> {
    if let Some(f) = a.get("admin-token-file") {
        return load_admin_key("admin-token-file", Path::new(f));
    }
    if let Some(f) = std::env::var_os("K10S_ADMIN_TOKEN_FILE").filter(|f| !f.is_empty()) {
        return load_admin_key("admin-token-file ($K10S_ADMIN_TOKEN_FILE)", Path::new(&f));
    }
    match state_dir().map(|d| d.join("admin.key")).filter(|p| p.exists()) {
        Some(p) => load_admin_key("admin-token-file", &p),
        None => Err("no admin key: pass --admin-token-file FILE (the node's DATA/admin.key) or set $K10S_ADMIN_TOKEN_FILE".into()),
    }
}

/// A signed request to the admin API; 401/403 get a hint.
fn admin_request(a: &Args, method: &str, path: &str, body: &[u8], timeout: Duration) -> Result<http::ClientResponse, String> {
    let addr = node_addr(a);
    let key = client_key(a)?;
    let r = http::request_signed(&addr, method, path, body, timeout, Some(&key)).map_err(|e| format!("cannot reach node {addr}: {e}"))?;
    if r.status == 401 || r.status == 403 {
        return Err(format!(
            "node {addr} refused: {} (HTTP {}); sign with the node's admin key: --admin-token-file FILE or $K10S_ADMIN_TOKEN_FILE",
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
    let doc = parse_document(&src).map_err(|e| format!("{file}: {e}"))?;
    let base = path.parent().unwrap_or(Path::new("."));
    let mut services = Vec::new();
    let mut blobs: Vec<Vec<u8>> = Vec::new();
    let mut os = OsPlan::default();
    if let Some(d) = &doc.os {
        for f in &d.images {
            let p = base.join(f);
            let blob = std::fs::read(&p).map_err(|e| format!("{}: {e}", p.display()))?;
            let info = osimage::inspect(&blob).map_err(|e| format!("{}: {e}", p.display()))?;
            let platform = format!("{}-{}", info.meta.arch, info.meta.profile);
            if os.image_for(&platform).is_some() {
                return Err(format!("{file}: line {}: two images for platform {platform}", d.line));
            }
            os.images.push(OsImage { platform, image: sha256(&blob) });
            blobs.push(blob);
        }
        os.wave = d.wave;
    }
    for d in &doc.services {
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
    let bundle = Bundle::new(Manifest { version: 0, services, os }, blobs);
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
        // A boolean takes no value: `--flag=false` must not turn it on.
        for v in ["--insecure-unauthenticated-cluster=false", "--insecure-unauthenticated-cluster=0", "--pin="] {
            let e = Args::parse(&[v.to_string()], &["insecure-unauthenticated-cluster", "pin"]).err().unwrap();
            assert!(e.contains("takes no value"), "{v}: {e}");
        }
    }
}
