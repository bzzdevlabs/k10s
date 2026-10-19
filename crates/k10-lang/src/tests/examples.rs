//! Every example service in /examples, run with the in-memory test host and checked against an independent
//! Rust implementation of what it computes.

use crate::testhost::{Response, ServiceConfig, TestHost, DELETE, GET, POST, PUT};
use crate::{compile_file_with_sources, render_diagnostics, Sources};
use k10_ir::Module;
use std::path::PathBuf;

pub fn example_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples").join(name)
}

pub fn compile_example(name: &str) -> Module {
    let mut sources = Sources::new();
    match compile_file_with_sources(&example_path(name), &mut sources) {
        Ok(m) => {
            k10_ir::validate(&m).expect("valid");
            m
        }
        Err(d) => panic!("{name}:\n{}", render_diagnostics(&d, &sources)),
    }
}

fn host_with(services: &[(&str, &str)]) -> TestHost {
    let mut h = TestHost::new();
    for (svc, file) in services {
        h.add(svc, compile_example(file)).unwrap();
    }
    h
}

fn ok(r: &Response) -> String {
    assert!(r.trap.is_none(), "trapped: {:?} ({})", r.trap, r.text());
    r.text()
}

// -------------------------------------------------------------------------------------------------------------
// Reference SHA-256 (FIPS 180-4), written independently of the Ten version.

const K256: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5, 0xd807aa98,
    0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786,
    0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8,
    0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13,
    0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819,
    0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a,
    0x5b9cca4f, 0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
    0xc67178f2,
];

pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut h: [u32; 8] =
        [0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19];
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&((data.len() as u64) * 8).to_be_bytes());
    for chunk in msg.chunks(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes(chunk[4 * i..4 * i + 4].try_into().unwrap());
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = hh.wrapping_add(s1).wrapping_add(ch).wrapping_add(K256[i]).wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (x, y) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *x = x.wrapping_add(y);
        }
    }
    let mut out = [0u8; 32];
    for i in 0..8 {
        out[4 * i..4 * i + 4].copy_from_slice(&h[i].to_be_bytes());
    }
    out
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// What bench/workloads/compute answers for `n` rounds.
pub fn compute_expected(n: u32) -> String {
    let mut h = sha256(b"k10s");
    for _ in 0..n {
        h = sha256(&h);
    }
    format!("{}\n", hex(&h))
}

#[test]
fn reference_sha256_is_correct() {
    assert_eq!(hex(&sha256(b"abc")), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    assert_eq!(hex(&sha256(b"")), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
    let long = b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq";
    assert_eq!(hex(&sha256(long)), "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1");
}

// -------------------------------------------------------------------------------------------------------------

#[test]
fn hello() {
    let mut h = host_with(&[("hello", "hello.ten")]);
    assert_eq!(ok(&h.get("hello", "/")), "hello");
    assert_eq!(ok(&h.post("hello", "/x", b"body")), "hello");
    let m = compile_example("hello.ten");
    assert_eq!(m.imports.len(), 1);
    assert_eq!(m.funcs.len(), 1);
}

#[test]
fn echo() {
    let mut h = host_with(&[("echo", "echo.ten")]);
    assert_eq!(ok(&h.get("echo", "/")), "hello");
    assert_eq!(ok(&h.post("echo", "/", b"xyz")), "xyz");
    assert_eq!(ok(&h.post("echo", "/", b"")), "hello");
    assert_eq!(ok(&h.request("echo", PUT, "/", b"xyz")), "hello");
    // Bodies larger than the 4 KiB streaming buffer.
    let big: Vec<u8> = (0..20_000u32).map(|i| (i * 7 + i / 251) as u8).collect();
    let r = h.post("echo", "/", &big);
    assert!(r.trap.is_none());
    assert_eq!(r.body, big);
}

#[test]
fn compute() {
    let mut h = host_with(&[("compute", "compute.ten")]);
    // n = 0 rounds is not reachable through the API (0 means "default"); check 1, 2, 5 and the default.
    for n in [1u32, 2, 5, 64] {
        assert_eq!(ok(&h.get("compute", &format!("/?n={n}"))), compute_expected(n), "n={n}");
    }
    let default = compute_expected(1000);
    assert_eq!(ok(&h.get("compute", "/")), default);
    for q in ["/?n=0", "/?n=-5", "/?n=abc", "/?n=", "/?n=1x", "/?n=100000001", "/?m=3", "/?n=99999999999999999999"] {
        assert_eq!(ok(&h.get("compute", q)), default, "query {q}");
    }
    // Other parameters around n, Atoi-style sign.
    assert_eq!(ok(&h.get("compute", "/?a=1&n=3&b")), compute_expected(3));
    assert_eq!(ok(&h.get("compute", "/?n=+4")), compute_expected(4));
}

#[test]
fn chain() {
    let mut h = host_with(&[("echo", "echo.ten"), ("chain", "chain.ten")]);
    assert_eq!(ok(&h.get("chain", "/")), "hello");
    // Without a backend: 502, like the Go version when the backend is unreachable.
    let mut lonely = host_with(&[("chain", "chain.ten")]);
    let r = lonely.get("chain", "/");
    assert_eq!((r.status, r.text().as_str()), (502, "backend error -1\n"));
    // Backend chosen by config, and non-200 statuses.
    let mut h2 = TestHost::new();
    let cfg = ServiceConfig { config: b"teapot".to_vec(), ..Default::default() };
    h2.add_with("chain", compile_example("chain.ten"), cfg).unwrap();
    h2.add("teapot", crate::tests::util::compile_ok("export fn handle() { status(418); out_str(\"short and stout\"); }")).unwrap();
    let r = h2.get("chain", "/");
    assert_eq!((r.status, r.text().as_str()), (502, "backend status 418\n"));
}

#[test]
fn counter() {
    let mut h = host_with(&[("counter", "counter.ten")]);
    for i in 1..=5 {
        assert_eq!(ok(&h.get("counter", "/")), format!("{i}\n"));
    }
    assert_eq!(h.kv("counter").unwrap().get(&b"counter"[..]).map(|v| v.as_slice()), Some(&b"5"[..]));
    // The state is durable, not in the instance: a new instance continues counting.
    h.restart("counter");
    assert_eq!(ok(&h.get("counter", "/")), "6\n");
}

#[test]
fn todo() {
    let mut h = host_with(&[("todo", "todo.ten")]);
    assert_eq!(ok(&h.get("todo", "/")), "[]");
    let r = h.post("todo", "/", b"buy milk");
    assert_eq!((r.status, ok(&r).as_str()), (201, r#"{"id":1,"title":"buy milk"}"#));
    let r = h.post("todo", "", b"say \"hi\"\n\\o/\x01");
    assert_eq!((r.status, ok(&r).as_str()), (201, r#"{"id":2,"title":"say \"hi\"\n\\o/\u0001"}"#));
    assert_eq!(ok(&h.post("todo", "/", b"third")), r#"{"id":3,"title":"third"}"#);
    assert_eq!(
        ok(&h.get("todo", "/")),
        r#"[{"id":1,"title":"buy milk"},{"id":2,"title":"say \"hi\"\n\\o/\u0001"},{"id":3,"title":"third"}]"#
    );
    assert_eq!(ok(&h.get("todo", "/2")), r#"{"id":2,"title":"say \"hi\"\n\\o/\u0001"}"#);
    assert_eq!(ok(&h.get("todo", "/3/?verbose=1")), r#"{"id":3,"title":"third"}"#);
    let r = h.request("todo", PUT, "/1", b"buy oat milk");
    assert_eq!((r.status, ok(&r).as_str()), (200, r#"{"id":1,"title":"buy oat milk"}"#));
    let r = h.request("todo", DELETE, "/2", b"");
    assert_eq!((r.status, ok(&r).as_str()), (200, r#"{"deleted":2}"#));
    let r = h.request("todo", DELETE, "/2", b"");
    assert_eq!((r.status, ok(&r).as_str()), (404, r#"{"error":"not found"}"#));
    let r = h.get("todo", "/2");
    assert_eq!((r.status, ok(&r).as_str()), (404, r#"{"error":"not found"}"#));
    let r = h.request("todo", PUT, "/9", b"x");
    assert_eq!((r.status, ok(&r).as_str()), (404, r#"{"error":"not found"}"#));
    assert_eq!(ok(&h.get("todo", "/")), r#"[{"id":1,"title":"buy oat milk"},{"id":3,"title":"third"}]"#);
    let r = h.get("todo", "/abc");
    assert_eq!((r.status, ok(&r).as_str()), (400, r#"{"error":"bad id"}"#));
    let r = h.request("todo", DELETE, "/", b"");
    assert_eq!((r.status, ok(&r).as_str()), (405, r#"{"error":"method not allowed"}"#));
    let r = h.request("todo", POST, "/1", b"");
    assert_eq!(r.status, 405);
    // ids keep increasing after deletions
    assert_eq!(ok(&h.post("todo", "/", b"four")), r#"{"id":4,"title":"four"}"#);
    let r = h.post("todo", "/", &[b'x'; 5000]);
    assert_eq!((r.status, ok(&r).as_str()), (413, r#"{"error":"title too long"}"#));
    // Many requests keep memory bounded (arena reset per request).
    for i in 0..200 {
        assert_eq!(h.get("todo", "/1").status, 200, "request {i}");
    }
    let _ = GET;
}

// -------------------------------------------------------------------------------------------------------------
// n-body reference (same operation order as nbody.ten / the benchmarks game C program).

#[derive(Clone, Copy)]
struct Body {
    x: f64,
    y: f64,
    z: f64,
    vx: f64,
    vy: f64,
    vz: f64,
    mass: f64,
}

#[allow(clippy::excessive_precision)]
fn nbody_reference(n: usize) -> String {
    let pi = std::f64::consts::PI;
    let sm = 4.0 * pi * pi;
    let dpy = 365.24;
    let b = |x, y, z, vx: f64, vy: f64, vz: f64, m: f64| Body { x, y, z, vx: vx * dpy, vy: vy * dpy, vz: vz * dpy, mass: m * sm };
    let mut bodies = [
        b(0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0),
        b(4.84143144246472090e+00, -1.16032004402742839e+00, -1.03622044471123109e-01, 1.66007664274403694e-03,
            7.69901118419740425e-03, -6.90460016972063023e-05, 9.54791938424326609e-04),
        b(8.34336671824457987e+00, 4.12479856412430479e+00, -4.03523417114321381e-01, -2.76742510726862411e-03,
            4.99852801234917238e-03, 2.30417297573763929e-05, 2.85885980666130812e-04),
        b(1.28943695621391310e+01, -1.51111514016986312e+01, -2.23307578892655734e-01, 2.96460137564761618e-03,
            2.37847173959480950e-03, -2.96589568540237556e-05, 4.36624404335156298e-05),
        b(1.53796971148509165e+01, -2.59193146099879641e+01, 1.79258772950371181e-01, 2.68067772490389322e-03,
            1.62824170038242295e-03, -9.51592254519715870e-05, 5.15138902046611451e-05),
    ];
    let (mut px, mut py, mut pz) = (0.0, 0.0, 0.0);
    for b in &bodies {
        px += b.vx * b.mass;
        py += b.vy * b.mass;
        pz += b.vz * b.mass;
    }
    bodies[0].vx = -px / sm;
    bodies[0].vy = -py / sm;
    bodies[0].vz = -pz / sm;
    let energy = |bodies: &[Body; 5]| {
        let mut e = 0.0;
        for i in 0..5 {
            let b = bodies[i];
            e += 0.5 * b.mass * (b.vx * b.vx + b.vy * b.vy + b.vz * b.vz);
            for b2 in &bodies[i + 1..] {
                let (dx, dy, dz) = (b.x - b2.x, b.y - b2.y, b.z - b2.z);
                e -= (b.mass * b2.mass) / (dx * dx + dy * dy + dz * dz).sqrt();
            }
        }
        e
    };
    let e0 = energy(&bodies);
    let dt = 0.01;
    for _ in 0..n {
        for i in 0..5 {
            for j in i + 1..5 {
                let (dx, dy, dz) =
                    (bodies[i].x - bodies[j].x, bodies[i].y - bodies[j].y, bodies[i].z - bodies[j].z);
                let d = (dx * dx + dy * dy + dz * dz).sqrt();
                let mag = dt / (d * d * d);
                let (mi, mj) = (bodies[i].mass, bodies[j].mass);
                bodies[i].vx -= dx * mj * mag;
                bodies[i].vy -= dy * mj * mag;
                bodies[i].vz -= dz * mj * mag;
                bodies[j].vx += dx * mi * mag;
                bodies[j].vy += dy * mi * mag;
                bodies[j].vz += dz * mi * mag;
            }
        }
        for b in bodies.iter_mut() {
            b.x += dt * b.vx;
            b.y += dt * b.vy;
            b.z += dt * b.vz;
        }
    }
    format!("{e0:.9}\n{:.9}\n", energy(&bodies))
}

#[test]
fn nbody() {
    let mut h = host_with(&[("nbody", "nbody.ten")]);
    assert_eq!(ok(&h.get("nbody", "/")), "-0.169075164\n-0.169087605\n");
    assert_eq!(nbody_reference(1000), "-0.169075164\n-0.169087605\n");
    for n in [0usize, 1, 10, 250] {
        assert_eq!(ok(&h.get("nbody", &format!("/?n={n}"))), nbody_reference(n), "n={n}");
    }
}

// -------------------------------------------------------------------------------------------------------------

fn sort_reference(n: usize) -> String {
    let mut x: u64 = 42;
    let mut a: Vec<i64> = (0..n)
        .map(|_| {
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            x.wrapping_mul(0x2545F4914F6CDD1D) as i64
        })
        .collect();
    a.sort_unstable();
    let sum = a.iter().enumerate().fold(0i64, |s, (i, v)| s.wrapping_add(v.wrapping_mul(i as i64 + 1)));
    format!("n={n} checksum={sum}\n")
}

#[test]
fn sort() {
    let mut h = TestHost::new();
    let cfg = ServiceConfig { mem_log2: Some(22), ..Default::default() };
    h.add_with("sort", compile_example("sort.ten"), cfg).unwrap();
    for n in [0usize, 1, 2, 15, 16, 17, 100, 1000, 4097, 30_000] {
        assert_eq!(ok(&h.get("sort", &format!("/?n={n}"))), sort_reference(n), "n={n}");
    }
    // Too little memory: 507, no trap.
    let mut small = TestHost::new();
    small.add_with("sort", compile_example("sort.ten"), ServiceConfig { mem_log2: Some(16), ..Default::default() }).unwrap();
    let r = small.get("sort", "/?n=100000");
    assert_eq!((r.status, ok(&r).as_str()), (507, "not enough memory for n=100000\n"));
    assert_eq!(ok(&small.get("sort", "/?n=100")), sort_reference(100));
    let r = small.get("sort", "/?n=-1");
    assert_eq!(r.status, 400);
}

#[test]
fn examples_are_small() {
    for name in ["hello.ten", "echo.ten", "compute.ten", "chain.ten", "counter.ten", "todo.ten", "nbody.ten", "sort.ten"] {
        let m = compile_example(name);
        let bytes = k10_ir::encode(&m).len();
        assert!(bytes < 4096, "{name}: {bytes} bytes");
        assert!(m.export("handle").is_some());
    }
}
