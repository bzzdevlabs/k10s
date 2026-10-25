//! mkbundle — builds test services and writes a deployable `k10_mesh::Bundle` (manifest + code blobs).
//!
//! ```text
//! mkbundle [-o out/bundle.k10b] [--version N] [--services a,b,...] [--replicate NAME:COUNT] [--mem-log2 K]
//!          [--max-instances N] [--ten-dir DIR | --no-ten]
//! mkbundle --list
//! mkbundle --version | -V            (alone: build version; `--version N` sets the manifest version)
//! ```
//!
//! Services built as TIR with `k10_ir::builder` (all import only what they use; memory layout: data at 0..,
//! scratch at 1024.., buffers at 4096..): hello, echo, compute, counter, chain, spin, callspin, probe, trap,
//! instcount, freshcount, config, log, time, random. Granted calls: chain → echo, callspin → spin, probe → hello.
//!
//! Services compiled from Ten sources with `k10_lang::compile_file` (from `--ten-dir`, default the repository's
//! `examples/`): sha (compute.ten, SHA-256 chain), nbody (nbody.ten, f64), sort (sort.ten, 2 MiB memory),
//! todo (todo.ten, KV + JSON REST API).

use std::collections::BTreeMap;
use std::process::exit;

use k10_ir::{FuncBuilder, Module, ModuleBuilder};
use k10_mesh::{sha256::sha256, Bundle, Calls, Manifest, ServiceSpec};

/// Imports declared on demand, in a fixed order per module.
struct Host {
    idx: BTreeMap<&'static str, u32>,
}

const HOST_PARAMS: &[(&str, u8)] = &[
    ("input_len", 0),
    ("input_read", 3),
    ("req_path", 2),
    ("req_method", 0),
    ("output_write", 2),
    ("output_status", 1),
    ("call", 4),
    ("call_resp_len", 0),
    ("call_resp_read", 3),
    ("kv_get", 4),
    ("kv_put", 4),
    ("kv_del", 2),
    ("log", 2),
    ("now_ns", 0),
    ("time_ms", 0),
    ("random", 0),
    ("config", 2),
];

impl Host {
    fn new(mb: &mut ModuleBuilder, names: &[&'static str]) -> Host {
        let mut idx = BTreeMap::new();
        for &n in names {
            let p = HOST_PARAMS.iter().find(|(h, _)| *h == n).unwrap_or_else(|| panic!("unknown import {n}")).1;
            idx.insert(n, mb.import(n, p, 1));
        }
        Host { idx }
    }
    fn f(&self, n: &str) -> u32 {
        *self.idx.get(n).unwrap_or_else(|| panic!("import {n} not declared"))
    }
}

/// Emits `out_dec(v)`: writes `v` (unsigned) in decimal via output_write, using memory [2048, 2080).
fn out_dec(mb: &mut ModuleBuilder, h: &Host) -> u32 {
    let write = h.f("output_write");
    mb.func(1, 0, |f| {
        let p = f.new_local();
        f.const_(2080).local_set(p);
        f.local_get(0).eqz().if_();
        f.local_get(p).const_(1).sub().local_tee(p).const_(b'0' as i64).store8(0);
        f.end();
        f.block().loop_();
        f.local_get(0).eqz().br_if(1);
        f.local_get(p).const_(1).sub().local_set(p);
        f.local_get(p).local_get(0).const_(10).rem_u().const_(b'0' as i64).add().store8(0);
        f.local_get(0).const_(10).div_u().local_set(0);
        f.br(0).end().end();
        f.local_get(p).const_(2080).local_get(p).sub().call(write).drop();
    })
}

/// `output_write(ptr, len)` of a constant string placed in a data segment.
fn write_const(f: &mut FuncBuilder, h: &Host, ptr: i64, len: usize) {
    f.const_(ptr).const_(len as i64).call(h.f("output_write")).drop();
}

/// Writes one byte (via memory at 4000).
fn put_char(f: &mut FuncBuilder, h: &Host, c: u8) {
    f.const_(4000).const_(c as i64).store8(0);
    f.const_(4000).const_(1).call(h.f("output_write")).drop();
}

fn hello() -> Module {
    let mut mb = ModuleBuilder::new();
    let h = Host::new(&mut mb, &["output_write"]);
    let msg = b"hello from k10-metal\n";
    mb.data(0, msg);
    let f = mb.func(0, 0, |f| write_const(f, &h, 0, msg.len()));
    mb.export("handle", f);
    mb.into_module()
}

/// Echoes the request body (copied in 32 KiB chunks), or the request path when the body is empty.
fn echo() -> Module {
    let mut mb = ModuleBuilder::new();
    mb.mem_log2(16);
    let h = Host::new(&mut mb, &["input_len", "input_read", "req_path", "output_write"]);
    let f = mb.func(0, 0, |f| {
        let (off, n) = (f.new_local(), f.new_local());
        f.call(h.f("input_len")).eqz().if_();
        // Empty body: echo the path (truncated to 2 KiB).
        f.const_(4096).const_(2048).call(h.f("req_path")).local_set(n);
        f.local_get(n).const_(2048).gt_u().if_().const_(2048).local_set(n).end();
        f.const_(4096).local_get(n).call(h.f("output_write")).drop();
        f.return_();
        f.end();
        f.block().loop_();
        f.const_(4096).local_get(off).const_(32768).call(h.f("input_read")).local_tee(n).eqz().br_if(1);
        f.const_(4096).local_get(n).call(h.f("output_write")).drop();
        f.local_get(off).local_get(n).add().local_set(off);
        f.br(0).end().end();
    });
    mb.export("handle", f);
    mb.into_module()
}

/// `/compute?n=N`: N rounds of xorshift64 (every digit in the path is part of N; default 1000); prints the hash.
fn compute() -> Module {
    let mut mb = ModuleBuilder::new();
    let h = Host::new(&mut mb, &["req_path", "output_write"]);
    let dec = out_dec(&mut mb, &h);
    let f = mb.func(0, 0, |f| {
        let (len, i, c, n, x, acc) = (f.new_local(), f.new_local(), f.new_local(), f.new_local(), f.new_local(), f.new_local());
        f.const_(1024).const_(256).call(h.f("req_path")).local_set(len);
        f.local_get(len).const_(256).gt_u().if_().const_(256).local_set(len).end();
        // Parse digits.
        f.block().loop_();
        f.local_get(i).local_get(len).ge_u().br_if(1);
        f.local_get(i).load8_u(1024).local_set(c);
        f.local_get(c).const_(b'0' as i64).ge_u().local_get(c).const_(b'9' as i64).le_u().and().if_();
        f.local_get(n).const_(10).mul().local_get(c).const_(b'0' as i64).sub().add().local_set(n);
        f.end();
        f.local_get(i).const_(1).add().local_set(i);
        f.br(0).end().end();
        f.local_get(n).eqz().if_().const_(1000).local_set(n).end();
        // xorshift64 rounds.
        f.const_(0x9E37_79B9_7F4A_7C15u64 as i64).local_set(x);
        f.block().loop_();
        f.local_get(n).eqz().br_if(1);
        f.local_get(x).local_get(x).const_(13).shl().xor().local_set(x);
        f.local_get(x).local_get(x).const_(7).shr_u().xor().local_set(x);
        f.local_get(x).local_get(x).const_(17).shl().xor().local_set(x);
        f.local_get(acc).local_get(x).add().local_set(acc);
        f.local_get(n).const_(1).sub().local_set(n);
        f.br(0).end().end();
        f.local_get(acc).const_(0xFFFF_FFFF).and().call(dec);
        put_char(f, &h, b'\n');
    });
    mb.export("handle", f);
    mb.into_module()
}

/// Durable (node-local, in-memory) counter in the KV store.
fn counter() -> Module {
    let mut mb = ModuleBuilder::new();
    let h = Host::new(&mut mb, &["kv_get", "kv_put", "output_write"]);
    let dec = out_dec(&mut mb, &h);
    mb.data(0, b"count");
    let f = mb.func(0, 0, |f| {
        let v = f.new_local();
        f.const_(0).const_(5).const_(64).const_(8).call(h.f("kv_get")).const_(8).eq().if_();
        f.const_(64).load64(0).local_set(v);
        f.end();
        f.local_get(v).const_(1).add().local_set(v);
        f.const_(64).local_get(v).store64(0);
        f.const_(0).const_(5).const_(64).const_(8).call(h.f("kv_put")).drop();
        f.local_get(v).call(dec);
        put_char(f, &h, b'\n');
    });
    mb.export("handle", f);
    mb.into_module()
}

/// `/kv/<key>`: GET → the value (404 if absent), PUT or POST → stores the body (up to 64 KiB), DELETE → removes
/// the key (404 if absent). The storage tests write through it.
fn kv() -> Module {
    let mut mb = ModuleBuilder::new();
    mb.mem_log2(17);
    let names = ["req_path", "req_method", "input_len", "input_read", "output_write", "output_status", "kv_get", "kv_put", "kv_del"];
    let h = Host::new(&mut mb, &names);
    let f = mb.func(0, 0, |f| {
        let (klen, m, n) = (f.new_local(), f.new_local(), f.new_local());
        // The key: the path after its leading '/', at 1025.
        f.const_(1024).const_(256).call(h.f("req_path")).local_set(klen);
        f.local_get(klen).const_(256).gt_u().local_get(klen).const_(2).lt_u().or().if_();
        f.const_(400).call(h.f("output_status")).drop().return_();
        f.end();
        f.call(h.f("req_method")).local_set(m);
        f.local_get(m).eqz().if_();
        f.const_(1025).local_get(klen).const_(1).sub().const_(4096).const_(65536).call(h.f("kv_get")).local_tee(n);
        f.const_(0).lt_s().if_().const_(404).call(h.f("output_status")).drop().else_();
        f.const_(4096).local_get(n).call(h.f("output_write")).drop();
        f.end();
        f.return_();
        f.end();
        f.local_get(m).const_(3).eq().if_();
        f.const_(1025).local_get(klen).const_(1).sub().call(h.f("kv_del")).eqz().if_();
        f.const_(404).call(h.f("output_status")).drop();
        f.end();
        f.return_();
        f.end();
        f.call(h.f("input_len")).local_set(n);
        f.local_get(n).const_(65536).gt_u().if_().const_(413).call(h.f("output_status")).drop().return_().end();
        f.const_(4096).const_(0).local_get(n).call(h.f("input_read")).drop();
        f.const_(1025).local_get(klen).const_(1).sub().const_(4096).local_get(n).call(h.f("kv_put")).drop();
    });
    mb.export("handle", f);
    mb.into_module()
}

/// Calls `echo/chain` with a body and returns "chain -> <echo's response>".
fn chain() -> Module {
    call_to(b"echo/chain")
}

/// Calls the infinite-loop service: the callee is trapped by its CPU budget, so this answers 502 "call failed: -4"
/// (with `max_instances = 1`, concurrent callers also exercise waiting for a busy callee).
fn callspin() -> Module {
    call_to(b"spin")
}

fn call_to(name: &'static [u8]) -> Module {
    let mut mb = ModuleBuilder::new();
    mb.mem_log2(16);
    let h = Host::new(&mut mb, &["call", "call_resp_len", "call_resp_read", "output_write", "output_status"]);
    let dec = out_dec(&mut mb, &h);
    let body = b"ping from chain\n";
    let prefix = b"chain -> ";
    let fail = b"call failed: -";
    mb.data(0, name).data(32, body).data(64, prefix).data(96, fail);
    let f = mb.func(0, 0, |f| {
        let (s, n) = (f.new_local(), f.new_local());
        f.const_(0).const_(name.len() as i64).const_(32).const_(body.len() as i64).call(h.f("call")).local_set(s);
        f.local_get(s).const_(0).lt_s().if_();
        f.const_(502).call(h.f("output_status")).drop();
        write_const(f, &h, 96, fail.len());
        f.const_(0).local_get(s).sub().call(dec);
        f.return_();
        f.end();
        write_const(f, &h, 64, prefix.len());
        f.call(h.f("call_resp_len")).local_set(n);
        f.local_get(n).const_(32768).gt_u().if_().const_(32768).local_set(n).end();
        f.const_(4096).const_(0).local_get(n).call(h.f("call_resp_read")).drop();
        f.const_(4096).local_get(n).call(h.f("output_write")).drop();
    });
    mb.export("handle", f);
    mb.into_module()
}

/// Calls the service named by the request path (`/probe/<name>[/<path>]`) and answers like `chain`; it may call
/// only hello, so other targets exercise denied calls.
fn probe() -> Module {
    let mut mb = ModuleBuilder::new();
    mb.mem_log2(16);
    let imports = ["req_path", "call", "call_resp_len", "call_resp_read", "output_write", "output_status"];
    let h = Host::new(&mut mb, &imports);
    let dec = out_dec(&mut mb, &h);
    let prefix = b"probe -> ";
    let fail = b"call failed: -";
    mb.data(64, prefix).data(96, fail);
    let f = mb.func(0, 0, |f| {
        let (s, n) = (f.new_local(), f.new_local());
        // Target = request path without its leading '/' (at 1024, at most 256 bytes).
        f.const_(1024).const_(256).call(h.f("req_path")).local_set(n);
        f.local_get(n).const_(256).gt_u().if_().const_(256).local_set(n).end();
        f.local_get(n).eqz().if_().const_(1).local_set(n).end();
        f.const_(1025).local_get(n).const_(1).sub().const_(0).const_(0).call(h.f("call")).local_set(s);
        f.local_get(s).const_(0).lt_s().if_();
        f.const_(502).call(h.f("output_status")).drop();
        write_const(f, &h, 96, fail.len());
        f.const_(0).local_get(s).sub().call(dec);
        f.return_();
        f.end();
        write_const(f, &h, 64, prefix.len());
        f.call(h.f("call_resp_len")).local_set(n);
        f.local_get(n).const_(32768).gt_u().if_().const_(32768).local_set(n).end();
        f.const_(4096).const_(0).local_get(n).call(h.f("call_resp_read")).drop();
        f.const_(4096).local_get(n).call(h.f("output_write")).drop();
    });
    mb.export("handle", f);
    mb.into_module()
}

/// Never returns: exercises preemption and the CPU budget.
fn spin() -> Module {
    let mut mb = ModuleBuilder::new();
    let f = mb.func(0, 0, |f| {
        f.loop_().br(0).end();
    });
    mb.export("handle", f);
    mb.into_module()
}

/// Traps (`unreachable`): the instance is discarded and the request gets a 500.
fn trap() -> Module {
    let mut mb = ModuleBuilder::new();
    let f = mb.func(0, 0, |f| {
        f.unreachable();
    });
    mb.export("handle", f);
    mb.into_module()
}

/// Instance-local state: `init` stores 1000 at address 128, each request increments and prints it.
/// Deployed twice: `instcount` (state persists in the instance) and `freshcount` (fresh = every request starts
/// from the post-init snapshot, so it always prints 1001).
fn instcount() -> Module {
    let mut mb = ModuleBuilder::new();
    let h = Host::new(&mut mb, &["output_write"]);
    let dec = out_dec(&mut mb, &h);
    let init = mb.func(0, 0, |f| {
        f.const_(128).const_(1000).store64(0);
    });
    let f = mb.func(0, 0, |f| {
        f.const_(128).const_(128).load64(0).const_(1).add().store64(0);
        f.const_(128).load64(0).call(dec);
        put_char(f, &h, b'\n');
    });
    mb.export("init", init).export("handle", f);
    mb.into_module()
}

/// Returns the service's config bytes from the manifest.
fn config() -> Module {
    let mut mb = ModuleBuilder::new();
    let h = Host::new(&mut mb, &["config", "output_write"]);
    let f = mb.func(0, 0, |f| {
        let n = f.new_local();
        f.const_(256).const_(512).call(h.f("config")).local_set(n);
        f.local_get(n).const_(512).gt_u().if_().const_(512).local_set(n).end();
        f.const_(256).local_get(n).call(h.f("output_write")).drop();
    });
    mb.export("handle", f);
    mb.into_module()
}

/// Logs a line (serial console + /_k10s/logs/log) and answers "logged".
fn log() -> Module {
    let mut mb = ModuleBuilder::new();
    let h = Host::new(&mut mb, &["log", "output_write"]);
    let line = b"hello from the log service";
    let ans = b"logged\n";
    mb.data(0, line).data(64, ans);
    let f = mb.func(0, 0, |f| {
        f.const_(0).const_(line.len() as i64).call(h.f("log")).drop();
        write_const(f, &h, 64, ans.len());
    });
    mb.export("handle", f);
    mb.into_module()
}

/// Prints `time_ms now_ns` (wall-clock ms and monotonic ns).
fn time() -> Module {
    let mut mb = ModuleBuilder::new();
    let h = Host::new(&mut mb, &["time_ms", "now_ns", "output_write"]);
    let dec = out_dec(&mut mb, &h);
    let f = mb.func(0, 0, |f| {
        f.call(h.f("time_ms")).call(dec);
        put_char(f, &h, b' ');
        f.call(h.f("now_ns")).call(dec);
        put_char(f, &h, b'\n');
    });
    mb.export("handle", f);
    mb.into_module()
}

/// Prints a random 32-bit number.
fn random() -> Module {
    let mut mb = ModuleBuilder::new();
    let h = Host::new(&mut mb, &["random", "output_write"]);
    let dec = out_dec(&mut mb, &h);
    let f = mb.func(0, 0, |f| {
        f.call(h.f("random")).const_(0xFFFF_FFFF).and().call(dec);
        put_char(f, &h, b'\n');
    });
    mb.export("handle", f);
    mb.into_module()
}

struct Def {
    name: &'static str,
    build: fn() -> Module,
    cpu_ms: u32,
    fresh: bool,
    config: &'static [u8],
    /// Services it may `call` (dropped when not in the bundle).
    calls: &'static [&'static str],
}

/// Services compiled from Ten sources: (service name, file in the Ten directory, cpu_ms, memory log2 if larger
/// than the module's minimum).
const TEN: &[(&str, &str, u32, Option<u8>)] = &[
    ("sha", "compute.ten", 30_000, None),
    ("nbody", "nbody.ten", 30_000, None),
    ("sort", "sort.ten", 30_000, Some(21)),
    ("todo", "todo.ten", 1000, None),
];

fn compile_ten(dir: &std::path::Path, file: &str) -> Module {
    let path = dir.join(file);
    let mut sources = k10_lang::Sources::new();
    k10_lang::compile_file_with_sources(&path, &mut sources).unwrap_or_else(|d| {
        eprintln!("{}", k10_lang::render_diagnostics(&d, &sources));
        panic!("{}: compile error", path.display())
    })
}

const DEFS: &[Def] = &[
    Def { name: "hello", build: hello, cpu_ms: 1000, fresh: false, config: b"", calls: &[] },
    Def { name: "echo", build: echo, cpu_ms: 1000, fresh: false, config: b"", calls: &[] },
    Def { name: "compute", build: compute, cpu_ms: 5000, fresh: false, config: b"", calls: &[] },
    Def { name: "counter", build: counter, cpu_ms: 1000, fresh: false, config: b"", calls: &[] },
    Def { name: "kv", build: kv, cpu_ms: 1000, fresh: false, config: b"", calls: &[] },
    Def { name: "chain", build: chain, cpu_ms: 1000, fresh: false, config: b"", calls: &["echo"] },
    Def { name: "spin", build: spin, cpu_ms: 100, fresh: false, config: b"", calls: &[] },
    Def { name: "callspin", build: callspin, cpu_ms: 1000, fresh: false, config: b"", calls: &["spin"] },
    Def { name: "probe", build: probe, cpu_ms: 1000, fresh: false, config: b"", calls: &["hello"] },
    Def { name: "trap", build: trap, cpu_ms: 1000, fresh: false, config: b"", calls: &[] },
    Def { name: "instcount", build: instcount, cpu_ms: 1000, fresh: false, config: b"", calls: &[] },
    Def { name: "freshcount", build: instcount, cpu_ms: 1000, fresh: true, config: b"", calls: &[] },
    Def {
        name: "config",
        build: config,
        cpu_ms: 1000,
        fresh: false,
        config: b"greeting=hello from the manifest\n",
        calls: &[],
    },
    Def { name: "log", build: log, cpu_ms: 1000, fresh: false, config: b"", calls: &[] },
    Def { name: "time", build: time, cpu_ms: 1000, fresh: false, config: b"", calls: &[] },
    Def { name: "random", build: random, cpu_ms: 1000, fresh: false, config: b"", calls: &[] },
];

fn usage() -> ! {
    eprintln!(
        "usage: mkbundle [-o FILE] [--version N] [--services a,b,..] [--replicate NAME:COUNT] [--mem-log2 K] \
         [--max-instances N] [--ten-dir DIR | --no-ten] | --list | --version"
    );
    exit(2)
}

fn main() {
    let mut out = String::from("out/bundle.k10b");
    let mut version = 1u64;
    let mut only: Option<Vec<String>> = None;
    let mut replicate: Option<(String, usize)> = None;
    let mut mem_log2: Option<u8> = None;
    let mut max_instances = 64u32;
    let mut ten_dir = Some(std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../examples")));
    let mut list = false;
    let mut args = std::env::args().skip(1);
    if args.len() == 1 && matches!(std::env::args().nth(1).as_deref(), Some("--version" | "-V")) {
        println!("mkbundle {}", env!("K10S_VERSION_FULL"));
        return;
    }
    while let Some(a) = args.next() {
        let mut val = || args.next().unwrap_or_else(|| usage());
        match a.as_str() {
            "-o" | "--out" => out = val(),
            "--version" => version = val().parse().unwrap_or_else(|_| usage()),
            "--services" => only = Some(val().split(',').map(str::to_string).collect()),
            "--replicate" => {
                let v = val();
                let (n, c) = v.split_once(':').unwrap_or_else(|| usage());
                replicate = Some((n.to_string(), c.parse().unwrap_or_else(|_| usage())));
            }
            "--mem-log2" => mem_log2 = Some(val().parse().unwrap_or_else(|_| usage())),
            "--max-instances" => max_instances = val().parse().unwrap_or_else(|_| usage()),
            "--ten-dir" => ten_dir = Some(val().into()),
            "--no-ten" => ten_dir = None,
            "--list" => list = true,
            _ => usage(),
        }
    }
    // Every service: (name, module, cpu_ms, fresh, config, memory log2 override, calls).
    type Svc = (String, Module, u32, bool, Vec<u8>, Option<u8>, &'static [&'static str]);
    let def = |d: &Def| -> Svc { (d.name.into(), (d.build)(), d.cpu_ms, d.fresh, d.config.to_vec(), None, d.calls) };
    let mut all: Vec<Svc> = DEFS.iter().map(def).collect();
    if let Some(dir) = &ten_dir {
        for &(name, file, cpu_ms, mem) in TEN {
            all.push((name.to_string(), compile_ten(dir, file), cpu_ms, false, Vec::new(), mem, &[]));
        }
    }
    if list {
        for (name, m, cpu_ms, fresh, _, _, _) in &all {
            let bytes = k10_ir::encode(m);
            println!("{:<11} {:>5} bytes  cpu_ms={:<5} fresh={}", name, bytes.len(), cpu_ms, fresh);
        }
        return;
    }

    let mut services = Vec::new();
    let mut grants: Vec<&[&str]> = Vec::new();
    let mut blobs: Vec<Vec<u8>> = Vec::new();
    let mut add = |name: String, d: &Svc| {
        let m = &d.1;
        k10_ir::validate(m).unwrap_or_else(|e| panic!("{}: invalid module: {e:?}", d.0));
        let bytes = k10_ir::encode(m);
        let hash = sha256(&bytes);
        if !blobs.iter().any(|b| sha256(b) == hash) {
            blobs.push(bytes);
        }
        services.push(ServiceSpec {
            name,
            code_hash: hash,
            replicas: 1,
            mem_log2: mem_log2.or(d.5).unwrap_or(m.min_mem_log2.max(16)).max(m.min_mem_log2),
            max_instances,
            cpu_ms: d.2,
            fresh: d.3,
            config: d.4.clone(),
            calls: Calls::none(),
        });
        grants.push(d.6);
    };
    if let Some((name, count)) = &replicate {
        let d = all.iter().find(|d| &d.0 == name).unwrap_or_else(|| usage());
        for i in 0..*count {
            add(format!("{name}-{i}"), d);
        }
    } else {
        for d in &all {
            if only.as_ref().is_some_and(|o| !o.iter().any(|n| *n == d.0)) {
                continue;
            }
            add(d.0.clone(), d);
        }
    }
    // Callees must be in the manifest: drop those left out by `--services` / `--replicate`.
    let names: Vec<String> = services.iter().map(|s| s.name.clone()).collect();
    for (s, wanted) in services.iter_mut().zip(grants) {
        let present: Vec<&str> = wanted.iter().copied().filter(|c| names.iter().any(|n| n == c)).collect();
        s.calls = Calls::from_list(&present).expect("valid callees");
    }
    services.sort_by(|a, b| a.name.cmp(&b.name));
    let manifest = Manifest { version, services, ..Default::default() };
    manifest.validate().unwrap_or_else(|e| panic!("manifest: {e}"));
    let n = manifest.services.len();
    let bundle = Bundle::new(manifest, blobs);
    let bytes = bundle.encode();
    if let Some(dir) = std::path::Path::new(&out).parent() {
        std::fs::create_dir_all(dir).ok();
    }
    std::fs::write(&out, &bytes).unwrap_or_else(|e| panic!("{out}: {e}"));
    eprintln!("mkbundle: wrote {out}: {} bytes, {n} service(s), {} blob(s), manifest v{version}", bytes.len(), bundle.blobs.len());
}
