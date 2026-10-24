//! k10boot: the UEFI loader of the k10s images (metal/README.md, "Booting"; docs/OS.md, "Tiers"). It is
//! `\EFI\BOOT\BOOTX64.EFI` of K10S-BOOT and picks the machine's tier from `/k10s/tier`:
//!
//! * `linux`: chain-loads systemd-boot (`\EFI\systemd\systemd-bootx64.efi`), which boots the Tier 1 A/B UKIs with its
//!   boot counting; under Secure Boot the firmware (or shim) verifies it as any image.
//! * `metal`: boots the native kernel `/k10s/metal.elf` (native.rs): verified against the k10s release key and the
//!   operator's keys, handed a PVH `start_info` with the files of K10S-BOOT as modules. A kernel that fails
//!   verification, or that failed to get healthy on its last two boots (the boot counter), boots Tier 1 instead.
//! * `auto`: `metal` if one of the machine's network adapters (PCI, as the firmware enumerated it) is in the
//!   supported-device table (k10-bootproto `devices`), else `linux`.
//!
//! `k10s.tier=X` on the loader's command line (UEFI load options) overrides the file; the rest of the command line
//! is the native kernel's, else `metal_cmdline` of `/k10s/tier`.

#![no_std]
#![no_main]

extern crate alloc;

#[macro_use]
mod efi;
mod native;

use alloc::string::String;
use alloc::vec::Vec;
use core::ffi::c_void;
use core::ptr::null_mut;
use efi::{Handle, Status, SystemTable, Volume};
use k10_bootproto::devices::{self, Class};
use k10_bootproto::tier::{self, Tier};

#[global_allocator]
static POOL: efi::Pool = efi::Pool;

/// SBAT metadata (shim's revocation scheme, https://github.com/rhboot/shim/blob/main/SBAT.md): shim starts no image
/// without it. The generation (1) goes up when a security fix must revoke older k10boot builds.
const SBAT_CSV: &str = concat!(
    "sbat,1,SBAT Version,sbat,1,https://github.com/rhboot/shim/blob/main/SBAT.md\n",
    "k10boot,1,k10s,k10boot,",
    env!("CARGO_PKG_VERSION"),
    ",https://github.com/bzzdevlabs/k10s\n"
);

#[used]
#[link_section = ".sbat"]
static SBAT: [u8; SBAT_CSV.len()] = {
    let (mut a, b, mut i) = ([0u8; SBAT_CSV.len()], SBAT_CSV.as_bytes(), 0);
    while i < b.len() {
        a[i] = b[i];
        i += 1;
    }
    a
};

const TIER_FILE: &str = "k10s/tier";
const KERNEL_FILE: &str = "k10s/metal.elf";
/// Tier 1's loader.
const SYSTEMD_BOOT: &str = "EFI/systemd/systemd-bootx64.efi";

#[no_mangle]
extern "efiapi" fn efi_main(image: Handle, st: *mut SystemTable) -> Status {
    // SAFETY: the firmware's arguments.
    unsafe { efi::init(image, st) };
    log!("{} (UEFI)", env!("K10S_VERSION_FULL"));
    let status = run();
    log!("cannot boot ({}): back to the firmware", efi::St(status));
    efi::stall_ms(3000);
    status
}

fn run() -> Status {
    // SAFETY: our own image handle.
    let Some(li) = (unsafe { efi::protocol::<efi::LoadedImage>(efi::image(), &efi::LOADED_IMAGE) }) else {
        return efi::LOAD_ERROR;
    };
    let li = unsafe { &*li };
    let Some((device, esp)) = boot_volume(li.device_handle) else {
        log!("no K10S-BOOT volume (none has /{TIER_FILE} or /{KERNEL_FILE})");
        return efi::NOT_FOUND;
    };
    let options = load_options(li);
    let mut forced = None;
    let mut words = Vec::new();
    for w in options.split_ascii_whitespace() {
        match w.strip_prefix("k10s.tier=") {
            Some(t) => forced = Some(t),
            None => words.push(w),
        }
    }
    let tier_file = match esp.read(TIER_FILE, 64 << 10) {
        Ok(f) => f,
        Err(e) => {
            log!("{e}");
            None
        }
    };
    let setting = |key: &str| tier_file.as_ref().and_then(|f| tier::get(&f.data, key));
    let (tier, from) = match forced {
        Some(t) => (Tier::parse(t), "the command line"),
        None if tier_file.is_none() => (Some(Tier::Linux), "the default (no /k10s/tier)"),
        None => (setting("tier").and_then(Tier::parse), "/k10s/tier"),
    };
    let tier = tier.unwrap_or_else(|| {
        log!("{from} has no valid tier=auto|linux|metal: linux");
        Tier::Linux
    });
    let cmdline = if words.is_empty() { String::from(setting("metal_cmdline").unwrap_or("")) } else { words.join(" ") };
    log!("tier {} (from {from})", tier.name());

    let native = match tier {
        Tier::Linux => false,
        Tier::Metal => true,
        Tier::Auto => auto_select(),
    };
    if native {
        let stamp = tier_file.as_ref().map_or([0; 16], |f| f.mtime);
        // Returns only when the kernel is not booted.
        let why = native::boot(&esp, &cmdline, stamp);
        log!("native kernel not booted: {why}");
        log!("booting Tier 1 instead");
    }
    drop(esp);
    chainload(device)
}

/// The K10S-BOOT volume: the one this loader was started from if it has the k10s files (an ESP), else the first
/// that has them (k10boot started otherwise, e.g. QEMU's `-kernel`, maybe before the firmware connected the disks).
fn boot_volume(own: Handle) -> Option<(Handle, Volume)> {
    let ours = |h: Handle| Volume::open(h).filter(|v| v.exists(TIER_FILE) || v.exists(KERNEL_FILE)).map(|v| (h, v));
    let any = || efi::handles(&efi::SIMPLE_FILE_SYSTEM).into_iter().find_map(ours);
    ours(own).or_else(any).or_else(|| {
        efi::connect_all();
        any()
    })
}

/// The load options as text: UCS-2 of printable characters only (firmware boot entries often carry binary data
/// there), without the image path the UEFI shell puts first.
fn load_options(li: &efi::LoadedImage) -> String {
    let n = li.load_options_size as usize / 2;
    if li.load_options.is_null() || n == 0 {
        return String::new();
    }
    // SAFETY: the firmware's load options of `load_options_size` bytes (alignment not guaranteed).
    let raw = unsafe { core::slice::from_raw_parts(li.load_options, n * 2) };
    let mut s = String::new();
    for c in raw.chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])) {
        match c {
            0 => break,
            0x20..=0x7e => s.push(c as u8 as char),
            0x09 | 0x0a | 0x0d => s.push(' '),
            _ => return String::new(),
        }
    }
    let first = s.split_ascii_whitespace().next().unwrap_or("");
    if first.to_ascii_lowercase().ends_with(".efi") {
        s = String::from(s.trim_start()[first.len()..].trim());
    }
    String::from(s.trim())
}

/// `tier=auto`: whether a network adapter the native kernel drives is present.
fn auto_select() -> bool {
    // Firmware that boots fast may not have enumerated every PCI device yet.
    efi::connect_all();
    let mut seen = Vec::new();
    for h in efi::handles(&efi::PCI_IO) {
        // SAFETY: the handle has the PCI I/O protocol.
        let Some(pci) = (unsafe { efi::protocol::<efi::PciIo>(h, &efi::PCI_IO) }) else { continue };
        let (mut id, mut class) = (0u32, 0u32);
        // Config space dwords 0 (vendor, device) and 2 (revision, class code), 32-bit reads.
        let ok = unsafe {
            ((*pci).pci_read)(pci, 2, 0, 1, &mut id as *mut u32 as *mut c_void) == efi::SUCCESS
                && ((*pci).pci_read)(pci, 2, 8, 1, &mut class as *mut u32 as *mut c_void) == efi::SUCCESS
        };
        let (vendor, device) = (id as u16, (id >> 16) as u16);
        if !ok || vendor == 0xffff {
            continue;
        }
        let (mut seg, mut bus, mut dev, mut func) = (0, 0, 0, 0);
        unsafe { ((*pci).get_location)(pci, &mut seg, &mut bus, &mut dev, &mut func) };
        let at = alloc::format!("{seg:04x}:{bus:02x}:{dev:02x}.{func}");
        if let Some(d) = devices::find(vendor, device).filter(|d| d.class == Class::Network) {
            log!("tier auto: {} {vendor:04x}:{device:04x} at {at} is supported: native kernel", d.driver);
            return true;
        }
        if class >> 24 == 0x02 {
            seen.push(alloc::format!("{vendor:04x}:{device:04x} at {at}"));
        }
    }
    if seen.is_empty() {
        log!("tier auto: no network adapter found: linux");
    } else {
        log!("tier auto: no supported network adapter ({}): linux", seen.join(", "));
    }
    false
}

/// Starts Tier 1's systemd-boot from K10S-BOOT; returns only if that fails.
fn chainload(device: Handle) -> Status {
    let Some(path) = efi::file_device_path(device, SYSTEMD_BOOT) else {
        log!("K10S-BOOT has no device path");
        return efi::LOAD_ERROR;
    };
    let mut h: Handle = null_mut();
    // SAFETY: a device path built from the volume's; the firmware (or shim) loads and verifies the image.
    let s = unsafe { (efi::bs().load_image)(0, efi::image(), path.as_ptr(), core::ptr::null(), 0, &mut h) };
    if s != efi::SUCCESS {
        log!("cannot load /{SYSTEMD_BOOT}: {}", efi::St(s));
        return s;
    }
    let s = unsafe { (efi::bs().start_image)(h, null_mut(), null_mut()) };
    log!("/{SYSTEMD_BOOT} returned: {}", efi::St(s));
    if s == efi::SUCCESS {
        efi::ABORTED
    } else {
        s
    }
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    log!("PANIC: {info}");
    efi::stall_ms(5000);
    // SAFETY: back to the firmware, which tries its next boot option.
    unsafe { (efi::bs().exit)(efi::image(), efi::ABORTED, 0, core::ptr::null()) };
    loop {
        unsafe { core::arch::asm!("cli; hlt") };
    }
}
