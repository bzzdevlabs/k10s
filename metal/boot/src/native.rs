//! Booting the native kernel: `/k10s/metal.elf` of K10S-BOOT, a signed k10s image (`k10s image sign`, profile
//! `native`) whose payload is the kernel's ELF, verified against the k10s release key, the keys built into k10boot
//! and, with Secure Boot off, the operator's `os_key` of k10s.conf (with Secure Boot on, k10s.conf is not covered by
//! any signature, so its keys do not count). Then, as QEMU's PVH loader would: the ELF's segments at their physical
//! addresses, a PVH `start_info` (version 1: E820 map, command line, ACPI RSDP, modules) below 4 GiB, and the jump
//! to the PVH entry in 32-bit protected mode with paging off. The modules (k10-bootproto) carry k10s.conf, the
//! bundle, the key files k10s.conf names, the GOP framebuffer and the runtime services for the boot counter.

use crate::efi::{self, St, Volume, Where};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::arch::global_asm;
use k10_bootproto::{conf, counter, e820, EfiHandoff, Framebuffer, PixelFormat, RuntimeRegion};
use k10_bootproto::{MOD_BUNDLE, MOD_CONF, MOD_EFI, MOD_FRAMEBUFFER};
use k10_mesh::osimage;
use k10_mesh::sha256::{sha256, to_hex};

const RELEASE_KEYS: &str = include_str!("../../../os/release-key.pub");
const BUILT_IN_KEYS: &str = include_str!(concat!(env!("OUT_DIR"), "/keys.pub"));
const KERNEL: &str = "k10s/metal.elf";
const BUNDLE: &str = "k10s/bundle.k10b";
const CONF: &str = "k10s.conf";
const PAGE: u64 = 4096;
const FOUR_GIB: u64 = 1 << 32;
/// The kernel starts its application processors at this page (metal/kernel/src/smp.rs).
const TRAMPOLINE: u64 = 0x8000;
const MAX_KERNEL: usize = 64 << 20;
const MAX_BUNDLE: usize = 64 << 20;
const MAX_CONF: usize = 64 << 10;
/// The kernel's limit for a key file.
const MAX_SECRET: usize = 4096;
/// The kernel's limits (metal/kernel/src/pvh.rs).
const MAX_MODULES: usize = 16;
const CMDLINE_MAX: usize = 2048;
const MAX_E820: usize = 256;
// The start_info block: start_info, module list, module names, command line, then the E820 map.
const SI_MODLIST: usize = 64;
const NAME_LEN: usize = 64;
const SI_NAMES: usize = SI_MODLIST + 32 * MAX_MODULES;
const SI_CMDLINE: usize = SI_NAMES + NAME_LEN * MAX_MODULES;
const SI_MEMMAP: usize = 4096;
const SI_LEN: usize = SI_MEMMAP + 24 * MAX_E820;
const PVH_MAGIC: u32 = 0x336e_c578;

global_asm!(
    r#"
/* Copied below 4 GiB and called as fn(start_info, entry) (System V: rdi, rsi): leaves long mode for 32-bit
   protected mode with paging off (CR4.PCIDE first: clearing CR0.PG with it set faults), then enters the kernel's
   PVH entry with ebx = start_info, as QEMU's PVH loader does. Position-independent. */
.text
.balign 16
.globl k10boot_stub
k10boot_stub:
    cli
    mov %rdi, %rbx
    mov %rsi, %rbp
    mov %cr4, %rax
    and $~(1 << 17), %rax
    mov %rax, %cr4
    lea k10boot_gdt(%rip), %rax
    sub $16, %rsp
    movw $(k10boot_gdt_end - k10boot_gdt - 1), (%rsp)
    mov %rax, 2(%rsp)
    lgdt (%rsp)
    lea k10boot_pm32(%rip), %rax
    pushq $0x10
    push %rax
    lretq
.code32
k10boot_pm32:
    movw $0x18, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %fs
    movw %ax, %gs
    movw %ax, %ss
    movl %cr0, %eax
    andl $0x7fffffff, %eax
    movl %eax, %cr0
    /* EFER: LME and NXE off; CR4 as at a PVH entry (the kernel sets what it needs; LA57 can only go now). */
    movl $0xC0000080, %ecx
    rdmsr
    andl $~((1 << 8) | (1 << 11)), %eax
    wrmsr
    xorl %eax, %eax
    movl %eax, %cr4
    jmp *%ebp
.code64
.balign 8
k10boot_gdt:
    .quad 0
    .quad 0x00AF9A000000FFFF        /* 0x08: 64-bit code */
    .quad 0x00CF9A000000FFFF        /* 0x10: 32-bit code */
    .quad 0x00CF92000000FFFF        /* 0x18: data */
k10boot_gdt_end:
.globl k10boot_stub_end
k10boot_stub_end:
"#,
    options(att_syntax)
);

extern "C" {
    static k10boot_stub: u8;
    static k10boot_stub_end: u8;
}

/// Boots the kernel; returns only why it did not (boot services still up: Tier 1 boots instead).
pub fn boot(esp: &Volume, cmdline: &str, tier_stamp: [u8; 16]) -> String {
    match prepare(esp, cmdline, tier_stamp) {
        Ok(b) => b.start(),
        Err(e) => e,
    }
}

struct Boot {
    entry: u64,
    start_info: u64,
    stub: u64,
    efi_module: u64,
    counter: counter::State,
    modules: usize,
    /// The key files' pages: wiped when the native boot is given up (Tier 1 would reuse that memory as is).
    secrets: Vec<(u64, usize)>,
}

fn wipe(secrets: &[(u64, usize)]) {
    for &(at, len) in secrets {
        // SAFETY: pages this loader allocated for a key file.
        unsafe { core::ptr::write_bytes(at as *mut u8, 0, len) };
    }
}

struct Elf<'a> {
    image: &'a [u8],
    lo: u64,
    hi: u64,
    entry: u64,
    /// (file offset, physical address, file size, memory size)
    segments: Vec<(usize, u64, usize, u64)>,
}

fn rd<const N: usize>(b: &[u8], o: usize) -> Option<[u8; N]> {
    b.get(o..o.checked_add(N)?)?.try_into().ok()
}

fn u16_at(b: &[u8], o: usize) -> Option<u16> {
    rd(b, o).map(u16::from_le_bytes)
}

fn u32_at(b: &[u8], o: usize) -> Option<u32> {
    rd(b, o).map(u32::from_le_bytes)
}

fn u64_at(b: &[u8], o: usize) -> Option<u64> {
    rd(b, o).map(u64::from_le_bytes)
}

impl<'a> Elf<'a> {
    /// An x86-64 ELF executable with its PT_LOAD segments between 1 MiB and 4 GiB; the entry is the PVH one
    /// (`XEN_ELFNOTE_PHYS32_ENTRY`).
    fn parse(image: &'a [u8]) -> Result<Elf<'a>, &'static str> {
        let bad = "not an x86-64 ELF executable";
        if image.get(..6) != Some(b"\x7fELF\x02\x01") || u16_at(image, 16) != Some(2) || u16_at(image, 18) != Some(0x3e) {
            return Err(bad);
        }
        let (phoff, phsize, phnum) = (u64_at(image, 32).ok_or(bad)? as usize, u16_at(image, 54).ok_or(bad)? as usize, u16_at(image, 56).ok_or(bad)?);
        let (mut lo, mut hi, mut entry, mut segments) = (u64::MAX, 0u64, None, Vec::new());
        for i in 0..phnum as usize {
            let ph = image.get(phoff + i * phsize..phoff + (i + 1) * phsize).ok_or(bad)?;
            let field = |o| u64_at(ph, o).ok_or(bad);
            let (off, paddr, filesz, memsz) = (field(8)?, field(24)?, field(32)?, field(40)?);
            match u32_at(ph, 0).ok_or(bad)? {
                1 => {
                    let end = paddr.checked_add(memsz).ok_or(bad)?;
                    if filesz > memsz || off.checked_add(filesz).is_none_or(|e| e > image.len() as u64) || paddr < 1 << 20 || end > FOUR_GIB {
                        return Err("a segment outside the file, or not between 1 MiB and 4 GiB");
                    }
                    lo = lo.min(paddr);
                    hi = hi.max(end);
                    segments.push((off as usize, paddr, filesz as usize, memsz));
                }
                4 => entry = entry.or(pvh_entry(image.get(off as usize..(off + filesz) as usize).ok_or(bad)?)),
                _ => {}
            }
        }
        let entry = entry.ok_or("no PVH entry note (XEN_ELFNOTE_PHYS32_ENTRY)")?;
        if segments.is_empty() || !(lo..hi).contains(&entry) || entry >= FOUR_GIB {
            return Err("no loadable segment holds the PVH entry");
        }
        Ok(Elf { image, lo: lo & !(PAGE - 1), hi: hi.div_ceil(PAGE) * PAGE, entry, segments })
    }
}

/// The address in a Xen note (type 18, name "Xen"), as QEMU reads it.
fn pvh_entry(notes: &[u8]) -> Option<u64> {
    let mut p = notes;
    while p.len() >= 12 {
        let (namesz, descsz, kind) = (u32_at(p, 0)? as usize, u32_at(p, 4)? as usize, u32_at(p, 8)?);
        let desc = 12 + namesz.div_ceil(4) * 4;
        let next = desc + descsz.div_ceil(4) * 4;
        if kind == 18 && p.get(12..12 + namesz) == Some(b"Xen\0") {
            return match descsz {
                8 => u64_at(p, desc),
                4 => u32_at(p, desc).map(u64::from),
                _ => None,
            };
        }
        p = p.get(next..)?;
    }
    None
}

fn hex32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut k = [0u8; 32];
    for (i, b) in k.iter_mut().enumerate() {
        *b = u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(k)
}

/// Hex public keys, one per line (`#` comments).
fn keys(text: &str) -> impl Iterator<Item = [u8; 32]> + '_ {
    text.lines().filter_map(|l| hex32(l.split('#').next().unwrap_or("").trim()))
}

fn counter_guid() -> efi::Guid {
    efi::Guid::from_bytes(counter::GUID)
}

/// `n` bytes in fresh pages below 4 GiB (the kernel's boot page tables map only that much).
fn below_4g(data: &[u8], kind: u32) -> Result<u64, String> {
    let pages = (data.len() as u64).div_ceil(PAGE).max(1) as usize;
    let p = efi::allocate_pages(Where::Below(FOUR_GIB), kind, pages).map_err(|e| format!("out of memory below 4 GiB ({})", St(e)))?;
    // SAFETY: just allocated, identity-mapped.
    unsafe {
        core::ptr::write_bytes(p as *mut u8, 0, pages * PAGE as usize);
        core::ptr::copy_nonoverlapping(data.as_ptr(), p as *mut u8, data.len());
    }
    Ok(p)
}

/// The E820 type of the page at `addr` in the current memory map.
fn page_type(addr: u64) -> Result<u32, String> {
    let mut map = Vec::new();
    let (n, dsize, _) = efi::memory_map(&mut map).map_err(|e| format!("GetMemoryMap: {}", St(e)))?;
    Ok((0..n)
        .map(|i| efi::descriptor(&map, dsize, i))
        .find(|&(_, start, pages, _)| (start..start + pages * PAGE).contains(&addr))
        .map_or(e820::RESERVED, |(kind, _, _, attr)| e820::from_efi(kind, attr)))
}

fn framebuffer() -> Option<Framebuffer> {
    let gop = efi::locate::<efi::GraphicsOutput>(&efi::GRAPHICS_OUTPUT)?;
    // SAFETY: the protocol's current mode.
    let mode = unsafe { (*gop).mode.as_ref()? };
    let info = unsafe { mode.info.as_ref()? };
    let [red, green, blue, reserved] = info.pixel_masks;
    let format = match info.pixel_format {
        0 => PixelFormat::Rgbx,
        1 => PixelFormat::Bgrx,
        2 => PixelFormat::Bitmask { red, green, blue, reserved },
        _ => return None,
    };
    Some(Framebuffer {
        base: mode.frame_buffer_base,
        size: mode.frame_buffer_size as u64,
        width: info.horizontal_resolution,
        height: info.vertical_resolution,
        stride: info.pixels_per_scan_line,
        format,
    })
}

fn prepare(esp: &Volume, cmdline: &str, tier_stamp: [u8; 16]) -> Result<Boot, String> {
    if cmdline.len() >= CMDLINE_MAX {
        return Err(format!("a command line of {} bytes (at most {})", cmdline.len(), CMDLINE_MAX - 1));
    }
    let blob = esp.read(KERNEL, MAX_KERNEL)?.ok_or("no /k10s/metal.elf on K10S-BOOT")?.data;
    let conf = match esp.read(CONF, MAX_CONF)? {
        Some(f) => Some(String::from_utf8(f.data).map_err(|_| "/k10s.conf is not UTF-8")?),
        None => None,
    };
    let conf = conf.as_deref();

    // Trust, then the signature.
    let secure = efi::secure_boot();
    let mut trusted: Vec<[u8; 32]> = keys(RELEASE_KEYS).chain(keys(BUILT_IN_KEYS)).collect();
    if let Some(v) = conf.and_then(|c| conf::get(c, "os_key")) {
        if secure {
            log!("Secure Boot is on: the os_key of k10s.conf is not trusted for the kernel (only keys built into k10boot)");
        } else {
            trusted.extend(v.split(',').filter_map(|k| hex32(k.trim())));
        }
    }
    let info = osimage::verify(&blob, &trusted).map_err(|e| format!("/k10s/metal.elf refused: {e}"))?;
    if info.meta.arch != "x86_64" {
        return Err(format!("/k10s/metal.elf is an image for {}", info.meta.arch));
    }
    log!("kernel {} {} signed by {}… (Secure Boot {})", info.meta.profile, info.meta.version, &to_hex(&info.key)[..16],
         if secure { "on" } else { "off" });
    let elf = Elf::parse(&blob[..info.payload_len]).map_err(|e| format!("/k10s/metal.elf: {e}"))?;

    // The application processors' start-up page must be memory the kernel may use once boot services are gone.
    match page_type(TRAMPOLINE)? {
        e820::RAM => {
            // Keeps the firmware off it until then (it may already hold boot services data, freed at exit).
            let _ = efi::allocate_pages(Where::At(TRAMPOLINE), efi::LOADER_DATA, 1);
        }
        t => return Err(format!("page {TRAMPOLINE:#x} (application processor start-up) is firmware memory (E820 type {t})")),
    }

    // The boot counter: refused once the last native boots never got healthy; written last (start).
    let mut buf = [0u8; counter::LEN];
    let prev = match efi::get_variable(counter::NAME, &counter_guid(), &mut buf) {
        Ok(Some(n)) => counter::State::decode(&buf[..n]),
        Ok(None) => None,
        // Counting from zero would never pin Tier 1.
        Err(e) => return Err(format!("cannot read the boot counter ({})", St(e))),
    };
    let state = counter::count(prev, sha256(&blob), tier_stamp).map_err(|n| {
        format!("pinned to Tier 1: the last {n} boots of this kernel never got healthy (write /k10s/tier again, or install another kernel, to retry)")
    })?;

    // The kernel at its physical addresses.
    let pages = ((elf.hi - elf.lo) / PAGE) as usize;
    efi::allocate_pages(Where::At(elf.lo), efi::LOADER_DATA, pages)
        .map_err(|e| format!("cannot place the kernel at {:#x}..{:#x} ({})", elf.lo, elf.hi, St(e)))?;
    // SAFETY: the pages just allocated, identity-mapped; segments are inside the image (Elf::parse).
    unsafe {
        core::ptr::write_bytes(elf.lo as *mut u8, 0, pages * PAGE as usize);
        for &(off, paddr, filesz, _) in &elf.segments {
            core::ptr::copy_nonoverlapping(elf.image[off..].as_ptr(), paddr as *mut u8, filesz);
        }
    }

    // Modules.
    let mut modules: Vec<(&str, u64, usize)> = Vec::new();
    if let Some(c) = conf {
        modules.push((MOD_CONF, below_4g(c.as_bytes(), efi::LOADER_DATA)?, c.len()));
    }
    if let Some(b) = esp.read(BUNDLE, MAX_BUNDLE)? {
        log!("bundle /{BUNDLE}: {} bytes", b.data.len());
        modules.push((MOD_BUNDLE, below_4g(&b.data, efi::LOADER_DATA)?, b.data.len()));
    }
    if let Some(fb) = framebuffer() {
        log!("framebuffer {}x{} at {:#x}", fb.width, fb.height, fb.base);
        modules.push((MOD_FRAMEBUFFER, below_4g(&fb.encode(), efi::LOADER_DATA)?, Framebuffer::LEN));
    }
    let efi_module = below_4g(&[0; EfiHandoff::MAX_LEN], efi::LOADER_DATA)?;
    modules.push((MOD_EFI, efi_module, EfiHandoff::MAX_LEN));
    let base = efi::allocate_pages(Where::Below(FOUR_GIB), efi::LOADER_DATA, SI_LEN.div_ceil(PAGE as usize))
        .map_err(|e| format!("out of memory below 4 GiB ({})", St(e)))?;
    // The jump stub, in code pages below 4 GiB.
    // SAFETY: the stub's bounds (global_asm above).
    let stub = unsafe { core::slice::from_raw_parts(&raw const k10boot_stub, &raw const k10boot_stub_end as usize - &raw const k10boot_stub as usize) };
    let stub = below_4g(stub, efi::LOADER_CODE)?;

    // The key files last: whatever fails from here on wipes them.
    let mut secrets = Vec::new();
    for key in conf::FILE_KEYS {
        let Some(path) = conf.and_then(|c| conf::get(c, key)) else { continue };
        let Some(rel) = conf::boot_file(path) else {
            log!("k10s.conf {key}={path}: not a file of K10S-BOOT (/boot/NAME)");
            continue;
        };
        match esp.read(rel, MAX_SECRET) {
            Ok(Some(mut f)) => {
                let at = below_4g(&f.data, efi::LOADER_DATA);
                k10_mesh::crypto::zeroize(&mut f.data);
                let Ok(at) = at else {
                    wipe(&secrets);
                    return Err(at.unwrap_err());
                };
                secrets.push((at, f.data.len()));
                modules.push((path, at, f.data.len()));
            }
            Ok(None) => log!("k10s.conf {key}={path}: no such file on K10S-BOOT"),
            Err(e) => log!("k10s.conf {key}: {e}"),
        }
    }
    let too_long = modules.iter().find(|m| m.0.len() >= NAME_LEN);
    if modules.len() > MAX_MODULES || too_long.is_some() {
        wipe(&secrets);
        return Err(match too_long {
            Some(m) => format!("module name {} longer than {} bytes", m.0, NAME_LEN - 1),
            None => format!("{} modules (at most {MAX_MODULES})", modules.len()),
        });
    }

    // start_info (the E820 map is filled in at exit).
    let mut si = alloc::vec![0u8; SI_LEN];
    let put = |si: &mut [u8], o: usize, v: &[u8]| si[o..o + v.len()].copy_from_slice(v);
    let rsdp = efi::configuration_table(&efi::ACPI_20_TABLE).or_else(|| efi::configuration_table(&efi::ACPI_10_TABLE)).unwrap_or(0);
    put(&mut si, 0, &PVH_MAGIC.to_le_bytes());
    put(&mut si, 4, &1u32.to_le_bytes());
    put(&mut si, 12, &(modules.len() as u32).to_le_bytes());
    put(&mut si, 16, &(base + SI_MODLIST as u64).to_le_bytes());
    put(&mut si, 24, &(base + SI_CMDLINE as u64).to_le_bytes());
    put(&mut si, 32, &rsdp.to_le_bytes());
    put(&mut si, 40, &(base + SI_MEMMAP as u64).to_le_bytes());
    for (i, &(name, paddr, size)) in modules.iter().enumerate() {
        let (m, n) = (SI_MODLIST + 32 * i, SI_NAMES + NAME_LEN * i);
        put(&mut si, m, &paddr.to_le_bytes());
        put(&mut si, m + 8, &(size as u64).to_le_bytes());
        put(&mut si, m + 16, &(base + n as u64).to_le_bytes());
        put(&mut si, n, name.as_bytes());
    }
    put(&mut si, SI_CMDLINE, cmdline.as_bytes());
    // SAFETY: the pages just allocated.
    unsafe { core::ptr::copy_nonoverlapping(si.as_ptr(), base as *mut u8, SI_LEN) };
    Ok(Boot { entry: elf.entry, start_info: base, stub, efi_module, counter: state, modules: modules.len(), secrets })
}

impl Boot {
    /// Counts the boot, exits boot services and jumps to the kernel; returns only why not (boot services still up).
    fn start(self) -> String {
        let give_up = |why: String| {
            wipe(&self.secrets);
            why
        };
        // Writing a variable may allocate: before the final memory map. An uncounted boot would have no fallback.
        if let Err(e) = efi::set_variable(counter::NAME, &counter_guid(), counter::ATTRIBUTES, &self.counter.encode()) {
            return give_up(format!("cannot write the boot counter ({})", St(e)));
        }
        log!("native boot {} of {} before Tier 1 is pinned: starting the kernel at {:#x} ({} modules)",
             self.counter.failures, counter::MAX_FAILURES, self.entry, self.modules);
        let mut map = Vec::new();
        let dsize = match efi::memory_map(&mut map) {
            Ok((_, d, _)) => d,
            Err(e) => return give_up(format!("GetMemoryMap: {}", St(e))),
        };
        // Room for what the allocations from here on add: none may happen once ExitBootServices was tried.
        map.resize(map.len() + 64 * dsize, 0);
        let e820 = (self.start_info + SI_MEMMAP as u64) as *mut e820::Entry;
        for attempt in 0..16 {
            let (mut size, mut key, mut dsize, mut ver) = (map.len(), 0usize, 0usize, 0u32);
            // SAFETY: a buffer of `size` bytes.
            let s = unsafe { (efi::bs().get_memory_map)(&mut size, map.as_mut_ptr(), &mut key, &mut dsize, &mut ver) };
            match s {
                efi::SUCCESS => {}
                _ if attempt == 0 => return give_up(format!("GetMemoryMap: {}", St(s))),
                _ => break,
            }
            let n = size / dsize;
            // SAFETY: the start_info block's E820 area of MAX_E820 entries, below 4 GiB.
            let out = unsafe { core::slice::from_raw_parts_mut(e820, MAX_E820) };
            let mut m = 0;
            for i in 0..n {
                let (kind, start, pages, attr) = efi::descriptor(&map, dsize, i);
                if m == MAX_E820 {
                    // Merged on the way: firmware maps have hundreds of descriptors, mostly adjacent RAM.
                    m = e820::normalize(&mut out[..m]);
                }
                if m < MAX_E820 {
                    out[m] = e820::Entry { addr: start, size: pages * PAGE, kind: e820::from_efi(kind, attr), reserved: 0 };
                    m += 1;
                }
            }
            let m = e820::normalize(&mut out[..m]);
            // SAFETY: start_info's memmap_entries field.
            unsafe { ((self.start_info + 48) as *mut u32).write(m as u32) };
            let runtime = (0..n).map(|i| efi::descriptor(&map, dsize, i)).filter(|d| d.3 & efi::MEMORY_RUNTIME != 0);
            // SAFETY: the module's pages (EfiHandoff::MAX_LEN bytes).
            let out = unsafe { core::slice::from_raw_parts_mut(self.efi_module as *mut u8, EfiHandoff::MAX_LEN) };
            let st = efi::system_table();
            EfiHandoff::encode(out, st as *const _ as u64, st.runtime_services as u64, true,
                               runtime.map(|(kind, phys, pages, attr)| RuntimeRegion { kind, phys, pages, attr }));
            if attempt == 0 {
                // SAFETY: boot services are up; the firmware's watchdog is off for the OS from here.
                unsafe { (efi::bs().set_watchdog_timer)(0, 0, 0, core::ptr::null()) };
            }
            match efi::exit_boot_services(key) {
                Ok(()) => {
                    // SAFETY: the stub copied below 4 GiB; it never returns.
                    let jump: extern "sysv64" fn(u64, u64) -> ! = unsafe { core::mem::transmute(self.stub) };
                    jump(self.start_info, self.entry);
                }
                Err(efi::INVALID_PARAMETER) => continue,
                Err(e) if attempt == 0 => return give_up(format!("ExitBootServices: {}", St(e))),
                Err(_) => break,
            }
        }
        // Boot services may be half torn down after failed exits, so no Tier 1 from here: a cold reset, the boot
        // already counted.
        wipe(&self.secrets);
        // SAFETY: runtime services outlive boot services.
        unsafe { (efi::rt().reset_system)(0, efi::ABORTED, 0, core::ptr::null()) }
    }
}
