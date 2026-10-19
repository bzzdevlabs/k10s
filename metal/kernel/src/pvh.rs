//! PVH boot information (`struct hvm_start_info`, Xen public/arch-x86/hvm/start_info.h), filled by QEMU's
//! `pvh.bin` option ROM: E820 memory map (version ≥ 1), kernel command line, and `-initrd` as module 0.
//!
//! Everything is copied into fixed-size kernel structures before the frame allocator starts handing out memory,
//! except the module *contents*, whose physical ranges are reserved until the runtime has consumed them.

use core::ptr::read_unaligned;

const START_MAGIC: u32 = 0x336e_c578;
pub const MAX_MEMMAP: usize = 64;
pub const MAX_MODULES: usize = 4;
const MAX_CMDLINE: usize = 512;

/// E820 types.
pub const E820_RAM: u32 = 1;

#[repr(C)]
#[derive(Clone, Copy)]
struct StartInfo {
    magic: u32,
    version: u32,
    flags: u32,
    nr_modules: u32,
    modlist_paddr: u64,
    cmdline_paddr: u64,
    rsdp_paddr: u64,
    // version >= 1
    memmap_paddr: u64,
    memmap_entries: u32,
    reserved: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct ModEntry {
    paddr: u64,
    size: u64,
    cmdline_paddr: u64,
    reserved: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct MemRegion {
    pub addr: u64,
    pub size: u64,
    pub kind: u32,
    reserved: u32,
}

#[derive(Clone, Copy, Default, Debug)]
pub struct Module {
    pub paddr: u64,
    pub size: u64,
}

pub struct BootInfo {
    pub memmap: [MemRegion; MAX_MEMMAP],
    pub memmap_len: usize,
    pub modules: [Module; MAX_MODULES],
    pub modules_len: usize,
    cmdline: [u8; MAX_CMDLINE],
    cmdline_len: usize,
}

impl BootInfo {
    pub fn memmap(&self) -> &[MemRegion] {
        &self.memmap[..self.memmap_len]
    }
    pub fn modules(&self) -> &[Module] {
        &self.modules[..self.modules_len]
    }
    pub fn cmdline(&self) -> &str {
        core::str::from_utf8(&self.cmdline[..self.cmdline_len]).unwrap_or("")
    }
    /// `true` if the command line contains the word `flag` (or `flag=...`).
    pub fn has_flag(&self, flag: &str) -> bool {
        self.cmdline().split_ascii_whitespace().any(|w| w == flag || w.split('=').next() == Some(flag))
    }
    /// Value of `key=value` on the command line.
    pub fn arg(&self, key: &str) -> Option<&str> {
        self.cmdline().split_ascii_whitespace().find_map(|w| w.strip_prefix(key)?.strip_prefix('='))
    }
}

/// Parses the start info at `paddr` (identity-mapped). Panics on a bad magic: there is no other boot path.
///
/// # Safety
/// `paddr` must be the pointer QEMU passed in `ebx`.
pub unsafe fn parse(paddr: u64) -> BootInfo {
    let si: StartInfo = read_unaligned(paddr as *const StartInfo);
    assert!(si.magic == START_MAGIC, "PVH start info: bad magic {:#x}", si.magic);
    let mut bi = BootInfo {
        memmap: [MemRegion::default(); MAX_MEMMAP],
        memmap_len: 0,
        modules: [Module::default(); MAX_MODULES],
        modules_len: 0,
        cmdline: [0; MAX_CMDLINE],
        cmdline_len: 0,
    };
    assert!(si.version >= 1 && si.memmap_paddr != 0, "PVH start info v{} has no memory map", si.version);
    let n = (si.memmap_entries as usize).min(MAX_MEMMAP);
    for i in 0..n {
        bi.memmap[i] = read_unaligned((si.memmap_paddr as *const MemRegion).add(i));
    }
    bi.memmap_len = n;
    let n = (si.nr_modules as usize).min(MAX_MODULES);
    for i in 0..n {
        let m: ModEntry = read_unaligned((si.modlist_paddr as *const ModEntry).add(i));
        bi.modules[i] = Module { paddr: m.paddr, size: m.size };
    }
    bi.modules_len = n;
    if si.cmdline_paddr != 0 {
        let p = si.cmdline_paddr as *const u8;
        let mut len = 0;
        while len < MAX_CMDLINE && *p.add(len) != 0 {
            bi.cmdline[len] = *p.add(len);
            len += 1;
        }
        bi.cmdline_len = len;
    }
    bi
}
