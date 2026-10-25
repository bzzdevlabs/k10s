//! K10S-DATA through the firmware's Block I/O protocol: the GPT of the disk that holds K10S-BOOT (else of any disk),
//! the store's superblock (k10_kvlog::layout), and the native kernel slots that cluster-rolled updates write. The
//! loader only reads here; the kernel writes the slots and their state.

use alloc::string::String;
use alloc::vec::Vec;
use core::ffi::c_void;

use k10_kvlog::{gpt, layout};

use crate::efi::{self, BlockIo, Handle, St};

/// Reads go through this aligned buffer (the firmware may want any alignment up to a page).
const CHUNK: usize = 1 << 20;

pub struct Data {
    bio: *mut BlockIo,
    media_id: u32,
    bs: u64,
    first: u64,
    /// The store's superblock (`None`: a blank partition, no store yet).
    pub sb: Option<layout::Super>,
    /// The partition's unique GUID.
    pub unique: [u8; 16],
    /// The disk's PCI controller: (vendor, device, class code), when the firmware tells.
    pub controller: Option<(u16, u16, u32)>,
}

fn media(bio: *mut BlockIo) -> &'static efi::BlockIoMedia {
    // SAFETY: a Block I/O protocol's media record, valid while boot services run.
    unsafe { &*(*bio).media }
}

/// Reads `len` bytes at block `lba` of a disk.
fn read(bio: *mut BlockIo, lba: u64, len: usize) -> Result<Vec<u8>, String> {
    let m = media(bio);
    let bs = m.block_size as usize;
    let layout = core::alloc::Layout::from_size_align(CHUNK, 4096).unwrap();
    // SAFETY: a fresh allocation, freed below.
    let buf = unsafe { alloc::alloc::alloc(layout) };
    if buf.is_null() || bs == 0 || CHUNK % bs != 0 {
        return Err("no memory".into());
    }
    let mut out = Vec::with_capacity(len);
    let mut res = Ok(());
    while out.len() < len {
        let n = (len - out.len()).div_ceil(bs).min(CHUNK / bs) * bs;
        let at = lba + (out.len() / bs) as u64;
        // SAFETY: `n` bytes of the buffer; the firmware reads whole blocks.
        let s = unsafe { ((*bio).read_blocks)(bio, m.media_id, at, n, buf as *mut c_void) };
        if s != efi::SUCCESS {
            res = Err(alloc::format!("reading block {at}: {}", St(s)));
            break;
        }
        let take = n.min(len - out.len());
        // SAFETY: the firmware filled `n` ≥ `take` bytes.
        out.extend_from_slice(unsafe { core::slice::from_raw_parts(buf, take) });
    }
    // SAFETY: allocated above with this layout.
    unsafe { alloc::alloc::dealloc(buf, layout) };
    res.map(|()| out)
}

/// K10S-DATA of the disk under `esp` (the boot volume's handle), else of the first disk that has one, with its store's
/// superblock; `None` when there is none.
pub fn open(esp: Handle) -> Option<Data> {
    let esp_path = efi::device_path(esp).unwrap_or_default();
    let mut disks: Vec<(Handle, Vec<u8>)> = efi::handles(&efi::BLOCK_IO)
        .into_iter()
        .filter_map(|h| {
            // SAFETY: the handle has the Block I/O protocol.
            let bio = unsafe { efi::protocol::<BlockIo>(h, &efi::BLOCK_IO)? };
            let m = media(bio);
            (m.logical_partition == 0 && m.present != 0).then(|| (h, efi::device_path(h).unwrap_or_default()))
        })
        .collect();
    // The disk whose path the boot volume's continues comes first.
    disks.sort_by_key(|(_, p)| !(!p.is_empty() && esp_path.starts_with(p)));
    for (h, path) in disks {
        // SAFETY: as above.
        let Some(bio) = (unsafe { efi::protocol::<BlockIo>(h, &efi::BLOCK_IO) }) else { continue };
        let bs = media(bio).block_size as u64;
        let Ok(hd) = read(bio, 1, bs as usize) else { continue };
        let Ok(header) = gpt::Header::parse(&hd) else { continue };
        let Ok(array) = read(bio, header.entries_lba, header.array_bytes()) else { continue };
        let Some(e) = gpt::entries(&header, &array).ok().and_then(|v| v.into_iter().find(|e| e.is_data())) else { continue };
        let Ok(head) = read(bio, e.first_lba, 2 * layout::UNIT as usize) else { continue };
        let unit = layout::UNIT as usize;
        let sb = layout::pick(&head[..layout::SUPER_LEN], &head[unit..unit + layout::SUPER_LEN]).ok();
        let controller = efi::handle_on_path(&efi::PCI_IO, &path).and_then(|p| {
            // SAFETY: the handle has the PCI I/O protocol.
            let pci = unsafe { efi::protocol::<efi::PciIo>(p, &efi::PCI_IO)? };
            let (mut id, mut class) = (0u32, 0u32);
            let ok = unsafe {
                ((*pci).pci_read)(pci, 2, 0, 1, &mut id as *mut u32 as *mut c_void) == efi::SUCCESS
                    && ((*pci).pci_read)(pci, 2, 8, 1, &mut class as *mut u32 as *mut c_void) == efi::SUCCESS
            };
            ok.then_some((id as u16, (id >> 16) as u16, class >> 8))
        });
        return Some(Data { bio, media_id: media(bio).media_id, bs, first: e.first_lba, sb, unique: e.unique.0, controller });
    }
    None
}

impl Data {
    /// The signed image in kernel slot `slot` (its recorded length).
    pub fn slot(&self, slot: u8) -> Result<Vec<u8>, String> {
        let len = self.sb.map_or(0, |sb| sb.slots[slot as usize].len) as usize;
        if media(self.bio).media_id != self.media_id {
            return Err("the disk changed".into());
        }
        read(self.bio, self.first + layout::slot_offset(slot) / self.bs, len)
    }
}
