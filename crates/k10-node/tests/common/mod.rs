#![allow(dead_code)]
use k10_mesh::{sha256::sha256, Bundle, Manifest, ServiceSpec};
use k10_node::http::{Client, ClientResponse};
use k10_node::{Node, NodeConfig};
use std::path::PathBuf;
use std::time::Duration;

pub fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("k10-node-it-{tag}-{}-{}", std::process::id(), k10_node::sys::mono_ns()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

pub fn config(shards: usize) -> NodeConfig {
    NodeConfig {
        http: "127.0.0.1:0".parse().unwrap(),
        shards,
        call_timeout: Duration::from_secs(2),
        ..NodeConfig::default()
    }
}

pub fn start(cfg: NodeConfig) -> Node {
    Node::start(cfg).expect("node start")
}

pub struct Svc {
    pub spec: ServiceSpec,
    pub code: Vec<u8>,
}

pub fn svc(name: &str, m: &k10_ir::Module) -> Svc {
    let code = k10_ir::encode(m);
    Svc {
        spec: ServiceSpec {
            name: name.into(),
            code_hash: sha256(&code),
            replicas: 1,
            mem_log2: 16,
            max_instances: 64,
            cpu_ms: 1000,
            fresh: false,
            config: Vec::new(),
            calls: Vec::new(),
        },
        code,
    }
}

impl Svc {
    pub fn max(mut self, n: u32) -> Self {
        self.spec.max_instances = n;
        self
    }
    pub fn cpu(mut self, ms: u32) -> Self {
        self.spec.cpu_ms = ms;
        self
    }
    pub fn fresh(mut self) -> Self {
        self.spec.fresh = true;
        self
    }
    pub fn mem(mut self, log2: u8) -> Self {
        self.spec.mem_log2 = log2;
        self
    }
    pub fn config(mut self, c: &str) -> Self {
        self.spec.config = c.as_bytes().to_vec();
        self
    }
    /// The services this one may `call` (manifest allowlist).
    pub fn calls(mut self, names: &[&str]) -> Self {
        self.spec.calls = k10_mesh::manifest::canonical_calls(names);
        self
    }
}

pub fn bundle(svcs: Vec<Svc>) -> Bundle {
    let mut blobs: Vec<Vec<u8>> = Vec::new();
    let mut specs = Vec::new();
    for s in svcs {
        if !blobs.contains(&s.code) {
            blobs.push(s.code);
        }
        specs.push(s.spec);
    }
    Bundle::new(Manifest { version: 0, services: specs }, blobs)
}

pub fn client(node: &Node) -> Client {
    Client::connect(&node.http_addr().to_string(), Duration::from_secs(30)).unwrap()
}

pub fn get(node: &Node, path: &str) -> ClientResponse {
    client(node).request("GET", path, b"").unwrap()
}

pub fn post(node: &Node, path: &str, body: &[u8]) -> ClientResponse {
    client(node).request("POST", path, body).unwrap()
}

/// Extracts `"key":<number>` following the first occurrence of `"name":"<svc>"` in the status JSON.
pub fn svc_stat(status: &str, svc: &str, key: &str) -> i64 {
    let at = status.find(&format!("\"name\":\"{svc}\"")).unwrap_or_else(|| panic!("service {svc} not in status"));
    stat_from(&status[at..], key)
}

pub fn stat_from(s: &str, key: &str) -> i64 {
    let k = format!("\"{key}\":");
    let at = s.find(&k).unwrap_or_else(|| panic!("{key} not in status")) + k.len();
    let num: String = s[at..].chars().take_while(|c| c.is_ascii_digit() || *c == '-').collect();
    num.parse().unwrap()
}
