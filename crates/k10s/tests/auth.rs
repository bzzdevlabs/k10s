//! Authentication: the admin API's bearer token, loopback-only default and separate listener; the cluster key
//! (sealed gossip, authenticated peer links) and the refusal to run a cluster without one.

mod support;
use support::*;

use k10_node::http;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

const T: Duration = Duration::from_secs(10);

fn write_secret(path: &Path, s: &str) {
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path).unwrap();
    writeln!(f, "{s}").unwrap();
}

fn get(addr: &str, path: &str, token: Option<&str>) -> (u16, String) {
    let r = http::request_auth(addr, "GET", path, b"", T, token).unwrap();
    (r.status, r.text())
}

/// Sends a raw request head and returns the first response bytes (the node answers before reading any body).
fn raw(addr: &str, head: &str) -> String {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(T)).unwrap();
    s.write_all(head.as_bytes()).unwrap();
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match s.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn k10s(args: &[&str], envs: &[(&str, &str)]) -> std::process::Output {
    let mut c = Command::new(bin());
    c.args(args).env_remove("K10S_ADMIN_TOKEN").env_remove("K10S_ADMIN_TOKEN_FILE");
    for (k, v) in envs {
        c.env(k, v);
    }
    c.output().unwrap()
}

#[test]
fn admin_token_guards_everything_but_health_and_metrics() {
    let dir = tmpdir("authtok");
    let tok = dir.join("admin.token");
    write_secret(&tok, "correct-horse-battery-staple");
    let n = spawn("tok", &dir.join("data"), &["--http".into(), "127.0.0.1:0".into(), "--shards".into(), "1".into(), "--admin-token-file".into(), tok.display().to_string()]);
    let t = Some("correct-horse-battery-staple");
    for p in ["/_k10s/status", "/_k10s/logs/x", "/_k10s/cluster", "/_k10s/nope"] {
        assert_eq!(get(&n.http, p, None).0, 401, "{p} without a token");
        assert_eq!(get(&n.http, p, Some("correct-horse-battery-stapl")).0, 401, "{p} with a wrong token");
    }
    assert_eq!(get(&n.http, "/_k10s/status", t).0, 200);
    assert_eq!(get(&n.http, "/_k10s/health", None).0, 200);
    assert_eq!(get(&n.http, "/_k10s/metrics", None).0, 200);
    // The challenge, and an apply refused before its (claimed 200 MiB) body is read.
    let r = raw(&n.http, "POST /_k10s/apply HTTP/1.1\r\nhost: x\r\ncontent-length: 209715200\r\n\r\n");
    assert!(r.starts_with("HTTP/1.1 401") && r.contains("www-authenticate: Bearer") && r.contains("connection: close"), "{r}");
    let r = raw(&n.http, "GET /_k10s/status HTTP/1.1\r\nauthorization: Basic Y29ycmVjdA==\r\n\r\n");
    assert!(r.starts_with("HTTP/1.1 401"), "{r}");
    // CLI: refused without the token (with a hint), accepted with --admin-token-file or the environment.
    let m = dir.join("app.k10s");
    std::fs::write(&m, format!("service echo {{ code = \"{}\" memory = 64KiB }}\n", examples_dir().join("echo.ten").display())).unwrap();
    let no = k10s(&["apply", m.to_str().unwrap(), "--node", &n.http], &[]);
    assert!(!no.status.success() && String::from_utf8_lossy(&no.stderr).contains("admin token"), "{no:?}");
    let ok = k10s(&["apply", m.to_str().unwrap(), "--node", &n.http, "--admin-token-file", tok.to_str().unwrap()], &[]);
    assert!(ok.status.success(), "{ok:?}");
    let ok = k10s(&["status", "--json", "--node", &n.http], &[("K10S_ADMIN_TOKEN_FILE", tok.to_str().unwrap())]);
    assert!(ok.status.success() && String::from_utf8_lossy(&ok.stdout).contains("\"name\":\"echo\""), "{ok:?}");
    let ok = k10s(&["logs", "echo", "--node", &n.http], &[("K10S_ADMIN_TOKEN", "correct-horse-battery-staple")]);
    assert!(ok.status.success(), "{ok:?}");
    // Services are unaffected.
    assert_eq!(http::request(&n.http, "GET", "/echo/x", b"", T).unwrap().status, 200);
    let _ = std::fs::remove_dir_all(&dir);
}

fn examples_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples")
}

#[test]
fn without_a_token_only_loopback_clients_may_administer() {
    let dir = tmpdir("authlo");
    let n = spawn("lo", &dir.join("data"), &["--http".into(), "0.0.0.0:0".into(), "--shards".into(), "1".into()]);
    let port = n.http.rsplit(':').next().unwrap().to_string();
    let lo = format!("127.0.0.1:{port}");
    assert_eq!(get(&lo, "/_k10s/status", None).0, 200);
    // A local proxy relaying a remote client is not a loopback client.
    let r = raw(&lo, "GET /_k10s/status HTTP/1.1\r\nx-forwarded-for: 203.0.113.9\r\n\r\n");
    assert!(r.starts_with("HTTP/1.1 403"), "{r}");
    let ip = k10_node::cluster::detect_ip();
    if ip.is_loopback() {
        eprintln!("no non-loopback address on this machine: remote case skipped");
    } else {
        let remote = format!("{ip}:{port}");
        let (st, body) = get(&remote, "/_k10s/status", None);
        assert_eq!(st, 403, "{body}");
        let r = raw(&remote, "POST /_k10s/apply HTTP/1.1\r\ncontent-length: 1000\r\n\r\n");
        assert!(r.starts_with("HTTP/1.1 403"), "{r}");
        assert_eq!(get(&remote, "/_k10s/health", None).0, 200);
        assert_eq!(get(&remote, "/_k10s/metrics", None).0, 200);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn separate_admin_listener() {
    let dir = tmpdir("authsep");
    let tok = dir.join("admin.token");
    write_secret(&tok, "sep-token");
    let n = spawn(
        "sep",
        &dir.join("data"),
        &["--http".into(), "127.0.0.1:0".into(), "--shards".into(), "2".into(), "--admin".into(), "127.0.0.1:0".into(), "--admin-token-file".into(), tok.display().to_string()],
    );
    let log = n.log();
    let at = log.find("admin API on http://").expect("admin listener line") + "admin API on http://".len();
    let admin = log[at..].split(':').take(2).collect::<Vec<_>>().join(":");
    let admin = admin.trim_end_matches(|c: char| !c.is_ascii_digit()).to_string();
    // Ingress: only health; admin listener: the API (with the token), no services.
    assert_eq!(get(&n.http, "/_k10s/health", None).0, 200);
    assert_eq!(get(&n.http, "/_k10s/status", Some("sep-token")).0, 404);
    assert_eq!(get(&n.http, "/_k10s/metrics", None).0, 404);
    assert_eq!(get(&admin, "/_k10s/status", None).0, 401);
    assert_eq!(get(&admin, "/_k10s/metrics", None).0, 200);
    let body = bundle(0, &[echo_svc()]).encode();
    let r = http::request_auth(&admin, "POST", "/_k10s/apply", &body, T, Some("sep-token")).unwrap();
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(get(&admin, "/echo", None).0, 404);
    assert_eq!(http::request(&n.http, "GET", "/echo", b"", T).unwrap().text(), "hello");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn cluster_mode_needs_a_key_or_an_explicit_insecure_flag() {
    let dir = tmpdir("authnokey");
    let out = k10s(&["node", "--http", "127.0.0.1:0", "--gossip", "127.0.0.1:0", "--rpc", "127.0.0.1:0", "--data", dir.join("d").to_str().unwrap()], &[]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success() && err.contains("--cluster-key-file") && err.contains("keygen"), "{err}");
    let bad = dir.join("bad.key");
    write_secret(&bad, "not-hex");
    let out = k10s(&["node", "--gossip", "127.0.0.1:0", "--cluster-key-file", bad.to_str().unwrap(), "--data", dir.join("d").to_str().unwrap()], &[]);
    assert!(!out.status.success() && String::from_utf8_lossy(&out.stderr).contains("64 hex digits"), "{out:?}");
    // keygen: a new 0600 file, never overwritten.
    let k = dir.join("new.key");
    let out = k10s(&["keygen", "-o", k.to_str().unwrap()], &[]);
    assert!(out.status.success(), "{out:?}");
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(&k).unwrap().permissions().mode() & 0o777, 0o600);
    assert!(k10_mesh::secure::ClusterKey::parse_hex(&std::fs::read_to_string(&k).unwrap()).is_ok());
    assert!(!k10s(&["keygen", "-o", k.to_str().unwrap()], &[]).status.success());
    // Explicitly insecure nodes still form a cluster (and say so loudly).
    let a = spawn("ins1", &dir.join("i1"), &["--http".into(), "127.0.0.1:0".into(), "--gossip".into(), "127.0.0.1:0".into(), "--rpc".into(), "127.0.0.1:0".into(), "--probe-ms".into(), "100".into(), "--insecure-unauthenticated-cluster".into()]);
    let b = spawn(
        "ins2",
        &dir.join("i2"),
        &["--http".into(), "127.0.0.1:0".into(), "--gossip".into(), "127.0.0.1:0".into(), "--rpc".into(), "127.0.0.1:0".into(), "--probe-ms".into(), "100".into(), "--insecure-unauthenticated-cluster".into(), "--join".into(), a.gossip.clone()],
    );
    assert!(a.log().contains("WARNING") && a.log().contains("NOT authenticated"));
    wait_for("insecure pair converges", Duration::from_secs(20), || live(&a) == 2 && live(&b) == 2);
    deploy(&a.http, &[echo_svc().replicas(1)]);
    wait_for("insecure pair serves echo", Duration::from_secs(20), || {
        [&a, &b].iter().all(|n| http::request(&n.http, "GET", "/echo", b"", T).is_ok_and(|r| r.text() == "hello"))
    });
    let _ = std::fs::remove_dir_all(&dir);
}

fn live(n: &NodeProc) -> i64 {
    let st = n.status();
    st.find("\"cluster\":{").and_then(|i| num_after(&st[i..], "live_members")).unwrap_or(0)
}

fn cluster_num(n: &NodeProc, key: &str) -> i64 {
    let st = n.status();
    st.find("\"cluster\":{").and_then(|i| num_after(&st[i..], key)).unwrap_or(-1)
}

fn rpc_addr(n: &NodeProc) -> String {
    let st = n.status();
    let s = &st[st.find("\"cluster\":{").unwrap()..];
    let r = s.find("\"rpc\":\"").unwrap() + 7;
    s[r..r + s[r..].find('"').unwrap()].to_string()
}

fn node_args(join: Option<&str>) -> Vec<String> {
    let mut a: Vec<String> = ["--http", "127.0.0.1:0", "--gossip", "127.0.0.1:0", "--rpc", "127.0.0.1:0", "--probe-ms", "100", "--shards", "1"].iter().map(|s| s.to_string()).collect();
    if let Some(j) = join {
        a.extend(["--join".into(), j.into()]);
    }
    a
}

/// The pre-authentication attacks: a node with another key, forged plaintext gossip (a desired state pointing at
/// the attacker), and raw peer connections (plaintext `Hello` + `Call`, a wrong-key handshake, a silent socket).
#[test]
fn outsiders_cannot_join_forge_gossip_or_use_peer_links() {
    let dir = tmpdir("authcl");
    let n1 = spawn("n1", &dir.join("n1"), &node_args(None));
    let n2 = spawn("n2", &dir.join("n2"), &node_args(Some(&n1.gossip)));
    wait_for("2 members", Duration::from_secs(20), || live(&n1) == 2 && live(&n2) == 2);
    assert!(n1.status().contains(&format!("\"key_id\":\"{}\"", cluster_key().id_hex())));
    deploy(&n1.http, &[echo_svc().replicas(2)]);
    wait_for("echo everywhere", Duration::from_secs(20), || {
        [&n1, &n2].iter().all(|n| http::request(&n.http, "GET", "/echo", b"", T).is_ok_and(|r| r.text() == "hello"))
    });
    let v0 = n1.status().find("\"desired\":{").map(|i| num_after(&n1.status()[i..], "version"));

    // 1. A node with another key never becomes a member, and is told nothing.
    let other = dir.join("other.key");
    write_secret(&other, &"ab".repeat(32));
    let mut a = node_args(Some(&n1.gossip));
    a.extend(["--cluster-key-file".into(), other.display().to_string()]);
    let x = spawn("x", &dir.join("x"), &a);
    wait_for("n1 rejects x's gossip", Duration::from_secs(20), || cluster_num(&n1, "gossip_bad_tag") > 0);
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!((live(&n1), live(&n2), live(&x)), (2, 2, 1));

    // 2. Forged plaintext gossip announcing a desired state: dropped before decoding.
    let mut forger = k10_mesh::Gossip::new(
        k10_mesh::Member::new(k10_mesh::NodeId(0xe71), "127.0.0.1:9".parse().unwrap(), "127.0.0.1:9".parse().unwrap(), "127.0.0.1:9".parse().unwrap(), k10_mesh::Capacity { mem_bytes: 1 << 30, cores: 1 }),
        k10_mesh::Config::default(),
        1,
    );
    let _ = forger.set_desired(1_000_000, [0xee; 32]);
    let pkts = forger.join(&[n1.gossip.parse().unwrap()], 0).packets;
    let sock = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let before = cluster_num(&n1, "gossip_rejected");
    for (to, p) in &pkts {
        sock.send_to(p, to).unwrap();
    }
    wait_for("forged datagrams rejected", Duration::from_secs(10), || cluster_num(&n1, "gossip_rejected") >= before + pkts.len() as i64);
    let v1 = n1.status().find("\"desired\":{").map(|i| num_after(&n1.status()[i..], "version"));
    assert_eq!(v0, v1, "desired state unchanged");
    assert_eq!(live(&n1), 2);

    // 3. Peer links: plaintext Hello + Call, a wrong-key handshake, and a silent connection are all cut off.
    let rpc = rpc_addr(&n1);
    let served = cluster_num(&n1, "served_for_peers");
    let fails = cluster_num(&n1, "link_auth_failures");
    let mut s = TcpStream::connect(&rpc).unwrap();
    let mut frames = Vec::new();
    let me = k10_mesh::Member::new(k10_mesh::NodeId(0xe71), "127.0.0.1:9".parse().unwrap(), "127.0.0.1:9".parse().unwrap(), "127.0.0.1:9".parse().unwrap(), k10_mesh::Capacity { mem_bytes: 1 << 30, cores: 1 });
    k10_mesh::wire::Msg::Hello { member: me }.encode(0, &mut frames).unwrap();
    k10_mesh::wire::Msg::Call { service: "echo", path: b"", body: b"", deadline_ms: 1000, method: 0 }.encode(1, &mut frames).unwrap();
    s.write_all(&frames).unwrap();
    assert!(closed_soon(&mut s, Duration::from_secs(10)), "plaintext peer not disconnected");
    let mut s = TcpStream::connect(&rpc).unwrap();
    wrong_key_handshake(&mut s);
    assert!(closed_soon(&mut s, Duration::from_secs(10)), "wrong-key peer not disconnected");
    let t0 = Instant::now();
    let mut s = TcpStream::connect(&rpc).unwrap();
    assert!(closed_soon(&mut s, Duration::from_secs(15)), "silent peer not disconnected");
    assert!(t0.elapsed() >= Duration::from_secs(4), "handshake timeout is 5 s");
    wait_for("auth failures counted", Duration::from_secs(5), || cluster_num(&n1, "link_auth_failures") >= fails + 3);
    assert_eq!(cluster_num(&n1, "served_for_peers"), served, "nothing ran for an unauthenticated peer");
    // A key holder is a full peer: its calls run.
    let mut s = TcpStream::connect(&rpc).unwrap();
    let mut sec = peer_handshake(&mut s, 0xf00d);
    let mut out = Vec::new();
    sec.push(&k10_mesh::wire::Msg::Call { service: "echo", path: b"/x", body: b"", deadline_ms: 5000, method: 0 }, 7, &mut out).unwrap();
    sec.seal_pending(&mut out);
    s.write_all(&out).unwrap();
    wait_for("authenticated call served", Duration::from_secs(10), || cluster_num(&n1, "served_for_peers") > served);
    // The cluster is unharmed.
    assert_eq!((live(&n1), live(&n2)), (2, 2));
    assert_eq!(http::request(&n2.http, "GET", "/echo", b"", T).unwrap().text(), "hello");
    drop(x);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The node's own `Auth` read, then ours under another key, then a sealed frame the node cannot open.
fn wrong_key_handshake(s: &mut TcpStream) {
    use k10_mesh::secure::{ClusterKey, SecureLink, AUTH_FRAME_LEN};
    let mut sec = SecureLink::new(&ClusterKey::new(&[1; 32]), k10_mesh::NodeId(0xbadc0de), [9; 32]);
    let mut out = Vec::new();
    sec.start(&mut out);
    s.write_all(&out).unwrap();
    out.clear();
    let mut dec = k10_mesh::wire::FrameDecoder::new(AUTH_FRAME_LEN);
    let mut buf = [0u8; 256];
    let payload = loop {
        if let Some(f) = dec.next().unwrap() {
            break f.payload.to_vec();
        }
        let n = s.read(&mut buf).unwrap();
        assert!(n > 0);
        dec.feed(&buf[..n]);
    };
    sec.on_auth(&payload, None, &mut out).unwrap();
    sec.push(&k10_mesh::wire::Msg::Ping, 1, &mut out).unwrap();
    sec.seal_pending(&mut out);
    let _ = s.write_all(&out);
}

/// True once the peer closes the connection (EOF or reset) within `d`; whatever it sent first is skipped.
fn closed_soon(s: &mut TcpStream, d: Duration) -> bool {
    s.set_read_timeout(Some(d)).unwrap();
    let t = Instant::now();
    let mut buf = [0u8; 4096];
    while t.elapsed() < d {
        match s.read(&mut buf) {
            Ok(0) => return true,
            Ok(_) => continue,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => return false,
            Err(_) => return true,
        }
    }
    false
}
