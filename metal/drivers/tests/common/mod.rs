//! Test harness: a [`MockHw`] in front of a register model of a chip (each test file has its models), DMA memory
//! from the host heap (its address doubles as the "physical" one, so models read and write descriptors and buffers
//! directly), virtual time advanced by `delay_us`.

#![allow(dead_code)]

use std::alloc::{alloc_zeroed, dealloc, Layout};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

use k10_drivers::{DmaMem, Hw, Regs};

pub struct HostMem {
    ptr: *mut u8,
    layout: Layout,
}

impl DmaMem for HostMem {
    fn paddr(&self) -> u64 {
        self.ptr as u64
    }
    fn ptr(&self) -> *mut u8 {
        self.ptr
    }
    fn len(&self) -> usize {
        self.layout.size()
    }
}

impl Drop for HostMem {
    fn drop(&mut self) {
        unsafe { dealloc(self.ptr, self.layout) }
    }
}

/// A chip's registers and behavior.
pub trait Model {
    fn read(&mut self, off: u32, bytes: u8) -> u32;
    fn write(&mut self, off: u32, bytes: u8, v: u32);
    fn config_read32(&mut self, _off: u16) -> u32 {
        0
    }
    fn config_write16(&mut self, _off: u16, _v: u16) {}
    /// Virtual time passed.
    fn advance(&mut self, _us: u64) {}
}

pub struct MockHw<M: Model> {
    pub dev: Rc<RefCell<M>>,
    pub logs: Rc<RefCell<Vec<String>>>,
    pub now_us: Rc<Cell<u64>>,
}

impl<M: Model> Clone for MockHw<M> {
    fn clone(&self) -> Self {
        MockHw { dev: self.dev.clone(), logs: self.logs.clone(), now_us: self.now_us.clone() }
    }
}

impl<M: Model> MockHw<M> {
    pub fn new(m: M) -> MockHw<M> {
        MockHw { dev: Rc::new(RefCell::new(m)), logs: Rc::default(), now_us: Rc::default() }
    }
    pub fn logged(&self, s: &str) -> bool {
        self.logs.borrow().iter().any(|l| l.contains(s))
    }
}

impl<M: Model> Regs for MockHw<M> {
    fn read32(&self, off: u32) -> u32 {
        self.dev.borrow_mut().read(off, 4)
    }
    fn write32(&self, off: u32, v: u32) {
        self.dev.borrow_mut().write(off, 4, v)
    }
    fn read16(&self, off: u32) -> u16 {
        self.dev.borrow_mut().read(off, 2) as u16
    }
    fn write16(&self, off: u32, v: u16) {
        self.dev.borrow_mut().write(off, 2, v as u32)
    }
    fn read8(&self, off: u32) -> u8 {
        self.dev.borrow_mut().read(off, 1) as u8
    }
    fn write8(&self, off: u32, v: u8) {
        self.dev.borrow_mut().write(off, 1, v as u32)
    }
}

impl<M: Model> Hw for MockHw<M> {
    type Mem = HostMem;
    fn config_read32(&self, off: u16) -> u32 {
        self.dev.borrow_mut().config_read32(off)
    }
    fn config_write16(&self, off: u16, v: u16) {
        self.dev.borrow_mut().config_write16(off, v)
    }
    fn bus_master(&self) {}
    fn delay_us(&self, us: u32) {
        self.now_us.set(self.now_us.get() + us as u64);
        self.dev.borrow_mut().advance(us as u64);
    }
    fn dma(&self, bytes: usize) -> Option<HostMem> {
        let layout = Layout::from_size_align(bytes.div_ceil(4096).max(1) * 4096, 4096).ok()?;
        let ptr = unsafe { alloc_zeroed(layout) };
        (!ptr.is_null()).then_some(HostMem { ptr, layout })
    }
    fn random(&self) -> u64 {
        0x1234_5678_9ABC_DEF0
    }
    fn log(&self, args: std::fmt::Arguments) {
        self.logs.borrow_mut().push(args.to_string());
    }
}

/// A plain register file with byte-addressed sub-word access (little endian), for models to build on.
#[derive(Default)]
pub struct RegFile {
    pub r: std::collections::HashMap<u32, u32>,
}

impl RegFile {
    pub fn get(&self, off: u32) -> u32 {
        *self.r.get(&off).unwrap_or(&0)
    }
    pub fn set(&mut self, off: u32, v: u32) {
        self.r.insert(off, v);
    }
    pub fn read(&self, off: u32, bytes: u8) -> u32 {
        let d = self.get(off & !3) >> ((off & 3) * 8);
        match bytes {
            1 => d & 0xFF,
            2 => d & 0xFFFF,
            _ => d,
        }
    }
    pub fn write(&mut self, off: u32, bytes: u8, v: u32) {
        let shift = (off & 3) * 8;
        let mask: u32 = match bytes {
            1 => 0xFF,
            2 => 0xFFFF,
            _ => !0,
        } << shift;
        let old = self.get(off & !3);
        self.set(off & !3, (old & !mask) | ((v << shift) & mask));
    }
}

pub unsafe fn rd32(addr: u64) -> u32 {
    unsafe { std::ptr::read_volatile(addr as *const u32) }
}
pub unsafe fn wr32(addr: u64, v: u32) {
    unsafe { std::ptr::write_volatile(addr as *mut u32, v) }
}
pub unsafe fn rd64(addr: u64) -> u64 {
    unsafe { rd32(addr) as u64 | (rd32(addr + 4) as u64) << 32 }
}

/// A test frame: broadcast destination, the given source, EtherType 0x0800, `len` bytes in all.
pub fn frame(len: usize, tag: u8) -> Vec<u8> {
    let mut f = vec![tag; len];
    f[..6].copy_from_slice(&[0xFF; 6]);
    f[6..12].copy_from_slice(&[2, 0, 0, 0, 0, tag]);
    f[12..14].copy_from_slice(&[0x08, 0x00]);
    f
}

/// A PHY: MII registers per (page, register), a page-select register at 0x1F.
#[derive(Default)]
pub struct Phy {
    pub regs: std::collections::HashMap<(u16, u8), u16>,
    pub page: u16,
}

impl Phy {
    pub fn get(&self, page: u16, reg: u8) -> u16 {
        *self.regs.get(&(page, reg)).unwrap_or(&0)
    }
    pub fn set(&mut self, page: u16, reg: u8, v: u16) {
        self.regs.insert((page, reg), v);
    }
}
