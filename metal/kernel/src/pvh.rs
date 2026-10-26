//! PVH boot information (`struct hvm_start_info`, Xen public/arch-x86/hvm/start_info.h): E820 memory map (version
//! ≥ 1), kernel command line, ACPI RSDP and modules. Filled by QEMU's `pvh.bin` option ROM (`-initrd` as module 0),
//! by k10boot (UEFI) or by the Multiboot2 shim (boot.rs); loaders name their modules (k10-bootproto).
//!
//! Everything is copied into fixed-size kernel structures before the frame allocator starts handing out memory,
//! except the module *contents*, whose physical ranges are reserved until the runtime has consumed them. The
//! k10s.conf module is read here too: its keys that mean the same for the native kernel become boot arguments
//! (bootcfg.rs), so `arg`/`has_flag` see the command line and K10S-BOOT's configuration alike.

use core::ptr::read_unaligned;
use k10_bootproto::e820;

use crate::mem::PhysAddr;

const START_MAGIC: u32 = 0x336e_c578;
/// As many as k10boot hands over (merged).
pub const MAX_MEMMAP: usize = 256;
pub const MAX_MODULES: usize = 16;
const MAX_CMDLINE: usize = 2048;
/// The command line plus the arguments derived from k10s.conf.
const MAX_ARGS: usize = 4096;
const MAX_NAME: usize = 64;

pub use e820::Entry as MemRegion;
pub const E820_RAM: u32 = e820::RAM;

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

#[derive(Clone, Copy, Debug)]
pub struct Module {
    pub paddr: PhysAddr,
    pub size: u64,
    name: [u8; MAX_NAME],
    name_len: usize,
}

impl Module {
    const EMPTY: Module = Module { paddr: PhysAddr::new(0), size: 0, name: [0; MAX_NAME], name_len: 0 };

    /// The loader's name for it (empty for QEMU's `-initrd`).
    pub fn name(&self) -> &str {
        core::str::from_utf8(&self.name[..self.name_len]).unwrap_or("")
    }

    /// The contents.
    ///
    /// # Safety
    /// The module must not have been released (frames::release_module).
    pub unsafe fn bytes(&self) -> &'static [u8] {
        unsafe { core::slice::from_raw_parts(self.paddr.ptr::<u8>(), self.size as usize) }
    }
}

pub struct BootInfo {
    pub memmap: [MemRegion; MAX_MEMMAP],
    pub memmap_len: usize,
    pub modules: [Module; MAX_MODULES],
    pub modules_len: usize,
    /// Physical address of the ACPI RSDP, 0 if the loader gave none (QEMU's PVH: search the BIOS areas).
    pub rsdp: u64,
    cmdline: [u8; MAX_CMDLINE],
    cmdline_len: usize,
    args: [u8; MAX_ARGS],
    args_len: usize,
}

impl BootInfo {
    pub fn memmap(&self) -> &[MemRegion] {
        &self.memmap[..self.memmap_len]
    }
    pub fn modules(&self) -> &[Module] {
        &self.modules[..self.modules_len]
    }
    /// The module named `name`.
    pub fn module(&self, name: &str) -> Option<(usize, &Module)> {
        self.modules().iter().enumerate().find(|(_, m)| m.name() == name)
    }
    /// The command line as the loader gave it.
    pub fn cmdline(&self) -> &str {
        core::str::from_utf8(&self.cmdline[..self.cmdline_len]).unwrap_or("")
    }
    /// The command line and the arguments derived from k10s.conf (bootcfg.rs).
    fn args(&self) -> &str {
        core::str::from_utf8(&self.args[..self.args_len]).unwrap_or("")
    }
    /// `true` if the arguments contain the word `flag` (or `flag=...`).
    pub fn has_flag(&self, flag: &str) -> bool {
        self.args().split_ascii_whitespace().any(|w| w == flag || w.split('=').next() == Some(flag))
    }
    /// Value of `key=value` in the arguments (the command line's first).
    pub fn arg(&self, key: &str) -> Option<&str> {
        self.args().split_ascii_whitespace().find_map(|w| w.strip_prefix(key)?.strip_prefix('='))
    }
    /// k10s.conf as the loader handed it over (module `k10s.conf`), if it is UTF-8.
    pub fn conf(&self) -> Option<&'static str> {
        // SAFETY: the configuration module is never released.
        self.module(k10_bootproto::MOD_CONF).and_then(|(_, m)| core::str::from_utf8(unsafe { m.bytes() }).ok())
    }
    /// Whether a boot loader that names its modules (k10boot, GRUB) booted the kernel, rather than QEMU's `-kernel`.
    pub fn from_loader(&self) -> bool {
        self.modules().iter().any(|m| !m.name().is_empty())
    }
}

/// Parses the start info at `paddr` (through the direct map). Panics on a bad magic: there is no other boot path.
///
/// # Safety
/// `paddr` must be the pointer the loader passed in `ebx`.
pub unsafe fn parse(paddr: PhysAddr) -> BootInfo {
    let si: StartInfo = read_unaligned(paddr.ptr::<StartInfo>());
    assert!(si.magic == START_MAGIC, "PVH start info: bad magic {:#x}", si.magic);
    let mut bi = BootInfo {
        memmap: [MemRegion::default(); MAX_MEMMAP],
        memmap_len: 0,
        modules: [Module::EMPTY; MAX_MODULES],
        modules_len: 0,
        rsdp: si.rsdp_paddr,
        cmdline: [0; MAX_CMDLINE],
        cmdline_len: 0,
        args: [0; MAX_ARGS],
        args_len: 0,
    };
    assert!(si.version >= 1 && si.memmap_paddr != 0, "PVH start info v{} has no memory map", si.version);
    // Merged as it is copied: UEFI maps have hundreds of entries, adjacent ones mostly of the same type.
    let mut n = 0;
    for i in 0..si.memmap_entries as usize {
        let e: MemRegion = read_unaligned(PhysAddr::new(si.memmap_paddr).ptr::<MemRegion>().add(i));
        if n == MAX_MEMMAP {
            n = e820::normalize(&mut bi.memmap);
            if n == MAX_MEMMAP {
                crate::kprintln!("k10-metal: memory map: entries past {MAX_MEMMAP} ignored");
                break;
            }
        }
        bi.memmap[n] = e;
        n += 1;
    }
    bi.memmap_len = e820::normalize(&mut bi.memmap[..n]);
    let n = (si.nr_modules as usize).min(MAX_MODULES);
    for i in 0..n {
        let m: ModEntry = read_unaligned(PhysAddr::new(si.modlist_paddr).ptr::<ModEntry>().add(i));
        let mut module = Module { paddr: PhysAddr::new(m.paddr), size: m.size, ..Module::EMPTY };
        module.name_len = copy_cstr(m.cmdline_paddr, &mut module.name);
        // A name is printable ASCII: qboot leaves module command lines undefined (their upper half is garbage).
        if !module.name[..module.name_len].iter().all(|c| c.is_ascii_graphic()) {
            module.name_len = 0;
        }
        bi.modules[i] = module;
    }
    bi.modules_len = n;
    bi.cmdline_len = copy_cstr(si.cmdline_paddr, &mut bi.cmdline);
    let mut args = [0u8; MAX_ARGS];
    let mut a = crate::bootcfg::Args::new(&mut args);
    a.push(bi.cmdline());
    if let Some((_, conf)) = bi.module(k10_bootproto::MOD_CONF) {
        match core::str::from_utf8(conf.bytes()) {
            Ok(text) => crate::bootcfg::derive(text, bi.cmdline(), &mut a),
            Err(_) => crate::kprintln!("k10-metal: k10s.conf is not UTF-8: ignored"),
        }
    }
    bi.args_len = a.len();
    bi.args = args;
    bi
}

/// Copies the NUL-terminated string at `paddr` (0: none) into `out`; returns its length (cut at `out`'s size).
/// Only the first 4 GiB are in the direct map this early: a pointer beyond is no string.
unsafe fn copy_cstr(paddr: u64, out: &mut [u8]) -> usize {
    if paddr == 0 || paddr >= (1 << 32) - out.len() as u64 {
        return 0;
    }
    let p = PhysAddr::new(paddr).ptr::<u8>().cast_const();
    let mut len = 0;
    while len < out.len() && *p.add(len) != 0 {
        out[len] = *p.add(len);
        len += 1;
    }
    len
}
