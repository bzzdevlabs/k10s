//! Executable mappings of JIT code, measured from `/proc/self/maps` (own test binary with a single test, so no other
//! node in the process maps code concurrently): every apply maps the new modules, retired versions unmap theirs
//! once drained, and shutting the node down unmaps everything.
#![cfg(target_arch = "x86_64")]

mod common;
use common::*;
use k10_node::http::Client;
use k10_node::{demo, BackendKind, Node, NodeConfig};
use std::sync::atomic::{AtomicBool, Ordering::*};
use std::time::{Duration, Instant};

/// Total size of the process's anonymous executable mappings (JIT code: no file, inode 0).
fn anon_exec_bytes() -> u64 {
    let maps = std::fs::read_to_string("/proc/self/maps").unwrap();
    maps.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            let (range, perms, inode) = (f[0], f[1], f[4]);
            if !perms.contains('x') || inode != "0" || f.len() > 5 {
                return None;
            }
            let (a, b) = range.split_once('-')?;
            Some(u64::from_str_radix(b, 16).ok()? - u64::from_str_radix(a, 16).ok()?)
        })
        .sum()
}

fn exec_bytes(node: &Node) -> u64 {
    let s = node.status_json();
    stat_from(&s[s.find("\"code\":{").unwrap()..], "exec_bytes") as u64
}

#[test]
fn jit_code_mappings_follow_module_lifetimes() {
    let base = anon_exec_bytes();
    let node = Node::start(NodeConfig { backend: BackendKind::Jit, ..config(2) }).unwrap();
    assert_eq!(anon_exec_bytes(), base, "no code before the first apply");
    let v = |i: usize| {
        let m = match i % 3 {
            0 => demo::burn(),
            1 => demo::path_echo(),
            _ => demo::memcount(true),
        };
        bundle(vec![svc("svc", &m).config(&format!("{}", 1 + i % 2)), svc("echo", &demo::echo())])
    };
    node.apply(v(0)).unwrap();
    let e = exec_bytes(&node);
    assert!(e >= 2 * 4096);
    assert_eq!(anon_exec_bytes() - base, e, "status exec_bytes = mapped bytes");
    let addr = node.http_addr().to_string();
    let stop = AtomicBool::new(false);
    std::thread::scope(|s| {
        for _ in 0..4 {
            s.spawn(|| {
                let mut c = Client::connect(&addr, Duration::from_secs(10)).unwrap();
                while !stop.load(Relaxed) {
                    assert_eq!(c.request("GET", "/svc", b"").unwrap().status, 200);
                }
            });
        }
        for i in 1..=30 {
            std::thread::sleep(Duration::from_millis(10));
            node.apply(v(i)).unwrap();
        }
        stop.store(true, Relaxed);
    });
    let t = Instant::now();
    loop {
        let mapped = anon_exec_bytes() - base;
        let e = exec_bytes(&node);
        let s = node.status_json();
        if mapped == e && stat_from(&s[s.find("\"code\":{").unwrap()..], "modules") == 2 {
            break;
        }
        assert!(t.elapsed() < Duration::from_secs(5), "mapped {mapped} B, status says {e} B: {}", node.status_json());
        std::thread::sleep(Duration::from_millis(20));
    }
    node.shutdown();
    assert_eq!(anon_exec_bytes(), base, "everything unmapped after shutdown");
}
