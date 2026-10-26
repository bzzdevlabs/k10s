//! Control-method power buttons, found in the DSDT and SSDTs without an AML interpreter. A power button device
//! (`_HID` PNP0C0C) is a control method one when its press reaches the OS as a general-purpose event: the GPE's
//! handler method in `\_GPE` (`_Lxx` level, `_Exx` edge; xx is the GPE number in hex) runs `Notify (BUTTON, 0x80)`.
//! The kernel enables those GPEs and takes the event itself instead of running the method. Not found this way: a
//! notify reached through another method (`_L1D` calling into a Super I/O device), an embedded controller's query
//! (`_Qxx`, laptops), or the Generic Event Device of hardware-reduced platforms (its `_EVT` reads a register).

use alloc::vec::Vec;

/// `Name (_HID, EisaId ("PNP0C0C"))` and `Name (_HID, "PNP0C0C")`.
const HID_EISA: &[u8] = b"\x08_HID\x0c\x41\xd0\x0c\x0c";
const HID_STR: &[u8] = b"\x08_HID\x0dPNP0C0C\x00";
const DEVICE_OP: [u8; 2] = [0x5b, 0x82];
const METHOD_OP: u8 = 0x14;
const NOTIFY_OP: u8 = 0x86;
const BYTE_PREFIX: u8 = 0x0a;

/// A GPE whose method notifies a power button, and whether it is level-triggered (`_Lxx`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Gpe {
    pub number: u16,
    pub level: bool,
}

/// PkgLength at `at`: (the package's length counted from `at`, the encoding's size).
fn pkg_length(aml: &[u8], at: usize) -> Option<(usize, usize)> {
    let lead = *aml.get(at)?;
    let extra = (lead >> 6) as usize;
    if extra == 0 {
        return Some(((lead & 0x3f) as usize, 1));
    }
    let mut len = (lead & 0x0f) as usize;
    for i in 0..extra {
        len |= (*aml.get(at + 1 + i)? as usize) << (4 + 8 * i);
    }
    Some((len, 1 + extra))
}

fn name_seg(b: &[u8]) -> bool {
    b.len() == 4 && (b[0].is_ascii_uppercase() || b[0] == b'_') && b[1..].iter().all(|&c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'_')
}

/// The NameString at `at`: (its last NameSeg, its size). The null name has no segment.
fn name_string(aml: &[u8], at: usize) -> Option<(Option<[u8; 4]>, usize)> {
    let mut i = at;
    if aml.get(i) == Some(&b'\\') {
        i += 1;
    } else {
        while aml.get(i) == Some(&b'^') {
            i += 1;
        }
    }
    let (count, start) = match *aml.get(i)? {
        0x00 => return Some((None, i + 1 - at)),
        0x2e => (2, i + 1),
        0x2f => (*aml.get(i + 1)? as usize, i + 2),
        _ => (1, i),
    };
    if count == 0 {
        return None;
    }
    let end = start + 4 * count;
    let segs = aml.get(start..end)?;
    if !segs.chunks(4).all(name_seg) {
        return None;
    }
    Some((Some(segs[end - start - 4..].try_into().unwrap()), end - at))
}

/// The NameSegs of the devices whose `_HID` is PNP0C0C: for each `_HID`, the innermost `Device` around it.
fn buttons(aml: &[u8]) -> Vec<[u8; 4]> {
    let mut out = Vec::new();
    for pat in [HID_EISA, HID_STR] {
        for hid in aml.windows(pat.len()).enumerate().filter(|(_, w)| *w == pat).map(|(i, _)| i) {
            // Devices nest: the last one starting before the _HID whose package reaches past it is the innermost.
            let mut best = None;
            for d in (0..hid).filter(|&d| aml[d..].starts_with(&DEVICE_OP)) {
                let Some((len, n)) = pkg_length(aml, d + 2) else { continue };
                let Some((Some(seg), _)) = name_string(aml, d + 2 + n) else { continue };
                if d + 2 + len > hid {
                    best = Some(seg);
                }
            }
            if let Some(seg) = best {
                if !out.contains(&seg) {
                    out.push(seg);
                }
            }
        }
    }
    out
}

/// GPEs whose `_Lxx`/`_Exx` method runs `Notify (B, 0x80)` for a power button B, in the AML of the DSDT and
/// SSDTs (each table's definition blocks, without their headers).
pub fn gpes(tables: &[&[u8]]) -> Vec<Gpe> {
    let names: Vec<[u8; 4]> = tables.iter().flat_map(|t| buttons(t)).collect();
    let mut out = Vec::new();
    if names.is_empty() {
        return out;
    }
    for aml in tables {
        for m in (0..aml.len()).filter(|&i| aml[i] == METHOD_OP) {
            let Some((len, n)) = pkg_length(aml, m + 1) else { continue };
            let Some((Some(seg), name_len)) = name_string(aml, m + 1 + n) else { continue };
            let level = match seg[1] {
                b'L' => true,
                b'E' => false,
                _ => continue,
            };
            let hex = core::str::from_utf8(&seg[2..]).ok().filter(|h| h.bytes().all(|c| c.is_ascii_hexdigit()));
            let Some(number) = hex.and_then(|h| u16::from_str_radix(h, 16).ok()) else { continue };
            if seg[0] != b'_' {
                continue;
            }
            // The body follows the name and the MethodFlags byte, up to the end of the package.
            let (body, end) = (m + 1 + n + name_len + 1, (m + 1 + len).min(aml.len()));
            let notifies = (body..end).any(|i| {
                aml[i] == NOTIFY_OP
                    && name_string(aml, i + 1).is_some_and(|(s, k)| {
                        s.is_some_and(|s| names.contains(&s)) && aml.get(i + 1 + k..i + 3 + k) == Some(&[BYTE_PREFIX, 0x80])
                    })
            });
            let gpe = Gpe { number, level };
            if notifies && !out.contains(&gpe) {
                out.push(gpe);
            }
        }
    }
    out
}
