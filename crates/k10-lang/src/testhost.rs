//! An in-memory implementation of the k10s host API (docs/HOSTAPI.md) on top of the `k10_ir` interpreter, for
//! testing Ten services without a node (feature `testhost`).
//!
//! ```
//! use k10_lang::testhost::TestHost;
//! let m = k10_lang::compile(r#"export fn handle() { out_str("hi"); }"#, None).unwrap();
//! let mut h = TestHost::new();
//! h.add("hello", m).unwrap();
//! let r = h.get("hello", "/");
//! assert_eq!((r.status, r.text().as_str()), (200, "hi"));
//! ```
//!
//! Semantics follow the node runtime: an instance's memory persists across requests (unless the service is
//! `fresh`, in which case every request starts from the snapshot taken after `init`); a trap answers 500 (calls:
//! -4) and discards the instance; service-to-service calls are POST requests dispatched synchronously to the
//! callee's instance (a call to a service that is already running — e.g. itself — returns -2, unavailable).
//! `now_ns`, `time_ms` and `random` are deterministic.

use k10_ir::{sem, Host, Interp, Module, Trap};
use std::collections::{BTreeMap, HashMap};

/// `req_method()` values.
pub const GET: i64 = 0;
pub const POST: i64 = 1;
pub const PUT: i64 = 2;
pub const DELETE: i64 = 3;

/// Response body limit (`output_write` beyond it traps with `Host(1)`).
pub const MAX_BODY: usize = 16 << 20;
pub const MAX_KEY: usize = 1024;
pub const MAX_VALUE: usize = 1 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HostFn {
    InputLen,
    InputRead,
    ReqPath,
    ReqMethod,
    OutputWrite,
    OutputStatus,
    Call,
    CallRespLen,
    CallRespRead,
    KvGet,
    KvPut,
    KvDel,
    Log,
    NowNs,
    TimeMs,
    Random,
    Config,
}

/// Name and parameter count of every host function.
pub const HOST_FUNCTIONS: &[(&str, u8)] = &[
    ("input_len", 0),
    ("input_read", 3),
    ("req_path", 2),
    ("req_method", 0),
    ("output_write", 2),
    ("output_status", 1),
    ("call", 4),
    ("call_resp_len", 0),
    ("call_resp_read", 3),
    ("kv_get", 4),
    ("kv_put", 4),
    ("kv_del", 2),
    ("log", 2),
    ("now_ns", 0),
    ("time_ms", 0),
    ("random", 0),
    ("config", 2),
];

fn host_fn(name: &str) -> Option<HostFn> {
    use HostFn::*;
    Some(match name {
        "input_len" => InputLen,
        "input_read" => InputRead,
        "req_path" => ReqPath,
        "req_method" => ReqMethod,
        "output_write" => OutputWrite,
        "output_status" => OutputStatus,
        "call" => Call,
        "call_resp_len" => CallRespLen,
        "call_resp_read" => CallRespRead,
        "kv_get" => KvGet,
        "kv_put" => KvPut,
        "kv_del" => KvDel,
        "log" => Log,
        "now_ns" => NowNs,
        "time_ms" => TimeMs,
        "random" => Random,
        "config" => Config,
        _ => return None,
    })
}

/// Deployment options of a test service.
#[derive(Clone, Debug, Default)]
pub struct ServiceConfig {
    /// Instance memory size `2^mem_log2` (default: `max(min_mem_log2, 16)`).
    pub mem_log2: Option<u8>,
    /// Bytes returned by `config()`.
    pub config: Vec<u8>,
    /// Every request starts from the memory snapshot taken after `init`.
    pub fresh: bool,
}

struct Service {
    interp: Interp,
    imports: Vec<HostFn>,
    module: Module,
    mem_log2: u8,
    cfg: ServiceConfig,
    /// Live instance memory (None until created or after a trap).
    mem: Option<Vec<u8>>,
    snapshot: Option<Vec<u8>>,
}

/// Result of one request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Response {
    pub status: i64,
    pub body: Vec<u8>,
    /// The trap that aborted the invocation, if any (status is then 500).
    pub trap: Option<Trap>,
}

impl Response {
    /// The body as (lossy) UTF-8.
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// A set of services plus the per-service KV stores, logs and deterministic clocks.
#[derive(Default)]
pub struct TestHost {
    services: HashMap<String, Service>,
    kv: HashMap<String, BTreeMap<Vec<u8>, Vec<u8>>>,
    logs: Vec<(String, String)>,
    clock_ns: i64,
    rng: u64,
    /// Number of host calls made (all services).
    pub host_calls: u64,
}

impl TestHost {
    pub fn new() -> TestHost {
        TestHost { clock_ns: 1_000_000, rng: 0x9E37_79B9_7F4A_7C15, ..TestHost::default() }
    }

    /// Deploys `module` as service `name` with default options. Fails like a node would: invalid module, unknown
    /// import, wrong import signature or missing `handle` export.
    pub fn add(&mut self, name: &str, module: Module) -> Result<(), String> {
        self.add_with(name, module, ServiceConfig::default())
    }

    pub fn add_with(&mut self, name: &str, module: Module, cfg: ServiceConfig) -> Result<(), String> {
        let interp = Interp::new(&module).map_err(|e| format!("invalid module: {e}"))?;
        let mut imports = Vec::new();
        for im in &module.imports {
            let f = host_fn(&im.name).ok_or_else(|| format!("unknown host function `{}`", im.name))?;
            let params = HOST_FUNCTIONS.iter().find(|h| h.0 == im.name).unwrap().1;
            if im.params != params || im.results != 1 {
                return Err(format!("host function `{}` imported with the wrong signature", im.name));
            }
            imports.push(f);
        }
        if module.export("handle").is_none() {
            return Err("module does not export `handle`".into());
        }
        let mem_log2 = cfg.mem_log2.unwrap_or(module.min_mem_log2.max(16)).max(module.min_mem_log2);
        self.services.insert(
            name.to_string(),
            Service { interp, imports, module, mem_log2, cfg, mem: None, snapshot: None },
        );
        Ok(())
    }

    /// GET request.
    pub fn get(&mut self, service: &str, path: &str) -> Response {
        self.request(service, GET, path, b"")
    }

    /// POST request.
    pub fn post(&mut self, service: &str, path: &str, body: &[u8]) -> Response {
        self.request(service, POST, path, body)
    }

    /// Lines logged so far, as (service, line).
    pub fn logs(&self) -> &[(String, String)] {
        &self.logs
    }

    /// The KV store of a service.
    pub fn kv(&self, service: &str) -> Option<&BTreeMap<Vec<u8>, Vec<u8>>> {
        self.kv.get(service)
    }

    /// Discards the instance of a service (the next request creates a new one and runs `init` again).
    pub fn restart(&mut self, service: &str) {
        if let Some(s) = self.services.get_mut(service) {
            s.mem = None;
        }
    }

    /// Size of a service's instance memory in bytes (`2^mem_log2`).
    pub fn mem_size(&self, service: &str) -> Option<usize> {
        self.services.get(service).map(|s| 1usize << s.mem_log2)
    }

    /// Sends a request to `service` (path suffix including any `?query`). Unknown service → status 404.
    pub fn request(&mut self, service: &str, method: i64, path: &str, body: &[u8]) -> Response {
        let Some(mut svc) = self.services.remove(service) else {
            return Response { status: 404, body: b"unknown service".to_vec(), trap: None };
        };
        let r = self.run(service, &mut svc, method, path.as_bytes(), body);
        self.services.insert(service.to_string(), svc);
        r
    }

    fn run(&mut self, name: &str, svc: &mut Service, method: i64, path: &[u8], body: &[u8]) -> Response {
        if svc.mem.is_none() {
            let mut mem = k10_ir::new_memory(&svc.module, svc.mem_log2);
            if let Some(init) = svc.interp.export("init") {
                let imports = svc.imports.clone();
                let mut inv = Invocation::new(self, name, svc, GET, b"", b"");
                inv.imports = &imports;
                let res = svc.interp.invoke(&mut mem, svc.mem_log2, &mut inv, init, &[]);
                if let Err(t) = res {
                    return Response { status: 500, body: format!("init trapped: {t:?}").into_bytes(), trap: Some(t) };
                }
            }
            if svc.cfg.fresh {
                svc.snapshot = Some(mem.clone());
            }
            svc.mem = Some(mem);
        } else if let (true, Some(snap), Some(mem)) = (svc.cfg.fresh, &svc.snapshot, &mut svc.mem) {
            mem.copy_from_slice(snap);
        }
        let handle = svc.interp.export("handle").expect("handle export");
        let mut mem = svc.mem.take().unwrap();
        let imports = svc.imports.clone();
        let mut inv = Invocation::new(self, name, svc, method, path, body);
        inv.imports = &imports;
        let res = svc.interp.invoke(&mut mem, svc.mem_log2, &mut inv, handle, &[]);
        let (status, output) = (inv.status, std::mem::take(&mut inv.output));
        match res {
            Ok(_) => {
                svc.mem = Some(mem);
                Response { status, body: output, trap: None }
            }
            Err(t) => Response { status: 500, body: format!("trap: {t:?}").into_bytes(), trap: Some(t) },
        }
    }

    fn next_random(&mut self) -> i64 {
        // xorshift64*
        let mut x = self.rng;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.rng = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D) as i64
    }
}

struct Invocation<'a> {
    host: &'a mut TestHost,
    service: String,
    imports: &'a [HostFn],
    method: i64,
    path: Vec<u8>,
    input: Vec<u8>,
    output: Vec<u8>,
    status: i64,
    last_call: Vec<u8>,
    config: Vec<u8>,
}

impl<'a> Invocation<'a> {
    fn new(host: &'a mut TestHost, name: &str, svc: &Service, method: i64, path: &[u8], body: &[u8]) -> Invocation<'a> {
        Invocation {
            host,
            service: name.to_string(),
            imports: &[],
            method,
            path: path.to_vec(),
            input: body.to_vec(),
            output: Vec::new(),
            status: 200,
            last_call: Vec::new(),
            config: svc.cfg.config.clone(),
        }
    }
}

/// Validates a (ptr, len) range of instance memory and returns it as a slice range.
fn range(mem: &[u8], ptr: i64, n: i64) -> Result<std::ops::Range<usize>, Trap> {
    let size = (mem.len() - k10_ir::TAIL_BYTES) as u64;
    let start = sem::check_range(ptr, n, size)?;
    Ok(start..start + n as usize)
}

/// Copies `src[off..off+n]` (clamped) to memory at `dst`; returns the number of bytes copied.
fn copy_out(mem: &mut [u8], src: &[u8], dst: i64, off: i64, n: i64) -> Result<i64, Trap> {
    let off = (off.max(0) as usize).min(src.len());
    let n = (n.max(0) as usize).min(src.len() - off);
    let r = range(mem, dst, n as i64)?;
    mem[r].copy_from_slice(&src[off..off + n]);
    Ok(n as i64)
}

impl Host for Invocation<'_> {
    fn call(&mut self, import: u32, a: &[i64], mem: &mut [u8]) -> Result<i64, Trap> {
        self.host.host_calls += 1;
        let f = self.imports[import as usize];
        Ok(match f {
            HostFn::InputLen => self.input.len() as i64,
            HostFn::InputRead => copy_out(mem, &self.input, a[0], a[1], a[2])?,
            HostFn::ReqPath => {
                let n = a[1].max(0).min(self.path.len() as i64);
                copy_out(mem, &self.path, a[0], 0, n)?;
                self.path.len() as i64
            }
            HostFn::ReqMethod => self.method,
            HostFn::OutputWrite => {
                let r = range(mem, a[0], a[1])?;
                if self.output.len() + r.len() > MAX_BODY {
                    return Err(Trap::Host(1));
                }
                self.output.extend_from_slice(&mem[r]);
                a[1]
            }
            HostFn::OutputStatus => {
                self.status = a[0];
                0
            }
            HostFn::Call => {
                let name = mem[range(mem, a[0], a[1])?].to_vec();
                let body = mem[range(mem, a[2], a[3])?].to_vec();
                let name = String::from_utf8_lossy(&name).into_owned();
                let (svc, path) = match name.find('/') {
                    Some(i) => (name[..i].to_string(), name[i..].to_string()),
                    None => (name, String::new()),
                };
                if !self.host.services.contains_key(&svc) {
                    self.last_call.clear();
                    return Ok(if svc == self.service { -2 } else { -1 });
                }
                let r = self.host.request(&svc, POST, &path, &body);
                self.last_call = r.body;
                if r.trap.is_some() {
                    self.last_call.clear();
                    -4
                } else {
                    r.status
                }
            }
            HostFn::CallRespLen => self.last_call.len() as i64,
            HostFn::CallRespRead => copy_out(mem, &self.last_call, a[0], a[1], a[2])?,
            HostFn::KvGet => {
                let key = mem[range(mem, a[0], a[1])?].to_vec();
                if key.len() > MAX_KEY {
                    return Err(Trap::Host(2));
                }
                let store = self.host.kv.entry(self.service.clone()).or_default();
                match store.get(&key) {
                    None => -1,
                    Some(v) => {
                        let v = v.clone();
                        copy_out(mem, &v, a[2], 0, a[3])?;
                        v.len() as i64
                    }
                }
            }
            HostFn::KvPut => {
                let key = mem[range(mem, a[0], a[1])?].to_vec();
                let val = mem[range(mem, a[2], a[3])?].to_vec();
                if key.len() > MAX_KEY || val.len() > MAX_VALUE {
                    return Err(Trap::Host(2));
                }
                self.host.kv.entry(self.service.clone()).or_default().insert(key, val);
                0
            }
            HostFn::KvDel => {
                let key = mem[range(mem, a[0], a[1])?].to_vec();
                if key.len() > MAX_KEY {
                    return Err(Trap::Host(2));
                }
                i64::from(self.host.kv.entry(self.service.clone()).or_default().remove(&key).is_some())
            }
            HostFn::Log => {
                let line = String::from_utf8_lossy(&mem[range(mem, a[0], a[1])?]).into_owned();
                self.host.logs.push((self.service.clone(), line));
                0
            }
            HostFn::NowNs => {
                self.host.clock_ns += 1000;
                self.host.clock_ns
            }
            HostFn::TimeMs => 1_700_000_000_000 + self.host.clock_ns / 1_000_000,
            HostFn::Random => self.host.next_random(),
            HostFn::Config => {
                let n = a[1].max(0).min(self.config.len() as i64);
                let cfg = self.config.clone();
                copy_out(mem, &cfg, a[0], 0, n)?;
                cfg.len() as i64
            }
        })
    }
}
