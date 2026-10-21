//! Host ABI conformance: one sequence of host calls with expected results, run against each runtime's [`Env`]
//! (hosted node unit test, k10-metal selftest), so both stay identical where the ABI is defined.

use crate::*;
use alloc::vec;

/// The request and config the runtime builds its environment with (and an empty KV namespace).
pub const METHOD: i64 = 1;
pub const PATH: &[u8] = b"/a?b=c";
pub const BODY: &[u8] = b"hello world";
pub const CONFIG: &[u8] = b"cfg-bytes";

/// Instance memory size `S` of the run (room for an oversized `call` target).
const S: i64 = 1 << 17;

/// Reads back what host functions set.
pub trait Observe {
    fn status(&self) -> u16;
    fn response(&self) -> &[u8];
}

/// Runs every case on `env`; `Err` names the first mismatch.
pub fn run<E: Env + Observe>(env: &mut E) -> Result<(), String> {
    use HostFn::*;
    const OOB: Result<i64, Trap> = Err(Trap::MemOutOfBounds);
    let mut mem = vec![0u8; S as usize + TAIL_BYTES];
    let mut step = 0;
    let mut call = |env: &mut E, mem: &mut [u8], f: HostFn, a: &[i64], want: Result<i64, Trap>| {
        step += 1;
        let got = dispatch(env, f, a, mem);
        if got == want {
            Ok(())
        } else {
            Err(format!("step {step}: {f:?}{a:?} = {got:?}, want {want:?}"))
        }
    };
    let expect = |ok: bool, what: &str| if ok { Ok(()) } else { Err(String::from(what)) };

    call(env, &mut mem, InputLen, &[], Ok(11))?;
    call(env, &mut mem, InputRead, &[100, 6, 100], Ok(5))?;
    expect(&mem[100..105] == b"world", "input_read copies")?;
    call(env, &mut mem, InputRead, &[100, 11, 5], Ok(0))?;
    call(env, &mut mem, InputRead, &[100, -1, 5], Ok(0))?;
    call(env, &mut mem, InputRead, &[S + 10, 0, 2], Ok(2))?;
    expect(&mem[10..12] == b"he", "masked destination")?;
    // The whole declared range is checked, even when fewer bytes would be copied.
    call(env, &mut mem, InputRead, &[S - 4, 8, 100], OOB)?;
    call(env, &mut mem, InputRead, &[0, 0, -1], OOB)?;
    call(env, &mut mem, ReqPath, &[200, 3], Ok(6))?;
    expect(&mem[200..203] == b"/a?", "req_path copies")?;
    call(env, &mut mem, ReqPath, &[S - 10, 100], OOB)?;
    call(env, &mut mem, ReqPath, &[0, -1], OOB)?;
    call(env, &mut mem, ReqMethod, &[], Ok(METHOD))?;
    call(env, &mut mem, Config, &[300, 100], Ok(9))?;
    expect(&mem[300..309] == CONFIG, "config copies")?;
    call(env, &mut mem, Config, &[S - 20, 100], OOB)?;

    mem[0..3].copy_from_slice(b"abc");
    call(env, &mut mem, OutputWrite, &[0, 3], Ok(3))?;
    call(env, &mut mem, OutputWrite, &[S - 2, 4], OOB)?;
    expect(env.response() == b"abc", "output_write appends")?;
    for (code, want) in [(404, 404), (700, 500), (99, 500), (599, 599)] {
        call(env, &mut mem, OutputStatus, &[code], Ok(0))?;
        expect(env.status() == want, "output_status clamps to 100..=599")?;
    }

    call(env, &mut mem, CallRespLen, &[], Ok(0))?;
    call(env, &mut mem, CallRespRead, &[0, 0, 10], Ok(0))?;
    call(env, &mut mem, CallRespRead, &[S - 2, 0, 10], OOB)?;
    call(env, &mut mem, Call, &[0, MAX_CALL_TARGET as i64 + 1, 0, 0], Ok(CALL_UNAVAILABLE))?;
    call(env, &mut mem, Call, &[0, 4, S - 2, 4], OOB)?;

    mem[0..3].copy_from_slice(b"key");
    mem[10..15].copy_from_slice(b"value");
    call(env, &mut mem, KvGet, &[0, 3, S - 2, 10], OOB)?;
    call(env, &mut mem, KvGet, &[0, 3, 100, 10], Ok(-1))?;
    call(env, &mut mem, KvPut, &[0, 3, 10, 5], Ok(0))?;
    mem[100..110].fill(0);
    call(env, &mut mem, KvGet, &[0, 3, 100, 2], Ok(5))?;
    expect(&mem[100..103] == b"va\0", "kv_get copies at most max")?;
    call(env, &mut mem, KvGet, &[0, 3, 0, 10], Ok(5))?;
    expect(&mem[0..5] == b"value", "kv_get key and destination overlap")?;
    mem[0..3].copy_from_slice(b"key");
    call(env, &mut mem, KvPut, &[0, MAX_KEY as i64 + 1, 10, 5], Err(Trap::Host(TRAP_KV_LIMIT)))?;
    call(env, &mut mem, KvGet, &[0, MAX_KEY as i64 + 1, 10, 5], Err(Trap::Host(TRAP_KV_LIMIT)))?;
    call(env, &mut mem, KvPut, &[0, 3, S - 2, 5], OOB)?;
    call(env, &mut mem, KvDel, &[0, 3], Ok(1))?;
    call(env, &mut mem, KvDel, &[0, 3], Ok(0))?;

    mem[0] = b'n';
    call(env, &mut mem, KvAdd, &[0, 1, 0], Ok(0))?;
    call(env, &mut mem, KvGet, &[0, 1, 100, 8], Ok(-1))?;
    call(env, &mut mem, KvAdd, &[0, 1, 41], Ok(41))?;
    call(env, &mut mem, KvAdd, &[0, 1, 1], Ok(42))?;
    call(env, &mut mem, KvGet, &[0, 1, 100, 8], Ok(8))?;
    expect(mem[100..108] == 42i64.to_le_bytes(), "kv_add stores 8 bytes LE")?;
    mem[10..13].copy_from_slice(b"txt");
    call(env, &mut mem, KvPut, &[10, 3, 10, 3], Ok(0))?;
    call(env, &mut mem, KvAdd, &[10, 3, 1], Err(Trap::Host(TRAP_KV_NOT_INT)))?;
    call(env, &mut mem, KvAdd, &[S - 2, 10, 1], OOB)?;

    mem[20..22].copy_from_slice(b"k1");
    mem[200..205].copy_from_slice(b"aabbb");
    call(env, &mut mem, KvCas, &[20, 2, 200, -1, 2], Ok(1))?;
    call(env, &mut mem, KvCas, &[20, 2, 200, -1, 2], Ok(0))?;
    call(env, &mut mem, KvCas, &[20, 2, 200, 2, 3], Ok(1))?;
    call(env, &mut mem, KvGet, &[20, 2, 300, 8], Ok(3))?;
    expect(&mem[300..303] == b"bbb", "kv_cas stores the new value")?;
    call(env, &mut mem, KvCas, &[20, 2, 200, 2, 3], Ok(0))?;
    call(env, &mut mem, KvCas, &[20, 2, 202, 3, -1], Ok(1))?;
    call(env, &mut mem, KvGet, &[20, 2, 300, 8], Ok(-1))?;
    call(env, &mut mem, KvCas, &[20, 2, 0, -1, -1], Ok(1))?;
    call(env, &mut mem, KvCas, &[20, 2, 200, -2, 1], OOB)?;
    call(env, &mut mem, KvCas, &[20, 2, S - 100, 50, 51], OOB)?;
    call(env, &mut mem, KvCas, &[20, 2, 0, i64::MAX, i64::MAX], OOB)?;
    call(env, &mut mem, KvCas, &[0, MAX_KEY as i64 + 1, 200, -1, 1], Err(Trap::Host(TRAP_KV_LIMIT)))?;

    mem[0..4].copy_from_slice(b"line");
    call(env, &mut mem, Log, &[0, 4], Ok(0))?;
    call(env, &mut mem, Log, &[S - 2, 4], OOB)?;
    let t0 = dispatch(env, NowNs, &[], &mut mem);
    let t1 = dispatch(env, NowNs, &[], &mut mem);
    expect(matches!((t0, t1), (Ok(a), Ok(b)) if b >= a), "now_ns is monotonic")?;
    expect(matches!(dispatch(env, TimeMs, &[], &mut mem), Ok(ms) if ms > 1_600_000_000_000), "time_ms is Unix ms")?;
    expect(dispatch(env, Random, &[], &mut mem).is_ok(), "random")?;
    Ok(())
}
