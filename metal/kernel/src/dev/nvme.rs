//! NVMe (NVM Express base specification 2.0) behind [`BlockDevice`]: the admin queue pair, one I/O queue pair
//! (storage runs on CPU 0), the first active namespace, a PRP list page per request slot, polled completions whose
//! MSI-X message wakes CPU 0 from `hlt`. Flush goes to the controller only when it reports a volatile write cache;
//! power off and reboot announce a normal shutdown (CC.SHN) so the controller flushes and parks.

use alloc::vec::Vec;

use super::{BlockDevice, BlockError, BlockOp, BlockRequest, DmaRegion};
use crate::kprintln;
use crate::mem::frames::Tag;
use crate::mem::{PhysAddr, VirtAddr};

const CAP: u64 = 0x00;
const CC: u64 = 0x14;
const CSTS: u64 = 0x1C;
const AQA: u64 = 0x24;
const ASQ: u64 = 0x28;
const ACQ: u64 = 0x30;
const DOORBELLS: u64 = 0x1000;

const CC_EN: u32 = 1;
/// I/O submission and completion entries of 64 and 16 bytes (2^6, 2^4).
const CC_QES: u32 = 6 << 16 | 4 << 20;
const CC_SHN_NORMAL: u32 = 1 << 14;
const CSTS_RDY: u32 = 1;
const CSTS_CFS: u32 = 2;

const ADMIN_DEPTH: u16 = 16;
const IO_DEPTH: u16 = 64;
const PAGE: u64 = 4096;
/// Largest request (the PRP list of one page covers 2 MiB).
const MAX_BYTES: u64 = 1 << 20;

const OP_FLUSH: u8 = 0x00;
const OP_WRITE: u8 = 0x01;
const OP_READ: u8 = 0x02;
const ADMIN_CREATE_SQ: u8 = 0x01;
const ADMIN_CREATE_CQ: u8 = 0x05;
const ADMIN_IDENTIFY: u8 = 0x06;
const ADMIN_SET_FEATURES: u8 = 0x09;

struct Queue {
    sq: DmaRegion,
    cq: DmaRegion,
    depth: u16,
    sq_tail: u16,
    cq_head: u16,
    phase: u16,
    sq_db: VirtAddr,
    cq_db: VirtAddr,
}

impl Queue {
    fn new(depth: u16, sq_db: VirtAddr, cq_db: VirtAddr) -> Option<Queue> {
        Some(Queue {
            sq: DmaRegion::alloc(depth as usize * 64, Tag::Kernel)?,
            cq: DmaRegion::alloc(depth as usize * 16, Tag::Kernel)?,
            depth,
            sq_tail: 0,
            cq_head: 0,
            phase: 1,
            sq_db,
            cq_db,
        })
    }

    /// Queues a command and rings the submission doorbell.
    fn push(&mut self, cmd: &[u32; 16]) {
        let at = self.sq.virt() + self.sq_tail as u64 * 64;
        for (i, w) in cmd.iter().enumerate() {
            // SAFETY: inside the submission queue's region.
            unsafe { core::ptr::write_volatile((at + 4 * i as u64).ptr::<u32>(), *w) };
        }
        self.sq_tail = (self.sq_tail + 1) % self.depth;
        // SAFETY: the mapped doorbell register.
        unsafe { core::ptr::write_volatile(self.sq_db.ptr::<u32>(), self.sq_tail as u32) };
    }

    /// The next completion: (command id, status field without the phase bit).
    fn pop(&mut self) -> Option<(u16, u16)> {
        let at = self.cq.virt() + self.cq_head as u64 * 16;
        // SAFETY: inside the completion queue's region.
        let st = unsafe { core::ptr::read_volatile((at + 14).ptr::<u16>()) };
        if st & 1 != self.phase {
            return None;
        }
        let cid = unsafe { core::ptr::read_volatile((at + 12).ptr::<u16>()) };
        self.cq_head += 1;
        if self.cq_head == self.depth {
            self.cq_head = 0;
            self.phase ^= 1;
        }
        // SAFETY: the mapped doorbell register.
        unsafe { core::ptr::write_volatile(self.cq_db.ptr::<u32>(), self.cq_head as u32) };
        Some((cid, st >> 1))
    }
}

pub struct Nvme {
    regs: VirtAddr,
    admin: Queue,
    io: Queue,
    nsid: u32,
    block_size: u32,
    blocks: u64,
    max_blocks: u32,
    /// A volatile write cache: flushes go to the controller.
    vwc: bool,
    /// One PRP list page per request slot.
    prps: DmaRegion,
    free: Vec<u16>,
    tags: Vec<u32>,
    /// Completions decided without the controller.
    done: Vec<(u32, Result<(), BlockError>)>,
    timeout_ms: u64,
    admin_cid: u16,
    /// Completions raise an MSI-X message.
    irq: bool,
}

// SAFETY: owned registers and regions, used by one CPU at a time.
unsafe impl Send for Nvme {}

fn r32(regs: VirtAddr, off: u64) -> u32 {
    // SAFETY: the mapped register block.
    unsafe { core::ptr::read_volatile((regs + off).ptr::<u32>()) }
}

fn w32(regs: VirtAddr, off: u64, v: u32) {
    // SAFETY: the mapped register block.
    unsafe { core::ptr::write_volatile((regs + off).ptr::<u32>(), v) }
}

fn r64(regs: VirtAddr, off: u64) -> u64 {
    r32(regs, off) as u64 | (r32(regs, off + 4) as u64) << 32
}

fn w64(regs: VirtAddr, off: u64, v: u64) {
    w32(regs, off, v as u32);
    w32(regs, off + 4, (v >> 32) as u32);
}

/// Waits until `CSTS & mask == want` (false after `ms`).
fn wait_csts(regs: VirtAddr, mask: u32, want: u32, ms: u64) -> bool {
    let deadline = crate::time::now_ns() + ms * 1_000_000;
    while r32(regs, CSTS) & mask != want {
        if crate::time::now_ns() > deadline {
            return false;
        }
        core::hint::spin_loop();
    }
    true
}

impl Nvme {
    pub fn probe(d: &crate::pci::Device) -> Option<Nvme> {
        let at = |msg: &str| kprintln!("nvme {:02x}:{:02x}.{}: {msg}", d.bus, d.dev, d.func);
        let (io, base) = d.bar(0);
        let size = d.bar_size(0);
        let mapped = || crate::arch::paging::map_mmio(PhysAddr::new(base), size);
        let Some(regs) = (if io || base == 0 || size < 0x2000 { None } else { mapped() }) else {
            at("no usable register BAR");
            return None;
        };
        // Memory space on, bus mastering only once the controller is reset.
        d.enable(false);
        let cap = r64(regs, CAP);
        let timeout_ms = ((cap >> 24) & 0xFF).max(1) * 500;
        let dstrd = (cap >> 32) & 0xF;
        if (cap >> 48) & 0xF != 0 || (cap >> 37) & 1 == 0 {
            at("4 KiB pages or the NVM command set unsupported");
            return None;
        }
        if r32(regs, CC) & CC_EN != 0 {
            w32(regs, CC, r32(regs, CC) & !CC_EN);
        }
        if !wait_csts(regs, CSTS_RDY, 0, timeout_ms) {
            at("does not reset");
            return None;
        }
        d.enable(true);
        let mqes = (cap & 0xFFFF) as u16;
        let db = |y: u64| regs + DOORBELLS + y * (4 << dstrd);
        let ad = ADMIN_DEPTH.min(mqes + 1);
        let iod = IO_DEPTH.min(mqes + 1);
        let (Some(admin), Some(io), Some(prps), Some(buf)) = (
            Queue::new(ad, db(0), db(1)),
            Queue::new(iod, db(2), db(3)),
            DmaRegion::alloc((iod as usize - 1) * PAGE as usize, Tag::Kernel),
            DmaRegion::alloc(PAGE as usize, Tag::Kernel),
        ) else {
            at("no memory for the queues");
            return None;
        };
        w32(regs, AQA, ((ad - 1) as u32) << 16 | (ad - 1) as u32);
        w64(regs, ASQ, admin.sq.paddr().get());
        w64(regs, ACQ, admin.cq.paddr().get());
        w32(regs, CC, CC_EN | CC_QES);
        if !wait_csts(regs, CSTS_RDY | CSTS_CFS, CSTS_RDY, timeout_ms) {
            at("does not become ready");
            return None;
        }
        let mut n = Nvme {
            regs,
            admin,
            io,
            nsid: 1,
            block_size: 512,
            blocks: 0,
            max_blocks: 0,
            vwc: false,
            prps,
            free: (0..iod - 1).rev().collect(),
            tags: alloc::vec![0; iod as usize - 1],
            done: Vec::new(),
            timeout_ms,
            admin_cid: 0,
            irq: false,
        };
        let fail = |n: &mut Nvme, what: &str, st: u16| {
            at(&alloc::format!("{what} failed (status {st:#x})"));
            n.disable();
        };
        // Identify controller: volatile write cache, largest transfer.
        if let Err(st) = n.admin_cmd(ADMIN_IDENTIFY, 0, buf.paddr().get(), [1, 0, 0, 0, 0, 0]) {
            fail(&mut n, "identify controller", st);
            return None;
        }
        let id = buf.as_slice();
        n.vwc = id[525] & 1 != 0;
        let mdts = id[77];
        // The first active namespace (namespace list), else 1.
        if n.admin_cmd(ADMIN_IDENTIFY, 0, buf.paddr().get(), [2, 0, 0, 0, 0, 0]).is_ok() {
            n.nsid = u32::from_le_bytes(buf.as_slice()[..4].try_into().unwrap()).max(1);
        }
        if let Err(st) = n.admin_cmd(ADMIN_IDENTIFY, n.nsid, buf.paddr().get(), [0, 0, 0, 0, 0, 0]) {
            fail(&mut n, "identify namespace", st);
            return None;
        }
        let ns = buf.as_slice();
        let nsze = u64::from_le_bytes(ns[..8].try_into().unwrap());
        let lbaf = u32::from_le_bytes(ns[128 + 4 * (ns[26] & 0xF) as usize..][..4].try_into().unwrap());
        let (lbads, ms) = ((lbaf >> 16) & 0xFF, lbaf & 0xFFFF);
        if !(9..=12).contains(&lbads) || ms != 0 {
            let what = alloc::format!("namespace {} format (2^{lbads}-byte blocks, {ms} metadata bytes)", n.nsid);
            fail(&mut n, &what, 0);
            return None;
        }
        n.block_size = 1 << lbads;
        n.blocks = nsze;
        let max = if mdts == 0 { MAX_BYTES } else { MAX_BYTES.min(PAGE << mdts.min(20)) };
        n.max_blocks = (max / n.block_size as u64) as u32;
        // One I/O queue pair; its completions raise MSI-X entry 0, which wakes CPU 0.
        let _ = n.admin_cmd(ADMIN_SET_FEATURES, 0, 0, [0x07, 0, 0, 0, 0, 0]);
        let msix = d.msix().filter(|m| m.route(0, crate::arch::idt::VEC_DISK));
        let ien = if msix.is_some() { 2 } else { 0 };
        let qsize = ((iod - 1) as u32) << 16 | 1;
        if let Err(st) = n.admin_cmd(ADMIN_CREATE_CQ, 0, n.io.cq.paddr().get(), [qsize, 1 | ien, 0, 0, 0, 0]) {
            fail(&mut n, "create I/O completion queue", st);
            return None;
        }
        if let Err(st) = n.admin_cmd(ADMIN_CREATE_SQ, 0, n.io.sq.paddr().get(), [qsize, 1 | 1 << 16, 0, 0, 0, 0]) {
            fail(&mut n, "create I/O submission queue", st);
            return None;
        }
        if let Some(m) = &msix {
            d.msix_enable(m);
            n.irq = true;
        }
        if crate::verbose() { kprintln!(
            "nvme {:02x}:{:02x}.{}: namespace {}, {} MiB in {}-byte blocks, {} write cache, {}",
            d.bus, d.dev, d.func, n.nsid, (n.blocks * n.block_size as u64) >> 20, n.block_size,
            if n.vwc { "volatile" } else { "no volatile" }, if msix.is_some() { "MSI-X" } else { "polled" }
        ); }
        Some(n)
    }

    /// Runs an admin command and waits for it: `Err(status)` on failure or timeout (0xFFFF).
    fn admin_cmd(&mut self, op: u8, nsid: u32, prp1: u64, cdw: [u32; 6]) -> Result<u32, u16> {
        self.admin_cid = self.admin_cid.wrapping_add(1);
        let cid = self.admin_cid;
        let mut c = [0u32; 16];
        c[0] = op as u32 | (cid as u32) << 16;
        c[1] = nsid;
        c[6] = prp1 as u32;
        c[7] = (prp1 >> 32) as u32;
        c[10..16].copy_from_slice(&cdw);
        self.admin.push(&c);
        let deadline = crate::time::now_ns() + self.timeout_ms * 1_000_000;
        loop {
            if let Some((id, st)) = self.admin.pop() {
                if id == cid {
                    return if st == 0 { Ok(0) } else { Err(st) };
                }
            }
            if crate::time::now_ns() > deadline {
                return Err(0xFFFF);
            }
            core::hint::spin_loop();
        }
    }

    fn disable(&mut self) {
        w32(self.regs, CC, r32(self.regs, CC) & !CC_EN);
        wait_csts(self.regs, CSTS_RDY, 0, self.timeout_ms);
    }
}

impl BlockDevice for Nvme {
    fn name(&self) -> &'static str {
        "nvme"
    }
    fn block_size(&self) -> u32 {
        self.block_size
    }
    fn blocks(&self) -> u64 {
        self.blocks
    }
    fn read_only(&self) -> bool {
        false
    }
    fn max_blocks(&self) -> u32 {
        self.max_blocks
    }

    fn submit(&mut self, _q: usize, r: BlockRequest) -> Result<(), BlockError> {
        let op = match r.op {
            BlockOp::Read => OP_READ,
            BlockOp::Write => OP_WRITE,
            BlockOp::Flush if !self.vwc => {
                self.done.push((r.tag, Ok(())));
                return Ok(());
            }
            BlockOp::Flush => OP_FLUSH,
        };
        if r.op != BlockOp::Flush
            && (r.blocks == 0 || r.blocks > self.max_blocks || r.lba.checked_add(r.blocks as u64).is_none_or(|e| e > self.blocks))
        {
            return Err(BlockError::Range);
        }
        let Some(slot) = self.free.pop() else { return Err(BlockError::Busy) };
        self.tags[slot as usize] = r.tag;
        let mut c = [0u32; 16];
        c[0] = op as u32 | (slot as u32) << 16;
        c[1] = self.nsid;
        if r.op != BlockOp::Flush {
            let bytes = r.blocks as u64 * self.block_size as u64;
            let buf = r.paddr.get();
            let next = (buf | (PAGE - 1)) + 1;
            let prp2 = if buf + bytes <= next {
                0
            } else if buf + bytes <= next + PAGE {
                next
            } else {
                // A PRP list: the pages after the first.
                let off = slot as u64 * PAGE;
                let list = self.prps.virt() + off;
                let pages = (buf + bytes - next).div_ceil(PAGE);
                for i in 0..pages {
                    // SAFETY: this slot's PRP list page.
                    unsafe { core::ptr::write_volatile((list + 8 * i).ptr::<u64>(), next + i * PAGE) };
                }
                (self.prps.paddr() + off).get()
            };
            c[6] = buf as u32;
            c[7] = (buf >> 32) as u32;
            c[8] = prp2 as u32;
            c[9] = (prp2 >> 32) as u32;
            c[10] = r.lba as u32;
            c[11] = (r.lba >> 32) as u32;
            c[12] = r.blocks - 1;
        }
        self.io.push(&c);
        Ok(())
    }

    fn poll(&mut self, _q: usize) -> Option<(u32, Result<(), BlockError>)> {
        if let Some(d) = self.done.pop() {
            return Some(d);
        }
        let (cid, st) = self.io.pop()?;
        if cid as usize >= self.tags.len() || self.free.contains(&cid) {
            return None;
        }
        self.free.push(cid);
        Some((self.tags[cid as usize], if st == 0 { Ok(()) } else { Err(BlockError::Io) }))
    }

    fn has_interrupts(&self) -> bool {
        self.irq
    }

    fn shutdown(&mut self) {
        w32(self.regs, CC, (r32(self.regs, CC) & !(3 << 14)) | CC_SHN_NORMAL);
        // CSTS.SHST: 2 = shutdown processing complete.
        wait_csts(self.regs, 3 << 2, 2 << 2, self.timeout_ms.min(5000));
    }
}
