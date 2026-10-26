//! I/O APICs (from the MADT or MP table): every pin masked at boot, and a pin routed to a vector on the boot CPU
//! for devices that signal through interrupt lines rather than MSI-X (virtio-mmio). PCI INTx routing would need
//! `_PRT` (AML), so PCI devices use MSI-X only.

use alloc::vec::Vec;

use crate::mem::{PhysAddr, VirtAddr};
use crate::sync::IrqLock;

const IOREGSEL: u64 = 0x00;
const IOWIN: u64 = 0x10;
const REG_VER: u32 = 0x01;
const MASKED: u64 = 1 << 16;

struct Chip {
    base: VirtAddr,
    gsi_base: u32,
    pins: u32,
}

static CHIPS: IrqLock<Vec<Chip>> = IrqLock::new(Vec::new());

fn read(base: VirtAddr, reg: u32) -> u32 {
    unsafe {
        core::ptr::write_volatile((base + IOREGSEL).ptr::<u32>(), reg);
        core::ptr::read_volatile((base + IOWIN).ptr::<u32>())
    }
}

fn write(base: VirtAddr, reg: u32, v: u32) {
    unsafe {
        core::ptr::write_volatile((base + IOREGSEL).ptr::<u32>(), reg);
        core::ptr::write_volatile((base + IOWIN).ptr::<u32>(), v);
    }
}

/// Writes a redirection entry: mask it, set the destination (high half), then the low half (vector, mode, mask).
fn set_entry(c: &Chip, pin: u32, v: u64) {
    write(c.base, 0x10 + 2 * pin, MASKED as u32);
    write(c.base, 0x11 + 2 * pin, (v >> 32) as u32);
    write(c.base, 0x10 + 2 * pin, v as u32);
}

/// Registers the I/O APICs and masks every pin (firmware may leave some unmasked).
pub fn init(chips: &[crate::acpi::IoApic]) {
    let mut v = CHIPS.lock();
    let mut next = 0;
    for c in chips {
        let Some(base) = super::paging::map_mmio(PhysAddr::new(c.addr), 4096) else { continue };
        let pins = ((read(base, REG_VER) >> 16) & 0xFF) + 1;
        let gsi_base = if c.gsi_base == crate::acpi::GSI_AFTER_PREVIOUS { next } else { c.gsi_base };
        next = gsi_base + pins;
        let chip = Chip { base, gsi_base, pins };
        // Only pins left unmasked are written (each access is an exit under a hypervisor).
        for p in (0..pins).filter(|&p| read(chip.base, 0x10 + 2 * p) as u64 & MASKED == 0) {
            set_entry(&chip, p, MASKED);
        }
        v.push(chip);
    }
}

/// Routes `gsi` to `vector` on the CPU with local APIC id `dest` (fixed delivery, physical destination). MPS INTI
/// `flags`: polarity in bits 1:0 (3 = active low), trigger mode in bits 3:2 (3 = level); 0 = the bus default
/// (active high, edge). False if no I/O APIC has that GSI or the APIC id needs more than 8 bits.
pub fn route(gsi: u32, vector: u8, dest: u32, flags: u16) -> bool {
    let v = CHIPS.lock();
    let Some(c) = v.iter().find(|c| (c.gsi_base..c.gsi_base + c.pins).contains(&gsi)) else { return false };
    if dest > 255 {
        return false;
    }
    let low = flags & 3 == 3;
    let level = (flags >> 2) & 3 == 3;
    let e = vector as u64 | (low as u64) << 13 | (level as u64) << 15 | (dest as u64) << 56;
    set_entry(c, gsi - c.gsi_base, e);
    true
}
