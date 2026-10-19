//! Starts N gossip nodes on loopback (real UDP, one thread each), publishes a desired state, crashes one node
//! and prints what every node observes. `cargo run -p k10-mesh --example local_cluster -- 5`
use k10_mesh::driver::MeshThread;
use k10_mesh::{Addr, Capacity, Config, Event, Member, NodeId};
use std::time::{Duration, Instant};

fn main() -> std::io::Result<()> {
    let n: u64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(5);
    let any: Addr = "127.0.0.1:0".parse().unwrap();
    let mk = |i: u64| Member::new(NodeId(i), any, any, any, Capacity { mem_bytes: 1 << 30, cores: 2 });
    let cfg = Config { probe_interval_ms: 100, ..Config::default() };
    let first = MeshThread::spawn(mk(1), cfg.clone(), 1, vec![])?;
    let seed = first.local().gossip;
    let mut nodes = vec![first];
    for i in 2..=n {
        nodes.push(MeshThread::spawn(mk(i), cfg.clone(), i, vec![seed])?);
    }
    let t0 = Instant::now();
    while nodes.iter().any(|m| m.live_members().len() != n as usize) {
        std::thread::sleep(Duration::from_millis(5));
    }
    println!("{n} nodes converged in {:?}", t0.elapsed());
    nodes[0].set_desired(1, k10_mesh::sha256::sha256(b"manifest v1"));
    std::thread::sleep(Duration::from_millis(500));
    let victim = nodes.pop().unwrap();
    let t1 = Instant::now();
    victim.kill()?;
    while nodes.iter().any(|m| m.live_members().len() != n as usize - 1) {
        std::thread::sleep(Duration::from_millis(5));
    }
    println!("crash of node {n} seen by all survivors after {:?}", t1.elapsed());
    for m in &nodes {
        let evs: Vec<String> = m
            .events()
            .try_iter()
            .map(|e| match e {
                Event::MemberUp(m) => format!("up({})", m.id.0),
                Event::MemberDown(m) => format!("down({}, {:?})", m.id.0, m.state),
                Event::MemberUpdated(m) => format!("updated({}, {:?})", m.id.0, m.state),
                Event::DesiredState(d) => format!("desired(v{} from {})", d.version, d.origin.0),
            })
            .collect();
        println!("node {}: {}", m.local().id.0, evs.join(" "));
    }
    Ok(())
}
