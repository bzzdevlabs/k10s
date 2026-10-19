//! The ticker thread: time slicing.
//!
//! Every `tick` (1 ms) it looks at each shard's currently running invocation (`running_since`, set by the shard
//! around every coroutine resume). If it has been running longer than the time slice, the ticker stores
//! `usize::MAX` into the shard's published preemption word (`running_word`): the interpreter's safepoint hook (or,
//! later, the JIT's `stack_limit` check) then enters the runtime's interrupt hook, which accounts CPU time, enforces
//! the request's CPU budget and yields to other work.
//!
//! When no shard has run anything for 100 ticks the ticker parks; shards unpark it when they start running code
//! (a relaxed load of `ticker_parked` per resume, a `futex` wake only on the idle → busy transition).
//!
//! Safety: `running_word` points into a shard-owned fiber. Fibers are only freed when their shard exits, and the
//! node stops (joins) the ticker before telling shards to exit.

use crate::node::NodeShared;
use crate::sys::mono_ns;
use std::sync::atomic::Ordering::*;
use std::sync::Arc;

pub(crate) fn run(node: Arc<NodeShared>) {
    let slice = node.cfg.time_slice.as_nanos() as u64;
    let tick = node.cfg.tick;
    let mut idle_ticks = 0u32;
    while !node.stop.load(Acquire) {
        std::thread::sleep(tick);
        let now = mono_ns();
        let mut busy = false;
        for s in &node.shards {
            let since = s.running_since.load(Acquire);
            if since == 0 {
                continue;
            }
            busy = true;
            if now.saturating_sub(since) >= slice {
                let w = s.running_word.load(Acquire);
                if !w.is_null() {
                    // SAFETY: see module docs — the pointee (a fiber's preemption word) outlives the ticker.
                    unsafe { (*w).store(usize::MAX, Relaxed) };
                    s.stats.preempt_requests.fetch_add(1, Relaxed);
                }
            }
        }
        if busy {
            idle_ticks = 0;
            continue;
        }
        idle_ticks += 1;
        if idle_ticks >= 100 {
            node.ticker_parked.store(true, SeqCst);
            // Re-check after publishing `parked` (pairs with the shard's store of running_since + load of parked).
            let any = node.shards.iter().any(|s| s.running_since.load(SeqCst) != 0);
            if !any && !node.stop.load(SeqCst) {
                std::thread::park();
            }
            node.ticker_parked.store(false, SeqCst);
            idle_ticks = 0;
        }
    }
}
