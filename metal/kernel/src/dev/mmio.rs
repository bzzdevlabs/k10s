//! Memory-mapped devices without a bus to enumerate: virtio-mmio slots, described on the kernel command line in
//! Linux's format, `virtio_mmio.device=SIZE@BASE:IRQ[:ID]` (QEMU microvm without ACPI, Firecracker) or in the ACPI
//! DSDT (acpi.rs).

use alloc::vec::Vec;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MmioSlot {
    pub base: u64,
    pub size: u64,
    /// GSI of its interrupt line.
    pub irq: u32,
}

/// A number with an optional K/M/G suffix, decimal or 0x-prefixed hex (Linux `memparse`).
fn number(s: &str) -> Option<u64> {
    let (digits, shift) = match s.as_bytes().last()? {
        b'K' | b'k' => (&s[..s.len() - 1], 10),
        b'M' | b'm' => (&s[..s.len() - 1], 20),
        b'G' | b'g' => (&s[..s.len() - 1], 30),
        _ => (s, 0),
    };
    let v = match digits.strip_prefix("0x").or_else(|| digits.strip_prefix("0X")) {
        Some(h) => u64::from_str_radix(h, 16).ok()?,
        None => digits.parse().ok()?,
    };
    v.checked_shl(shift)
}

fn parse(v: &str) -> Option<MmioSlot> {
    let (size, rest) = v.split_once('@')?;
    let mut parts = rest.split(':');
    let base = number(parts.next()?)?;
    let irq = parts.next()?.parse().ok()?;
    Some(MmioSlot { base, size: number(size)?, irq })
}

/// Every well-formed `virtio_mmio.device=` word (malformed ones are reported and skipped).
pub fn from_cmdline(cmdline: &str) -> Vec<MmioSlot> {
    let mut v = Vec::new();
    for w in cmdline.split_ascii_whitespace() {
        let Some(arg) = w.strip_prefix("virtio_mmio.device=") else { continue };
        match parse(arg) {
            Some(s) if s.size >= 0x100 => v.push(s),
            _ => crate::kprintln!("virtio-mmio: ignoring malformed {w}"),
        }
    }
    v
}
