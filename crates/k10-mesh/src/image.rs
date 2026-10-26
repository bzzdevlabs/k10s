//! Image services: unmodified Linux programs from OCI images, converted to `K10I` images by the CLI and run under
//! the native kernel's Linux personality (docs/research/oci.md §10). Part of the desired state from manifest format 4.

use crate::caps::Arch;
use crate::codec::{put_u16, put_varint, DecodeError, Reader, Res};
use crate::manifest::{valid_name, varint, varint_u32, Calls};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt;
use core::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Node memory an image service's container takes beyond `mem_bytes` (kernel structures), charged by placement.
pub const IMAGE_OVERHEAD: u64 = 256 << 10;
/// Granularity of `mem_bytes` and `scratch_bytes`.
pub const PAGE: u64 = 4096;
pub const MAX_IMAGE_MEM: u64 = 64 << 30;
/// Entries of each list of one service (argv lists, env, secrets, routes, public ports, egress rules).
pub const MAX_LIST: usize = 1024;
/// Bytes of `entrypoint`, `args`, `env`, `secrets` and an `exec` health command together.
pub const MAX_EXEC_BYTES: usize = 256 << 10;
pub const MAX_PATH: usize = 4096;
pub const MAX_ROUTE: usize = 255;
pub const MAX_CPU_MILLI: u32 = 1_024_000;
pub const MAX_PIDS: u32 = 1 << 16;
pub const MAX_FDS: u32 = 1 << 20;
pub const MAX_STOP_GRACE_MS: u32 = 3_600_000;
pub const DEFAULT_PIDS: u32 = 1024;
pub const DEFAULT_FDS: u32 = 4096;
pub const DEFAULT_STOP_GRACE_MS: u32 = 10_000;

const F_ENTRYPOINT: u8 = 1;
const F_ARGS: u8 = 2;
const F_INIT: u8 = 4;
const F_ANY: u8 = 8;

/// One `K10I` image of the service for one architecture.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash, PartialOrd, Ord)]
pub struct ImageRef {
    pub arch: Arch,
    /// `sha256` of the `K10I` bytes.
    pub hash: [u8; 32],
}

/// What happens when the container's PID 1 exits.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash, Default, PartialOrd, Ord)]
pub enum Restart {
    #[default]
    Always,
    OnFailure,
    Never,
}

impl Restart {
    pub fn name(self) -> &'static str {
        match self {
            Restart::Always => "always",
            Restart::OnFailure => "on-failure",
            Restart::Never => "never",
        }
    }

    pub fn parse(s: &str) -> Option<Restart> {
        [Restart::Always, Restart::OnFailure, Restart::Never].into_iter().find(|r| r.name() == s)
    }
}

/// Readiness and liveness probe.
#[derive(Clone, PartialEq, Eq, Debug, Hash, Default, PartialOrd, Ord)]
pub enum Health {
    /// The process is alive.
    #[default]
    Process,
    /// `GET path` on `port` answers 2xx or 3xx.
    Http { port: u16, path: String },
    /// `port` accepts a connection.
    Tcp { port: u16 },
    /// The command exits 0 (run inside the container).
    Exec(Vec<String>),
}

impl Health {
    /// `process`, `http:PORT[/PATH]` (default `/`), `tcp:PORT` or `exec:CMD ARG…` (split on spaces, no quoting).
    pub fn parse(s: &str) -> Result<Health, String> {
        let h = if s == "process" {
            Health::Process
        } else if let Some(rest) = s.strip_prefix("http:") {
            let (port, path) = rest.find('/').map_or((rest, ""), |i| rest.split_at(i));
            Health::Http { port: parse_port(port)?, path: if path.is_empty() { "/".into() } else { path.to_string() } }
        } else if let Some(port) = s.strip_prefix("tcp:") {
            Health::Tcp { port: parse_port(port)? }
        } else if let Some(cmd) = s.strip_prefix("exec:") {
            Health::Exec(cmd.split(' ').filter(|a| !a.is_empty()).map(String::from).collect())
        } else {
            return Err(format!("unknown health check `{s}` (want process, http:PORT/PATH, tcp:PORT or exec:CMD)"));
        };
        h.validate()?;
        Ok(h)
    }

    fn validate(&self) -> Result<(), String> {
        match self {
            Health::Process | Health::Tcp { .. } => Ok(()),
            Health::Http { path, .. } if !path.starts_with('/') || path.len() > MAX_ROUTE || !path.bytes().all(url_byte) => {
                Err(format!("health path `{path}` (want /PATH of URL characters, at most {MAX_ROUTE} bytes)"))
            }
            Health::Http { .. } => Ok(()),
            Health::Exec(argv) if argv.is_empty() || argv.len() > 64 => Err("health `exec:` needs 1..=64 words".into()),
            Health::Exec(argv) => argv.iter().try_for_each(|a| arg_ok(a, "health command")),
        }
    }

    fn exec_bytes(&self) -> usize {
        match self {
            Health::Exec(argv) => argv.iter().map(|a| a.len() + 1).sum(),
            _ => 0,
        }
    }
}

impl fmt::Display for Health {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Health::Process => f.write_str("process"),
            Health::Http { port, path } => write!(f, "http:{port}{path}"),
            Health::Tcp { port } => write!(f, "tcp:{port}"),
            Health::Exec(argv) => write!(f, "exec:{}", argv.join(" ")),
        }
    }
}

/// A node TCP port whose connections go to the container's port `to` (reserved cluster-wide).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash, PartialOrd, Ord)]
pub struct PublicPort {
    pub port: u16,
    pub to: u16,
}

/// An allowed outgoing destination.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash, PartialOrd, Ord)]
pub enum Egress {
    /// Name resolution through the node's resolvers.
    Dns,
    /// `addr/prefix` (host bits zero), port 0 = any port.
    Net { addr: IpAddr, prefix: u8, port: u16 },
}

impl Egress {
    /// `dns`, `ADDR[/PREFIX][:PORT]` for IPv4, `ADDR[/PREFIX]` or `[ADDR[/PREFIX]]:PORT` for IPv6.
    pub fn parse(s: &str) -> Result<Egress, String> {
        if s == "dns" {
            return Ok(Egress::Dns);
        }
        let bad = || format!("egress `{s}` (want dns, CIDR, CIDR:PORT or [IPv6-CIDR]:PORT)");
        let (net, port) = if let Some(rest) = s.strip_prefix('[') {
            let (net, port) = rest.split_once("]:").ok_or_else(bad)?;
            (net, parse_port(port).map_err(|_| bad())?)
        } else if s.matches(':').count() == 1 {
            let (net, port) = s.split_once(':').ok_or_else(bad)?;
            (net, parse_port(port).map_err(|_| bad())?)
        } else {
            (s, 0)
        };
        let (addr, prefix) = match net.split_once('/') {
            Some((a, p)) if !p.is_empty() && p.len() <= 3 && p.bytes().all(|c| c.is_ascii_digit()) => {
                (a, Some(p.parse::<u8>().map_err(|_| bad())?))
            }
            Some(_) => return Err(bad()),
            None => (net, None),
        };
        let addr: IpAddr = addr.parse().map_err(|_| bad())?;
        let prefix = prefix.unwrap_or(if addr.is_ipv4() { 32 } else { 128 });
        let e = Egress::Net { addr, prefix, port };
        e.validate()?;
        Ok(e)
    }

    fn validate(&self) -> Result<(), String> {
        let Egress::Net { addr, prefix, .. } = *self else { return Ok(()) };
        let bits = if addr.is_ipv4() { 32 } else { 128 };
        if prefix > bits {
            return Err(format!("egress prefix /{prefix} longer than the address"));
        }
        if masked(addr, prefix) != addr {
            return Err(format!("egress `{self}` has host bits set (write {}/{prefix})", masked(addr, prefix)));
        }
        Ok(())
    }
}

fn masked(addr: IpAddr, prefix: u8) -> IpAddr {
    match addr {
        IpAddr::V4(a) => {
            let m = if prefix == 0 { 0 } else { u32::MAX << (32 - prefix.min(32)) };
            IpAddr::V4(Ipv4Addr::from(u32::from(a) & m))
        }
        IpAddr::V6(a) => {
            let m = if prefix == 0 { 0 } else { u128::MAX << (128 - prefix.min(128)) };
            IpAddr::V6(Ipv6Addr::from(u128::from(a) & m))
        }
    }
}

impl fmt::Display for Egress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Egress::Dns => f.write_str("dns"),
            Egress::Net { addr: IpAddr::V6(a), prefix, port } if port != 0 => write!(f, "[{a}/{prefix}]:{port}"),
            Egress::Net { addr, prefix, port: 0 } => write!(f, "{addr}/{prefix}"),
            Egress::Net { addr, prefix, port } => write!(f, "{addr}/{prefix}:{port}"),
        }
    }
}

fn parse_port(s: &str) -> Result<u16, String> {
    match s.parse::<u16>() {
        Ok(p) if p > 0 && s.bytes().all(|c| c.is_ascii_digit()) => Ok(p),
        _ => Err(format!("invalid port `{s}` (want 1..65535)")),
    }
}

fn url_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || b"-._~/%!$&'()*+,;=:@".contains(&c)
}

fn arg_ok(a: &str, what: &str) -> Result<(), String> {
    if a.contains('\0') {
        return Err(format!("{what} contains a NUL byte"));
    }
    Ok(())
}

/// `host[/path]` or `tls:host`: lowercase DNS names (a leading `*.` matches one label), a path of URL characters.
pub fn valid_route(r: &str) -> bool {
    if r.is_empty() || r.len() > MAX_ROUTE {
        return false;
    }
    let (tls, r) = match r.strip_prefix("tls:") {
        Some(rest) => (true, rest),
        None => (false, r),
    };
    let (host, path) = r.find('/').map_or((r, ""), |i| r.split_at(i));
    let host = host.strip_prefix("*.").unwrap_or(host);
    let label = |l: &str| {
        (1..=63).contains(&l.len())
            && l.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
            && !l.starts_with('-')
            && !l.ends_with('-')
    };
    !host.is_empty() && host.split('.').all(label) && (path.is_empty() || (!tls && path.bytes().all(url_byte)))
}

/// `user`: a name or uid, optionally `:group` (name or gid).
pub fn valid_user(u: &str) -> bool {
    let part = |p: &str| !p.is_empty() && p.bytes().all(|c| c.is_ascii_alphanumeric() || b"_.-".contains(&c));
    u.len() <= 64 && u.split(':').count() <= 2 && u.split(':').all(part)
}

/// An environment variable name: non-empty, no `=` or NUL, at most 255 bytes.
pub fn valid_env_key(k: &str) -> bool {
    !k.is_empty() && k.len() <= 255 && !k.bytes().any(|c| c == b'=' || c == 0)
}

/// An image service of the desired state. Its lists are canonical: sorted (images by architecture, `env` and
/// `secrets` by name, `public` by port) and without duplicates, which [`ImageService::canonicalize`] establishes and
/// `validate` checks, so the encoding is canonical without sorting.
#[derive(Clone, PartialEq, Eq, Debug, Hash, PartialOrd, Ord)]
pub struct ImageService {
    /// `[a-z0-9-]{1,63}`, unique among all services of a manifest.
    pub name: String,
    /// One image per architecture.
    pub images: Vec<ImageRef>,
    pub replicas: u32,
    /// Exact memory limit (multiple of 4 KiB, 4 KiB..64 GiB), scratch included.
    pub mem_bytes: u64,
    /// CPU quota in thousandths of a core; 0 = no quota.
    pub cpu_milli: u32,
    pub pids_max: u32,
    pub fds_max: u32,
    /// Replaces the image's Entrypoint (`None`: the image's).
    pub entrypoint: Option<Vec<String>>,
    /// Replaces the image's Cmd (`None`: the image's).
    pub args: Option<Vec<String>>,
    /// Working directory (empty: the image's).
    pub workdir: String,
    /// `user[:group]` (empty: the image's).
    pub user: String,
    /// Variables added to the image's environment.
    pub env: Vec<(String, String)>,
    /// Variables whose values are sealed with the cluster key (`secure::ClusterKey::seal_secret`).
    pub secrets: Vec<(String, Vec<u8>)>,
    /// Container port that receives routed connections and native callers (0: none).
    pub http: u16,
    /// HTTP routes (`host[/path]`) and TLS passthrough routes (`tls:host`).
    pub routes: Vec<String>,
    pub public: Vec<PublicPort>,
    /// Services this one may call (index-based like native services).
    pub calls: Calls,
    pub egress: Vec<Egress>,
    pub health: Health,
    pub stop_grace_ms: u32,
    /// Writable scratch (memory-backed, part of `mem_bytes`).
    pub scratch_bytes: u64,
    pub restart: Restart,
    /// Run a minimal reaper as PID 1.
    pub init: bool,
    /// Only nodes of this tenant run it (empty: nodes without a tenant).
    pub tenant: String,
}

impl ImageService {
    /// A service with the defaults of the text format: one replica, the image's command, restart always.
    pub fn new(name: &str, images: Vec<ImageRef>, mem_bytes: u64) -> ImageService {
        ImageService {
            name: name.to_string(),
            images,
            replicas: 1,
            mem_bytes,
            cpu_milli: 0,
            pids_max: DEFAULT_PIDS,
            fds_max: DEFAULT_FDS,
            entrypoint: None,
            args: None,
            workdir: String::new(),
            user: String::new(),
            env: Vec::new(),
            secrets: Vec::new(),
            http: 0,
            routes: Vec::new(),
            public: Vec::new(),
            calls: Calls::none(),
            egress: Vec::new(),
            health: Health::Process,
            stop_grace_ms: DEFAULT_STOP_GRACE_MS,
            scratch_bytes: (mem_bytes / 4) & !(PAGE - 1),
            restart: Restart::Always,
            init: false,
            tenant: String::new(),
        }
    }

    /// Sorts the lists into their canonical order (duplicates stay, for `validate` to report).
    pub fn canonicalize(&mut self) {
        self.images.sort();
        self.env.sort();
        self.secrets.sort();
        self.routes.sort();
        self.public.sort();
        self.egress.sort();
    }

    pub fn image_for(&self, arch: u8) -> Option<&[u8; 32]> {
        self.images.iter().find(|i| i.arch as u8 == arch).map(|i| &i.hash)
    }

    /// Node memory one replica takes: placement charges it, the hosting node reserves it.
    pub fn instance_bytes(&self) -> u64 {
        self.mem_bytes.saturating_add(IMAGE_OVERHEAD)
    }

    pub(crate) fn validate(&self) -> Result<(), String> {
        let n = &self.name;
        if !valid_name(n) {
            return Err(format!("invalid service name `{n}` (want [a-z0-9-], 1..=63 chars)"));
        }
        let e = |m: String| Err(format!("image service `{n}`: {m}"));
        if self.images.is_empty() {
            return e("no image".into());
        }
        match order(&self.images, |i| i.arch) {
            Some((a, true)) => return e(format!("two images for {}", a.name())),
            Some(_) => return e(UNSORTED.into()),
            None => {}
        }
        if self.replicas > crate::manifest::MAX_REPLICAS {
            return e("too many replicas".into());
        }
        if self.mem_bytes % PAGE != 0 || !(PAGE..=MAX_IMAGE_MEM).contains(&self.mem_bytes) {
            return e("memory must be a multiple of 4KiB between 4KiB and 64GiB".into());
        }
        if self.scratch_bytes % PAGE != 0 || self.scratch_bytes > self.mem_bytes {
            return e("scratch must be a multiple of 4KiB, at most `memory`".into());
        }
        if self.cpu_milli > MAX_CPU_MILLI {
            return e("cpu above 1024 cores".into());
        }
        if !(1..=MAX_PIDS).contains(&self.pids_max) || !(3..=MAX_FDS).contains(&self.fds_max) {
            return e(format!("pids_max must be 1..={MAX_PIDS} and fds_max 3..={MAX_FDS}"));
        }
        if self.stop_grace_ms > MAX_STOP_GRACE_MS {
            return e("stop_grace above 1h".into());
        }
        let argv = self.entrypoint.iter().chain(&self.args);
        if argv.clone().any(|l| l.len() > MAX_LIST) || self.env.len() > MAX_LIST || self.secrets.len() > MAX_LIST {
            return e(format!("more than {MAX_LIST} entries in a list"));
        }
        for a in argv.clone().flatten() {
            arg_ok(a, "an argument").or_else(e)?;
        }
        if !self.workdir.is_empty() && (!self.workdir.starts_with('/') || self.workdir.len() > MAX_PATH || self.workdir.contains('\0')) {
            return e("workdir must be an absolute path".into());
        }
        if !self.user.is_empty() && !valid_user(&self.user) {
            return e(format!("invalid user `{}` (want NAME or UID, optionally :GROUP)", self.user));
        }
        for (k, v) in &self.env {
            if !valid_env_key(k) {
                return e(format!("invalid variable name `{k}`"));
            }
            arg_ok(v, "an env value").or_else(e)?;
        }
        match order(&self.env, |(k, _)| k.as_str()) {
            Some((k, true)) => return e(format!("variable `{k}` set twice")),
            Some(_) => return e(UNSORTED.into()),
            None => {}
        }
        for (k, v) in &self.secrets {
            if !valid_env_key(k) || v.len() < crate::secure::SEALED_OVERHEAD {
                return e(format!("invalid secret `{k}`"));
            }
            if self.env.binary_search_by(|(ek, _)| ek.as_str().cmp(k)).is_ok() {
                return e(format!("variable `{k}` is both in env and in secrets"));
            }
        }
        match order(&self.secrets, |(k, _)| k.as_str()) {
            Some((k, true)) => return e(format!("secret `{k}` set twice")),
            Some(_) => return e(UNSORTED.into()),
            None => {}
        }
        let exec = argv.flatten().map(|a| a.len() + 1).sum::<usize>()
            + self.env.iter().map(|(k, v)| k.len() + v.len() + 2).sum::<usize>()
            + self.secrets.iter().map(|(k, v)| k.len() + v.len() + 2).sum::<usize>()
            + self.health.exec_bytes();
        if exec > MAX_EXEC_BYTES {
            return e(format!("arguments, env, secrets and health command exceed {} KiB", MAX_EXEC_BYTES >> 10));
        }
        if self.routes.len() > MAX_LIST || self.public.len() > MAX_LIST || self.egress.len() > MAX_LIST {
            return e(format!("more than {MAX_LIST} entries in a list"));
        }
        if let Some(r) = self.routes.iter().find(|r| !valid_route(r)) {
            return e(format!("invalid route `{r}` (want host[/path] or tls:host, lowercase)"));
        }
        match order(&self.routes, |r| r.as_str()) {
            Some((r, true)) => return e(format!("route `{r}` listed twice")),
            Some(_) => return e(UNSORTED.into()),
            None => {}
        }
        if !self.routes.is_empty() && self.http == 0 {
            return e("routes need `http` (the container port that receives them)".into());
        }
        if let Some(p) = self.public.iter().find(|p| p.port == 0 || p.to == 0) {
            return e(format!("public port {}:{} out of range", p.port, p.to));
        }
        match order(&self.public, |p| p.port) {
            Some((p, true)) => return e(format!("public port {p} listed twice")),
            Some(_) => return e(UNSORTED.into()),
            None => {}
        }
        for x in &self.egress {
            x.validate().or_else(e)?;
        }
        match order(&self.egress, |x| *x) {
            Some((x, true)) => return e(format!("egress `{x}` listed twice")),
            Some(_) => return e(UNSORTED.into()),
            None => {}
        }
        self.health.validate().or_else(e)?;
        if !self.tenant.is_empty() && !valid_name(&self.tenant) {
            return e(format!("invalid tenant `{}`", self.tenant));
        }
        Ok(())
    }

    /// The format-4 body after `name | kind` (`call_idx`: callee positions among the manifest's services); the
    /// lists as they are (canonical once `validate` accepts the service).
    pub(crate) fn encode_body(&self, out: &mut Vec<u8>, call_idx: &[usize]) {
        put_varint(out, self.images.len() as u64);
        for i in &self.images {
            out.push(i.arch as u8);
            out.extend_from_slice(&i.hash);
        }
        for v in [self.replicas as u64, self.mem_bytes / PAGE, self.cpu_milli as u64, self.pids_max as u64, self.fds_max as u64] {
            put_varint(out, v);
        }
        let flags = if self.entrypoint.is_some() { F_ENTRYPOINT } else { 0 }
            | if self.args.is_some() { F_ARGS } else { 0 }
            | if self.init { F_INIT } else { 0 }
            | if self.calls.is_any() { F_ANY } else { 0 };
        out.push(flags);
        out.push(self.restart as u8);
        for list in self.entrypoint.iter().chain(&self.args) {
            put_list(out, list);
        }
        put_bytes(out, self.workdir.as_bytes());
        put_short(out, self.user.as_bytes());
        put_varint(out, self.env.len() as u64);
        for (k, v) in &self.env {
            put_short(out, k.as_bytes());
            put_bytes(out, v.as_bytes());
        }
        put_varint(out, self.secrets.len() as u64);
        for (k, v) in &self.secrets {
            put_short(out, k.as_bytes());
            put_bytes(out, v);
        }
        put_varint(out, self.http as u64);
        put_varint(out, self.routes.len() as u64);
        self.routes.iter().for_each(|r| put_short(out, r.as_bytes()));
        put_varint(out, self.public.len() as u64);
        for p in &self.public {
            put_varint(out, p.port as u64);
            put_varint(out, p.to as u64);
        }
        put_varint(out, self.egress.len() as u64);
        for x in &self.egress {
            match *x {
                Egress::Dns => out.push(0),
                Egress::Net { addr, prefix, port } => {
                    match addr {
                        IpAddr::V4(a) => {
                            out.push(4);
                            out.extend_from_slice(&a.octets());
                        }
                        IpAddr::V6(a) => {
                            out.push(6);
                            out.extend_from_slice(&a.octets());
                        }
                    }
                    out.push(prefix);
                    put_u16(out, port);
                }
            }
        }
        match &self.health {
            Health::Process => out.push(0),
            Health::Http { port, path } => {
                out.push(1);
                put_varint(out, *port as u64);
                put_short(out, path.as_bytes());
            }
            Health::Tcp { port } => {
                out.push(2);
                put_varint(out, *port as u64);
            }
            Health::Exec(argv) => {
                out.push(3);
                put_list(out, argv);
            }
        }
        put_varint(out, self.stop_grace_ms as u64);
        put_varint(out, self.scratch_bytes / PAGE);
        put_short(out, self.tenant.as_bytes());
        put_varint(out, call_idx.len() as u64);
        call_idx.iter().for_each(|&i| put_varint(out, i as u64));
    }

    /// Decodes a body written by [`ImageService::encode_body`]: the service (calls `any` or none) and its callee
    /// indices (strictly increasing, below `count`). Only canonical encodings are accepted.
    pub(crate) fn decode_body(r: &mut Reader<'_>, name: &str, count: usize) -> Res<(ImageService, Vec<u32>)> {
        let n = list_len(r, Arch::ALL.len())?;
        let mut images = Vec::with_capacity(n);
        for _ in 0..n {
            let arch = Arch::from_u8(r.u8()?).ok_or(DecodeError::Invalid("image arch"))?;
            images.push(ImageRef { arch, hash: r.array::<32>()? });
        }
        let replicas = varint_u32(r)?;
        let mem_bytes = varint(r)?.checked_mul(PAGE).ok_or(DecodeError::Invalid("image memory"))?;
        let (cpu_milli, pids_max, fds_max) = (varint_u32(r)?, varint_u32(r)?, varint_u32(r)?);
        let flags = r.u8()?;
        if flags & !(F_ENTRYPOINT | F_ARGS | F_INIT | F_ANY) != 0 {
            return Err(DecodeError::Invalid("image flags"));
        }
        let restart = match r.u8()? {
            0 => Restart::Always,
            1 => Restart::OnFailure,
            2 => Restart::Never,
            _ => return Err(DecodeError::Invalid("image restart")),
        };
        let entrypoint = if flags & F_ENTRYPOINT != 0 { Some(get_list(r)?) } else { None };
        let args = if flags & F_ARGS != 0 { Some(get_list(r)?) } else { None };
        let workdir = get_str(r, MAX_PATH)?;
        let user = get_short(r)?;
        let n = list_len(r, MAX_LIST)?;
        let mut env = Vec::with_capacity(n);
        for _ in 0..n {
            env.push((get_short(r)?, get_str(r, MAX_EXEC_BYTES)?));
        }
        let n = list_len(r, MAX_LIST)?;
        let mut secrets = Vec::with_capacity(n);
        for _ in 0..n {
            let k = get_short(r)?;
            let len = varint(r)?;
            if len > MAX_EXEC_BYTES as u64 {
                return Err(DecodeError::TooLarge("secret"));
            }
            secrets.push((k, r.bytes(len as usize)?.to_vec()));
        }
        let http = u16::try_from(varint(r)?).map_err(|_| DecodeError::Invalid("image http port"))?;
        let n = list_len(r, MAX_LIST)?;
        let mut routes = Vec::with_capacity(n);
        for _ in 0..n {
            routes.push(get_short(r)?);
        }
        let n = list_len(r, MAX_LIST)?;
        let mut public = Vec::with_capacity(n);
        for _ in 0..n {
            let port = |r: &mut Reader<'_>| u16::try_from(varint(r)?).map_err(|_| DecodeError::Invalid("public port"));
            public.push(PublicPort { port: port(r)?, to: port(r)? });
        }
        let n = list_len(r, MAX_LIST)?;
        let mut egress = Vec::with_capacity(n);
        for _ in 0..n {
            let addr = match r.u8()? {
                0 => {
                    egress.push(Egress::Dns);
                    continue;
                }
                4 => IpAddr::V4(Ipv4Addr::from(r.array::<4>()?)),
                6 => IpAddr::V6(Ipv6Addr::from(r.array::<16>()?)),
                _ => return Err(DecodeError::Invalid("egress")),
            };
            egress.push(Egress::Net { addr, prefix: r.u8()?, port: r.u16()? });
        }
        let port = |r: &mut Reader<'_>| u16::try_from(varint(r)?).map_err(|_| DecodeError::Invalid("health port"));
        let health = match r.u8()? {
            0 => Health::Process,
            1 => Health::Http { port: port(r)?, path: get_short(r)? },
            2 => Health::Tcp { port: port(r)? },
            3 => Health::Exec(get_list(r)?),
            _ => return Err(DecodeError::Invalid("health")),
        };
        let stop_grace_ms = varint_u32(r)?;
        let scratch_bytes = varint(r)?.checked_mul(PAGE).ok_or(DecodeError::Invalid("image scratch"))?;
        let tenant = get_short(r)?;
        let n = varint(r)?;
        // Strictly increasing indices below `count`, at least a byte each: bounds n before allocating.
        if n > count as u64 || n > r.remaining() as u64 || (n > 0 && flags & F_ANY != 0) {
            return Err(DecodeError::Invalid("calls count"));
        }
        let mut idx: Vec<u32> = Vec::with_capacity(n as usize);
        for _ in 0..n {
            let x = varint(r)?;
            if x >= count as u64 || idx.last().is_some_and(|&p| x <= p as u64) {
                return Err(DecodeError::Invalid("calls index"));
            }
            idx.push(x as u32);
        }
        let s = ImageService {
            name: name.to_string(),
            images,
            replicas,
            mem_bytes,
            cpu_milli,
            pids_max,
            fds_max,
            entrypoint,
            args,
            workdir,
            user,
            env,
            secrets,
            http,
            routes,
            public,
            calls: if flags & F_ANY != 0 { Calls::any() } else { Calls::none() },
            egress,
            health,
            stop_grace_ms,
            scratch_bytes,
            restart,
            init: flags & F_INIT != 0,
            tenant,
        };
        // Lists out of their canonical order fail too: another order would be another encoding of the same service.
        s.validate().map_err(|_| DecodeError::Invalid("image service field"))?;
        Ok((s, idx))
    }
}

const UNSORTED: &str = "lists are not in canonical order (ImageService::canonicalize sorts them)";

/// The first key not strictly above its predecessor, and whether it equals it (a duplicate).
fn order<'a, T, K: Ord + Copy>(v: &'a [T], key: impl Fn(&'a T) -> K) -> Option<(K, bool)> {
    v.windows(2).map(|w| (key(&w[0]), key(&w[1]))).find(|(a, b)| a >= b).map(|(a, b)| (b, a == b))
}

fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    put_varint(out, b.len() as u64);
    out.extend_from_slice(b);
}

fn put_short(out: &mut Vec<u8>, b: &[u8]) {
    let n = b.len().min(255);
    out.push(n as u8);
    out.extend_from_slice(&b[..n]);
}

fn put_list(out: &mut Vec<u8>, list: &[String]) {
    put_varint(out, list.len() as u64);
    list.iter().for_each(|a| put_bytes(out, a.as_bytes()));
}

/// A count of entries of at least one byte each, at most `max`.
fn list_len(r: &mut Reader<'_>, max: usize) -> Res<usize> {
    let n = varint(r)?;
    if n > max as u64 || n > r.remaining() as u64 {
        return Err(DecodeError::Invalid("list length"));
    }
    Ok(n as usize)
}

fn get_str(r: &mut Reader<'_>, max: usize) -> Res<String> {
    let n = varint(r)?;
    if n > max as u64 {
        return Err(DecodeError::TooLarge("string"));
    }
    let s = core::str::from_utf8(r.bytes(n as usize)?).map_err(|_| DecodeError::Invalid("string (UTF-8)"))?;
    Ok(s.to_string())
}

fn get_short(r: &mut Reader<'_>) -> Res<String> {
    let n = r.u8()? as usize;
    let s = core::str::from_utf8(r.bytes(n)?).map_err(|_| DecodeError::Invalid("string (UTF-8)"))?;
    Ok(s.to_string())
}

fn get_list(r: &mut Reader<'_>) -> Res<Vec<String>> {
    let n = list_len(r, MAX_LIST)?;
    let mut v = Vec::with_capacity(n);
    for _ in 0..n {
        v.push(get_str(r, MAX_EXEC_BYTES)?);
    }
    Ok(v)
}
