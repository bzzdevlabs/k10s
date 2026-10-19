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
    let mut svcs = vec![echo_svc().replicas(1), Svc::new("relay", ten(RELAY)).mem(16).replicas(1)];
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
