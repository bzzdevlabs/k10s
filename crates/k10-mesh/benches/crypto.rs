//! Throughput of the cluster-authentication primitives (`cargo bench -p k10-mesh --bench crypto`): ChaCha20-
//! Poly1305 seal/open per message size (the peer-link frame path), XChaCha20-Poly1305 (gossip datagrams),
//! Poly1305 and ChaCha20 alone, and SHA-256 for comparison (content hashes).

use k10_mesh::crypto::aead::{xseal, Key};
use k10_mesh::crypto::chacha20::ChaCha20;
use k10_mesh::crypto::poly1305::poly1305;
use k10_mesh::sha256::sha256;
use std::hint::black_box;
use std::time::Instant;

fn bench(name: &str, bytes: usize, mut f: impl FnMut()) {
    // Calibrate to ~200 ms per sample, keep the best of 5 samples.
    let mut iters = 1u64;
    loop {
        let t = Instant::now();
        for _ in 0..iters {
            f();
        }
        if t.elapsed().as_millis() >= 20 {
            break;
        }
        iters *= 2;
    }
    iters *= 10;
    let mut best = f64::MAX;
    for _ in 0..5 {
        let t = Instant::now();
        for _ in 0..iters {
            f();
        }
        best = best.min(t.elapsed().as_secs_f64() / iters as f64);
    }
    let mbs = bytes as f64 / best / 1e6;
    println!("{name:<44} {:>10.1} ns/op {:>9.0} MB/s", best * 1e9, mbs);
}

fn main() {
    let key = Key::new(&[7; 32]);
    let nonce = [1u8; 12];
    for size in [16usize, 64, 128, 256, 1024, 1400, 16 << 10, 1 << 20] {
        let mut buf = vec![0x5au8; size];
        bench(&format!("chacha20-poly1305 seal {size} B"), size, || {
            let t = key.seal(black_box(&nonce), b"len!", black_box(&mut buf));
            black_box(t);
        });
        let mut ct = vec![0x5au8; size];
        let tag = key.seal(&nonce, b"len!", &mut ct);
        let mut work = ct.clone();
        bench(&format!("chacha20-poly1305 open {size} B"), size, || {
            work.copy_from_slice(&ct);
            assert!(key.open(black_box(&nonce), b"len!", black_box(&mut work), &tag));
        });
    }
    let mut d = vec![0u8; 1400];
    bench("xchacha20-poly1305 seal 1400 B (gossip)", 1400, || {
        black_box(xseal(&[3; 32], black_box(&[2; 24]), b"K\x01", black_box(&mut d)));
    });
    let big = vec![0x33u8; 1 << 20];
    bench("poly1305 1 MiB", big.len(), || {
        black_box(poly1305(&[9; 32], black_box(&big)));
    });
    let c = ChaCha20::new(&[1; 32], &[2; 12]);
    let mut b = vec![0u8; 1 << 20];
    bench("chacha20 1 MiB", b.len(), || c.apply(1, black_box(&mut b)));
    bench("sha256 1 MiB", big.len(), || {
        black_box(sha256(black_box(&big)));
    });
}
