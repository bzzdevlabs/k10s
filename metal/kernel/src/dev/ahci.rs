//! AHCI 1.3.1 (SATA) behind [`BlockDevice`]: one device per port with an ATA disk, taken over from the firmware
//! (BIOS/OS handoff, HBA reset). Requests are DMA commands (READ/WRITE DMA EXT, FLUSH CACHE EXT) in up to 32 command
//! slots, which the HBA executes in order (NCQ is not used); one PRD entry each (the buffers are physically
//! contiguous); completions are polled. A disk with its write cache on gets FLUSH CACHE EXT for each flush.

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use super::{BlockDevice, BlockError, BlockOp, BlockRequest, DmaRegion};
use crate::kprintln;
use crate::mem::frames::Tag;
use crate::mem::{PhysAddr, VirtAddr};

const CAP: u64 = 0x00;
const GHC: u64 = 0x04;
const PI: u64 = 0x0C;
const CAP2: u64 = 0x24;
const BOHC: u64 = 0x28;
const GHC_AE: u32 = 1 << 31;
const GHC_HR: u32 = 1;

const P_CLB: u64 = 0x00;
const P_FB: u64 = 0x08;
const P_IS: u64 = 0x10;
const P_IE: u64 = 0x14;
const P_CMD: u64 = 0x18;
const P_TFD: u64 = 0x20;
const P_SIG: u64 = 0x24;
const P_SSTS: u64 = 0x28;
const P_SERR: u64 = 0x30;
const P_CI: u64 = 0x38;
const CMD_ST: u32 = 1;
const CMD_SUD: u32 = 1 << 1;
const CMD_FRE: u32 = 1 << 4;
const CMD_FR: u32 = 1 << 14;
const CMD_CR: u32 = 1 << 15;
const IS_TFES: u32 = 1 << 30;
const TFD_BUSY: u32 = 0x88;
const SIG_ATA: u32 = 0x0000_0101;

const ATA_READ_DMA_EXT: u8 = 0x25;
const ATA_WRITE_DMA_EXT: u8 = 0x35;
const ATA_FLUSH_CACHE_EXT: u8 = 0xEA;
const ATA_IDENTIFY: u8 = 0xEC;

/// Port memory: command list (32 headers), received FIS area, then one command table per slot.
const CL: usize = 0;
const FB: usize = 1024;
const TABLES: usize = 2048;
const TABLE_BYTES: usize = 256;
/// Largest request (one PRD entry takes up to 4 MiB).
const MAX_BYTES: u32 = 1 << 20;

fn r32(at: VirtAddr) -> u32 {
    // SAFETY: a mapped HBA register.
    unsafe { core::ptr::read_volatile(at.ptr::<u32>()) }
}

fn w32(at: VirtAddr, v: u32) {
    // SAFETY: a mapped HBA register.
    unsafe { core::ptr::write_volatile(at.ptr::<u32>(), v) }
}

/// Waits until `cond()` (false after `ms`).
fn wait(ms: u64, cond: impl Fn() -> bool) -> bool {
    let deadline = crate::time::now_ns() + ms * 1_000_000;
    while !cond() {
        if crate::time::now_ns() > deadline {
            return false;
        }
        core::hint::spin_loop();
    }
    true
}

pub struct AhciDisk {
    /// The port's registers.
    port: VirtAddr,
    mem: DmaRegion,
    slots: u32,
    /// The HBA addresses all of memory (else requests must stay below 4 GiB).
    s64a: bool,
    block_size: u32,
    blocks: u64,
    /// Write cache on and FLUSH CACHE EXT supported: flushes go to the disk.
    flush: bool,
    /// Command slots in flight, and their tags.
    busy: u32,
    tags: [u32; 32],
    ready: VecDeque<(u32, Result<(), BlockError>)>,
}

// SAFETY: owned registers and memory, used by one CPU at a time.
unsafe impl Send for AhciDisk {}

/// The disks of the AHCI controller `d`.
pub fn probe(d: &crate::pci::Device) -> Vec<AhciDisk> {
    let at = |msg: &str| kprintln!("ahci {:02x}:{:02x}.{}: {msg}", d.bus, d.dev, d.func);
    let (io, bar) = d.bar(5);
    let size = d.bar_size(5);
    let abar = if io || bar == 0 || size < 0x180 { None } else { crate::arch::paging::map_mmio(PhysAddr::new(bar), size) };
    let Some(abar) = abar else {
        at("no usable ABAR");
        return Vec::new();
    };
    d.enable(false);
    // Take the HBA from the firmware, then reset it (no command of the firmware's survives).
    if r32(abar + CAP2) & 1 != 0 {
        w32(abar + BOHC, r32(abar + BOHC) | 2);
        if !wait(2000, || r32(abar + BOHC) & 1 == 0) {
            at("the firmware does not hand the HBA over");
            return Vec::new();
        }
    }
    w32(abar + GHC, r32(abar + GHC) | GHC_AE);
    w32(abar + GHC, r32(abar + GHC) | GHC_HR);
    if !wait(1000, || r32(abar + GHC) & GHC_HR == 0) {
        at("does not reset");
        return Vec::new();
    }
    w32(abar + GHC, GHC_AE);
    d.enable(true);
    let cap = r32(abar + CAP);
    let (slots, s64a) = (((cap >> 8) & 0x1F) + 1, cap >> 31 != 0);
    let pi = r32(abar + PI);
    let ports: Vec<u64> = (0..32u64).filter(|&p| pi & 1 << p != 0 && 0x100 + (p + 1) * 0x80 <= size).collect();
    // Staggered spin-up: the ports wait for us; then the links come up (COMRESET after the HBA reset).
    if cap & 1 << 27 != 0 {
        for &p in &ports {
            let port = abar + 0x100 + p * 0x80;
            w32(port + P_CMD, r32(port + P_CMD) | CMD_SUD);
        }
    }
    wait(20, || false);
    let mut disks = Vec::new();
    for p in ports {
        let port = abar + 0x100 + p * 0x80;
        // DET 3: a device and its PHY up; 1: a device whose link is still coming up; 0: nothing there.
        if r32(port + P_SSTS) & 0xF == 0 || !wait(1000, || r32(port + P_SSTS) & 0xF == 3) {
            continue;
        }
        match AhciDisk::start(port, slots, s64a) {
            Ok(disk) => {
                if crate::verbose() { kprintln!(
                    "ahci {:02x}:{:02x}.{} port {p}: {} MiB in {}-byte blocks, write cache {}",
                    d.bus, d.dev, d.func, (disk.blocks * disk.block_size as u64) >> 20, disk.block_size,
                    if disk.flush { "on" } else { "off or without flush" }
                ); }
                disks.push(disk);
            }
            Err(e) => at(&alloc::format!("port {p}: {e}")),
        }
    }
    disks
}

impl AhciDisk {
    fn start(port: VirtAddr, slots: u32, s64a: bool) -> Result<AhciDisk, &'static str> {
        let mem = DmaRegion::alloc(TABLES + 32 * TABLE_BYTES, Tag::Kernel).ok_or("no memory")?;
        let ident = DmaRegion::alloc(512, Tag::Kernel).ok_or("no memory")?;
        if !s64a && ((mem.paddr() + mem.len() as u64).get() > 1 << 32 || ident.paddr().get() >> 32 != 0) {
            return Err("a 32-bit HBA and no memory below 4 GiB");
        }
        let mut disk = AhciDisk {
            port,
            mem,
            slots,
            s64a,
            block_size: 512,
            blocks: 0,
            flush: false,
            busy: 0,
            tags: [0; 32],
            ready: VecDeque::new(),
        };
        disk.stop()?;
        let base = disk.mem.paddr().get();
        w32(port + P_CLB, (base + CL as u64) as u32);
        w32(port + P_CLB + 4, ((base + CL as u64) >> 32) as u32);
        w32(port + P_FB, (base + FB as u64) as u32);
        w32(port + P_FB + 4, ((base + FB as u64) >> 32) as u32);
        disk.go()?;
        // The signature comes with the device's first register FIS, received once FRE is on.
        if r32(port + P_SIG) != SIG_ATA {
            let sig = r32(port + P_SIG);
            disk.stop()?;
            return Err(if sig == 0xEB14_0101 { "an ATAPI device" } else { "not an ATA disk" });
        }
        disk.issue(0, ATA_IDENTIFY, 0, 0, ident.paddr(), 512, false);
        if !wait(5000, || r32(port + P_CI) & 1 == 0) || r32(port + P_IS) & IS_TFES != 0 {
            disk.stop()?;
            return Err("IDENTIFY DEVICE failed");
        }
        let id = |w: usize| u16::from_le_bytes([ident.as_slice()[2 * w], ident.as_slice()[2 * w + 1]]);
        if id(83) & 1 << 10 == 0 {
            disk.stop()?;
            return Err("no 48-bit addressing");
        }
        disk.blocks = (0..4).fold(0u64, |a, i| a | (id(100 + i) as u64) << (16 * i));
        if id(106) & 0xC000 == 0x4000 && id(106) & 1 << 12 != 0 {
            disk.block_size = 2 * (id(117) as u32 | (id(118) as u32) << 16);
        }
        if !disk.block_size.is_power_of_two() || !(512..=4096).contains(&disk.block_size) {
            disk.stop()?;
            return Err("unsupported logical sector size");
        }
        disk.flush = id(85) & 1 << 5 != 0 && id(83) & 1 << 13 != 0;
        Ok(disk)
    }

    /// Stops command processing and FIS reception.
    fn stop(&mut self) -> Result<(), &'static str> {
        let p = self.port;
        w32(p + P_CMD, r32(p + P_CMD) & !CMD_ST);
        if !wait(500, || r32(p + P_CMD) & CMD_CR == 0) {
            return Err("command list does not stop");
        }
        w32(p + P_CMD, r32(p + P_CMD) & !CMD_FRE);
        if !wait(500, || r32(p + P_CMD) & CMD_FR == 0) {
            return Err("FIS reception does not stop");
        }
        Ok(())
    }

    /// Clears the port's errors and starts it (FIS reception, then commands).
    fn go(&mut self) -> Result<(), &'static str> {
        let p = self.port;
        w32(p + P_SERR, u32::MAX);
        w32(p + P_IS, u32::MAX);
        w32(p + P_IE, 0);
        w32(p + P_CMD, r32(p + P_CMD) | CMD_FRE);
        if !wait(1000, || r32(p + P_TFD) & TFD_BUSY == 0) {
            return Err("device busy");
        }
        w32(p + P_CMD, r32(p + P_CMD) | CMD_ST);
        Ok(())
    }

    /// Fills command slot `slot` (header, command FIS, one PRD entry when `bytes` > 0) and issues it.
    fn issue(&mut self, slot: u32, cmd: u8, lba: u64, count: u32, buf: PhysAddr, bytes: u32, write: bool) {
        let (table_off, header_off) = ((TABLES + slot as usize * TABLE_BYTES) as u64, (CL + slot as usize * 32) as u64);
        let (table, header) = (self.mem.virt() + table_off, self.mem.virt() + header_off);
        let table_phys = (self.mem.paddr() + table_off).get();
        let paddr = buf.get();
        let mut fis = [0u8; 20];
        fis[0] = 0x27; // register host to device
        fis[1] = 0x80; // command
        fis[2] = cmd;
        fis[4..7].copy_from_slice(&lba.to_le_bytes()[..3]);
        fis[7] = 0x40; // LBA mode
        fis[8..11].copy_from_slice(&lba.to_le_bytes()[3..6]);
        fis[12..14].copy_from_slice(&(count as u16).to_le_bytes());
        // SAFETY: this slot's command table and header, which the HBA reads only once issued.
        unsafe {
            for (i, w) in fis.chunks(4).enumerate() {
                core::ptr::write_volatile((table + 4 * i as u64).ptr::<u32>(), u32::from_le_bytes(w.try_into().unwrap()));
            }
            let prdtl = if bytes > 0 { 1u32 } else { 0 };
            if bytes > 0 {
                let prd = table + 0x80;
                core::ptr::write_volatile(prd.ptr::<u32>(), paddr as u32);
                core::ptr::write_volatile((prd + 4).ptr::<u32>(), (paddr >> 32) as u32);
                core::ptr::write_volatile((prd + 8).ptr::<u32>(), 0);
                core::ptr::write_volatile((prd + 12).ptr::<u32>(), bytes - 1);
            }
            core::ptr::write_volatile(header.ptr::<u32>(), 5 | (write as u32) << 6 | prdtl << 16);
            core::ptr::write_volatile((header + 4).ptr::<u32>(), 0);
            core::ptr::write_volatile((header + 8).ptr::<u32>(), table_phys as u32);
            core::ptr::write_volatile((header + 12).ptr::<u32>(), (table_phys >> 32) as u32);
        }
        w32(self.port + P_CI, 1 << slot);
    }

    /// After a task file error: every command in flight fails, the port restarts.
    fn recover(&mut self) {
        let mut b = self.busy;
        while b != 0 {
            let s = b.trailing_zeros();
            b &= b - 1;
            self.ready.push_back((self.tags[s as usize], Err(BlockError::Io)));
        }
        self.busy = 0;
        if self.stop().and_then(|()| self.go()).is_err() {
            kprintln!("ahci: port at {:#x} does not restart after an error", self.port);
        }
    }
}

impl BlockDevice for AhciDisk {
    fn name(&self) -> &'static str {
        "ahci"
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
        MAX_BYTES / self.block_size
    }

    fn submit(&mut self, _q: usize, r: BlockRequest) -> Result<(), BlockError> {
        let bytes = r.blocks as u64 * self.block_size as u64;
        if r.op == BlockOp::Flush && !self.flush {
            self.ready.push_back((r.tag, Ok(())));
            return Ok(());
        }
        if r.op != BlockOp::Flush {
            if r.blocks == 0 || bytes > MAX_BYTES as u64 || r.lba.checked_add(r.blocks as u64).is_none_or(|e| e > self.blocks) {
                return Err(BlockError::Range);
            }
            if !self.s64a && r.paddr.get() + bytes > 1 << 32 {
                return Err(BlockError::Unsupported);
            }
        }
        let free = !self.busy & if self.slots == 32 { u32::MAX } else { (1 << self.slots) - 1 };
        if free == 0 {
            return Err(BlockError::Busy);
        }
        let slot = free.trailing_zeros();
        self.busy |= 1 << slot;
        self.tags[slot as usize] = r.tag;
        match r.op {
            BlockOp::Read => self.issue(slot, ATA_READ_DMA_EXT, r.lba, r.blocks, r.paddr, bytes as u32, false),
            BlockOp::Write => self.issue(slot, ATA_WRITE_DMA_EXT, r.lba, r.blocks, r.paddr, bytes as u32, true),
            BlockOp::Flush => self.issue(slot, ATA_FLUSH_CACHE_EXT, 0, 0, PhysAddr::new(0), 0, false),
        }
        Ok(())
    }

    fn poll(&mut self, _q: usize) -> Option<(u32, Result<(), BlockError>)> {
        if self.ready.is_empty() && self.busy != 0 {
            let is = r32(self.port + P_IS);
            if is != 0 {
                w32(self.port + P_IS, is);
            }
            if is & IS_TFES != 0 {
                self.recover();
            } else {
                let mut done = self.busy & !r32(self.port + P_CI);
                while done != 0 {
                    let s = done.trailing_zeros();
                    done &= done - 1;
                    self.busy &= !(1 << s);
                    self.ready.push_back((self.tags[s as usize], Ok(())));
                }
            }
        }
        self.ready.pop_front()
    }
}
