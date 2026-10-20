//! Cluster-mode abuse: a tenant service that forwards a hostile `call` across the mesh must not be able to crash
//! or wedge a peer's mesh thread. Regression for the mesh-thread panic where a huge non-UTF-8 call path (raw length
//! under the u16 wire limit, but 3× larger once UTF-8-lossy-expanded) overflowed `Msg::encode`'s length field and
//! panicked the mesh thread — stopping gossip and timing out every remote forward.

mod support;
use support::*;

use k10_node::http;
use std::time::Duration;

/// Starts an `n`-node cluster (first node is the seed) on ephemeral localhost ports, fast gossip.
fn cluster(tag: &str, n: usize) -> (std::path::PathBuf, Vec<NodeProc>) {
    let dir = tmpdir(tag);
    let base = ["--http", "127.0.0.1:0", "--gossip", "127.0.0.1:0", "--rpc", "127.0.0.1:0", "--probe-ms", "100", "--leave-grace-ms", "300"];
    let mut args: Vec<String> = base.iter().map(|s| s.to_string()).collect();
    let mut nodes = vec![spawn("n1", &dir.join("n1"), &args)];
    let seed = nodes[0].gossip.clone();
    for i in 2..=n {
        let mut a = args.clone();
        a.extend(["--join".into(), seed.clone()]);
        nodes.push(spawn(&format!("n{i}"), &dir.join(format!("n{i}")), &a));
        args = base.iter().map(|s| s.to_string()).collect();
    }
    for node in &nodes {
        wait_for(&format!("{} sees {n} live", node.name), Duration::from_secs(30), || {
            node.status().find("\"cluster\":{").is_some_and(|i| num_after(&node.status()[i..], "live_members") == Some(n as i64))
        });
    }
    (dir, nodes)
}

/// `relay` fans a call out to `sink0..sink9` with a 30 000-byte non-UTF-8 path each (under the 65535 raw wire
/// limit, but > it if expanded lossily). With 10 single-replica sinks across 3 nodes, at least one is remote, so a
/// peer mesh thread must encode the hostile path.
const RELAY: &str = r#"
buffer tgt: [u8; 40000];
export fn handle() {
    let s = 0;
    while s < 10 {
        tgt[0] = 's'; tgt[1] = 'i'; tgt[2] = 'n'; tgt[3] = 'k';
        tgt[4] = '0' + s; tgt[5] = '/';
        let n = 30000;
        let i = 0;
        while i < n {
            tgt[6 + i] = 255;
            i += 1;
        }
        call(tgt as *u8, 6 + n, null, 0);
        s += 1;
    }
    out_str("done");
}
"#;

fn wait_applied(nodes: &[NodeProc], v: i64) {
    for n in nodes {
        wait_for(&format!("{} applies v{v}", n.name), Duration::from_secs(30), || {
            n.status().find("\"applied\":{").is_some_and(|i| num_after(&n.status()[i..], "version") >= Some(v))
        });
    }
}

/// echo on every ingress (forwarded when the single replica is elsewhere — this needs a live mesh).
fn echo_everywhere(nodes: &[NodeProc]) {
    for n in nodes {
        let r = http::request(&n.http, "GET", "/echo", b"", Duration::from_secs(10))
            .unwrap_or_else(|e| panic!("echo via {}: transport error {e} (mesh likely dead)", n.name));
        assert_eq!((r.status, r.text().as_str()), (200, "hello"), "echo via {}", n.name);
    }
}

#[test]
fn hostile_nonutf8_call_path_does_not_crash_the_mesh() {
    let (dir, nodes) = cluster("meshabuse", 3);
    // echo has ONE replica, so reaching it from the other two ingresses forwards over the mesh: a dead mesh thread
    // would make this time out (the regression's symptom), not silently pass.
    let sinks: Vec<String> = (0..10).map(|i| format!("sink{i}")).collect();
    let sinks: Vec<&str> = sinks.iter().map(String::as_str).collect();
    let mut svcs = vec![echo_svc().replicas(1), Svc::new("relay", ten(RELAY)).mem(16).replicas(1).calls(&sinks)];
    for i in 0..10 {
        svcs.push(Svc::new(&format!("sink{i}"), ten(ECHO_TEN)).mem(16).replicas(1));
    }
    deploy(&nodes[0].http, &svcs);
    wait_applied(&nodes, 1);
    echo_everywhere(&nodes);

    // Hammer the hostile relay through every ingress: whichever node hosts relay forwards the fan-out calls to the
    // sinks placed on other nodes — the exact path that used to panic the peer's mesh thread.
    for round in 0..20 {
        for n in &nodes {
            let r = http::request(&n.http, "GET", "/relay", b"", Duration::from_secs(10));
            match r {
                Ok(resp) => assert!(resp.status == 200 || resp.status == 503, "relay via {} round {round}: {}", n.name, resp.status),
                Err(e) => panic!("relay via {} round {round}: transport error {e} (mesh thread likely died)", n.name),
            }
        }
    }
    // Forwarding really happened (the mesh path was exercised, not a trivial pass).
    let served: i64 = nodes.iter().filter_map(|n| num_after(n.status().split("\"cluster\":{").nth(1)?, "served_for_peers")).sum();
    assert!(served > 0, "no cross-node forwarding happened; test did not exercise the mesh");

    // Mesh + gossip still fully alive: forwarded echo works everywhere, and a fresh apply still converges on all nodes.
    for n in &nodes {
        assert!(n.healthy(), "{} unhealthy after the hostile fan-out", n.name);
    }
    echo_everywhere(&nodes);
    deploy(&nodes[1].http, &svcs); // v2 (same content, new version) — reaches every node only if gossip/mesh live
    wait_applied(&nodes, 2);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The node's peer-protocol (rpc) address, from its status.
fn rpc_addr(n: &NodeProc) -> String {
    let st = n.status();
    let at = st.find("\"cluster\":{").expect("cluster mode");
    let s = &st[at..];
    let r = s.find("\"rpc\":\"").unwrap() + 7;
    s[r..r + s[r..].find('"').unwrap()].to_string()
}

/// Peak resident set size (`VmHWM`) of a process, in KiB.
fn peak_rss_kb(pid: i32) -> u64 {
    let st = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
    let l = st.lines().find(|l| l.starts_with("VmHWM:")).unwrap();
    l.split_whitespace().nth(1).unwrap().parse().unwrap()
}

/// A "peer" that sends requests over the peer protocol and never reads the answers. Each `GetManifest` is answered
/// with the whole manifest (≈ 1 MB here, thanks to a 1 MB service config); the peer asks for 5 every 20 ms, so the
/// node writes replies until the socket buffers are full, then its write buffer for the link grows. It must stop at
/// its per-link cap (`LINK_HARD_CAP`, 80 MiB) and close the link instead of growing without bound (the peer asks for
/// up to 1 GB); meanwhile forwarding between the real members keeps working.
#[test]
fn peer_that_never_reads_is_cut_off() {
    use std::io::{Read, Write};
    let (dir, nodes) = cluster("meshnoread", 2);
    let big = echo_svc().replicas(1).config(&vec![b'x'; 1_000_000]);
    deploy(&nodes[0].http, &[big]);
    wait_applied(&nodes, 1);
    echo_everywhere(&nodes);
    let target = &nodes[1];
    let mut s = std::net::TcpStream::connect(rpc_addr(target)).unwrap();
    // A key holder (authenticated peers are trusted with the protocol, not with unbounded buffers).
    let mut sec = peer_handshake(&mut s, 0xbad);
    let mut w = s.try_clone().unwrap();
    let writer = std::thread::spawn(move || {
        let mut sent = 0u64;
        while sent < 1000 {
            let mut reqs = Vec::new();
            for _ in 0..5 {
                sent += 1;
                sec.push(&k10_mesh::wire::Msg::GetManifest { version: 1 }, sent, &mut reqs).unwrap();
            }
            sec.seal_pending(&mut reqs);
            if w.write_all(&reqs).is_err() {
                break; // the node closed the link
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        sent
    });
    let probe = Probe::start(&nodes[0].http, "/echo", "hello", Duration::from_millis(5));
    wait_for("link closed for overflow", Duration::from_secs(60), || {
        num_after(&target.status(), "links_overflowed").unwrap_or(0) >= 1
    });
    let sent = writer.join().unwrap();
    let lat = probe.stop();
    // The attacker's connection is closed: after what the socket buffers held, EOF (or a reset).
    let mut s = s;
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let mut buf = vec![0u8; 1 << 20];
    let mut got = 0usize;
    loop {
        match s.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => got += n,
        }
        assert!(got < 200 << 20, "the node kept sending past its cap ({got} bytes)");
    }
    let peak_mb = peak_rss_kb(target.pid()) / 1024;
    assert!(peak_mb < 400, "node peak RSS {peak_mb} MB: replies to a non-reading peer were not capped");
    assert!(sent < 1000, "the link was never closed under the writer ({sent} requests sent)");
    // The cluster is unharmed: forwarding works through both ingresses, and a new apply converges.
    assert_eq!(lat.errors, 0, "echo through n1 during the attack on n2: {lat:?}");
    assert!(target.healthy());
    echo_everywhere(&nodes);
    deploy(&nodes[0].http, &[echo_svc().replicas(1)]);
    wait_applied(&nodes, 2);
    echo_everywhere(&nodes);
    eprintln!("non-reading peer: {sent} requests sent, {got} reply bytes reached it before the close; node peak RSS {peak_mb} MB; echo {lat}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// CPU time (user + system, clock ticks) of a process.
fn cpu_ticks(pid: i32) -> u64 {
    let st = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let rest = &st[st.rfind(')').unwrap() + 2..];
    let f: Vec<&str> = rest.split_whitespace().collect();
    f[11].parse::<u64>().unwrap() + f[12].parse::<u64>().unwrap()
}

/// Out of file descriptors (`RLIMIT_NOFILE`): connections wait in the listeners' backlogs and `accept` fails with
/// `EMFILE`. Neither the HTTP listener (shards) nor the peer listener (mesh thread) may busy-loop on their
/// level-triggered readiness: they are paused with a back-off, the log line is rate-limited, and once descriptors
/// are free again the waiting connections are served.
#[test]
fn fd_exhaustion_does_not_spin() {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    let dir = tmpdir("emfile");
    let log = dir.join("node.log");
    let http_port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let http = format!("127.0.0.1:{http_port}");
    let mut cmd = Command::new(bin());
    cmd.args(["node", "--data", dir.join("data").to_str().unwrap(), "--http", &http, "--shards", "1"])
        .args(["--gossip", "127.0.0.1:0", "--rpc", "127.0.0.1:0", "--cluster-key-file", key_file().to_str().unwrap()])
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&log).unwrap());
    // SAFETY: setrlimit is async-signal-safe; nothing else runs between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            let lim = libc::rlimit { rlim_cur: 64, rlim_max: 64 };
            if libc::setrlimit(libc::RLIMIT_NOFILE, &lim) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().unwrap();
    struct Kill<'a>(&'a mut std::process::Child);
    impl Drop for Kill<'_> {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let pid = child.id() as i32;
    let _guard = Kill(&mut child);
    wait_for("node up", Duration::from_secs(20), || {
        http::request(&http, "GET", "/_k10s/health", b"", Duration::from_secs(2)).is_ok_and(|r| r.status == 200)
    });
    let st = http::request(&http, "GET", "/_k10s/status", b"", Duration::from_secs(5)).unwrap().text();
    let rpc = {
        let s = &st[st.find("\"cluster\":{").unwrap()..];
        let r = s.find("\"rpc\":\"").unwrap() + 7;
        s[r..r + s[r..].find('"').unwrap()].to_string()
    };
    // Exhaust the node's descriptors with idle HTTP connections, then leave more waiting on both listeners.
    let fds = || std::fs::read_dir(format!("/proc/{pid}/fd")).map(|d| d.count()).unwrap_or(0);
    let mut conns = Vec::new();
    wait_for("node out of file descriptors", Duration::from_secs(20), || {
        conns.push(std::net::TcpStream::connect(&http).unwrap());
        std::thread::sleep(Duration::from_millis(2));
        fds() >= 64 && conns.len() > 8
    });
    for _ in 0..8 {
        conns.push(std::net::TcpStream::connect(&http).unwrap());
        conns.push(std::net::TcpStream::connect(&rpc).unwrap());
    }
    std::thread::sleep(Duration::from_millis(300));
    let t0 = cpu_ticks(pid);
    std::thread::sleep(Duration::from_secs(2));
    let used = cpu_ticks(pid) - t0;
    // A spinning listener burns a whole core (≈ 200 ticks in 2 s).
    assert!(used < 40, "node used {used} CPU ticks in 2 s while out of file descriptors (spinning?)");
    let text = std::fs::read_to_string(&log).unwrap();
    let lines = text.lines().filter(|l| l.contains("accept:")).count();
    assert!((1..=4).contains(&lines), "{lines} accept log lines (want a rate-limited few):\n{text}");
    // Free the descriptors: the node accepts again and serves.
    drop(conns);
    wait_for("node serves again", Duration::from_secs(20), || {
        http::request(&http, "GET", "/_k10s/health", b"", Duration::from_secs(2)).is_ok_and(|r| r.status == 200)
    });
    let st = http::request(&http, "GET", "/_k10s/status", b"", Duration::from_secs(5)).unwrap().text();
    let http_pauses = section_num(&st, "http_stats", "accept_pauses").unwrap();
    let mesh_pauses = num_after(&st[st.find("\"cluster\":{").unwrap()..], "accept_pauses").unwrap();
    assert!(http_pauses >= 1 && mesh_pauses >= 1, "pauses: http {http_pauses}, mesh {mesh_pauses}");
    eprintln!("out of fds: {used} CPU ticks in 2 s, {lines} log line(s), pauses: http {http_pauses}, mesh {mesh_pauses}");
    drop(_guard);
    let _ = std::fs::remove_dir_all(&dir);
}
