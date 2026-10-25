//! First boot on a larger disk: `K10S-DATA` grows to the end of its disk. Images carry a small data partition; when it
//! is the last partition of the disk with free space behind it, init extends the partition (1 MiB aligned; on GPT
//! after moving the backup GPT to the disk's end, on an MBR disk up to 2^32 sectors), tells the kernel
//! (`BLKPG_RESIZE_PARTITION`) and, once mounted, grows the ext4 filesystem online (`EXT4_IOC_RESIZE_FS`). Other
//! partitions are never touched.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;

const BLKPG: libc::c_ulong = 0x1269;
const BLKPG_RESIZE_PARTITION: libc::c_int = 3;
/// `_IOW('f', 16, __u64)`
const EXT4_IOC_RESIZE_FS: libc::c_ulong = 0x4008_6610;

#[repr(C)]
struct BlkpgPartition {
    start: i64,
    length: i64,
    pno: libc::c_int,
    devname: [u8; 64],
    volname: [u8; 64],
}

#[repr(C)]
struct BlkpgIoctlArg {
    op: libc::c_int,
    flags: libc::c_int,
    datalen: libc::c_int,
    data: *mut BlkpgPartition,
}

use k10_kvlog::gpt::{grow as plan, grow_mbr as mbr_plan, protective_mbr};
#[cfg(test)]
use k10_kvlog::gpt::{put_u32, put_u64, seal_header};
#[cfg(test)]
use k10_kvlog::crc32;

fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

fn u64_at(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

fn sysfs_u64(path: &str) -> io::Result<u64> {
    fs::read_to_string(path)?.trim().parse().map_err(|_| io::Error::new(io::ErrorKind::InvalidData, path.to_string()))
}

/// Grows partition `/dev/NAME` to the end of its disk; returns its size in bytes when it changed.
pub fn partition(dev: &str) -> io::Result<Option<u64>> {
    let name = dev.trim_start_matches("/dev/");
    let pno = sysfs_u64(&format!("/sys/class/block/{name}/partition"))? as u32;
    let link = fs::canonicalize(format!("/sys/class/block/{name}"))?;
    let disk = link.parent().and_then(|p| p.file_name()).and_then(|d| d.to_str()).ok_or(io::ErrorKind::NotFound)?.to_string();
    let lbs = sysfs_u64(&format!("/sys/class/block/{disk}/queue/logical_block_size"))?;
    let total = sysfs_u64(&format!("/sys/class/block/{disk}/size"))? * 512 / lbs;
    let f = OpenOptions::new().read(true).write(true).open(format!("/dev/{disk}"))?;
    let read = |lba: u64, len: usize| -> io::Result<Vec<u8>> {
        let mut b = vec![0; len];
        f.read_exact_at(&mut b, lba * lbs)?;
        Ok(b)
    };
    let header = read(1, lbs as usize)?;
    if &header[..8] != b"EFI PART" {
        let Some((mbr, start, end)) = mbr_plan(&read(0, 512)?, lbs, total, pno).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))? else {
            return Ok(None);
        };
        f.write_all_at(&mbr, 0)?;
        f.sync_all()?;
        return resize(&f, pno, start, end, lbs).map(Some);
    }
    let array = u32_at(&header, 80) as u64 * u32_at(&header, 84) as u64;
    if array > 4096 * 512 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "GPT entry array too large"));
    }
    let entries = read(u64_at(&header, 72), array.div_ceil(lbs) as usize * lbs as usize)?;
    let p = match plan(&header, &entries, lbs, total, pno) {
        Ok(Some(p)) => p,
        Ok(None) => return Ok(None),
        Err(e) => return Err(io::Error::new(io::ErrorKind::InvalidData, e)),
    };
    // Whole blocks, zero-padded, backup first: until the primary header is rewritten, the old layout stays valid.
    let block = |b: &[u8]| {
        let mut v = b.to_vec();
        v.resize((b.len() as u64).div_ceil(lbs) as usize * lbs as usize, 0);
        v
    };
    f.write_all_at(&block(&p.entries), p.backup_entries_lba * lbs)?;
    f.write_all_at(&block(&p.backup), p.backup_lba * lbs)?;
    f.sync_all()?;
    f.write_all_at(&block(&p.entries), u64_at(&header, 72) * lbs)?;
    f.write_all_at(&block(&p.primary), lbs)?;
    // The protective MBR covers the whole disk (capped at 2^32 - 1 blocks).
    if let Some(mbr) = protective_mbr(&read(0, 512)?, total) {
        f.write_all_at(&mbr, 0)?;
    }
    f.sync_all()?;
    resize(&f, pno, p.start, p.end, lbs).map(Some)
}

/// Tells the kernel the new bounds of partition `pno` of the disk `f`; returns its size in bytes.
fn resize(f: &File, pno: u32, start: u64, end: u64, lbs: u64) -> io::Result<u64> {
    let bytes = (end - start + 1) * lbs;
    let mut part = BlkpgPartition { start: (start * lbs) as i64, length: bytes as i64, pno: pno as libc::c_int, devname: [0; 64], volname: [0; 64] };
    let mut arg = BlkpgIoctlArg { op: BLKPG_RESIZE_PARTITION, flags: 0, datalen: std::mem::size_of::<BlkpgPartition>() as libc::c_int, data: &mut part };
    // SAFETY: valid descriptor and argument structures that outlive the call.
    if unsafe { libc::ioctl(f.as_raw_fd(), BLKPG as _, &mut arg) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(bytes)
}

/// Grows the ext4 filesystem of `dev`, mounted on `mountpoint`, to fill the partition; returns the new size in bytes
/// when it changed.
pub fn ext4(dev: &str, mountpoint: &str) -> io::Result<Option<u64>> {
    let name = dev.trim_start_matches("/dev/");
    let bytes = sysfs_u64(&format!("/sys/class/block/{name}/size"))? * 512;
    let mut sb = [0u8; 1024];
    File::open(dev)?.read_exact_at(&mut sb, 1024)?;
    if u16::from_le_bytes([sb[0x38], sb[0x39]]) != 0xEF53 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "not ext4"));
    }
    let bsize = 1024u64 << u32_at(&sb, 0x18);
    let hi = if u32_at(&sb, 0x60) & 0x80 != 0 { u32_at(&sb, 0x150) as u64 } else { 0 };
    let blocks = hi << 32 | u32_at(&sb, 0x04) as u64;
    let mut want = bytes / bsize;
    if want <= blocks {
        return Ok(None);
    }
    let dir = File::open(mountpoint)?;
    // SAFETY: valid descriptor and a u64 that outlives the call.
    if unsafe { libc::ioctl(dir.as_raw_fd(), EXT4_IOC_RESIZE_FS as _, &mut want) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Some(want * bsize))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A GPT of 128 entries with `parts` (start, end), its backup header at `alt`.
    fn gpt(parts: &[(u64, u64)], alt: u64) -> (Vec<u8>, Vec<u8>) {
        let mut e = vec![0u8; 128 * 128];
        for (i, &(s, end)) in parts.iter().enumerate() {
            e[i * 128] = 1;
            put_u64(&mut e[i * 128..], 32, s);
            put_u64(&mut e[i * 128..], 40, end);
        }
        let mut h = vec![0u8; 512];
        h[..8].copy_from_slice(b"EFI PART");
        put_u32(&mut h, 8, 0x0001_0000);
        put_u32(&mut h, 12, 92);
        put_u64(&mut h, 24, 1);
        put_u64(&mut h, 32, alt);
        put_u64(&mut h, 40, 34);
        put_u64(&mut h, 48, alt - 33);
        put_u64(&mut h, 72, 2);
        put_u32(&mut h, 80, 128);
        put_u32(&mut h, 84, 128);
        put_u32(&mut h, 88, crc32(&e));
        seal_header(&mut h);
        (h, e)
    }

    #[test]
    fn crc32_matches_ieee() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn grows_last_partition_to_aligned_disk_end() {
        // 64 MiB image written to a 1 GiB disk: backup GPT still at the image's end.
        let img = 64 * 2048;
        let (h, e) = gpt(&[(2048, 4095), (4096, img - 34)], img - 1);
        let total = 1024 * 2048;
        let p = plan(&h, &e, 512, total, 2).unwrap().unwrap();
        assert_eq!((p.start, p.end), (4096, total - 2048 - 1));
        assert_eq!((p.backup_lba, p.backup_entries_lba), (total - 1, total - 33));
        assert_eq!(u64_at(&p.primary, 32), total - 1);
        assert_eq!(u64_at(&p.primary, 48), total - 34);
        assert_eq!((u64_at(&p.backup, 24), u64_at(&p.backup, 32), u64_at(&p.backup, 72)), (total - 1, 1, total - 33));
        // Both headers and the array verify; applying the plan again is a no-op.
        assert_eq!(plan(&p.primary, &p.entries, 512, total, 2), Ok(None));
        let mut b = p.backup.clone();
        let crc = u32_at(&b, 16);
        put_u32(&mut b, 16, 0);
        assert_eq!(crc32(&b[..92]), crc);
        assert_eq!(u64_at(&p.entries, 40), 4095, "other partitions keep their bounds");
    }

    #[test]
    fn leaves_partitions_that_are_not_last_alone() {
        let total = 1024 * 2048;
        let (h, e) = gpt(&[(2048, 4095), (4096, 8191), (8192, 16383)], total - 1);
        assert_eq!(plan(&h, &e, 512, total, 2), Ok(None));
    }

    #[test]
    fn moves_the_backup_even_without_room_to_grow() {
        let img = 64 * 2048;
        let (h, e) = gpt(&[(2048, img - 34)], img - 1);
        let total = img + 10;
        let p = plan(&h, &e, 512, total, 1).unwrap().unwrap();
        assert_eq!((p.end, p.backup_lba), (img - 34, total - 1));
    }

    #[test]
    fn rejects_corrupt_or_foreign_tables() {
        let total = 1024 * 2048;
        let (mut h, e) = gpt(&[(2048, 4095)], 2048 * 64 - 1);
        assert!(plan(&h, &e, 512, total, 2).is_err(), "unused slot");
        assert!(plan(&h, &e, 512, 4096, 1).is_err(), "disk smaller than its GPT");
        let mut bad = e.clone();
        bad[40] ^= 1;
        assert!(plan(&h, &bad, 512, total, 1).is_err());
        h[60] ^= 1;
        assert!(plan(&h, &e, 512, total, 1).is_err());
    }

    /// An MBR with primary partitions (type, start, sectors).
    fn mbr(parts: &[(u8, u32, u32)]) -> Vec<u8> {
        let mut m = vec![0u8; 512];
        for (i, &(t, start, len)) in parts.iter().enumerate() {
            m[0x1BE + 16 * i + 4] = t;
            put_u32(&mut m, 0x1BE + 16 * i + 8, start);
            put_u32(&mut m, 0x1BE + 16 * i + 12, len);
        }
        m[510..512].copy_from_slice(&[0x55, 0xAA]);
        m
    }

    #[test]
    fn grows_the_last_mbr_partition() {
        // The Pi layout: K10S-BOOT, slots A and B, K10S-DATA last.
        let m = mbr(&[(0x0c, 2048, 65536), (0x0e, 67584, 262144), (0x0e, 329728, 262144), (0x83, 591872, 524288)]);
        let total = 32 << 21; // 32 GiB
        let (new, start, end) = mbr_plan(&m, 512, total, 4).unwrap().unwrap();
        assert_eq!((start, end), (591872, total - 1));
        assert_eq!(u32_at(&new, 0x1BE + 48 + 12) as u64, total - 591872);
        assert_eq!(new[..0x1BE + 48], m[..0x1BE + 48], "other entries unchanged");
        assert_eq!(mbr_plan(&new, 512, total, 4), Ok(None));
        assert_eq!(mbr_plan(&m, 512, total, 2), Ok(None), "not the last partition");
        // Unaligned disk end: stops at the last 1 MiB boundary; past 2 TiB: at 2^32 sectors.
        assert_eq!(mbr_plan(&m, 512, total + 100, 4).unwrap().unwrap().2, total - 1);
        assert_eq!(mbr_plan(&m, 512, 8 << 31, 4).unwrap().unwrap().2, u32::MAX as u64);
    }

    #[test]
    fn rejects_foreign_mbrs() {
        assert!(mbr_plan(&[0u8; 512], 512, 1 << 21, 1).is_err(), "no signature");
        assert!(mbr_plan(&mbr(&[(0xEE, 1, u32::MAX)]), 512, 1 << 21, 1).is_err(), "protective MBR");
        assert!(mbr_plan(&mbr(&[(0x83, 2048, 2048)]), 512, 1 << 21, 2).is_err(), "empty entry");
        assert!(mbr_plan(&mbr(&[(0x83, 2048, 2048)]), 512, 1 << 21, 5).is_err(), "logical partition");
    }

    #[test]
    fn handles_4k_sectors() {
        let img = 16 * 256;
        let (mut h, mut e) = gpt(&[(256, img - 6)], img - 1);
        put_u64(&mut h, 40, 6);
        put_u64(&mut h, 48, img - 6);
        put_u64(&mut e, 32, 256);
        put_u32(&mut h, 88, crc32(&e));
        seal_header(&mut h);
        let total = 256 * 256;
        let p = plan(&h, &e, 4096, total, 1).unwrap().unwrap();
        // 128 entries of 128 bytes are 4 blocks of 4 KiB.
        assert_eq!((p.backup_entries_lba, p.end), (total - 5, total - 256 - 1));
    }
}
