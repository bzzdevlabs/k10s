//! K10S-DATA on the native kernel (metal/README.md, "Storage"): the partition of a GPT disk (by type or name),
//! grown to the end of its disk on first boot (`k10.grow=0` keeps it), holding the store of `k10_kvlog` — the one
//! Tier 1's node writes too, so a machine keeps its KV data when it changes tiers. The services' namespaces are
//! replayed into memory at boot. Afterwards CPU 0 commits their writes in groups: the first write opens a 2 ms
//! window, then the batch goes out asynchronously (its writes, then a flush; the device's completion interrupt, or
//! the idle loop's polling, ends it), the hosted node's durability contract. Compaction (a checkpoint) and the
//! recovery after a failed write run synchronously. The node's own state is in the same store, as Tier 1 keeps it:
//! files (`k10_kvlog::files`: the node id, the cluster's desired state and its code) and the update agent's
//! records (`.native`). Without K10S-DATA the KV data lives in memory only.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use k10_kvlog::store::{Disk, IoError, Store, Writes};
use k10_kvlog::{files, gpt, layout, record};

use crate::dev::{self, BlockDevice, BlockOp, BlockRequest, DmaRegion};
use crate::kprintln;
use crate::mem::frames::Tag;
use crate::sync::Global;
use crate::time;

/// Group commit window.
const WINDOW_NS: u64 = 2_000_000;
/// Logs smaller than this are not compacted.
const COMPACT_MIN: u64 = 1 << 20;
/// DMA bounce buffer: batches go out in rounds of this size.
const BOUNCE: usize = 1 << 20;
const RETRY_MIN_NS: u64 = 10_000_000;
const RETRY_MAX_NS: u64 = 5_000_000_000;
/// Tags of the asynchronous batch's requests (synchronous requests use others).
const TAG_BATCH: u32 = 0x5700_0000;
/// The kernel's own namespace in the store (osupdate.rs: failed images).
const NATIVE: &[u8] = b".native";

/// The node's own namespaces (names starting with a dot: [`NATIVE`] and [`files::NS`]), kept here.
type Own = BTreeMap<Vec<u8>, BTreeMap<Vec<u8>, Vec<u8>>>;

static STORAGE: Global<Option<Storage>> = Global::new(None);
/// For `/_k10s/status` (any CPU): the store's size and log, whether writes are durable, batches and errors.
pub static SIZE: AtomicU64 = AtomicU64::new(0);
pub static LOG_BYTES: AtomicU64 = AtomicU64::new(0);
pub static DURABLE: AtomicBool = AtomicBool::new(false);
pub static COMMITS: AtomicU64 = AtomicU64::new(0);
pub static CHECKPOINTS: AtomicU64 = AtomicU64::new(0);
pub static ERRORS: AtomicU64 = AtomicU64::new(0);

/// Synchronous access to a byte range of a disk (`base` blocks in) through the bounce buffer.
struct Part<'a> {
    dev: &'a mut dyn BlockDevice,
    bounce: &'a mut DmaRegion,
    base: u64,
    size: u64,
}

impl Part<'_> {
    fn bs(&self) -> u64 {
        self.dev.block_size() as u64
    }

    /// Requests of whole blocks over `[off, off + len)` (`off`, `len` block multiples), at most a bounce buffer
    /// and the device's largest request each: (lba, blocks, offset in the range).
    fn chunks(&self, off: u64, len: usize) -> impl Iterator<Item = (u64, u32, usize)> {
        let bs = self.bs();
        let max = (BOUNCE as u64).min(self.dev.max_blocks() as u64 * bs) as usize;
        let base = self.base;
        (0..len).step_by(max).map(move |at| (base + (off + at as u64) / bs, ((len - at).min(max) as u64 / bs) as u32, at))
    }

    fn sync(&mut self, op: BlockOp, lba: u64, blocks: u32) -> Result<(), IoError> {
        let r = BlockRequest { op, lba, blocks, paddr: self.bounce.paddr(), tag: 1 };
        dev::block_sync(self.dev, r).map_err(|e| {
            kprintln!("storage: {} {:?} at block {lba} (+{blocks}): {e:?}", self.dev.name(), op);
            IoError
        })
    }
}

impl Disk for Part<'_> {
    fn size(&self) -> u64 {
        self.size
    }
    fn read(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), IoError> {
        let chunks: Vec<_> = self.chunks(offset, buf.len()).collect();
        for (lba, blocks, at) in chunks {
            self.sync(BlockOp::Read, lba, blocks)?;
            let n = blocks as usize * self.bs() as usize;
            buf[at..at + n].copy_from_slice(&self.bounce.as_slice()[..n]);
        }
        Ok(())
    }
    fn write(&mut self, offset: u64, buf: &[u8]) -> Result<(), IoError> {
        let chunks: Vec<_> = self.chunks(offset, buf.len()).collect();
        for (lba, blocks, at) in chunks {
            let n = blocks as usize * self.bs() as usize;
            self.bounce.as_mut_slice()[..n].copy_from_slice(&buf[at..at + n]);
            self.sync(BlockOp::Write, lba, blocks)?;
        }
        Ok(())
    }
    fn flush(&mut self) -> Result<(), IoError> {
        self.sync(BlockOp::Flush, 0, 0)
    }
}

/// The asynchronous batch in flight.
struct Batch {
    writes: Writes,
    /// A superblock first (written and flushed before the rest).
    super_pending: bool,
    /// Requests of the current step: (lba, blocks, run index, offset in the run).
    reqs: Vec<(u64, u32, usize, usize)>,
    next: usize,
    in_flight: u32,
    flushing: bool,
    failed: bool,
}

pub struct Storage {
    dev: Box<dyn BlockDevice>,
    bounce: DmaRegion,
    base: u64,
    size: u64,
    store: Store,
    batch: Option<Batch>,
    failures: u32,
    retry_at: u64,
    /// The node's own namespaces, and their bytes in a checkpoint.
    own: Own,
    own_live: u64,
    /// A batch never completed (the device stopped answering): nothing more is written.
    dead: bool,
}

fn rec_size(k: usize, v: usize) -> u64 {
    record::size(k, v) as u64
}

/// Bytes of `own` in a checkpoint (an NS record per namespace, a PUT per entry).
fn own_bytes(own: &Own) -> u64 {
    own.iter().map(|(ns, m)| rec_size(ns.len(), 0) + m.iter().map(|(k, v)| rec_size(k.len(), v.len())).sum::<u64>()).sum()
}

/// `K10S-DATA` on `d`: (first block, blocks, partition number, unique GUID), from the primary GPT.
fn find(p: &mut Part) -> Option<(u64, u64, u32, [u8; 16])> {
    let bs = p.bs();
    let mut h = alloc::vec![0u8; bs as usize];
    p.read(bs, &mut h).ok()?;
    let hd = gpt::Header::parse(&h).ok()?;
    let mut a = alloc::vec![0u8; (hd.array_bytes() as u64).div_ceil(bs) as usize * bs as usize];
    p.read(hd.entries_lba * bs, &mut a).ok()?;
    let e = gpt::entries(&hd, &a).ok()?.into_iter().find(|e| e.is_data())?;
    Some((e.first_lba, e.blocks(), e.number, e.unique.0))
}

/// Grows partition `pno` to the end of the disk (backup GPT first, as Tier 1's init does); the new size in blocks.
fn grow(p: &mut Part, pno: u32) -> Result<Option<u64>, String> {
    let bs = p.bs();
    let total = p.size / bs;
    let io = |_| String::from("I/O error");
    let mut h = alloc::vec![0u8; bs as usize];
    p.read(bs, &mut h).map_err(io)?;
    let hd = gpt::Header::parse(&h).map_err(String::from)?;
    let mut a = alloc::vec![0u8; (hd.array_bytes() as u64).div_ceil(bs) as usize * bs as usize];
    p.read(hd.entries_lba * bs, &mut a).map_err(io)?;
    let Some(plan) = gpt::grow(&h, &a, bs, total, pno)? else { return Ok(None) };
    let block = |b: &[u8]| {
        let mut v = b.to_vec();
        v.resize((b.len() as u64).div_ceil(bs) as usize * bs as usize, 0);
        v
    };
    p.write(plan.backup_entries_lba * bs, &block(&plan.entries)).map_err(io)?;
    p.write(plan.backup_lba * bs, &block(&plan.backup)).map_err(io)?;
    p.flush().map_err(io)?;
    p.write(hd.entries_lba * bs, &block(&plan.entries)).map_err(io)?;
    p.write(bs, &block(&plan.primary)).map_err(io)?;
    let mut mbr = alloc::vec![0u8; bs as usize];
    p.read(0, &mut mbr).map_err(io)?;
    if let Some(m) = gpt::protective_mbr(&mbr, total) {
        mbr[..512].copy_from_slice(&m);
        p.write(0, &mbr).map_err(io)?;
    }
    p.flush().map_err(io)?;
    Ok(Some(plan.end - plan.start + 1))
}

/// Finds K10S-DATA on the disks (the partition k10boot read, `want`, if it is there), opens its store (formatting a
/// blank one) and replays the KV namespaces. `grow`: take the rest of the disk. `k10.data=none` (or a disk-less
/// machine) keeps KV in memory.
pub fn init(data: Option<&str>, allow_grow: bool, want: Option<[u8; 16]>) {
    if data == Some("none") {
        kprintln!("storage: k10.data=none: KV data in memory only");
        return;
    }
    let Some(mut bounce) = DmaRegion::alloc(BOUNCE, Tag::Kernel) else {
        kprintln!("storage: no memory for I/O buffers: KV data in memory only");
        return;
    };
    let mut disks = core::mem::take(&mut *dev::BLOCK.lock());
    // The disk with the partition k10boot read goes first.
    if let Some(w) = want {
        let pos = disks.iter_mut().position(|d| {
            let size = d.blocks() * d.block_size() as u64;
            !d.read_only() && find(&mut Part { dev: &mut **d, bounce: &mut bounce, base: 0, size }).is_some_and(|f| f.3 == w)
        });
        if let Some(i) = pos {
            let d = disks.remove(i);
            disks.insert(0, d);
        }
    }
    let mut found = None;
    let mut rest = Vec::new();
    for mut d in disks {
        if found.is_none() && !d.read_only() && (d.block_size() as u64) <= layout::UNIT {
            let size = d.blocks() * d.block_size() as u64;
            let mut p = Part { dev: &mut *d, bounce: &mut bounce, base: 0, size };
            if let Some((first, blocks, pno, _)) = find(&mut p) {
                let blocks = if allow_grow {
                    match grow(&mut p, pno) {
                        Ok(Some(b)) => {
                            kprintln!("storage: K10S-DATA grown to {} MiB (end of disk)", (b * p.bs()) >> 20);
                            b
                        }
                        Ok(None) => blocks,
                        Err(e) => {
                            kprintln!("storage: growing K10S-DATA: {e}");
                            blocks
                        }
                    }
                } else {
                    blocks
                };
                found = Some((d, first, blocks, pno));
                continue;
            }
        }
        rest.push(d);
    }
    dev::BLOCK.lock().extend(rest);
    let Some((mut d, first, blocks, pno)) = found else {
        kprintln!("storage: no K10S-DATA partition: KV data in memory only");
        return;
    };
    let bs = d.block_size() as u64;
    let name = d.name();
    if (first * bs) % layout::UNIT != 0 {
        kprintln!("storage: K10S-DATA ({name}, partition {pno}) is not 4 KiB aligned: KV data in memory only");
        return;
    }
    let mut p = Part { dev: &mut *d, bounce: &mut bounce, base: first, size: blocks * bs };
    let mut replayed = 0u64;
    let mut own = Own::new();
    let opened = Store::open(&mut p, &mut |ns, r| {
        if ns.first() == Some(&b'.') {
            let m = own.entry(ns.to_vec()).or_default();
            if r.op == record::PUT {
                m.insert(r.key.to_vec(), r.value.to_vec());
            } else {
                m.remove(r.key);
            }
        } else if let Ok(ns) = core::str::from_utf8(ns) {
            crate::runtime::kv_restore(ns, r.op == record::PUT, r.key, r.value);
            replayed += 1;
        }
    });
    let store = match opened {
        Ok(s) => s,
        Err(k10_kvlog::store::Error::Blank) => {
            let mut id = [0u8; 16];
            if !crate::arch::rdrand_bytes(&mut id) {
                let t = crate::arch::rdtsc().to_le_bytes();
                id[..8].copy_from_slice(&t);
            }
            kprintln!("storage: K10S-DATA ({name}, partition {pno}) is blank: formatting");
            match Store::format(&mut p, id) {
                Ok(s) => s,
                Err(e) => return kprintln!("storage: formatting K10S-DATA: {e}: KV data in memory only"),
            }
        }
        Err(e) => return kprintln!("storage: K10S-DATA ({name}, partition {pno}): {e}: KV data in memory only"),
    };
    let own_live = own_bytes(&own);
    let live = crate::runtime::kv_log_on(store.max_live().saturating_sub(own_live));
    SIZE.store(store.superblock().size(), Ordering::Relaxed);
    LOG_BYTES.store(store.log_bytes(), Ordering::Relaxed);
    DURABLE.store(true, Ordering::Relaxed);
    kprintln!(
        "storage: K10S-DATA on {name} (partition {pno}): {} MiB store, {replayed} records replayed, {} KiB live",
        store.superblock().size() >> 20,
        live >> 10
    );
    *STORAGE.borrow_mut() =
        Some(Storage { dev: d, bounce, base: first, size: blocks * bs, store, batch: None, failures: 0, retry_at: 0, own, own_live, dead: false });
}

impl Storage {
    /// Starts a batch of the pending writes (or a checkpoint, which completes here).
    fn start(&mut self, live: u64, pending: usize) {
        if self.store.wants_checkpoint(live + self.own_live, pending as u64, COMPACT_MIN) {
            self.checkpoint("compaction");
            return;
        }
        for (ns, recs) in crate::runtime::kv_take_pending() {
            if let Err(e) = self.store.push(ns.as_bytes(), &recs) {
                self.store.set_degraded();
                return self.failed("write", &e);
            }
        }
        let writes = self.store.take_writes();
        if writes.is_empty() {
            return;
        }
        let mut b = Batch { super_pending: writes.first.is_some(), writes, reqs: Vec::new(), next: 0, in_flight: 0, flushing: false, failed: false };
        self.plan(&mut b);
        self.batch = Some(b);
        self.advance();
    }

    /// The requests of the batch's current step.
    fn plan(&self, b: &mut Batch) {
        b.reqs.clear();
        b.next = 0;
        let bs = self.dev.block_size() as u64;
        let max = (BOUNCE as u64).min(self.dev.max_blocks() as u64 * bs) as usize;
        let mut add = |idx: usize, off: u64, len: usize| {
            for at in (0..len).step_by(max) {
                b.reqs.push((self.base + (off + at as u64) / bs, ((len - at).min(max) as u64 / bs) as u32, idx, at));
            }
        };
        if b.super_pending {
            let (off, unit) = b.writes.first.as_ref().unwrap();
            add(usize::MAX, *off, unit.len());
        } else {
            for (i, (off, buf)) in b.writes.runs.iter().enumerate() {
                add(i, *off, buf.len());
            }
        }
    }

    /// Submits what the batch can submit now; finishes it when done.
    fn advance(&mut self) {
        loop {
            let Some(b) = self.batch.as_mut() else { return };
            if b.failed {
                if b.in_flight == 0 {
                    self.batch = None;
                    self.store.set_degraded();
                    self.failed("write", &k10_kvlog::store::Error::Io);
                }
                return;
            }
            if b.in_flight > 0 {
                return;
            }
            if b.next < b.reqs.len() {
                // A round: as many requests as the bounce buffer holds.
                let bs = self.dev.block_size() as usize;
                let mut used = 0;
                while b.next < b.reqs.len() {
                    let (lba, blocks, idx, at) = b.reqs[b.next];
                    let n = blocks as usize * bs;
                    if used + n > BOUNCE {
                        break;
                    }
                    let src = if idx == usize::MAX { &b.writes.first.as_ref().unwrap().1 } else { &b.writes.runs[idx].1 };
                    self.bounce.as_mut_slice()[used..used + n].copy_from_slice(&src[at..at + n]);
                    let r = BlockRequest { op: BlockOp::Write, lba, blocks, paddr: self.bounce.paddr() + used as u64, tag: TAG_BATCH + b.next as u32 };
                    if self.dev.submit(0, r).is_err() {
                        b.failed = true;
                        break;
                    }
                    b.in_flight += 1;
                    used += n;
                    b.next += 1;
                }
                if b.failed && b.in_flight == 0 {
                    continue;
                }
                return;
            }
            if !b.flushing {
                b.flushing = true;
                if self.dev.submit(0, BlockRequest { op: BlockOp::Flush, lba: 0, blocks: 0, paddr: 0, tag: TAG_BATCH - 1 }).is_err() {
                    b.failed = true;
                    continue;
                }
                b.in_flight = 1;
                return;
            }
            // The step is written and flushed.
            if b.super_pending {
                self.store.first_done(&b.writes);
                b.super_pending = false;
                b.flushing = false;
                let mut b = self.batch.take().unwrap();
                self.plan(&mut b);
                self.batch = Some(b);
                continue;
            }
            let b = self.batch.take().unwrap();
            self.store.writes_done(&b.writes);
            COMMITS.fetch_add(1, Ordering::Relaxed);
            LOG_BYTES.store(self.store.log_bytes(), Ordering::Relaxed);
            return;
        }
    }

    /// Collects completions.
    fn reap(&mut self) {
        while let Some((_, res)) = self.dev.poll(0) {
            let Some(b) = self.batch.as_mut() else { continue };
            b.in_flight = b.in_flight.saturating_sub(1);
            if res.is_err() {
                b.failed = true;
            }
        }
        self.advance();
    }

    /// Waits for the batch in flight; false if the store is unusable (the device stopped answering).
    fn drain(&mut self) -> bool {
        let deadline = time::now_ns() + 30_000_000_000;
        while self.batch.is_some() && !self.dead {
            if time::now_ns() >= deadline {
                // Its requests may still be in flight, into the bounce buffer: nothing else may use the device.
                kprintln!("storage: {} does not complete a write in 30 s: K10S-DATA is off until reboot", self.dev.name());
                self.dead = true;
                ERRORS.fetch_add(1, Ordering::Relaxed);
                DURABLE.store(false, Ordering::Relaxed);
                break;
            }
            self.reap();
            core::hint::spin_loop();
        }
        !self.dead
    }

    /// Writes the whole KV state as a checkpoint (compaction, or the rewrite of a degraded store).
    fn checkpoint(&mut self, why: &str) {
        if !self.drain() {
            return;
        }
        let nss = crate::runtime::kv_namespaces();
        let res = (|| {
            let mut p = Part { dev: &mut *self.dev, bounce: &mut self.bounce, base: self.base, size: self.size };
            self.store.checkpoint_begin(&mut p)?;
            for (ns, m) in &self.own {
                let mut recs = Vec::new();
                for (k, v) in m {
                    record::encode(&mut recs, record::PUT, k, v);
                }
                if !recs.is_empty() {
                    let mut p = Part { dev: &mut *self.dev, bounce: &mut self.bounce, base: self.base, size: self.size };
                    self.store.checkpoint_push(&mut p, ns, &recs)?;
                }
            }
            for ns in &nss {
                let recs = crate::runtime::kv_snapshot(ns);
                if !recs.is_empty() {
                    let mut p = Part { dev: &mut *self.dev, bounce: &mut self.bounce, base: self.base, size: self.size };
                    self.store.checkpoint_push(&mut p, ns.as_bytes(), &recs)?;
                }
            }
            let mut p = Part { dev: &mut *self.dev, bounce: &mut self.bounce, base: self.base, size: self.size };
            self.store.checkpoint_end(&mut p)
        })();
        match res {
            Ok(()) => {
                CHECKPOINTS.fetch_add(1, Ordering::Relaxed);
                LOG_BYTES.store(self.store.log_bytes(), Ordering::Relaxed);
                if self.failures > 0 {
                    kprintln!("storage: K10S-DATA rewritten, KV writes are durable again");
                    self.failures = 0;
                    DURABLE.store(true, Ordering::Relaxed);
                }
            }
            Err(e) => {
                self.store.checkpoint_abort();
                self.failed(why, &e);
            }
        }
    }

    /// After a failed write, flush or checkpoint: not durable until a checkpoint succeeds (with backoff).
    fn failed(&mut self, what: &str, e: &k10_kvlog::store::Error) {
        ERRORS.fetch_add(1, Ordering::Relaxed);
        if self.failures == 0 {
            kprintln!("storage: {what} failed ({e}): KV writes are not durable until the store is rewritten");
            DURABLE.store(false, Ordering::Relaxed);
        }
        self.failures += 1;
        self.retry_at = time::now_ns() + (RETRY_MIN_NS << (self.failures - 1).min(20)).min(RETRY_MAX_NS);
        // The memory has the data; the rewrite writes it all.
        let _ = crate::runtime::kv_take_pending();
    }

    fn poll(&mut self, force: bool) {
        if self.dead {
            let _ = crate::runtime::kv_take_pending();
            return;
        }
        self.reap();
        if self.batch.is_some() {
            return;
        }
        let now = time::now_ns();
        if self.failures > 0 {
            if force || now >= self.retry_at {
                self.checkpoint("rewrite");
            } else {
                let _ = crate::runtime::kv_take_pending();
            }
            return;
        }
        let at = crate::runtime::KV_DIRTY_AT.load(Ordering::Acquire);
        if at == 0 || !(force || now >= at + WINDOW_NS) {
            return;
        }
        let (dirty_at, pending, live) = crate::runtime::kv_log_state();
        if dirty_at != 0 {
            self.start(live, pending);
        }
    }
}

impl Storage {
    /// Writes records (PUT/DEL) of own namespace `ns`, committed before it returns.
    fn write_own(&mut self, ns: &[u8], recs: &[u8]) -> Result<(), String> {
        if !self.drain() {
            return Err("K10S-DATA does not respond".into());
        }
        if self.failures > 0 {
            self.checkpoint("rewrite");
            if self.failures > 0 {
                return Err("K10S-DATA is not writable".into());
            }
        }
        // Room for the result next to the KV data, and for a checkpoint after the append.
        let mut after = self.own_live + if self.own.contains_key(ns) { 0 } else { rec_size(ns.len(), 0) };
        let m = self.own.get(ns);
        for (r, _) in record::split(recs, 0) {
            after = after.saturating_sub(m.and_then(|m| m.get(r.key)).map_or(0, |v| rec_size(r.key.len(), v.len())));
            if r.op == record::PUT {
                after += rec_size(r.key.len(), r.value.len());
            }
        }
        let (_, pending, kv_live) = crate::runtime::kv_log_state();
        if kv_live + after > self.store.max_live() {
            return Err("K10S-DATA is full".into());
        }
        if self.store.wants_checkpoint(kv_live + self.own_live, (pending + recs.len()) as u64, COMPACT_MIN) {
            self.checkpoint("compaction");
            if self.failures > 0 {
                return Err("K10S-DATA is not writable".into());
            }
        }
        let res = self.store.push(ns, recs).inspect_err(|_| self.store.set_degraded()).and_then(|()| {
            let mut p = Part { dev: &mut *self.dev, bounce: &mut self.bounce, base: self.base, size: self.size };
            self.store.commit(&mut p)
        });
        if let Err(e) = res {
            self.failed("write", &e);
            return Err(alloc::format!("K10S-DATA: {e}"));
        }
        let m = self.own.entry(ns.to_vec()).or_default();
        for (r, _) in record::split(recs, 0) {
            if r.op == record::PUT {
                m.insert(r.key.to_vec(), r.value.to_vec());
            } else {
                m.remove(r.key);
            }
        }
        self.own_live = own_bytes(&self.own);
        crate::runtime::kv_log_cap(self.store.max_live().saturating_sub(self.own_live));
        LOG_BYTES.store(self.store.log_bytes(), Ordering::Relaxed);
        Ok(())
    }

    fn own_get(&self, ns: &[u8], key: &[u8]) -> Option<&Vec<u8>> {
        self.own.get(ns)?.get(key)
    }
}

/// Whether K10S-DATA is open (KV is durable, the kernel slots usable).
pub fn available() -> bool {
    STORAGE.borrow().is_some()
}

/// The store's superblock (kernel slots).
pub fn superblock() -> Option<layout::Super> {
    STORAGE.borrow().as_ref().map(|s| *s.store.superblock())
}

/// A value of the kernel's own namespace.
pub fn native_get(key: &[u8]) -> Option<Vec<u8>> {
    STORAGE.borrow().as_ref()?.own_get(NATIVE, key).cloned()
}

/// Writes (or with `None` deletes) a value of the kernel's own namespace, durably.
pub fn native_set(key: &[u8], value: Option<&[u8]>) -> Result<(), String> {
    let mut rec = Vec::new();
    match value {
        Some(v) => record::encode(&mut rec, record::PUT, key, v),
        None => record::encode(&mut rec, record::DEL, key, &[]),
    }
    STORAGE.borrow_mut().as_mut().ok_or("no K10S-DATA")?.write_own(NATIVE, &rec)
}

/// File `name` of the node's files (Tier 1's names: `node-id`, `manifest.k10m`, `blobs/<sha256 hex>`).
pub fn file(name: &str) -> Option<Vec<u8>> {
    let st = STORAGE.borrow();
    let s = st.as_ref()?;
    files::read(name.as_bytes(), &mut |k| s.own_get(files::NS, k).cloned())
}

/// Names of the node's files.
pub fn file_names() -> Vec<String> {
    let st = STORAGE.borrow();
    let Some(m) = st.as_ref().and_then(|s| s.own.get(files::NS)) else { return Vec::new() };
    m.keys().filter(|k| files::is_header_key(k)).filter_map(|k| String::from_utf8(k.clone()).ok()).collect()
}

/// Replaces (`Some`) or removes (`None`) files, durably and in one batch.
pub fn write_files(changes: &[(&str, Option<&[u8]>)]) -> Result<(), String> {
    let mut st = STORAGE.borrow_mut();
    let s = st.as_mut().ok_or("no K10S-DATA")?;
    let mut recs = Vec::new();
    for (name, data) in changes {
        let old = s.own_get(files::NS, name.as_bytes()).and_then(|h| files::Header::decode(h));
        match (data, old) {
            (Some(d), old) => {
                files::write(&mut recs, name.as_bytes(), old, d);
            }
            (None, Some(old)) => files::delete(&mut recs, name.as_bytes(), old),
            (None, None) => {}
        }
    }
    if recs.is_empty() {
        return Ok(());
    }
    s.write_own(files::NS, &recs)
}

/// Writes a signed kernel image into slot `slot` and arms it as the trial (data flushed before the superblock).
pub fn install_slot(slot: u8, image: &[u8], sha256: [u8; 32]) -> Result<(), String> {
    let mut st = STORAGE.borrow_mut();
    let s = st.as_mut().ok_or("no K10S-DATA")?;
    if !s.drain() {
        return Err("K10S-DATA does not respond".into());
    }
    let mut p = Part { dev: &mut *s.dev, bounce: &mut s.bounce, base: s.base, size: s.size };
    s.store.install_slot(&mut p, slot, image, sha256).map_err(|e| alloc::format!("writing kernel slot {}: {e}", (b'A' + slot) as char))
}

/// Sets the committed and trial slots.
pub fn set_boot(committed: Option<u8>, trial: Option<u8>) -> Result<(), String> {
    let mut st = STORAGE.borrow_mut();
    let s = st.as_mut().ok_or("no K10S-DATA")?;
    if !s.drain() {
        return Err("K10S-DATA does not respond".into());
    }
    let mut p = Part { dev: &mut *s.dev, bounce: &mut s.bounce, base: s.base, size: s.size };
    s.store.set_boot(&mut p, committed, trial).map_err(|e| alloc::format!("K10S-DATA superblock: {e}"))
}

/// CPU 0's main loop: completions, and the next group commit when its window closed.
pub fn poll() {
    if let Some(s) = STORAGE.borrow_mut().as_mut() {
        s.poll(false);
    }
}

/// Whether storage needs the main loop soon: a batch in flight on a device without completion interrupts.
pub fn busy() -> bool {
    STORAGE.borrow().as_ref().is_some_and(|s| s.batch.is_some() && !s.dead && !s.dev.has_interrupts())
}

/// When the main loop must run storage next (the end of the group commit window, a retry, or polling a batch).
pub fn next_deadline_ns() -> Option<u64> {
    let st = STORAGE.borrow();
    let s = st.as_ref().filter(|s| !s.dead)?;
    if s.batch.is_some() {
        return Some(time::now_ns() + if s.dev.has_interrupts() { 10_000_000 } else { 50_000 });
    }
    if s.failures > 0 {
        return Some(s.retry_at);
    }
    let at = crate::runtime::KV_DIRTY_AT.load(Ordering::Acquire);
    (at != 0).then_some(at + WINDOW_NS)
}

/// Before power off or reboot: every write committed, then the device told (NVMe's normal shutdown).
pub fn shutdown() {
    let mut st = STORAGE.borrow_mut();
    let Some(s) = st.as_mut() else { return };
    if s.drain() {
        s.poll(true);
        if s.drain() {
            s.dev.shutdown();
        }
    }
}

/// `"storage":{…}` for `/_k10s/status`.
pub fn status_json(out: &mut String) {
    use core::fmt::Write;
    let l = |a: &AtomicU64| a.load(Ordering::Relaxed);
    if l(&SIZE) == 0 {
        out.push_str("\"storage\":null");
        return;
    }
    let _ = write!(
        out,
        "\"storage\":{{\"size\":{},\"log_bytes\":{},\"durable\":{},\"commits\":{},\"checkpoints\":{},\"errors\":{}}}",
        l(&SIZE),
        l(&LOG_BYTES),
        DURABLE.load(Ordering::Relaxed),
        l(&COMMITS),
        l(&CHECKPOINTS),
        l(&ERRORS)
    );
}
