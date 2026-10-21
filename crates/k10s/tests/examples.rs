//! End-to-end: the Ten examples deployed with `k10s apply` (sources compiled by the CLI) to a real `k10s node`
//! process, checked over HTTP; `compute` against the Go reference workload; `counter` across a node restart.
//! The node runs the binary's default backend (`K10S_BACKEND`, else jit).

mod common;
mod support;
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
    let child = Command::new(k10s_bin())
        .args(["node", "--http", addr, "--data", data.to_str().unwrap(), "--shards", "2"])
        .args(["--admin-token-file", support::admin_key_file().to_str().unwrap()])
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
    let out = Command::new(k10s_bin())
        .args(["apply", manifest.to_str().unwrap(), "--node", &addr])
        .env("K10S_ADMIN_TOKEN_FILE", support::admin_key_file())
        .output()
        .unwrap();
    let txt = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{txt} {}", String::from_utf8_lossy(&out.stderr));
    assert!(txt.contains("\"added\":[\"chain\",\"compute\",\"counter\",\"echo\",\"hello\",\"nbody\",\"sort\",\"todo\"]"), "{txt}");
    let st = support::admin_req(&addr, "GET", "/_k10s/status", b"", Duration::from_secs(5)).unwrap().text();
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
    let before = calls(&support::admin_req(&addr, "GET", "/_k10s/status", b"", Duration::from_secs(5)).unwrap().text());
    assert_eq!(http::request(&addr, "GET", "/chain", b"", Duration::from_secs(5)).unwrap().text(), "hello");
    let after = calls(&support::admin_req(&addr, "GET", "/_k10s/status", b"", Duration::from_secs(5)).unwrap().text());
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

/// Applies a manifest file to the node at `addr` with the CLI.
fn apply(addr: &str, manifest: &std::path::Path) {
    let out = Command::new(k10s_bin())
        .args(["apply", manifest.to_str().unwrap(), "--node", addr])
        .env("K10S_ADMIN_TOKEN_FILE", support::admin_key_file())
        .output().unwrap();
    assert!(out.status.success(), "{} {}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
}

/// `counter` and `todo` under concurrency: 64 keep-alive clients on a 4-shard node (up to 64 instances of each
/// service, running in parallel on every shard). Every increment is counted exactly once (kv_add), every created
/// item gets its own id (kv_add) and the index loses no insertion or removal (kv_cas loops).
#[test]
fn atomic_kv_under_concurrency() {
    const CLIENTS: usize = 64;
    const INCS: usize = 100;
    const ITEMS: usize = 16;
    let dir = tmpdir("atomic");
    let addr = format!("127.0.0.1:{}", free_port());
    let child = Command::new(k10s_bin())
        .args(["node", "--http", &addr, "--data", dir.join("data").to_str().unwrap(), "--shards", "4"])
        .args(["--admin-token-file", support::admin_key_file().to_str().unwrap()])
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let node = Proc(child);
    wait_http(&addr, "/_k10s/health");
    let d = common::examples_dir();
    let manifest = dir.join("atomic.k10s");
    std::fs::write(
        &manifest,
        format!(
            "service counter {{ code = \"{}\" memory = 64KiB }}\nservice todo {{ code = \"{}\" memory = 1MiB }}\n",
            d.join("counter.ten").display(),
            d.join("todo.ten").display()
        ),
    )
    .unwrap();
    apply(&addr, &manifest);
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(CLIENTS));
    let threads: Vec<_> = (0..CLIENTS)
        .map(|t| {
            let (addr, barrier) = (addr.clone(), barrier.clone());
            std::thread::spawn(move || {
                let mut c = http::Client::connect(&addr, Duration::from_secs(30)).unwrap();
                barrier.wait();
                let mut seen = Vec::with_capacity(INCS);
                for _ in 0..INCS {
                    let r = c.request("GET", "/counter", b"").unwrap();
                    assert_eq!(r.status, 200, "{}", r.text());
                    seen.push(r.text().trim_end().parse::<i64>().unwrap());
                }
                // Create items, delete every other one.
                let mut kept = Vec::new();
                for i in 0..ITEMS {
                    let r = c.request("POST", "/todo", format!("t{t}-{i}").as_bytes()).unwrap();
                    assert_eq!(r.status, 201, "{}", r.text());
                    let txt = r.text();
                    let id: i64 = txt["{\"id\":".len()..txt.find(',').unwrap()].parse().unwrap();
                    if i % 2 == 0 {
                        let r = c.request("DELETE", &format!("/todo/{id}"), b"").unwrap();
                        assert_eq!(r.status, 200, "{}", r.text());
                    } else {
                        kept.push(id);
                    }
                }
                (seen, kept)
            })
        })
        .collect();
    let mut counts = Vec::new();
    let mut kept = Vec::new();
    for th in threads {
        let (s, k) = th.join().unwrap();
        counts.extend(s);
        kept.extend(k);
    }
    counts.sort_unstable();
    let want: Vec<i64> = (1..=(CLIENTS * INCS) as i64).collect();
    assert!(counts == want, "every increment answered a distinct value 1..={}", CLIENTS * INCS);
    let r = http::request(&addr, "GET", "/counter", b"", Duration::from_secs(5)).unwrap();
    assert_eq!(r.text(), format!("{}\n", CLIENTS * INCS + 1));
    // Distinct ids, and the index holds exactly the items not deleted.
    kept.sort_unstable();
    let n = kept.len();
    kept.dedup();
    assert_eq!(kept.len(), n, "todo ids are unique");
    let list = http::request(&addr, "GET", "/todo", b"", Duration::from_secs(5)).unwrap().text();
    let mut listed: Vec<i64> =
        list.split("{\"id\":").skip(1).map(|s| s[..s.find(',').unwrap()].parse().unwrap()).collect();
    assert_eq!(listed.len(), CLIENTS * ITEMS / 2, "{list}");
    listed.sort_unstable();
    assert_eq!(listed, kept);
    // The service's instances really ran concurrently (otherwise this test proves nothing).
    let st = support::admin_req(&addr, "GET", "/_k10s/status", b"", Duration::from_secs(5)).unwrap().text();
    let at = st.find("\"name\":\"counter\"").unwrap();
    let peak: u64 = {
        let s = &st[at..];
        let p = s.find("\"peak\":").unwrap() + 7;
        s[p..].chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse().unwrap()
    };
    assert!(peak >= 2, "counter ran on {peak} instance(s) at most: {st}");
    node.sigterm();
    let _ = std::fs::remove_dir_all(&dir);
}
