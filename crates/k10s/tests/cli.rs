//! Runs the real `k10s` binary: `node` as a child process, then `apply`, `status`, `logs`, HTTP traffic, SIGTERM;
//! `build` and `run` on Ten sources. The node's backend is the binary's default (`K10S_BACKEND`, else jit).

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
    std::fs::write(dir.join("broken.ten"), "export fn handle() {\n    out_str(nope);\n}\n").unwrap();
    std::fs::write(dir.join("broken.k10s"), "service x { code = \"broken.ten\" }\n").unwrap();
    std::fs::write(dir.join("hi.ten"), "export fn handle() { out_str(\"hi from ten\"); }\n").unwrap();
    std::fs::write(dir.join("ten.k10s"), "service hi { code = \"hi.ten\" memory = 64KiB }\n").unwrap();
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let spawn = || {
        Command::new(bin)
            .args(["node", "--http", &addr, "--data", data.to_str().unwrap(), "--shards", "2"])
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
    // .ten sources are compiled by `apply`; errors are rendered diagnostics and nothing is sent.
    let out = Command::new(bin).args(["apply", dir.join("bad.k10s").to_str().unwrap(), "--node", &addr]).output().unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("cannot read") && err.contains("could not compile"), "{err}");
    let out = Command::new(bin).args(["apply", dir.join("broken.k10s").to_str().unwrap(), "--node", &addr]).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("cannot find `nope`") && err.contains("broken.ten:2:13") && err.contains("^^^^"), "{err}");
    let out = Command::new(bin).args(["apply", dir.join("ten.k10s").to_str().unwrap(), "--node", &addr]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stdout).contains("\"added\":[\"hi\"]"));
    assert_eq!(http::request(&addr, "GET", "/hi", b"", Duration::from_secs(5)).unwrap().text(), "hi from ten");
    // The first manifest again: `hi` is removed, the others are unchanged.
    let out = Command::new(bin).args(["apply", dir.join("app.k10s").to_str().unwrap(), "--node", &addr]).output().unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("\"removed\":[\"hi\"]"), "{}", String::from_utf8_lossy(&out.stdout));
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
    // Single-node mode: no cluster view.
    let mut err = String::new();
    std::io::Read::read_to_string(child.stderr.as_mut().unwrap(), &mut err).unwrap();
    assert!(!err.contains("cluster node"), "{err}");
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
    let out = Command::new(bin).args(["node", "--backend", "wasm"]).output().unwrap();
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown backend `wasm` (jit|interp)"));
    let out = Command::new(bin).args(["build"]).output().unwrap();
    assert!(String::from_utf8_lossy(&out.stderr).contains("usage: k10s build"));
    let out = Command::new(bin).args(["run", "x.ten", "--path", "nope"]).output().unwrap();
    assert!(!out.status.success());
    let out = Command::new(bin).args(["status", "--node", "127.0.0.1:1"]).output().unwrap();
    assert!(String::from_utf8_lossy(&out.stderr).contains("cannot reach node"));
}

/// `k10s build`: writes TIR next to the source (or `-o`), rendered diagnostics and exit code 1 on errors.
#[test]
fn build_ten() {
    let bin = env!("CARGO_BIN_EXE_k10s");
    let dir = tmpdir("build");
    let src = dir.join("svc.ten");
    std::fs::write(&src, "export fn handle() {\n    out_str(\"built\");\n}\n").unwrap();
    let out = Command::new(bin).args(["build", src.to_str().unwrap()]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let m = k10_ir::decode(&std::fs::read(dir.join("svc.k10")).unwrap()).unwrap();
    k10_ir::validate(&m).unwrap();
    assert!(m.export("handle").is_some());
    let o = dir.join("other.k10");
    let out = Command::new(bin).args(["build", src.to_str().unwrap(), "-o", o.to_str().unwrap()]).output().unwrap();
    assert!(out.status.success());
    assert_eq!(std::fs::read(&o).unwrap(), std::fs::read(dir.join("svc.k10")).unwrap());
    // Errors: every diagnostic, rendered with the offending line and a caret; nothing written.
    let bad = dir.join("bad.ten");
    std::fs::write(&bad, "export fn handle() {\n    let x: i64 = \"s\";\n    out_str(y);\n}\n").unwrap();
    let out = Command::new(bin).args(["build", bad.to_str().unwrap()]).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("error: mismatched types") && err.contains("bad.ten:2:18"), "{err}");
    assert!(err.contains("cannot find `y`") && err.contains("3 |     out_str(y);"), "{err}");
    assert!(err.contains("could not compile") && err.contains("(2 errors)"), "{err}");
    assert!(!dir.join("bad.k10").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

/// `k10s run`: one request in-process, body on stdout, on both backends.
#[test]
fn run_ten() {
    let bin = env!("CARGO_BIN_EXE_k10s");
    let ex = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples");
    let dir = tmpdir("run");
    for backend in ["jit", "interp"] {
        let run = |args: &[&str]| {
            let out = Command::new(bin).arg("run").args(args).args(["--backend", backend]).output().unwrap();
            (out.status.success(), String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned())
        };
        let (ok, body, err) = run(&[ex.join("hello.ten").to_str().unwrap()]);
        assert!(ok && body == "hello" && err.contains("HTTP 200"), "{backend}: {body} {err}");
        let (ok, body, _) = run(&[ex.join("echo.ten").to_str().unwrap(), "--method", "POST", "--body", "ping pong"]);
        assert!(ok && body == "ping pong", "{backend}: {body}");
        std::fs::write(dir.join("body.txt"), b"from a file").unwrap();
        let at = format!("@{}", dir.join("body.txt").display());
        let (ok, body, _) = run(&[ex.join("echo.ten").to_str().unwrap(), "--method", "post", "--body", &at]);
        assert!(ok && body == "from a file", "{backend}: {body}");
        let (ok, body, _) = run(&[ex.join("nbody.ten").to_str().unwrap(), "--path", "/?n=1000"]);
        assert!(ok && body == "-0.169075164\n-0.169087605\n", "{backend}: {body}");
        let (ok, body, _) = run(&[ex.join("todo.ten").to_str().unwrap(), "--method", "POST", "--path", "/", "--body", "x"]);
        assert!(ok && body == "{\"id\":1,\"title\":\"x\"}", "{backend}: {body}");
        // A trap fails the command (exit 1) and says why.
        std::fs::write(dir.join("trap.ten"), "export fn handle() { out_int(1 / input_len()); }\n").unwrap();
        let (ok, _, err) = run(&[dir.join("trap.ten").to_str().unwrap()]);
        assert!(!ok && err.contains("HTTP 500") && err.contains("DivByZero"), "{backend}: {err}");
        // Service logs go to stderr.
        std::fs::write(dir.join("log.ten"), "export fn handle() { log_str(\"hello log\"); }\n").unwrap();
        let (ok, _, err) = run(&[dir.join("log.ten").to_str().unwrap()]);
        assert!(ok && err.contains("[run] hello log"), "{backend}: {err}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}
