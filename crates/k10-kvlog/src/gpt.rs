//! GUID partition tables (UEFI 2.10 §5.3): reading the header and entry array, finding a partition by type or name,
//! and the rewrite that grows the last partition to the end of a larger disk (primary and backup GPT; also the MBR
//! equivalent). Callers do the I/O.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::crc32;

/// Partitions grow to a multiple of this.
pub const ALIGN: u64 = 1 << 20;
/// Largest entry array read (4096 entries of 128 bytes; the usual array is 128 × 128).
pub const MAX_ARRAY: u64 = 4096 * 128;

/// A GUID in its on-disk byte order (the first three fields little-endian).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Guid(pub [u8; 16]);

const fn hex(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        b'A'..=b'F' => c - b'A' + 10,
        _ => panic!("not a hex digit"),
    }
}

impl Guid {
    /// From the text form `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx` (panics on anything else: for constants).
    pub const fn parse(s: &str) -> Guid {
        let s = s.as_bytes();
        assert!(s.len() == 36 && s[8] == b'-' && s[13] == b'-' && s[18] == b'-' && s[23] == b'-');
        let mut text = [0u8; 16];
        let (mut i, mut o) = (0, 0);
        while i < 36 {
            if s[i] == b'-' {
                i += 1;
                continue;
            }
            text[o] = hex(s[i]) << 4 | hex(s[i + 1]);
            i += 2;
            o += 1;
        }
        let mut g = text;
        // Mixed endian: the 32-, 16- and 16-bit fields are stored little-endian.
        g[0] = text[3];
        g[1] = text[2];
        g[2] = text[1];
        g[3] = text[0];
        g[4] = text[5];
        g[5] = text[4];
        g[6] = text[7];
        g[7] = text[6];
        Guid(g)
    }

    pub fn is_zero(&self) -> bool {
        self.0 == [0; 16]
    }
}

impl core::fmt::Display for Guid {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let g = &self.0;
        write!(f, "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-", g[3], g[2], g[1], g[0], g[5], g[4], g[7], g[6])?;
        write!(f, "{:02x}{:02x}-", g[8], g[9])?;
        g[10..].iter().try_for_each(|b| write!(f, "{b:02x}"))
    }
}

/// The partition type of `K10S-DATA` (both tiers).
pub const K10S_DATA: Guid = Guid::parse("6b10c5a0-da7a-4000-8000-000000000001");
/// EFI system partition (`K10S-BOOT`).
pub const ESP: Guid = Guid::parse("c12a7328-f81f-11d2-ba4b-00a0c93ec93b");
/// Linux filesystem data: `K10S-DATA` of images built before the store (ext4).
pub const LINUX_DATA: Guid = Guid::parse("0fc63daf-8483-4772-8e79-3d69d8477de4");
/// The partition name both tiers look for.
pub const DATA_NAME: &str = "K10S-DATA";

fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

fn u64_at(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

pub fn put_u32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_le_bytes());
}

pub fn put_u64(b: &mut [u8], o: usize, v: u64) {
    b[o..o + 8].copy_from_slice(&v.to_le_bytes());
}

/// A verified GPT header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Header {
    pub my_lba: u64,
    pub alternate_lba: u64,
    pub first_usable: u64,
    pub last_usable: u64,
    pub disk_guid: Guid,
    pub entries_lba: u64,
    pub entries: u32,
    pub entry_size: u32,
    pub entries_crc: u32,
}

impl Header {
    /// The header at the start of `block` (LBA 1, or the backup's), checksum verified.
    pub fn parse(block: &[u8]) -> Result<Header, &'static str> {
        if block.len() < 92 || &block[..8] != b"EFI PART" {
            return Err("no GPT header");
        }
        let size = u32_at(block, 12) as usize;
        if !(92..=block.len()).contains(&size) {
            return Err("bad GPT header size");
        }
        let mut h = block[..size].to_vec();
        let crc = u32_at(&h, 16);
        put_u32(&mut h, 16, 0);
        if crc32(&h) != crc {
            return Err("GPT header checksum mismatch");
        }
        let (n, esize) = (u32_at(&h, 80), u32_at(&h, 84));
        if esize < 128 || esize % 8 != 0 || n == 0 || n as u64 * esize as u64 > MAX_ARRAY {
            return Err("bad GPT entry array");
        }
        Ok(Header {
            my_lba: u64_at(&h, 24),
            alternate_lba: u64_at(&h, 32),
            first_usable: u64_at(&h, 40),
            last_usable: u64_at(&h, 48),
            disk_guid: Guid(h[56..72].try_into().unwrap()),
            entries_lba: u64_at(&h, 72),
            entries: n,
            entry_size: esize,
            entries_crc: u32_at(&h, 88),
        })
    }

    /// Bytes of the entry array.
    pub fn array_bytes(&self) -> usize {
        self.entries as usize * self.entry_size as usize
    }
}

/// A used partition entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// 1-based, as Linux numbers partitions.
    pub number: u32,
    pub type_guid: Guid,
    pub unique: Guid,
    pub first_lba: u64,
    pub last_lba: u64,
    pub attributes: u64,
    /// UTF-16LE, NUL-padded.
    pub name: [u16; 36],
}

impl Entry {
    pub fn name_is(&self, s: &str) -> bool {
        let n = self.name.iter().position(|&c| c == 0).unwrap_or(36);
        self.name[..n].iter().copied().eq(s.encode_utf16())
    }

    pub fn name(&self) -> String {
        let n = self.name.iter().position(|&c| c == 0).unwrap_or(36);
        char::decode_utf16(self.name[..n].iter().copied()).map(|c| c.unwrap_or('?')).collect()
    }

    pub fn blocks(&self) -> u64 {
        self.last_lba + 1 - self.first_lba
    }

    /// `K10S-DATA`: by type, or by name (images before the store had a Linux data type).
    pub fn is_data(&self) -> bool {
        self.type_guid == K10S_DATA || self.name_is(DATA_NAME)
    }
}

/// The used entries of an array (at least `h.array_bytes()` bytes), checksum verified.
pub fn entries(h: &Header, array: &[u8]) -> Result<Vec<Entry>, &'static str> {
    let array = array.get(..h.array_bytes()).ok_or("short GPT entry array")?;
    if crc32(array) != h.entries_crc {
        return Err("GPT entry array checksum mismatch");
    }
    let mut out = Vec::new();
    for (i, e) in array.chunks_exact(h.entry_size as usize).enumerate() {
        let type_guid = Guid(e[..16].try_into().unwrap());
        if type_guid.is_zero() {
            continue;
        }
        let (first_lba, last_lba) = (u64_at(e, 32), u64_at(e, 40));
        if last_lba < first_lba {
            continue;
        }
        let mut name = [0u16; 36];
        for (j, c) in name.iter_mut().enumerate() {
            *c = u16::from_le_bytes([e[56 + 2 * j], e[57 + 2 * j]]);
        }
        out.push(Entry {
            number: i as u32 + 1,
            type_guid,
            unique: Guid(e[16..32].try_into().unwrap()),
            first_lba,
            last_lba,
            attributes: u64_at(e, 48),
            name,
        });
    }
    Ok(out)
}

/// Recomputes a header's checksum.
pub fn seal_header(h: &mut [u8]) {
    let size = u32_at(h, 12) as usize;
    put_u32(h, 16, 0);
    let crc = crc32(&h[..size]);
    put_u32(h, 16, crc);
}

/// The GPT rewrite that grows partition `pno` (1-based) to the end of a disk of `total` logical blocks of `lbs`
/// bytes. Write order: entries and header at the backup location, flush, then the primary ones (until the primary
/// header is rewritten the old layout stays valid).
#[derive(Debug, PartialEq, Eq)]
pub struct Grow {
    pub primary: Vec<u8>,
    pub backup: Vec<u8>,
    pub entries: Vec<u8>,
    pub backup_entries_lba: u64,
    pub backup_lba: u64,
    /// The partition in logical blocks, after the change.
    pub start: u64,
    pub end: u64,
}

/// `Ok(None)` when the partition already ends at the disk's end (or cannot grow: another partition lies behind it)
/// and the backup GPT is in place.
pub fn grow(header: &[u8], entries: &[u8], lbs: u64, total: u64, pno: u32) -> Result<Option<Grow>, String> {
    if header.len() < 92 || &header[..8] != b"EFI PART" {
        return Err("no GPT header".into());
    }
    let hsize = u32_at(header, 12) as usize;
    if !(92..=header.len()).contains(&hsize) {
        return Err(format!("GPT header size {hsize}"));
    }
    let mut h = header[..hsize].to_vec();
    let crc = u32_at(&h, 16);
    put_u32(&mut h, 16, 0);
    if crc32(&h) != crc || u64_at(&h, 24) != 1 {
        return Err("GPT header checksum mismatch".into());
    }
    let (n, esize) = (u32_at(&h, 80) as usize, u32_at(&h, 84) as usize);
    if esize < 128 || esize % 8 != 0 || n == 0 || n > 4096 || entries.len() < n * esize {
        return Err(format!("GPT entry array {n} x {esize}"));
    }
    let mut e = entries[..n * esize].to_vec();
    if crc32(&e) != u32_at(&h, 88) {
        return Err("GPT entry array checksum mismatch".into());
    }
    let idx = pno.checked_sub(1).map(|i| i as usize).filter(|&i| i < n).ok_or("partition number outside the GPT")?;
    let ent = |i: usize| &e[i * esize..(i + 1) * esize];
    let used = |i: usize| ent(i)[..16].iter().any(|&b| b != 0);
    let (start, end) = (u64_at(ent(idx), 32), u64_at(ent(idx), 40));
    if !used(idx) || end < start {
        return Err(format!("partition {pno} is not in the GPT"));
    }
    if (0..n).any(|i| i != idx && used(i) && u64_at(ent(i), 40) > end) {
        return Ok(None);
    }
    let array_blocks = (n * esize).div_ceil(lbs as usize) as u64;
    let backup_lba = total.checked_sub(1).ok_or("empty disk")?;
    let backup_entries_lba = backup_lba.checked_sub(array_blocks).ok_or("disk too small")?;
    let last_usable = backup_entries_lba - 1;
    if last_usable < u64_at(&h, 48) {
        return Err("the disk is smaller than its GPT".into());
    }
    let align = (ALIGN / lbs).max(1);
    let new_end = ((last_usable + 1) / align * align).saturating_sub(1).max(end);
    if new_end == end && u64_at(&h, 32) == backup_lba {
        return Ok(None);
    }
    put_u64(&mut e[idx * esize..], 40, new_end);
    put_u64(&mut h, 32, backup_lba);
    put_u64(&mut h, 48, last_usable);
    put_u32(&mut h, 88, crc32(&e));
    let mut b = h.clone();
    put_u64(&mut b, 24, backup_lba);
    put_u64(&mut b, 32, 1);
    put_u64(&mut b, 72, backup_entries_lba);
    seal_header(&mut h);
    seal_header(&mut b);
    Ok(Some(Grow { primary: h, backup: b, entries: e, backup_entries_lba, backup_lba, start, end: new_end }))
}

/// The protective MBR (sector 0) of a grown GPT disk of `total` blocks, if `mbr` is one.
pub fn protective_mbr(mbr: &[u8], total: u64) -> Option<Vec<u8>> {
    if mbr.len() < 512 || mbr[510..512] != [0x55, 0xAA] || mbr[0x1BE + 4] != 0xEE {
        return None;
    }
    let mut m = mbr[..512].to_vec();
    put_u32(&mut m, 0x1BE + 12, (total - 1).min(u32::MAX as u64) as u32);
    Some(m)
}

/// The MBR rewrite that grows primary partition `pno` (1-4) to the end of a disk of `total` logical blocks: the new
/// sector 0 and the partition's start and end, or `Ok(None)` when it cannot grow.
pub fn grow_mbr(mbr: &[u8], lbs: u64, total: u64, pno: u32) -> Result<Option<(Vec<u8>, u64, u64)>, String> {
    if mbr.len() < 512 || mbr[510..512] != [0x55, 0xAA] {
        return Err("no partition table".into());
    }
    let idx = pno.checked_sub(1).filter(|&i| i < 4).ok_or("not a primary MBR partition")? as usize;
    let ent = |i: usize| &mbr[0x1BE + 16 * i..0x1BE + 16 * (i + 1)];
    let bounds = |i: usize| (u32_at(ent(i), 8) as u64, u32_at(ent(i), 12) as u64);
    if ent(0)[4] == 0xEE {
        return Err("protective MBR without a GPT".into());
    }
    let (start, len) = bounds(idx);
    if ent(idx)[4] == 0 || len == 0 {
        return Err(format!("partition {pno} is not in the MBR"));
    }
    let end = start + len - 1;
    if (0..4).any(|i| i != idx && ent(i)[4] != 0 && bounds(i).1 != 0 && bounds(i).0 + bounds(i).1 - 1 > end) {
        return Ok(None);
    }
    let align = (ALIGN / lbs).max(1);
    let new_end = (total / align * align).min(u32::MAX as u64 + 1).saturating_sub(1).max(end);
    if new_end == end {
        return Ok(None);
    }
    let mut m = mbr[..512].to_vec();
    put_u32(&mut m, 0x1BE + 16 * idx + 12, (new_end - start + 1) as u32);
    Ok(Some((m, start, new_end)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn guids() {
        let g = Guid::parse("C12A7328-F81F-11D2-BA4B-00A0C93EC93B");
        assert_eq!(g.0, [0x28, 0x73, 0x2a, 0xc1, 0x1f, 0xf8, 0xd2, 0x11, 0xba, 0x4b, 0x00, 0xa0, 0xc9, 0x3e, 0xc9, 0x3b]);
        assert_eq!(alloc::format!("{g}"), "c12a7328-f81f-11d2-ba4b-00a0c93ec93b");
        assert_eq!(alloc::format!("{K10S_DATA}"), "6b10c5a0-da7a-4000-8000-000000000001");
    }

    #[test]
    fn parses_and_finds() {
        let mut e = vec![0u8; 128 * 128];
        let names = ["K10S-BOOT", "K10S-DATA"];
        for (i, (t, n)) in [(ESP, names[0]), (K10S_DATA, names[1])].iter().enumerate() {
            let x = &mut e[i * 128..];
            x[..16].copy_from_slice(&t.0);
            x[16] = i as u8 + 1;
            put_u64(x, 32, 2048 + i as u64 * 10_000);
            put_u64(x, 40, 2048 + i as u64 * 10_000 + 9_999);
            for (j, c) in n.encode_utf16().enumerate() {
                x[56 + 2 * j..58 + 2 * j].copy_from_slice(&c.to_le_bytes());
            }
        }
        let mut h = vec![0u8; 512];
        h[..8].copy_from_slice(b"EFI PART");
        put_u32(&mut h, 12, 92);
        put_u64(&mut h, 24, 1);
        put_u64(&mut h, 72, 2);
        put_u32(&mut h, 80, 128);
        put_u32(&mut h, 84, 128);
        put_u32(&mut h, 88, crc32(&e));
        seal_header(&mut h);
        let hd = Header::parse(&h).unwrap();
        let ents = entries(&hd, &e).unwrap();
        assert_eq!(ents.len(), 2);
        let d = ents.iter().find(|x| x.is_data()).unwrap();
        assert_eq!((d.number, d.first_lba, d.blocks(), d.name()), (2, 12_048, 10_000, "K10S-DATA".into()));
        assert!(ents[0].name_is("K10S-BOOT") && !ents[0].name_is("K10S-BOO") && !ents[0].is_data());
        e[200] ^= 1;
        assert!(entries(&hd, &e).is_err());
        h[100] = 1;
        assert!(Header::parse(&h).is_ok(), "bytes past the header size are not covered");
        h[30] ^= 1;
        assert!(Header::parse(&h).is_err());
    }
}
