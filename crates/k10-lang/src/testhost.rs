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
//! Semantics follow the node runtime (the host functions are `k10_host::dispatch`): an instance's memory persists
//! across requests (unless the service is `fresh`, in which case every request starts from the snapshot taken after
//! `init`); a trap answers 500 (calls: -4) and discards the instance; service-to-service calls are POST requests
//! dispatched synchronously to the callee's instance (a call to a service that is already running — e.g. itself —
//! returns -2, unavailable). `now_ns`, `time_ms` and `random` are deterministic.

use k10_host::{Env, HostFn, KvRefused, CALL_TRAPPED, CALL_UNAVAILABLE, CALL_UNKNOWN};
use k10_ir::{Host, Interp, Module, Trap};
use std::collections::{BTreeMap, HashMap};

/// `req_method()` values.
pub const GET: i64 = 0;
pub const POST: i64 = 1;
pub const PUT: i64 = 2;
pub const DELETE: i64 = 3;

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
    clock_ns: u64,
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
        let imports = k10_host::resolve_imports(&module)?;
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

    /// Sets a key in a service's KV store (e.g. data left by an earlier version of the service).
    pub fn kv_set(&mut self, service: &str, key: &[u8], value: &[u8]) {
        self.kv.entry(service.to_string()).or_default().insert(key.to_vec(), value.to_vec());
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

    fn next_random(&mut self) -> u64 {
        // xorshift64*
        let mut x = self.rng;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.rng = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
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

impl Invocation<'_> {
    fn store(&mut self) -> &mut BTreeMap<Vec<u8>, Vec<u8>> {
        self.host.kv.entry(self.service.clone()).or_default()
    }
}

impl Host for Invocation<'_> {
    fn call(&mut self, import: u32, a: &[i64], mem: &mut [u8]) -> Result<i64, Trap> {
        self.host.host_calls += 1;
        let f = self.imports[import as usize];
        k10_host::dispatch(self, f, a, mem)
    }
}

impl Env for Invocation<'_> {
    fn method(&self) -> i64 {
        self.method
    }
    fn path(&self) -> &[u8] {
        &self.path
    }
    fn body(&self) -> &[u8] {
        &self.input
    }
    fn config(&self) -> &[u8] {
        &self.config
    }
    fn output_len(&self) -> usize {
        self.output.len()
    }
    fn output(&mut self, bytes: &[u8]) -> bool {
        self.output.extend_from_slice(bytes);
        true
    }
    fn set_status(&mut self, status: u16) {
        self.status = status as i64;
    }
    fn call(&mut self, target: Vec<u8>, body: Vec<u8>) -> i64 {
        let name = String::from_utf8_lossy(&target).into_owned();
        let (svc, path) = match name.find('/') {
            Some(i) => (name[..i].to_string(), name[i..].to_string()),
            None => (name, String::new()),
        };
        if !self.host.services.contains_key(&svc) {
            self.last_call.clear();
            return if svc == self.service { CALL_UNAVAILABLE } else { CALL_UNKNOWN };
        }
        let r = self.host.request(&svc, POST, &path, &body);
        self.last_call = r.body;
        if r.trap.is_some() {
            self.last_call.clear();
            CALL_TRAPPED
        } else {
            r.status
        }
    }
    fn call_response(&self) -> &[u8] {
        &self.last_call
    }
    fn kv_get(&mut self, key: &[u8], f: &mut dyn FnMut(Option<&[u8]>)) {
        f(self.store().get(key).map(|v| &v[..]))
    }
    fn kv_put(&mut self, key: &[u8], value: &[u8]) -> Result<(), KvRefused> {
        self.store().insert(key.to_vec(), value.to_vec());
        Ok(())
    }
    fn kv_del(&mut self, key: &[u8]) -> bool {
        self.store().remove(key).is_some()
    }
    fn kv_add(&mut self, key: &[u8], delta: i64) -> Result<i64, KvRefused> {
        let store = self.store();
        let cur = match store.get(key) {
            None => 0,
            Some(v) => i64::from_le_bytes(<[u8; 8]>::try_from(&v[..]).map_err(|_| KvRefused::NotInt)?),
        };
        if delta != 0 {
            store.insert(key.to_vec(), cur.wrapping_add(delta).to_le_bytes().to_vec());
        }
        Ok(cur.wrapping_add(delta))
    }
    fn kv_cas(&mut self, key: &[u8], expected: Option<&[u8]>, new: Option<&[u8]>) -> Result<bool, KvRefused> {
        let store = self.store();
        if store.get(key).map(|v| &v[..]) != expected {
            return Ok(false);
        }
        match new {
            Some(v) => store.insert(key.to_vec(), v.to_vec()),
            None => store.remove(key),
        };
        Ok(true)
    }
    fn log(&mut self, line: &[u8]) {
        let line = String::from_utf8_lossy(line).into_owned();
        self.host.logs.push((self.service.clone(), line));
    }
    fn now_ns(&mut self) -> u64 {
        self.host.clock_ns += 1000;
        self.host.clock_ns
    }
    fn time_ms(&mut self) -> u64 {
        1_700_000_000_000 + self.host.clock_ns / 1_000_000
    }
    fn random(&mut self) -> u64 {
        self.host.next_random()
    }
}
