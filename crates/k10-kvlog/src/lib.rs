//! The k10s data format, shared by the hosted node (Tier 1 included), the native kernel and its loader:
//!
//! * [`record`] — the KV record codec: `len:u32 | crc32:u32 | op:u8 | klen:u16 | key | value`. The hosted node's
//!   per-service log files (`DATA/kv/<service>.log`) are plain sequences of these records.
//! * [`store`] — `K10S-DATA`, the raw data partition both tiers use: a superblock pair, two kernel slots and a
//!   segmented log of the same records, namespaced, with checkpoints (compaction) and recovery that keeps exactly a
//!   prefix of each namespace's writes after a power cut; in format version 2, an object area beside the log for
//!   large content-addressed objects ([`store::objects`]). [`layout`] holds its on-disk structures.
//! * [`files`] — whole files (blobs, the manifest, node identity) as chunked records of a reserved namespace.
//! * [`gpt`] — GUID partition tables: finding `K10S-DATA`, growing it to the end of its disk.
//!
//! No I/O happens here: the store drives a [`store::Disk`] that each environment implements (a file or a block
//! device on Linux, NVMe/AHCI/virtio-blk in the native kernel).

#![no_std]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod crc;
pub mod files;
pub mod gpt;
pub mod layout;
pub mod record;
pub mod store;

pub use crc::crc32;
