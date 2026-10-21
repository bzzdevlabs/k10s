//! QEMU's firmware configuration device (fw_cfg) over its I/O ports: write an item's selector to 0x510, read its
//! bytes from 0x511. Items are numbered (`-smp` count) or named files (`-fw_cfg name=opt/…,file=PATH`) listed in
//! the file directory. Secrets (cluster and admin keys) come this way, never on the kernel command line, which the
//! host shows in its process list.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::arch;

const SELECTOR: u16 = 0x510;
const DATA: u16 = 0x511;
const SIGNATURE: u16 = 0x00;
/// Number of CPUs (`-smp`), little-endian u16.
pub const NB_CPUS: u16 = 0x05;
const FILE_DIR: u16 = 0x19;
/// Directory entry: size (BE u32), selector (BE u16), reserved (u16), NUL-padded name (56 bytes).
const DIR_ENTRY: usize = 64;
/// Largest secret file accepted (an admin key is at most 512 characters).
const SECRET_MAX: usize = 4096;

/// Reads the next bytes of the selected item.
fn read_on(buf: &mut [u8]) {
    for b in buf {
        *b = arch::inb(DATA);
    }
}

fn read(key: u16, buf: &mut [u8]) {
    arch::outw(SELECTOR, key);
    read_on(buf);
}

/// Whether the device is present (its signature item reads "QEMU").
pub fn present() -> bool {
    let mut sig = [0u8; 4];
    read(SIGNATURE, &mut sig);
    &sig == b"QEMU"
}

/// A numbered little-endian u16 item (`None` without fw_cfg).
pub fn read_u16(key: u16) -> Option<u16> {
    if !present() {
        return None;
    }
    let mut b = [0u8; 2];
    read(key, &mut b);
    Some(u16::from_le_bytes(b))
}

/// The contents of file `name`: `Ok(None)` if there is no such file (or no fw_cfg), `Err(size)` if it is larger
/// than `max` bytes.
pub fn file(name: &str, max: usize) -> Result<Option<Vec<u8>>, u32> {
    if !present() {
        return Ok(None);
    }
    let mut count = [0u8; 4];
    read(FILE_DIR, &mut count);
    let mut found = None;
    for _ in 0..u32::from_be_bytes(count) {
        let mut e = [0u8; DIR_ENTRY];
        read_on(&mut e);
        let n = &e[8..];
        if &n[..n.iter().position(|&c| c == 0).unwrap_or(n.len())] == name.as_bytes() {
            found = Some((u32::from_be_bytes([e[0], e[1], e[2], e[3]]), u16::from_be_bytes([e[4], e[5]])));
            break;
        }
    }
    let Some((size, key)) = found else { return Ok(None) };
    if size as usize > max {
        return Err(size);
    }
    // Sized up front: no reallocation leaves a copy of a secret behind.
    let mut v = alloc::vec![0u8; size as usize];
    read(key, &mut v);
    Ok(Some(v))
}

/// Reads and parses secret file `name` (`opt/k10s/…`), then wipes the raw bytes. `Ok(None)`: not given.
pub fn secret<T>(name: &str, parse: impl FnOnce(&str) -> Result<T, &'static str>) -> Result<Option<T>, String> {
    let mut raw = match file(name, SECRET_MAX) {
        Ok(Some(b)) => b,
        Ok(None) => return Ok(None),
        Err(n) => return Err(format!("fw_cfg {name}: {n} bytes, more than a key file can hold ({SECRET_MAX})")),
    };
    let r = match core::str::from_utf8(&raw) {
        Ok(s) => parse(s).map(Some).map_err(|e| format!("fw_cfg {name}: {e}")),
        Err(_) => Err(format!("fw_cfg {name}: not text")),
    };
    k10_mesh::crypto::zeroize(&mut raw);
    r
}
