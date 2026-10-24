//! virtio-blk on any virtio transport, behind [`BlockDevice`]: one request queue, polled completions.
//!
//! A request is a three-descriptor chain — header (type, sector), data, status byte — at fixed descriptors
//! `3k, 3k+1, 3k+2` for slot `k`, so nothing is allocated per request; headers and status bytes live in one DMA
//! region. Sectors are always 512 bytes on the wire; the logical block size comes from BLK_SIZE. Without the FLUSH
//! feature the device has no volatile write cache, so a flush completes at once.

use alloc::vec::Vec;

use super::{Desc, Queue, Transport, DESC_F_NEXT, DESC_F_WRITE};
use crate::dev::{BlockDevice, BlockError, BlockOp, BlockRequest, DmaRegion};
use crate::kprintln;
use crate::mem::frames::Tag;

const F_SIZE_MAX: u64 = 1 << 1;
const F_SEG_MAX: u64 = 1 << 2;
const F_RO: u64 = 1 << 5;
const F_BLK_SIZE: u64 = 1 << 6;
const F_FLUSH: u64 = 1 << 9;

const T_IN: u32 = 0;
const T_OUT: u32 = 1;
const T_FLUSH: u32 = 4;

const S_OK: u8 = 0;
const S_UNSUPP: u8 = 2;

/// Header (16 bytes) and status byte of one slot.
const SLOT_BYTES: usize = 32;
/// Requests in flight at most.
const QUEUE_SIZE: u16 = 96;

pub struct VirtioBlk {
    /// Kept for the device's lifetime (its registers).
    _t: Transport,
    name: &'static str,
    q: Queue,
    slots: DmaRegion,
    free: Vec<u16>,
    tags: Vec<u32>,
    block_size: u32,
    blocks: u64,
    read_only: bool,
    flush: bool,
    max_bytes: u64,
    /// Completions decided without the device.
    done: Vec<(u32, Result<(), BlockError>)>,
}

impl VirtioBlk {
    pub fn probe_pci(d: &crate::pci::Device) -> Option<VirtioBlk> {
        let t = super::pci::transport(d)?;
        let name = if t.modern() { "virtio-blk (pci)" } else { "virtio-blk (pci legacy)" };
        Self::init(t, name)
    }

    pub fn probe_mmio(m: super::mmio::Mmio) -> Option<VirtioBlk> {
        let t = Transport::Mmio(m);
        let name = if t.modern() { "virtio-blk (mmio)" } else { "virtio-blk (mmio legacy)" };
        Self::init(t, name)
    }

    fn init(t: Transport, name: &'static str) -> Option<VirtioBlk> {
        let f = t.negotiate(F_SIZE_MAX | F_SEG_MAX | F_RO | F_BLK_SIZE | F_FLUSH)?;
        let Some(q) = Queue::new(&t, 0, QUEUE_SIZE, None, Tag::Kernel) else {
            kprintln!("{name}: request queue unavailable");
            t.set_status(0);
            return None;
        };
        let nslots = q.size / 3;
        let Some(slots) = DmaRegion::alloc(nslots as usize * SLOT_BYTES, Tag::Kernel).filter(|_| nslots > 0) else {
            kprintln!("{name}: no memory for the request slots (or a queue under 3 entries)");
            t.set_status(0);
            return None;
        };
        q.set_no_interrupt(true);
        t.ready();
        let sectors = t.config::<8>(0);
        let block_size = if f & F_BLK_SIZE != 0 { (t.config::<4>(20) as u32).clamp(512, 65536) } else { 512 };
        let block_size = if block_size.is_power_of_two() { block_size } else { 512 };
        // One data descriptor per request: its length is bounded by SIZE_MAX, and by 4 MiB.
        let size_max = if f & F_SIZE_MAX != 0 { t.config::<4>(8).max(block_size as u64) } else { u32::MAX as u64 };
        let max_bytes = size_max.min(4 << 20) / block_size as u64 * block_size as u64;
        Some(VirtioBlk {
            _t: t,
            name,
            q,
            slots,
            free: (0..nslots).rev().collect(),
            tags: alloc::vec![0; nslots as usize],
            block_size,
            blocks: sectors * 512 / block_size as u64,
            read_only: f & F_RO != 0,
            flush: f & F_FLUSH != 0,
            max_bytes,
            done: Vec::new(),
        })
    }

    fn slot(&self, k: u16) -> u64 {
        self.slots.paddr() + k as u64 * SLOT_BYTES as u64
    }
}

impl BlockDevice for VirtioBlk {
    fn name(&self) -> &'static str {
        self.name
    }
    fn block_size(&self) -> u32 {
        self.block_size
    }
    fn blocks(&self) -> u64 {
        self.blocks
    }
    fn read_only(&self) -> bool {
        self.read_only
    }
    fn max_blocks(&self) -> u32 {
        (self.max_bytes / self.block_size as u64) as u32
    }

    fn submit(&mut self, _q: usize, r: BlockRequest) -> Result<(), BlockError> {
        let bytes = r.blocks as u64 * self.block_size as u64;
        let kind = match r.op {
            BlockOp::Read => T_IN,
            BlockOp::Write if self.read_only => return Err(BlockError::ReadOnly),
            BlockOp::Write => T_OUT,
            BlockOp::Flush => T_FLUSH,
        };
        if r.op != BlockOp::Flush && (r.blocks == 0 || bytes > self.max_bytes || r.lba.checked_add(r.blocks as u64).is_none_or(|e| e > self.blocks)) {
            return Err(BlockError::Range);
        }
        if r.op == BlockOp::Flush && !self.flush {
            // No volatile cache: complete at once (on the next poll).
            self.done.push((r.tag, Ok(())));
            return Ok(());
        }
        let Some(k) = self.free.pop() else { return Err(BlockError::Busy) };
        self.tags[k as usize] = r.tag;
        let hdr = self.slot(k);
        let status = hdr + 16;
        let sector = r.lba * (self.block_size as u64 / 512);
        // SAFETY: slot k's header and status byte, owned by this driver until the request completes.
        unsafe {
            core::ptr::write_volatile(hdr as *mut u32, kind);
            core::ptr::write_volatile((hdr + 4) as *mut u32, 0);
            core::ptr::write_volatile((hdr + 8) as *mut u64, if kind == T_FLUSH { 0 } else { sector });
            core::ptr::write_volatile(status as *mut u8, 0xFF);
        }
        let (d0, d1, d2) = (3 * k, 3 * k + 1, 3 * k + 2);
        if r.op == BlockOp::Flush {
            self.q.set_desc(d0, Desc { addr: hdr, len: 16, flags: DESC_F_NEXT, next: d2 });
        } else {
            self.q.set_desc(d0, Desc { addr: hdr, len: 16, flags: DESC_F_NEXT, next: d1 });
            let w = if r.op == BlockOp::Read { DESC_F_WRITE } else { 0 };
            self.q.set_desc(d1, Desc { addr: r.paddr, len: bytes as u32, flags: DESC_F_NEXT | w, next: d2 });
        }
        self.q.set_desc(d2, Desc { addr: status, len: 1, flags: DESC_F_WRITE, next: 0 });
        self.q.push(d0);
        self.q.kick();
        Ok(())
    }

    fn poll(&mut self, _q: usize) -> Option<(u32, Result<(), BlockError>)> {
        if let Some(d) = self.done.pop() {
            return Some(d);
        }
        let k = loop {
            let (head, _) = self.q.pop_used()?;
            // Not a chain head of ours, or not in flight: ignored.
            let k = head / 3;
            if head % 3 == 0 && (k as usize) < self.tags.len() && !self.free.contains(&k) {
                break k;
            }
        };
        let st = unsafe { core::ptr::read_volatile((self.slot(k) + 16) as *const u8) };
        self.free.push(k);
        let res = match st {
            S_OK => Ok(()),
            S_UNSUPP => Err(BlockError::Unsupported),
            _ => Err(BlockError::Io),
        };
        Some((self.tags[k as usize], res))
    }
}
