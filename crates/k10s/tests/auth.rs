//! Authentication: signed admin requests (generated key, separate listener, CLI key lookup); the cluster key
//! (sealed gossip, authenticated peer links, bounded pre-authentication input, rotation) and the refusal to run a
//! cluster without one.

mod support;
use support::*;

use k10_mesh::admin_auth::AdminKey;
use k10_mesh::secure::{ClusterKey, ClusterKeys, Link};
use k10_mesh::wire::{Msg, DEFAULT_MAX_FRAME};
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

fn get(addr: &str, path: &str, key: Option<&AdminKey>) -> (u16, String) {
    let r = http::request_signed(addr, "GET", path, b"", T, key).unwrap();
    (r.status, r.text())
}

/// Sends raw request bytes and returns everything the node answers until it closes or goes quiet.
fn raw(addr: &str, req: &str) -> String {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    s.write_all(req.as_bytes()).unwrap();
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

/// The CLI with no admin key in its environment (and an empty state directory) unless `envs` give one.
fn k10s(args: &[&str], envs: &[(&str, &str)]) -> std::process::Output {
    let mut c = Command::new(bin());
    let home = std::env::temp_dir().join(format!("k10s-test-nohome-{}", std::process::id()));
    c.args(args).env_remove("K10S_ADMIN_TOKEN_FILE").env("K10S_HOME", home);
    for (k, v) in envs {
        c.env(k, v);
    }
    c.output().unwrap()
}

fn examples_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples")
}

#[test]
fn admin_requests_must_be_signed() {
    let dir = tmpdir("authsig");
    let args: Vec<String> = vec!["--http".into(), "127.0.0.1:0".into(), "--shards".into(), "1".into()];
    let n = spawn("sig", &dir.join("data"), &args);
    let key = admin_key();
    let other = AdminKey::parse(&"x".repeat(40)).unwrap();
    for p in ["/_k10s/status", "/_k10s/logs/x", "/_k10s/cluster", "/_k10s/nope", "/_k10s/metrics"] {
        assert_eq!(get(&n.http, p, None).0, 401, "{p} unsigned");
        assert_eq!(get(&n.http, p, Some(&other)).0, 401, "{p} signed with another key");
    }
    assert_eq!(get(&n.http, "/_k10s/status", Some(&key)).0, 200);
    assert_eq!(get(&n.http, "/_k10s/health", None).0, 200);
    // A signature binds the node process (its open audience), the target and the time, and works once.
    let aud = http::request(&n.http, "GET", "/_k10s/audience", b"", T).unwrap().text().trim().to_string();
    assert_eq!(aud.len(), 32);
    let now = k10_node::sys::wall_ms();
    let req = |target: &str, t: u64, n: u8| {
        let a = key.authorization(&aud, "GET", "/_k10s/status", t, &[n; 16], b"");
        format!("GET {target} HTTP/1.1\r\nauthorization: {a}\r\nconnection: close\r\n\r\n")
    };
    assert!(raw(&n.http, &req("/_k10s/status", now, 1)).starts_with("HTTP/1.1 200"));
    let r = raw(&n.http, &req("/_k10s/status", now, 1));
    assert!(r.starts_with("HTTP/1.1 401") && r.contains("replayed"), "{r}");
    assert!(raw(&n.http, &req("/_k10s/logs/x", now, 2)).starts_with("HTTP/1.1 401"));
    let r = raw(&n.http, &req("/_k10s/status", now - 120_000, 3));
    assert!(r.starts_with("HTTP/1.1 401") && r.contains("clock"), "{r}");
    // Refused before the (claimed 200 MiB) body is read, with the challenge.
    let r = raw(&n.http, "POST /_k10s/apply HTTP/1.1\r\nhost: x\r\ncontent-length: 209715200\r\n\r\n");
    assert!(r.starts_with("HTTP/1.1 401") && r.contains("www-authenticate: K10s-HMAC") && r.contains("connection: close"), "{r}");
    // CLI: refused without the key (with a hint); found through --admin-token-file, the environment, the state dir.
    let m = dir.join("app.k10s");
    std::fs::write(&m, format!("service echo {{ code = \"{}\" memory = 64KiB }}\n", examples_dir().join("echo.ten").display())).unwrap();
    let kf = admin_key_file().to_str().unwrap();
    let no = k10s(&["apply", m.to_str().unwrap(), "--node", &n.http], &[]);
    assert!(!no.status.success() && String::from_utf8_lossy(&no.stderr).contains("admin key"), "{no:?}");
    let ok = k10s(&["apply", m.to_str().unwrap(), "--node", &n.http, "--admin-token-file", kf], &[]);
    assert!(ok.status.success(), "{ok:?}");
    let ok = k10s(&["status", "--json", "--node", &n.http], &[("K10S_ADMIN_TOKEN_FILE", kf)]);
    assert!(ok.status.success() && String::from_utf8_lossy(&ok.stdout).contains("\"name\":\"echo\""), "{ok:?}");
    let home = dir.join("home");
    std::fs::create_dir(&home).unwrap();
    std::fs::copy(kf, home.join("admin.key")).unwrap();
    let ok = k10s(&["logs", "echo", "--node", &n.http], &[("K10S_HOME", home.to_str().unwrap())]);
    assert!(ok.status.success(), "{ok:?}");
    let bad = dir.join("bad.key");
    write_secret(&bad, "short");
    let out = k10s(&["status", "--node", &n.http, "--admin-token-file", bad.to_str().unwrap()], &[]);
    assert!(!out.status.success() && String::from_utf8_lossy(&out.stderr).contains("32 to 512"), "{out:?}");
    // Services are unaffected.
    assert_eq!(http::request(&n.http, "GET", "/echo/x", b"", T).unwrap().status, 200);
    // A captured request is refused by another node holding the same key, and by the same node once restarted.
    let n2 = spawn("sig2", &dir.join("data2"), &args);
    let r = raw(&n2.http, &req("/_k10s/status", k10_node::sys::wall_ms(), 4));
    assert!(r.starts_with("HTTP/1.1 401") && r.contains("another node"), "{r}");
    let captured = req("/_k10s/status", k10_node::sys::wall_ms(), 5);
    drop(n);
    let n = spawn("sig", &dir.join("data"), &args);
    let r = raw(&n.http, &captured);
    assert!(r.starts_with("HTTP/1.1 401") && r.contains("earlier start"), "{r}");
    assert_eq!(get(&n.http, "/_k10s/status", Some(&key)).0, 200);
    drop(n2);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_node_generates_its_admin_key() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tmpdir("authgen");
    let data = dir.join("data");
    let args: Vec<String> = ["--http", "127.0.0.1:0", "--shards", "1"].iter().map(|s| s.to_string()).collect();
    let mut n = spawn_raw("gen", &data, &args);
    let kf = data.join("admin.key");
    assert!(n.log().contains("generated the admin key"), "{}", n.log());
    assert_eq!(std::fs::metadata(&kf).unwrap().permissions().mode() & 0o777, 0o600);
    let key = AdminKey::parse(&std::fs::read_to_string(&kf).unwrap()).unwrap();
    assert_eq!(get(&n.http, "/_k10s/status", None).0, 401);
    assert_eq!(get(&n.http, "/_k10s/status", Some(&key)).0, 200);
    let first = std::fs::read(&kf).unwrap();
    n.terminate(T);
    let n = spawn_raw("gen", &data, &args);
    assert_eq!(std::fs::read(&kf).unwrap(), first, "kept across restarts");
    assert_eq!(get(&n.http, "/_k10s/status", Some(&key)).0, 200);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn separate_admin_listener() {
    let dir = tmpdir("authsep");
    let n = spawn("sep", &dir.join("data"), &["--http".into(), "127.0.0.1:0".into(), "--shards".into(), "2".into(), "--admin".into(), "127.0.0.1:0".into()]);
    let log = n.log();
    let at = log.find("admin API on http://").expect("admin listener line") + "admin API on http://".len();
    let admin = log[at..].split(':').take(2).collect::<Vec<_>>().join(":");
    let admin = admin.trim_end_matches(|c: char| !c.is_ascii_digit()).to_string();
    let key = admin_key();
    // Ingress: only health; admin listener: the API (signed; metrics open there), no services.
    assert_eq!(get(&n.http, "/_k10s/health", None).0, 200);
    for p in ["/_k10s/status", "/_k10s/metrics", "/_k10s/audience"] {
        assert_eq!(get(&n.http, p, None).0, 404, "{p} on the ingress");
    }
    let e = http::request_signed(&n.http, "GET", "/_k10s/status", b"", T, Some(&key)).unwrap_err();
    assert!(e.to_string().contains("audience: HTTP 404"), "{e}");
    assert_eq!(get(&admin, "/_k10s/status", None).0, 401);
    assert_eq!(get(&admin, "/_k10s/metrics", None).0, 200);
    let body = bundle(0, &[echo_svc()]).encode();
    let r = http::request_signed(&admin, "POST", "/_k10s/apply", &body, T, Some(&key)).unwrap();
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(get(&admin, "/echo", None).0, 404);
    assert_eq!(http::request(&n.http, "GET", "/echo", b"", T).unwrap().text(), "hello");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn cluster_mode_needs_a_key_or_an_explicit_insecure_flag() {
    let dir = tmpdir("authnokey");
    let d = dir.join("d");
    let d = d.to_str().unwrap();
    let out = k10s(&["node", "--http", "127.0.0.1:0", "--gossip", "127.0.0.1:0", "--rpc", "127.0.0.1:0", "--data", d], &[]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success() && err.contains("--cluster-key-file") && err.contains("keygen"), "{err}");
    let bad = dir.join("bad.key");
    write_secret(&bad, "not-hex");
    let out = k10s(&["node", "--gossip", "127.0.0.1:0", "--cluster-key-file", bad.to_str().unwrap(), "--data", d], &[]);
    assert!(!out.status.success() && String::from_utf8_lossy(&out.stderr).contains("64 hex digits"), "{out:?}");
    // Booleans take no value: `=false` must not turn the insecure mode on (nor anything else).
    for flag in ["--insecure-unauthenticated-cluster=false", "--insecure-unauthenticated-cluster=0"] {
        let out = k10s(&["node", "--gossip", "127.0.0.1:0", flag, "--data", d], &[]);
        assert!(!out.status.success() && String::from_utf8_lossy(&out.stderr).contains("takes no value"), "{out:?}");
    }
    let kf = key_file().to_str().unwrap();
    let out = k10s(&["node", "--gossip", "127.0.0.1:0", "--cluster-key-file", kf, "--gossip-max-skew", "0", "--data", d], &[]);
    assert!(!out.status.success() && String::from_utf8_lossy(&out.stderr).contains("at least 1"), "{out:?}");
    let out = k10s(&["node", "--gossip", "127.0.0.1:0", "--cluster-key-file", kf, "--cluster-next-key-file", kf, "--data", d], &[]);
    assert!(!out.status.success(), "the next key differs from the current one: {out:?}");
    // keygen: a new 0600 file, never overwritten.
    let k = dir.join("new.key");
    let out = k10s(&["keygen", "-o", k.to_str().unwrap()], &[]);
    assert!(out.status.success(), "{out:?}");
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(&k).unwrap().permissions().mode() & 0o777, 0o600);
    assert!(ClusterKey::parse_hex(&std::fs::read_to_string(&k).unwrap()).is_ok());
    assert!(!k10s(&["keygen", "-o", k.to_str().unwrap()], &[]).status.success());
    // Explicitly insecure nodes still form a cluster (and say so loudly).
    let ins = |join: Option<&str>| {
        let mut a = node_args(join);
        a.push("--insecure-unauthenticated-cluster".into());
        a
    };
    let a = spawn("ins1", &dir.join("i1"), &ins(None));
    let b = spawn("ins2", &dir.join("i2"), &ins(Some(&a.gossip)));
    assert!(a.log().contains("WARNING") && a.log().contains("NOT authenticated"));
    wait_for("insecure pair converges", Duration::from_secs(20), || live(&a) == 2 && live(&b) == 2);
    deploy(&a.http, &[echo_svc().replicas(1)]);
    wait_for("insecure pair serves echo", Duration::from_secs(20), || serves_echo(&[&a, &b]));
    let _ = std::fs::remove_dir_all(&dir);
}

fn live(n: &NodeProc) -> i64 {
    cluster_num(n, "live_members").max(0)
}

fn cluster_num(n: &NodeProc, key: &str) -> i64 {
    let st = n.status();
    st.find("\"cluster\":{").and_then(|i| num_after(&st[i..], key)).unwrap_or(-1)
}

fn serves_echo(nodes: &[&NodeProc]) -> bool {
    nodes.iter().all(|n| http::request(&n.http, "GET", "/echo", b"", T).is_ok_and(|r| r.text() == "hello"))
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
/// the attacker), and raw peer connections (plaintext `Hello` + `Call`, a wrong key, oversized or forged records,
/// silent sockets, a flood of half-open handshakes).
#[test]
fn outsiders_cannot_join_forge_gossip_or_use_peer_links() {
    let dir = tmpdir("authcl");
    let n1 = spawn("n1", &dir.join("n1"), &node_args(None));
    let n2 = spawn("n2", &dir.join("n2"), &node_args(Some(&n1.gossip)));
    wait_for("2 members", Duration::from_secs(20), || live(&n1) == 2 && live(&n2) == 2);
    assert!(n1.status().contains(&format!("\"key_id\":\"{}\"", cluster_key().id_hex())));
    assert!(n1.log().contains(&format!("cluster key id {}", cluster_key().id_hex())) && !n1.log().contains(std::fs::read_to_string(key_file()).unwrap().trim()));
    deploy(&n1.http, &[echo_svc().replicas(2)]);
    wait_for("echo everywhere", Duration::from_secs(20), || serves_echo(&[&n1, &n2]));
    let v0 = n1.status().find("\"desired\":{").map(|i| num_after(&n1.status()[i..], "version"));

    // 1. A node with another key never becomes a member, and is told nothing.
    let other = dir.join("other.key");
    write_secret(&other, &"ab".repeat(32));
    let mut a = node_args(Some(&n1.gossip));
    a.extend(["--cluster-key-file".into(), other.display().to_string()]);
    let x = spawn("x", &dir.join("x"), &a);
    wait_for("n1 rejects x's gossip", Duration::from_secs(20), || cluster_num(&n1, "gossip_unknown_key") > 0);
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!((live(&n1), live(&n2), live(&x)), (2, 2, 1));

    // 2. Forged plaintext gossip announcing a desired state: dropped before decoding.
    let mut forger = k10_mesh::Gossip::new(test_member(0xe71), k10_mesh::Config::default(), 1);
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

    // 3. Peer links without the key are cut off and run nothing.
    let rpc = rpc_addr(&n1);
    let served = cluster_num(&n1, "served_for_peers");
    let fails = cluster_num(&n1, "link_auth_failures");
    let mut s = TcpStream::connect(&rpc).unwrap();
    let mut frames = Vec::new();
    Msg::Hello { member: test_member(0xe71) }.encode(0, &mut frames).unwrap();
    Msg::Call { service: "echo", path: b"", body: b"", deadline_ms: 1000, method: 0 }.encode(1, &mut frames).unwrap();
    s.write_all(&frames).unwrap();
    assert!(closed_soon(&mut s, T), "plaintext peer not disconnected");
    // A wrong key: no key in common.
    let mut s = TcpStream::connect(&rpc).unwrap();
    send_auth(&mut s, &ClusterKeys::new(ClusterKey::new(&[1; 32]), None).unwrap());
    assert!(closed_soon(&mut s, T), "wrong-key peer not disconnected");
    // The key's id (public) but not the key: a 64 MiB record header is refused at once, a full record of junk
    // fails its tag (nothing larger than one record is ever buffered before the peer proves the key).
    let keys = ClusterKeys::new(cluster_key(), None).unwrap();
    let mut s = TcpStream::connect(&rpc).unwrap();
    send_auth(&mut s, &keys);
    s.write_all(&(64u32 << 20).to_le_bytes()).unwrap();
    assert!(closed_soon(&mut s, T), "oversized record not refused");
    let mut s = TcpStream::connect(&rpc).unwrap();
    send_auth(&mut s, &keys);
    let rec = (64u32 << 10) + 16;
    let _ = s.write_all(&rec.to_le_bytes()).and_then(|_| s.write_all(&vec![0x5a; rec as usize]));
    assert!(closed_soon(&mut s, T), "forged record not refused");
    // A silent connection times out.
    let t0 = Instant::now();
    let mut s = TcpStream::connect(&rpc).unwrap();
    assert!(closed_soon(&mut s, Duration::from_secs(15)), "silent peer not disconnected");
    assert!(t0.elapsed() >= Duration::from_secs(4), "handshake timeout is 5 s");
    wait_for("auth failures counted", Duration::from_secs(5), || cluster_num(&n1, "link_auth_failures") >= fails + 5);
    // Half-open handshakes are capped: beyond 128, the oldest goes first (long before its timeout).
    let mut first = TcpStream::connect(&rpc).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    let flood: Vec<TcpStream> = (0..128).map(|_| TcpStream::connect(&rpc).unwrap()).collect();
    let t0 = Instant::now();
    assert!(closed_soon(&mut first, Duration::from_secs(4)) && t0.elapsed() < Duration::from_secs(3), "oldest half-open link not evicted");
    drop(flood);
    assert_eq!(cluster_num(&n1, "served_for_peers"), served, "nothing ran for an unauthenticated peer");
    // A key holder is a full peer: its calls run.
    let mut s = TcpStream::connect(&rpc).unwrap();
    let (mut sec, mut out) = peer_handshake(&mut s, 0xf00d);
    sec.push(&Msg::Call { service: "echo", path: b"/x", body: b"", deadline_ms: 5000, method: 0 }, 7, &mut out).unwrap();
    peer_send(&mut s, &mut sec, &mut out);
    wait_for("authenticated call served", Duration::from_secs(10), || cluster_num(&n1, "served_for_peers") > served);
    // The cluster is unharmed.
    assert_eq!((live(&n1), live(&n2)), (2, 2));
    assert_eq!(http::request(&n2.http, "GET", "/echo", b"", T).unwrap().text(), "hello");
    drop(x);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Sends only our `Auth` (what a link lets out before the node's).
fn send_auth(s: &mut TcpStream, keys: &ClusterKeys) {
    let mut out = Vec::new();
    let link = Link::new(Some(keys), &test_member(0xbadc0de), None, [9; 32], DEFAULT_MAX_FRAME, &mut out);
    s.write_all(&out[..out.len() - link.held()]).unwrap();
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

/// The documented rotation, one node at a time on a serving 3-node cluster: add the next key everywhere, switch
/// the current key, drop the old one. Every step keeps the cluster whole and serving.
#[test]
fn rolling_key_rotation_on_three_nodes() {
    let dir = tmpdir("authrot");
    let old = key_file().display().to_string();
    let new = new_key_file("rotation-key").display().to_string();
    let keyed = |join: Option<&str>, cur: &str, next: Option<&str>| {
        let mut a = node_args(join);
        a.extend(["--cluster-key-file".into(), cur.to_string()]);
        if let Some(n) = next {
            a.extend(["--cluster-next-key-file".into(), n.to_string()]);
        }
        a
    };
    let mut nodes = vec![spawn("r0", &dir.join("r0"), &keyed(None, &old, None))];
    for i in 1..3 {
        let join = nodes[0].gossip.clone();
        nodes.push(spawn(&format!("r{i}"), &dir.join(format!("r{i}")), &keyed(Some(&join), &old, None)));
    }
    let whole = |nodes: &[NodeProc]| nodes.iter().all(|n| live(n) == 3);
    wait_for("3 members", Duration::from_secs(20), || whole(&nodes));
    deploy(&nodes[0].http, &[echo_svc().replicas(1)]);
    for (step, (cur, next)) in [(&old, Some(&new)), (&new, Some(&old)), (&new, None)].into_iter().enumerate() {
        for i in 0..3 {
            nodes[i].terminate(T);
            let join = nodes[(i + 1) % 3].gossip.clone();
            nodes[i] = spawn(&format!("r{i}"), &dir.join(format!("r{i}")), &keyed(Some(&join), cur, next.map(|s| s.as_str())));
            wait_for(&format!("step {step}, node {i}: 3 members"), Duration::from_secs(30), || whole(&nodes));
            let refs: Vec<&NodeProc> = nodes.iter().collect();
            wait_for(&format!("step {step}, node {i}: echo everywhere"), Duration::from_secs(30), || serves_echo(&refs));
        }
    }
    let id = cluster_key_of(Path::new(&new)).id_hex();
    for n in &nodes {
        assert!(n.status().contains(&format!("\"key_id\":\"{id}\",\"next_key_id\":null")), "{}", n.status());
        assert_eq!(cluster_num(n, "link_auth_failures"), 0, "{}", n.log());
    }
    let _ = std::fs::remove_dir_all(&dir);
}
