//! The Ten examples (`examples/*.ten`) as a deployment, and the answers expected from them (shared by
//! `tests/examples.rs` and `tests/cluster.rs`).
#![allow(dead_code)]

use k10_mesh::sha256::{sha256, to_hex};
use k10_node::http::Client;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The binary under test: `K10S_BIN` (e.g. a release archive's), else this build's.
pub fn k10s_bin() -> &'static str {
    static BIN: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    BIN.get_or_init(|| std::env::var("K10S_BIN").unwrap_or_else(|_| env!("CARGO_BIN_EXE_k10s").into()))
}

pub fn examples_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples").canonicalize().expect("examples/ directory")
}

/// Every example as a service (absolute `code` paths, so the manifest can live anywhere). `chain` calls `echo`
/// (its config names the backend); `sort` needs 16 B per element (n = 100 000 by default) of heap.
pub fn examples_manifest() -> String {
    let d = examples_dir();
    let p = |f: &str| d.join(f).display().to_string();
    format!(
        concat!(
            "service hello   {{ code = \"{}\" memory = 64KiB }}\n",
            "service echo    {{ code = \"{}\" memory = 1MiB }}\n",
            "service compute {{ code = \"{}\" memory = 64KiB cpu_ms = 60000 }}\n",
            "service chain   {{ code = \"{}\" memory = 64KiB config = \"echo\" calls = [\"echo\"] }}\n",
            "service counter {{ code = \"{}\" memory = 64KiB }}\n",
            "service todo    {{ code = \"{}\" memory = 1MiB }}\n",
            "service nbody   {{ code = \"{}\" memory = 64KiB cpu_ms = 60000 }}\n",
            "service sort    {{ code = \"{}\" memory = 4MiB cpu_ms = 60000 }}\n",
        ),
        p("hello.ten"),
        p("echo.ten"),
        p("compute.ten"),
        p("chain.ten"),
        p("counter.ten"),
        p("todo.ten"),
        p("nbody.ten"),
        p("sort.ten"),
    )
}

/// What bench/workloads/compute (Go) answers to `/?n=N`: hex(sha256^(N+1)("k10s")) + "\n".
pub fn compute_expected(n: usize) -> String {
    let mut h = sha256(b"k10s");
    for _ in 0..n {
        h = sha256(&h);
    }
    format!("{}\n", to_hex(&h))
}

fn req(c: &mut Client, method: &str, path: &str, body: &[u8]) -> (u16, String) {
    let r = c.request(method, path, body).unwrap_or_else(|e| panic!("{method} {path}: {e}"));
    (r.status, r.text())
}

/// Checks the stateless examples and `chain` → `echo` through the ingress at `addr`.
pub fn check_stateless(addr: &str) {
    let mut c = Client::connect(addr, Duration::from_secs(30)).unwrap();
    assert_eq!(req(&mut c, "GET", "/hello", b""), (200, "hello".into()), "via {addr}");
    assert_eq!(req(&mut c, "GET", "/echo", b""), (200, "hello".into()));
    assert_eq!(req(&mut c, "POST", "/echo/x", b"some body"), (200, "some body".into()));
    let big: Vec<u8> = (0..100_000u32).map(|i| b'a' + (i % 26) as u8).collect();
    let r = c.request("POST", "/echo", &big).unwrap();
    assert!(r.status == 200 && r.body == big, "100 kB echo");
    assert_eq!(req(&mut c, "GET", "/compute?n=1000", b""), (200, compute_expected(1000)));
    assert_eq!(req(&mut c, "GET", "/compute/?n=1", b""), (200, compute_expected(1)));
    // Go `strconv.Atoi` rules: absent, junk, <= 0 → 1000.
    for q in ["", "?n=0", "?n=-5", "?n=12x", "?m=3"] {
        assert_eq!(req(&mut c, "GET", &format!("/compute{q}"), b""), (200, compute_expected(1000)), "compute{q}");
    }
    assert_eq!(req(&mut c, "GET", "/chain", b""), (200, "hello".into()), "chain → echo via {addr}");
    assert_eq!(req(&mut c, "GET", "/nbody?n=1000", b""), (200, "-0.169075164\n-0.169087605\n".into()));
    assert_eq!(req(&mut c, "GET", "/sort?n=1000", b"").0, 200);
    assert_eq!(req(&mut c, "GET", "/sort", b""), (200, "n=100000 checksum=373804995104525048\n".into()));
    assert_eq!(req(&mut c, "GET", "/sort?n=1000000", b"").0, 507, "not enough memory");
}

/// A to-do life cycle through `addr`; returns the id it created (and deleted).
pub fn check_todo(addr: &str) -> i64 {
    let mut c = Client::connect(addr, Duration::from_secs(30)).unwrap();
    let (st, created) = req(&mut c, "POST", "/todo", b"buy \"milk\"");
    assert_eq!(st, 201, "{created}");
    let id: i64 = created
        .strip_prefix("{\"id\":")
        .and_then(|r| r.split(',').next())
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("{created}"));
    assert_eq!(created, format!("{{\"id\":{id},\"title\":\"buy \\\"milk\\\"\"}}"));
    let (st, list) = req(&mut c, "GET", "/todo", b"");
    assert!(st == 200 && list.contains(&created), "{list}");
    assert_eq!(req(&mut c, "GET", &format!("/todo/{id}"), b""), (200, created));
    let (st, upd) = req(&mut c, "PUT", &format!("/todo/{id}"), b"buy bread");
    assert_eq!((st, upd), (200, format!("{{\"id\":{id},\"title\":\"buy bread\"}}")));
    assert_eq!(req(&mut c, "DELETE", &format!("/todo/{id}"), b""), (200, format!("{{\"deleted\":{id}}}")));
    assert_eq!(req(&mut c, "GET", &format!("/todo/{id}"), b""), (404, "{\"error\":\"not found\"}".into()));
    assert_eq!(req(&mut c, "GET", "/todo/abc", b"").0, 400);
    assert_eq!(req(&mut c, "PATCH", "/todo", b"").0, 405);
    id
}

/// One counter increment through `addr`.
pub fn counter_next(addr: &str) -> i64 {
    let mut c = Client::connect(addr, Duration::from_secs(30)).unwrap();
    let (st, t) = req(&mut c, "GET", "/counter", b"");
    assert_eq!(st, 200, "{t}");
    assert!(t.ends_with('\n'), "{t:?}");
    t.trim_end().parse().unwrap_or_else(|_| panic!("{t:?}"))
}
