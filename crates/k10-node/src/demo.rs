//! Hand-built TIR service modules (via `k10_ir::builder`) for tests, examples and benchmarks — until the Ten
//! compiler exists. Every module imports the whole host API (unused imports are harmless).
//!
//! Memory layout used by these modules: `[64..)` constant strings (data segments), `[512..540)` decimal scratch,
//! `[1024..1280)` path / name buffer, `[2048..2056)` KV value scratch, `[4096..)` I/O buffer.

use k10_ir::{FuncBuilder, Module, ModuleBuilder};

const DEC_END: i64 = 540;
const PATH_BUF: i64 = 1024;
const IO_BUF: i64 = 4096;

/// Function indices of the host imports (declared in docs/HOSTAPI.md order).
#[derive(Clone, Copy)]
pub struct Imp {
    pub input_len: u32,
    pub input_read: u32,
    pub req_path: u32,
    pub req_method: u32,
    pub output_write: u32,
    pub output_status: u32,
    pub call: u32,
    pub call_resp_len: u32,
    pub call_resp_read: u32,
    pub kv_get: u32,
    pub kv_put: u32,
    pub kv_del: u32,
    pub log: u32,
    pub now_ns: u32,
    pub time_ms: u32,
    pub random: u32,
    pub config: u32,
}

/// Optional `init()` body.
pub type InitBody<'a> = Option<&'a dyn Fn(&mut FuncBuilder, &Imp)>;

/// A module builder with all host imports declared, plus helper functions.
pub struct Mb {
    pub mb: ModuleBuilder,
    pub imp: Imp,
    /// `write_dec(v)`: appends the decimal form of `v` to the response.
    pub write_dec: u32,
    /// `parse_dec(ptr, len) -> i64`: parses leading ASCII digits.
    pub parse_dec: u32,
}

impl Mb {
    pub fn new(mem_log2: u8) -> Mb {
        let mut mb = ModuleBuilder::new();
        mb.mem_log2(mem_log2);
        let mut i = |n: &str, p: u8| mb.import(n, p, 1);
        let imp = Imp {
            input_len: i("input_len", 0),
            input_read: i("input_read", 3),
            req_path: i("req_path", 2),
            req_method: i("req_method", 0),
            output_write: i("output_write", 2),
            output_status: i("output_status", 1),
            call: i("call", 4),
            call_resp_len: i("call_resp_len", 0),
            call_resp_read: i("call_resp_read", 3),
            kv_get: i("kv_get", 4),
            kv_put: i("kv_put", 4),
            kv_del: i("kv_del", 2),
            log: i("log", 2),
            now_ns: i("now_ns", 0),
            time_ms: i("time_ms", 0),
            random: i("random", 0),
            config: i("config", 2),
        };
        let ow = imp.output_write;
        // write_dec(v): locals p=1 neg=2 x=3
        let write_dec = mb.func(1, 0, |f| {
            f.locals(3);
            f.local_get(0).const_(0).lt_s().local_set(2);
            f.local_get(2).if_().const_(0).local_get(0).sub().local_set(3).else_().local_get(0).local_set(3).end();
            f.const_(DEC_END).local_set(1);
            f.block().loop_();
            f.local_get(1).const_(1).sub().local_set(1);
            f.local_get(1).local_get(3).const_(10).rem_u().const_(48).add().store8(0);
            f.local_get(3).const_(10).div_u().local_tee(3).const_(0).ne().br_if(0);
            f.end().end();
            f.local_get(2).if_().local_get(1).const_(1).sub().local_tee(1).const_(45).store8(0).end();
            f.local_get(1).const_(DEC_END).local_get(1).sub().call(ow).drop();
        });
        // parse_dec(ptr, len) -> i64: locals i=2 acc=3 c=4
        let parse_dec = mb.func(2, 1, |f| {
            f.locals(3);
            f.block().loop_();
            f.local_get(2).local_get(1).ge_u().br_if(1);
            f.local_get(0).local_get(2).add().load8_u(0).const_(48).sub().local_tee(4).const_(9).gt_u().br_if(1);
            f.local_get(3).const_(10).mul().local_get(4).add().local_set(3);
            f.local_get(2).const_(1).add().local_set(2);
            f.br(0);
            f.end().end();
            f.local_get(3);
        });
        Mb { mb, imp, write_dec, parse_dec }
    }

    /// Adds `handle` (and optionally `init`) and builds the module.
    pub fn finish(mut self, handle: impl FnOnce(&mut FuncBuilder, &Imp, u32, u32), init: InitBody) -> Module {
        let (imp, wd, pd) = (self.imp, self.write_dec, self.parse_dec);
        let h = self.mb.func(0, 0, |f| handle(f, &imp, wd, pd));
        self.mb.export("handle", h);
        if let Some(init) = init {
            let i = self.mb.func(0, 0, |f| init(f, &imp));
            self.mb.export("init", i);
        }
        self.mb.into_module()
    }
}

/// Emits `output_write(ptr, len)` of a constant string stored in a data segment at `at`.
fn emit_const(mb: &mut ModuleBuilder, at: u64, s: &[u8]) {
    mb.data(at, s);
}

/// `echo`: GET (empty body) → `hello\n`; otherwise echoes the request body (truncated to `S - 4096`).
pub fn echo() -> Module {
    let mut m = Mb::new(16);
    emit_const(&mut m.mb, 64, b"hello\n");
    m.finish(
        |f, i, _, _| {
            f.locals(1);
            f.call(i.input_len).local_tee(0).eqz().if_();
            f.const_(64).const_(6).call(i.output_write).drop();
            f.else_();
            // n = min(n, mem.size - 4096)
            f.local_get(0).mem_size().const_(IO_BUF).sub().local_get(0).mem_size().const_(IO_BUF).sub().lt_u().select().local_set(0);
            f.const_(IO_BUF).const_(0).local_get(0).call(i.input_read).drop();
            f.const_(IO_BUF).local_get(0).call(i.output_write).drop();
            f.end();
        },
        None,
    )
}

/// `empty`: does nothing (status 200, empty body). Measures invocation overhead.
pub fn empty() -> Module {
    Mb::new(12).finish(|_, _, _, _| {}, None)
}

/// `path`: responds with `method:<code> path:<req_path>` (path truncated to 256 bytes).
pub fn path_echo() -> Module {
    let mut m = Mb::new(16);
    emit_const(&mut m.mb, 64, b"method:");
    emit_const(&mut m.mb, 80, b" path:");
    m.finish(
        |f, i, wd, _| {
            f.locals(1);
            f.const_(64).const_(7).call(i.output_write).drop();
            f.call(i.req_method).call(wd);
            f.const_(80).const_(6).call(i.output_write).drop();
            f.const_(PATH_BUF).const_(256).call(i.req_path).local_set(0);
            f.local_get(0).const_(256).local_get(0).const_(256).lt_s().select().local_set(0);
            f.const_(PATH_BUF).local_get(0).call(i.output_write).drop();
        },
        None,
    )
}

/// `caller`: calls `target` + the request path suffix with the request body. On success responds with the callee's
/// status and body; on a call error responds 502 with `call error <code>`.
pub fn caller(target: &str) -> Module {
    let mut m = Mb::new(16);
    let t = target.as_bytes();
    emit_const(&mut m.mb, 1024, t); // name at PATH_BUF, path suffix appended after it
    emit_const(&mut m.mb, 96, b"call error ");
    let tl = t.len() as i64;
    m.finish(
        move |f, i, wd, _| {
            f.locals(3); // 0 = st, 1 = plen, 2 = n
            // path suffix after the name
            f.const_(PATH_BUF + tl).const_(256).call(i.req_path).local_set(1);
            f.local_get(1).const_(256).local_get(1).const_(256).lt_s().select().local_set(1);
            f.call(i.input_len).local_set(2);
            f.const_(IO_BUF).const_(0).local_get(2).call(i.input_read).drop();
            f.const_(PATH_BUF).const_(tl).local_get(1).add().const_(IO_BUF).local_get(2).call(i.call).local_tee(0);
            f.const_(0).lt_s().if_();
            f.const_(502).call(i.output_status).drop();
            f.const_(96).const_(11).call(i.output_write).drop();
            f.local_get(0).call(wd);
            f.else_();
            f.local_get(0).call(i.output_status).drop();
            f.call(i.call_resp_len).local_set(2);
            f.const_(IO_BUF).const_(0).local_get(2).call(i.call_resp_read).drop();
            f.const_(IO_BUF).local_get(2).call(i.output_write).drop();
            f.end();
        },
        None,
    )
}

/// `call_loop`: calls `target` (empty body) N times, N = decimal request body; responds with the number of calls
/// that returned a status ≥ 0.
pub fn call_loop(target: &str) -> Module {
    let mut m = Mb::new(16);
    let t = target.as_bytes();
    emit_const(&mut m.mb, 1024, t);
    let tl = t.len() as i64;
    m.finish(
        move |f, i, wd, pd| {
            f.locals(3); // 0 = n, 1 = i, 2 = ok
            f.call(i.input_len).local_set(0);
            f.const_(IO_BUF).const_(0).local_get(0).call(i.input_read).drop();
            f.const_(IO_BUF).local_get(0).call(pd).local_set(0);
            f.block().loop_();
            f.local_get(1).local_get(0).ge_s().br_if(1);
            f.const_(PATH_BUF).const_(tl).const_(IO_BUF).const_(0).call(i.call).const_(0).ge_s();
            f.local_get(2).add().local_set(2);
            f.local_get(1).const_(1).add().local_set(1);
            f.br(0);
            f.end().end();
            f.local_get(2).call(wd);
        },
        None,
    )
}

/// `call_probe`: `/<probe>/<target>` with body `N` (default 1) calls `<target>` N times and answers `<status>
/// <call_resp_len> <ns>` (last call; all N): what a workload observes of a call. The path must not be empty.
pub fn call_probe() -> Module {
    let mut m = Mb::new(16);
    emit_const(&mut m.mb, 64, b" ");
    m.finish(
        |f, i, wd, pd| {
            f.locals(5); // 0 = target len, 1 = status, 2 = n, 3 = k, 4 = t0 / ns
            f.const_(PATH_BUF).const_(256).call(i.req_path).local_set(0);
            f.local_get(0).const_(256).local_get(0).const_(256).lt_s().select().local_set(0);
            f.local_get(0).const_(1).sub().local_set(0); // without the leading '/'
            f.call(i.input_len).local_set(2);
            f.local_get(2).const_(20).local_get(2).const_(20).lt_s().select().local_set(2);
            f.const_(IO_BUF).const_(0).local_get(2).call(i.input_read).drop();
            f.const_(IO_BUF).local_get(2).call(pd).local_set(2);
            f.local_get(2).eqz().if_().const_(1).local_set(2).end();
            f.call(i.now_ns).local_set(4);
            f.block().loop_();
            f.local_get(3).local_get(2).ge_s().br_if(1);
            f.const_(PATH_BUF + 1).local_get(0).const_(IO_BUF).const_(0).call(i.call).local_set(1);
            f.local_get(3).const_(1).add().local_set(3);
            f.br(0);
            f.end().end();
            f.call(i.now_ns).local_get(4).sub().local_set(4);
            f.local_get(1).call(wd);
            f.const_(64).const_(1).call(i.output_write).drop();
            f.call(i.call_resp_len).call(wd);
            f.const_(64).const_(1).call(i.output_write).drop();
            f.local_get(4).call(wd);
        },
        None,
    )
}

/// `kv`: `GET /<key>` → value (404 if absent), `POST|PUT /<key>` body → stores, `DELETE /<key>` → 200/404.
pub fn kv() -> Module {
    let m = Mb::new(16);
    m.finish(
        |f, i, _, _| {
            f.locals(3); // 0 = klen, 1 = method, 2 = n
            // key = path without its leading '/'
            f.const_(PATH_BUF).const_(256).call(i.req_path).local_set(0);
            f.local_get(0).const_(256).gt_s().if_().const_(400).call(i.output_status).drop().return_().end();
            f.local_get(0).eqz().if_().const_(400).call(i.output_status).drop().return_().end();
            f.call(i.req_method).local_set(1);
            f.local_get(1).eqz().if_();
            f.const_(PATH_BUF + 1).local_get(0).const_(1).sub().const_(IO_BUF).mem_size().const_(IO_BUF).sub().call(i.kv_get).local_tee(2);
            f.const_(0).lt_s().if_().const_(404).call(i.output_status).drop().else_();
            f.const_(IO_BUF).local_get(2).call(i.output_write).drop();
            f.end();
            f.return_();
            f.end();
            f.local_get(1).const_(3).eq().if_();
            f.const_(PATH_BUF + 1).local_get(0).const_(1).sub().call(i.kv_del).eqz().if_().const_(404).call(i.output_status).drop().end();
            f.return_();
            f.end();
            f.call(i.input_len).local_set(2);
            f.const_(IO_BUF).const_(0).local_get(2).call(i.input_read).drop();
            f.const_(PATH_BUF + 1).local_get(0).const_(1).sub().const_(IO_BUF).local_get(2).call(i.kv_put).drop();
        },
        None,
    )
}

/// `counter`: increments the 8-byte KV value `count` and responds with the new value in decimal.
pub fn counter() -> Module {
    let mut m = Mb::new(16);
    emit_const(&mut m.mb, 64, b"count");
    m.finish(
        |f, i, wd, _| {
            f.const_(2048).const_(0).store64(0);
            f.const_(64).const_(5).const_(2048).const_(8).call(i.kv_get).drop();
            f.const_(2048).const_(2048).load64(0).const_(1).add().store64(0);
            f.const_(64).const_(5).const_(2048).const_(8).call(i.kv_put).drop();
            f.const_(2048).load64(0).call(wd);
        },
        None,
    )
}

/// `memcount`: keeps a counter in instance memory; `/trap` traps (`unreachable`); otherwise responds with the
/// incremented counter. With `init`, the counter starts at 1000 (set by `init()`).
pub fn memcount(with_init: bool) -> Module {
    let m = Mb::new(16);
    let init_fn = |f: &mut FuncBuilder, _: &Imp| {
        f.const_(2048).const_(1000).store64(0);
    };
    let init: InitBody = if with_init { Some(&init_fn) } else { None };
    m.finish(
        |f, i, wd, _| {
            f.locals(1);
            f.const_(PATH_BUF).const_(16).call(i.req_path).local_set(0);
            // "/trap" = 2f 74 72 61 70
            f.local_get(0).const_(5).eq().if_();
            f.const_(PATH_BUF).load32_u(0).const_(0x6172_742f).eq().if_();
            f.const_(PATH_BUF + 4).load8_u(0).const_(0x70).eq().if_().unreachable().end();
            f.end();
            f.end();
            f.const_(2048).const_(2048).load64(0).const_(1).add().store64(0);
            f.const_(2048).load64(0).call(wd);
        },
        init,
    )
}

/// `spin`: loops forever (hits the CPU budget).
pub fn spin() -> Module {
    Mb::new(12).finish(
        |f, _, _, _| {
            f.locals(1);
            f.loop_().local_get(0).const_(1).add().local_set(0).br(0).end();
        },
        None,
    )
}

/// `burn`: busy-waits for N ms (N = decimal config, else the decimal request body, else 50) then responds `done`.
pub fn burn() -> Module {
    let mut m = Mb::new(16);
    emit_const(&mut m.mb, 64, b"done");
    m.finish(
        |f, i, _, pd| {
            f.locals(3); // 0 = ms, 1 = start, 2 = n
            f.const_(IO_BUF).const_(64).call(i.config).local_set(2);
            f.const_(IO_BUF).local_get(2).const_(64).local_get(2).const_(64).lt_s().select().call(pd).local_set(0);
            f.local_get(0).eqz().if_();
            f.call(i.input_len).local_set(2);
            f.const_(IO_BUF).const_(0).local_get(2).call(i.input_read).drop();
            f.const_(IO_BUF).local_get(2).call(pd).local_set(0);
            f.end();
            f.local_get(0).eqz().if_().const_(50).local_set(0).end();
            f.call(i.now_ns).local_set(1);
            f.block().loop_();
            f.call(i.now_ns).local_get(1).sub().local_get(0).const_(1_000_000).mul().ge_s().br_if(1);
            f.br(0);
            f.end().end();
            f.const_(64).const_(4).call(i.output_write).drop();
        },
        None,
    )
}

/// `dense`: `init()` writes `touch_kib` KiB (one byte per 64 bytes, so every page is dirtied); `handle` responds
/// with the sum of a few touched bytes.
pub fn dense(touch_kib: i64) -> Module {
    let m = Mb::new(16);
    let init = move |f: &mut FuncBuilder, _: &Imp| {
        f.locals(1);
        f.const_(IO_BUF).local_set(0);
        f.block().loop_();
        f.local_get(0).const_(IO_BUF + touch_kib * 1024).ge_u().br_if(1);
        f.local_get(0).local_get(0).store8(0);
        f.local_get(0).const_(64).add().local_set(0);
        f.br(0);
        f.end().end();
    };
    m.finish(
        |f, _, wd, _| {
            f.const_(IO_BUF + 64).load8_u(0).const_(IO_BUF + 128).load8_u(0).add().call(wd);
        },
        Some(&init),
    )
}

/// `logger`: logs the request body and responds `logged`.
pub fn logger() -> Module {
    let mut m = Mb::new(16);
    emit_const(&mut m.mb, 64, b"logged");
    m.finish(
        |f, i, _, _| {
            f.locals(1);
            f.call(i.input_len).local_set(0);
            f.const_(IO_BUF).const_(0).local_get(0).call(i.input_read).drop();
            f.const_(IO_BUF).local_get(0).call(i.log).drop();
            f.const_(64).const_(6).call(i.output_write).drop();
        },
        None,
    )
}

/// `config`: responds with the service's config bytes.
pub fn config_echo() -> Module {
    Mb::new(16).finish(
        |f, i, _, _| {
            f.locals(1);
            f.const_(IO_BUF).const_(1024).call(i.config).local_set(0);
            f.const_(IO_BUF).local_get(0).call(i.output_write).drop();
        },
        None,
    )
}

/// `status`: responds with status = the decimal request body.
pub fn status_echo() -> Module {
    Mb::new(16).finish(
        |f, i, _, pd| {
            f.locals(1);
            f.call(i.input_len).local_set(0);
            f.const_(IO_BUF).const_(0).local_get(0).call(i.input_read).drop();
            f.const_(IO_BUF).local_get(0).call(pd).call(i.output_status).drop();
        },
        None,
    )
}

/// `badread`: reads the body into an out-of-range buffer (traps `MemOutOfBounds`).
pub fn bad_read() -> Module {
    Mb::new(12).finish(
        |f, i, _, _| {
            f.const_(4000).const_(0).const_(200).call(i.input_read).drop();
        },
        None,
    )
}

/// `deep`: recursion of depth `body as decimal` (stack overflow beyond the interpreter budget).
pub fn deep() -> Module {
    let mut m = Mb::new(16);
    let rec = m.mb.declare(1, 1);
    m.mb.define_with(rec, |f| {
        f.local_get(0).eqz().if_().const_(0).return_().end();
        f.local_get(0).const_(1).sub().call(rec).const_(1).add();
    });
    m.finish(
        move |f, i, wd, pd| {
            f.locals(1);
            f.call(i.input_len).local_set(0);
            f.const_(IO_BUF).const_(0).local_get(0).call(i.input_read).drop();
            f.const_(IO_BUF).local_get(0).call(pd).call(rec).call(wd);
        },
        None,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_valid() {
        for (name, m) in [
            ("echo", echo()),
            ("empty", empty()),
            ("path", path_echo()),
            ("caller", caller("echo")),
            ("call_loop", call_loop("empty")),
            ("call_probe", call_probe()),
            ("kv", kv()),
            ("counter", counter()),
            ("memcount", memcount(true)),
            ("spin", spin()),
            ("burn", burn()),
            ("dense", dense(4)),
            ("logger", logger()),
            ("config", config_echo()),
            ("status", status_echo()),
            ("badread", bad_read()),
            ("deep", deep()),
        ] {
            k10_ir::validate(&m).unwrap_or_else(|e| panic!("{name}: {e}"));
            let bytes = k10_ir::encode(&m);
            assert_eq!(k10_ir::decode(&bytes).unwrap(), m, "{name}");
        }
    }
}
