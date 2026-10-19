//! End-to-end: the Ten examples deployed with `k10s apply` (sources compiled by the CLI) to a real `k10s node`
//! process, checked over HTTP; `compute` against the Go reference workload; `counter` across a node restart.
//! The node runs the binary's default backend (`K10S_BACKEND`, else jit).

mod common;
use common::*;
use k10_node::http;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Kills the child on drop (a failing assertion must not leak a node process).
struct Proc(Child);

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Proc {
    fn sigterm(mut self) {
        // SAFETY: plain syscall on our own child.
        unsafe { libc::kill(self.0.id() as i32, libc::SIGTERM) };
        let st = self.0.wait().unwrap();
        assert!(st.success(), "{st:?}");
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn tmpdir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("k10s-examples-{tag}-{}-{}", std::process::id(), k10_node::sys::mono_ns()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn wait_http(addr: &str, path: &str) {
    let t = Instant::now();
    while http::request(addr, "GET", path, b"", Duration::from_secs(1)).is_err() {
        assert!(t.elapsed() < Duration::from_secs(20), "{addr} did not come up");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn spawn_node(addr: &str, data: &std::path::Path) -> Proc {
    let child = Command::new(env!("CARGO_BIN_EXE_k10s"))
        .args(["node", "--http", addr, "--data", data.to_str().unwrap(), "--shards", "2"])
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let p = Proc(child);
    wait_http(addr, "/_k10s/health");
    p
}

#[test]
fn examples_on_a_node() {
    let dir = tmpdir("single");
    let data = dir.join("data");
    let addr = format!("127.0.0.1:{}", free_port());
    let node = spawn_node(&addr, &data);
    let manifest = dir.join("examples.k10s");
    std::fs::write(&manifest, examples_manifest()).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_k10s"))
        .args(["apply", manifest.to_str().unwrap(), "--node", &addr])
        .output()
        .unwrap();
    let txt = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{txt} {}", String::from_utf8_lossy(&out.stderr));
    assert!(txt.contains("\"added\":[\"chain\",\"compute\",\"counter\",\"echo\",\"hello\",\"nbody\",\"sort\",\"todo\"]"), "{txt}");
    let st = http::request(&addr, "GET", "/_k10s/status", b"", Duration::from_secs(5)).unwrap().text();
    let backend = std::env::var("K10S_BACKEND").unwrap_or_else(|_| "jit".into());
    assert!(st.contains(&format!("\"backend\":\"{backend}\"")), "{st}");
    assert!(st.contains("\"code\":{\"modules\":8,"), "{st}");

    check_stateless(&addr);
    check_todo(&addr);
    let first = check_todo(&addr);
    assert!(first >= 2, "ids keep increasing");
    for want in 1..=3 {
        assert_eq!(counter_next(&addr), want);
    }
    // The chain → echo call is a real service call (echo's request counter moves).
    let calls = |s: &str| -> i64 {
        let at = s.find("\"calls\":").unwrap() + 8;
        s[at..].chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse().unwrap()
    };
    let before = calls(&http::request(&addr, "GET", "/_k10s/status", b"", Duration::from_secs(5)).unwrap().text());
    assert_eq!(http::request(&addr, "GET", "/chain", b"", Duration::from_secs(5)).unwrap().text(), "hello");
    let after = calls(&http::request(&addr, "GET", "/_k10s/status", b"", Duration::from_secs(5)).unwrap().text());
    assert_eq!(after, before + 1);

    // Byte-identical to the Go reference workload, when its binary is built (bench/workloads/build.sh).
    let go = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../bench/bin/compute");
    if go.exists() {
        let port = free_port();
        let child = Command::new(&go).env("PORT", port.to_string()).stderr(Stdio::null()).spawn().unwrap();
        let _go = Proc(child);
        let goaddr = format!("127.0.0.1:{port}");
        wait_http(&goaddr, "/?n=1");
        for q in ["?n=1000", "?n=1", "?n=0", "?n=x", "?n=20000", ""] {
            let a = http::request(&goaddr, "GET", &format!("/{q}"), b"", Duration::from_secs(10)).unwrap();
            let b = http::request(&addr, "GET", &format!("/compute{q}"), b"", Duration::from_secs(10)).unwrap();
            assert_eq!((a.status, &a.body), (b.status, &b.body), "compute{q}: Go vs k10s");
        }
    } else {
        eprintln!("bench/bin/compute not built: comparing with the Rust SHA-256 only");
    }

    // Restart with the same data directory: the bundle is reloaded and the counter continues.
    node.sigterm();
    let node = spawn_node(&addr, &data);
    wait_http(&addr, "/hello");
    assert_eq!(counter_next(&addr), 4);
    assert_eq!(http::request(&addr, "GET", "/hello", b"", Duration::from_secs(5)).unwrap().text(), "hello");
    node.sigterm();
    let _ = std::fs::remove_dir_all(&dir);
}
