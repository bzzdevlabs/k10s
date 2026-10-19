//! Runs the real `k10s` binary: `node` as a child process, then `apply`, `status`, `logs`, HTTP traffic, SIGTERM.

use k10_node::demo;
use k10_node::http;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn tmpdir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("k10s-cli-{tag}-{}-{}", std::process::id(), k10_node::sys::mono_ns()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn node_apply_status_logs_and_restart() {
    let bin = env!("CARGO_BIN_EXE_k10s");
    let dir = tmpdir("e2e");
    let data = dir.join("data");
    std::fs::write(dir.join("echo.k10"), k10_ir::encode(&demo::echo())).unwrap();
    std::fs::write(dir.join("kv.k10"), k10_ir::encode(&demo::kv())).unwrap();
    std::fs::write(dir.join("logger.k10"), k10_ir::encode(&demo::logger())).unwrap();
    std::fs::write(
        dir.join("app.k10s"),
        "service echo { code = \"echo.k10\" memory = 64KiB }\nservice kv { code = \"kv.k10\" memory = 64KiB }\nservice logger { code = \"logger.k10\" memory = 64KiB }\n",
    )
    .unwrap();
    std::fs::write(dir.join("bad.k10s"), "service x { code = \"x.ten\" }\n").unwrap();
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let spawn = || {
        Command::new(bin)
            .args(["node", "--http", &addr, "--data", data.to_str().unwrap(), "--shards", "2", "--gossip", "127.0.0.1:7946"])
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    };
    let wait_up = || {
        let t = Instant::now();
        while http::request(&addr, "GET", "/_k10s/health", b"", Duration::from_secs(1)).is_err() {
            assert!(t.elapsed() < Duration::from_secs(10), "node did not come up");
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    let mut child = spawn();
    wait_up();
    let out = Command::new(bin).args(["apply", dir.join("app.k10s").to_str().unwrap(), "--node", &addr]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let txt = String::from_utf8_lossy(&out.stdout);
    assert!(txt.contains("\"added\":[\"echo\",\"kv\",\"logger\"]"), "{txt}");
    // .ten sources are a clear error until k10-lang is wired in.
    let out = Command::new(bin).args(["apply", dir.join("bad.k10s").to_str().unwrap(), "--node", &addr]).output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("Ten compiler"));
    assert_eq!(http::request(&addr, "GET", "/echo", b"", Duration::from_secs(5)).unwrap().text(), "hello\n");
    assert_eq!(http::request(&addr, "PUT", "/kv/k", b"v", Duration::from_secs(5)).unwrap().status, 200);
    assert_eq!(http::request(&addr, "POST", "/logger", b"from cli test", Duration::from_secs(5)).unwrap().status, 200);
    let out = Command::new(bin).args(["status", "--node", &addr]).output().unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("\"name\":\"echo\""));
    let out = Command::new(bin).args(["logs", "logger", "--node", &addr]).output().unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("from cli test"));
    // Graceful stop on SIGTERM, then restart: the bundle and KV data come back.
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    let st = child.wait().unwrap();
    assert!(st.success(), "{st:?}");
    let mut err = String::new();
    std::io::Read::read_to_string(child.stderr.as_mut().unwrap(), &mut err).unwrap();
    assert!(err.contains("cluster mode is not supported yet"), "{err}");
    let mut child = spawn();
    wait_up();
    assert_eq!(http::request(&addr, "GET", "/kv/k", b"", Duration::from_secs(5)).unwrap().text(), "v");
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    assert!(child.wait().unwrap().success());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn usage_errors() {
    let bin = env!("CARGO_BIN_EXE_k10s");
    let out = Command::new(bin).output().unwrap();
    assert!(!out.status.success());
    let out = Command::new(bin).args(["node", "--bogus", "1"]).output().unwrap();
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown flag --bogus"));
    let out = Command::new(bin).args(["node", "--backend", "jit"]).output().unwrap();
    assert!(String::from_utf8_lossy(&out.stderr).contains("not available yet"));
    let out = Command::new(bin).args(["status", "--node", "127.0.0.1:1"]).output().unwrap();
    assert!(String::from_utf8_lossy(&out.stderr).contains("cannot reach node"));
}
