//! meshpeer — a peer that authenticates to a node's peer port with the cluster key, then asks for the desired-state
//! manifest over and over and never reads the replies (a key holder that stopped reading). Writes until the node
//! closes the link or COUNT requests are sent, then prints `sent=N received=BYTES` (reply bytes that reached it).
//!
//! ```text
//! meshpeer HOST:PORT CLUSTER_KEY_FILE COUNT
//! ```

use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::exit;
use std::time::Duration;

use k10_mesh::secure::{ClusterKey, ClusterKeys, Link, LinkEvent};
use k10_mesh::wire::{Msg, DEFAULT_MAX_FRAME};
use k10_mesh::{Addr, Capacity, Member, NodeId};

/// Requests written per batch.
const BATCH: u64 = 256;

fn fail(msg: &str) -> ! {
    eprintln!("meshpeer: {msg}");
    exit(1)
}

fn send(s: &mut TcpStream, link: &mut Link, out: &mut Vec<u8>) -> std::io::Result<()> {
    link.seal(out);
    let n = out.len() - link.held();
    s.write_all(&out[..n])?;
    out.drain(..n);
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [addr, key, count] = &args[..] else {
        eprintln!("usage: meshpeer HOST:PORT CLUSTER_KEY_FILE COUNT");
        exit(2)
    };
    let count: u64 = count.parse().unwrap_or_else(|_| fail("COUNT: want a number"));
    let key = std::fs::read_to_string(key).unwrap_or_else(|e| fail(&format!("{key}: {e}")));
    let keys = ClusterKeys::new(ClusterKey::parse_hex(&key).unwrap_or_else(|e| fail(e)), None).unwrap_or_else(|e| fail(e));
    let mut s = TcpStream::connect(addr).unwrap_or_else(|e| fail(&format!("{addr}: {e}")));
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    s.set_write_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut nonce = [0u8; 32];
    std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut nonce)).unwrap_or_else(|e| fail(&e.to_string()));
    let a: Addr = "127.0.0.1:9".parse().unwrap();
    let me = Member::new(NodeId(0xbad), a, a, a, Capacity { mem_bytes: 1 << 30, cores: 1 });
    let mut out = Vec::new();
    let mut link = Link::new(Some(&keys), &me, None, nonce, DEFAULT_MAX_FRAME, &mut out);
    let mut buf = vec![0u8; 64 << 10];
    loop {
        send(&mut s, &mut link, &mut out).unwrap_or_else(|e| fail(&format!("handshake: {e}")));
        match link.next(&mut out) {
            Ok(Some(LinkEvent::Up(_))) => break,
            Ok(Some(LinkEvent::Frame(_))) => fail("a frame before Hello"),
            Ok(None) => match s.read(&mut buf) {
                Ok(n) if n > 0 => link.feed(&buf[..n]),
                r => fail(&format!("closed during the handshake: {r:?}")),
            },
            Err(e) => fail(&format!("handshake: {e}")),
        }
    }
    let mut sent = 0;
    while sent < count {
        for _ in 0..BATCH.min(count - sent) {
            sent += 1;
            link.push(&Msg::GetManifest { version: 0 }, sent, &mut out).unwrap();
        }
        if send(&mut s, &mut link, &mut out).is_err() {
            break;
        }
    }
    let mut received = 0u64;
    while let Ok(n) = s.read(&mut buf) {
        if n == 0 {
            break;
        }
        received += n as u64;
    }
    println!("sent={sent} received={received}");
}
