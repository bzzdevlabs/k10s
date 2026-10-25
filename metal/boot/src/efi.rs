//! The UEFI interfaces k10boot uses (UEFI 2.10), declared by hand: system table, boot and runtime services, and the
//! loaded-image, device-path, file-system, PCI I/O and graphics-output protocols. Plus the console, a pool
//! allocator, files and variables on top of them.

use alloc::string::String;
use alloc::vec::Vec;
use core::alloc::{GlobalAlloc, Layout};
use core::ffi::c_void;
use core::fmt;
use core::ptr::{null, null_mut};
use core::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

pub type Handle = *mut c_void;
pub type Status = usize;
type Unused = usize;

pub const SUCCESS: Status = 0;
const ERROR: usize = 1 << 63;
pub const INVALID_PARAMETER: Status = ERROR | 2;
pub const BUFFER_TOO_SMALL: Status = ERROR | 5;
pub const NOT_FOUND: Status = ERROR | 14;
pub const ABORTED: Status = ERROR | 21;
pub const LOAD_ERROR: Status = ERROR | 1;

/// A status for messages.
pub struct St(pub Status);

impl fmt::Display for St {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names = [
            "success", "load error", "invalid parameter", "unsupported", "bad buffer size", "buffer too small", "not ready",
            "device error", "write protected", "out of resources", "volume corrupted", "volume full", "no media", "media changed",
            "not found", "access denied", "no response", "no mapping", "timeout", "not started", "already started", "aborted",
            "ICMP error", "TFTP error", "protocol error", "incompatible version", "security violation",
        ];
        match names.get(self.0 & !ERROR) {
            Some(n) if self.0 == 0 || self.0 & ERROR != 0 => f.write_str(n),
            _ => write!(f, "status {:#x}", self.0),
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Guid(pub u32, pub u16, pub u16, pub [u8; 8]);

impl Guid {
    pub fn from_bytes(b: [u8; 16]) -> Guid {
        Guid(
            u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            u16::from_le_bytes([b[4], b[5]]),
            u16::from_le_bytes([b[6], b[7]]),
            b[8..].try_into().unwrap(),
        )
    }
}

pub const LOADED_IMAGE: Guid = Guid(0x5b1b31a1, 0x9562, 0x11d2, [0x8e, 0x3f, 0x00, 0xa0, 0xc9, 0x69, 0x72, 0x3b]);
pub const DEVICE_PATH: Guid = Guid(0x09576e91, 0x6d3f, 0x11d2, [0x8e, 0x39, 0x00, 0xa0, 0xc9, 0x69, 0x72, 0x3b]);
pub const SIMPLE_FILE_SYSTEM: Guid = Guid(0x964e5b22, 0x6459, 0x11d2, [0x8e, 0x39, 0x00, 0xa0, 0xc9, 0x69, 0x72, 0x3b]);
const FILE_INFO: Guid = Guid(0x09576e92, 0x6d3f, 0x11d2, [0x8e, 0x39, 0x00, 0xa0, 0xc9, 0x69, 0x72, 0x3b]);
pub const GRAPHICS_OUTPUT: Guid = Guid(0x9042a9de, 0x23dc, 0x4a38, [0x96, 0xfb, 0x7a, 0xde, 0xd0, 0x80, 0x51, 0x6a]);
pub const BLOCK_IO: Guid = Guid(0x964e5b21, 0x6459, 0x11d2, [0x8e, 0x39, 0x00, 0xa0, 0xc9, 0x69, 0x72, 0x3b]);
pub const PCI_IO: Guid = Guid(0x4cf5b200, 0x68b8, 0x4ca5, [0x9e, 0xec, 0xb2, 0x3e, 0x3f, 0x50, 0x02, 0x9a]);
pub const ACPI_20_TABLE: Guid = Guid(0x8868e871, 0xe4f1, 0x11d3, [0xbc, 0x22, 0x00, 0x80, 0xc7, 0x3c, 0x88, 0x81]);
pub const ACPI_10_TABLE: Guid = Guid(0xeb9d2d30, 0x2d88, 0x11d3, [0x9a, 0x16, 0x00, 0x90, 0x27, 0x3f, 0xc1, 0x4d]);
pub const GLOBAL_VARIABLE: Guid = Guid(0x8be4df61, 0x93ca, 0x11d2, [0xaa, 0x0d, 0x00, 0xe0, 0x98, 0x03, 0x2b, 0x8c]);

/// `EFI_MEMORY_TYPE` values k10boot allocates or checks.
pub const LOADER_CODE: u32 = 1;
pub const LOADER_DATA: u32 = 2;
/// `EFI_MEMORY_RUNTIME`: the region is used by runtime services.
pub const MEMORY_RUNTIME: u64 = 1 << 63;

#[repr(C)]
pub struct TableHeader {
    pub signature: u64,
    pub revision: u32,
    pub header_size: u32,
    pub crc32: u32,
    reserved: u32,
}

#[repr(C)]
pub struct SystemTable {
    pub hdr: TableHeader,
    pub firmware_vendor: *const u16,
    pub firmware_revision: u32,
    pub console_in_handle: Handle,
    pub con_in: *mut c_void,
    pub console_out_handle: Handle,
    pub con_out: *mut TextOutput,
    pub standard_error_handle: Handle,
    pub std_err: *mut TextOutput,
    pub runtime_services: *mut RuntimeServices,
    pub boot_services: *mut BootServices,
    pub number_of_table_entries: usize,
    pub configuration_table: *const ConfigurationTable,
}

#[repr(C)]
pub struct ConfigurationTable {
    pub vendor_guid: Guid,
    pub vendor_table: *const c_void,
}

#[repr(C)]
pub struct BootServices {
    pub hdr: TableHeader,
    raise_tpl: Unused,
    restore_tpl: Unused,
    pub allocate_pages: unsafe extern "efiapi" fn(u32, u32, usize, *mut u64) -> Status,
    pub free_pages: unsafe extern "efiapi" fn(u64, usize) -> Status,
    pub get_memory_map: unsafe extern "efiapi" fn(*mut usize, *mut u8, *mut usize, *mut usize, *mut u32) -> Status,
    pub allocate_pool: unsafe extern "efiapi" fn(u32, usize, *mut *mut u8) -> Status,
    pub free_pool: unsafe extern "efiapi" fn(*mut u8) -> Status,
    events: [Unused; 6],
    protocol_interfaces: [Unused; 3],
    pub handle_protocol: unsafe extern "efiapi" fn(Handle, *const Guid, *mut *mut c_void) -> Status,
    reserved: Unused,
    register_protocol_notify: Unused,
    locate_handle: Unused,
    pub locate_device_path: unsafe extern "efiapi" fn(*const Guid, *mut *const u8, *mut Handle) -> Status,
    install_configuration_table: Unused,
    pub load_image: unsafe extern "efiapi" fn(u8, Handle, *const u8, *const c_void, usize, *mut Handle) -> Status,
    pub start_image: unsafe extern "efiapi" fn(Handle, *mut usize, *mut *mut u16) -> Status,
    pub exit: unsafe extern "efiapi" fn(Handle, Status, usize, *const u16) -> Status,
    unload_image: Unused,
    pub exit_boot_services: unsafe extern "efiapi" fn(Handle, usize) -> Status,
    get_next_monotonic_count: Unused,
    pub stall: unsafe extern "efiapi" fn(usize) -> Status,
    pub set_watchdog_timer: unsafe extern "efiapi" fn(usize, u64, usize, *const u16) -> Status,
    pub connect_controller: unsafe extern "efiapi" fn(Handle, *const Handle, *const c_void, u8) -> Status,
    disconnect_controller: Unused,
    open_protocols: [Unused; 4],
    pub locate_handle_buffer: unsafe extern "efiapi" fn(u32, *const Guid, *const c_void, *mut usize, *mut *mut Handle) -> Status,
    pub locate_protocol: unsafe extern "efiapi" fn(*const Guid, *const c_void, *mut *mut c_void) -> Status,
}

#[repr(C)]
pub struct RuntimeServices {
    pub hdr: TableHeader,
    time_and_mapping: [Unused; 6],
    pub get_variable: unsafe extern "efiapi" fn(*const u16, *const Guid, *mut u32, *mut usize, *mut u8) -> Status,
    get_next_variable_name: Unused,
    pub set_variable: unsafe extern "efiapi" fn(*const u16, *const Guid, u32, usize, *const u8) -> Status,
    get_next_high_monotonic_count: Unused,
    pub reset_system: unsafe extern "efiapi" fn(u32, Status, usize, *const c_void) -> !,
}

#[repr(C)]
pub struct TextOutput {
    reset: Unused,
    pub output_string: unsafe extern "efiapi" fn(*mut TextOutput, *const u16) -> Status,
}

#[repr(C)]
pub struct LoadedImage {
    pub revision: u32,
    pub parent_handle: Handle,
    pub system_table: *mut SystemTable,
    pub device_handle: Handle,
    pub file_path: *const u8,
    reserved: *mut c_void,
    pub load_options_size: u32,
    pub load_options: *const u8,
    pub image_base: *mut c_void,
    pub image_size: u64,
    pub image_code_type: u32,
    pub image_data_type: u32,
    unload: Unused,
}

#[repr(C)]
pub struct SimpleFileSystem {
    revision: u64,
    open_volume: unsafe extern "efiapi" fn(*mut SimpleFileSystem, *mut *mut File) -> Status,
}

#[repr(C)]
pub struct File {
    revision: u64,
    open: unsafe extern "efiapi" fn(*mut File, *mut *mut File, *const u16, u64, u64) -> Status,
    close: unsafe extern "efiapi" fn(*mut File) -> Status,
    delete: Unused,
    read: unsafe extern "efiapi" fn(*mut File, *mut usize, *mut u8) -> Status,
    write: Unused,
    get_position: Unused,
    set_position: Unused,
    get_info: unsafe extern "efiapi" fn(*mut File, *const Guid, *mut usize, *mut u8) -> Status,
}

#[repr(C)]
pub struct GraphicsOutput {
    query_mode: Unused,
    set_mode: Unused,
    blt: Unused,
    pub mode: *const GraphicsMode,
}

#[repr(C)]
pub struct GraphicsMode {
    pub max_mode: u32,
    pub mode: u32,
    pub info: *const GraphicsModeInfo,
    pub size_of_info: usize,
    pub frame_buffer_base: u64,
    pub frame_buffer_size: usize,
}

#[repr(C)]
pub struct GraphicsModeInfo {
    pub version: u32,
    pub horizontal_resolution: u32,
    pub vertical_resolution: u32,
    /// 0 RGB, 1 BGR (8 bits per color, 32 per pixel), 2 bit mask, 3 blit only (no frame buffer).
    pub pixel_format: u32,
    /// Red, green, blue, reserved.
    pub pixel_masks: [u32; 4],
    pub pixels_per_scan_line: u32,
}

#[repr(C)]
pub struct PciIo {
    poll: [Unused; 2],
    mem: [Unused; 2],
    io: [Unused; 2],
    pub pci_read: unsafe extern "efiapi" fn(*mut PciIo, u32, u32, usize, *mut c_void) -> Status,
    pci_write: Unused,
    copy_mem: Unused,
    map: [Unused; 2],
    buffers: [Unused; 2],
    flush: Unused,
    pub get_location: unsafe extern "efiapi" fn(*mut PciIo, *mut usize, *mut usize, *mut usize, *mut usize) -> Status,
}

#[repr(C)]
pub struct BlockIoMedia {
    pub media_id: u32,
    pub removable: u8,
    pub present: u8,
    pub logical_partition: u8,
    pub read_only: u8,
    pub write_caching: u8,
    pub block_size: u32,
    pub io_align: u32,
    pub last_block: u64,
}

#[repr(C)]
pub struct BlockIo {
    pub revision: u64,
    pub media: *const BlockIoMedia,
    reset: Unused,
    pub read_blocks: unsafe extern "efiapi" fn(*mut BlockIo, u32, u64, usize, *mut c_void) -> Status,
    write_blocks: Unused,
    flush_blocks: Unused,
}

static SYSTEM_TABLE: AtomicPtr<SystemTable> = AtomicPtr::new(null_mut());
static IMAGE: AtomicPtr<c_void> = AtomicPtr::new(null_mut());
/// Cleared once boot services are gone: nothing may allocate or print any more. (Starting `true` also gives `.data`
/// file contents: with only zeroed statics it would have none and a raw-data pointer of 0, which shim refuses.)
static BOOT_SERVICES: AtomicBool = AtomicBool::new(true);

/// # Safety
/// The arguments of the image's entry point.
pub unsafe fn init(image: Handle, st: *mut SystemTable) {
    IMAGE.store(image, Ordering::Relaxed);
    SYSTEM_TABLE.store(st, Ordering::Relaxed);
}

pub fn image() -> Handle {
    IMAGE.load(Ordering::Relaxed)
}

pub fn system_table() -> &'static SystemTable {
    // SAFETY: set by `init` before anything else runs; the firmware keeps it alive.
    unsafe { &*SYSTEM_TABLE.load(Ordering::Relaxed) }
}

pub fn bs() -> &'static BootServices {
    // SAFETY: valid until ExitBootServices, after which only `exit_boot_services` callers run.
    unsafe { &*system_table().boot_services }
}

pub fn rt() -> &'static RuntimeServices {
    // SAFETY: the firmware's runtime services table.
    unsafe { &*system_table().runtime_services }
}

/// NUL-terminated UCS-2.
pub fn ucs2(s: &str) -> Vec<u16> {
    s.chars().map(|c| if (c as u32) < 0x10000 { c as u16 } else { b'?' as u16 }).chain([0]).collect()
}

pub struct Console;

impl fmt::Write for Console {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        if !BOOT_SERVICES.load(Ordering::Relaxed) {
            return Ok(());
        }
        let out = system_table().con_out;
        let mut buf = [0u16; 129];
        let mut n = 0;
        let flush = |buf: &mut [u16; 129], n: &mut usize| {
            buf[*n] = 0;
            // SAFETY: the firmware's console with a NUL-terminated string.
            unsafe { ((*out).output_string)(out, buf.as_ptr()) };
            *n = 0;
        };
        for c in s.chars() {
            if n >= 126 {
                flush(&mut buf, &mut n);
            }
            if c == '\n' {
                buf[n] = b'\r' as u16;
                n += 1;
            }
            buf[n] = if (c as u32) < 0x10000 { c as u16 } else { b'?' as u16 };
            n += 1;
        }
        flush(&mut buf, &mut n);
        Ok(())
    }
}

#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => {{
        use core::fmt::Write;
        let _ = writeln!($crate::efi::Console, "k10boot: {}", format_args!($($arg)*));
    }};
}

/// Pool allocations (EfiLoaderData), with the alignment Rust asks for.
pub struct Pool;

unsafe impl GlobalAlloc for Pool {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        if !BOOT_SERVICES.load(Ordering::Relaxed) {
            return null_mut();
        }
        // Pool memory is 8-byte aligned; more is had by over-allocating and keeping the pool pointer just below.
        let extra = if l.align() > 8 { l.align() } else { 0 };
        let mut p = null_mut();
        // SAFETY: boot services are up.
        if unsafe { (bs().allocate_pool)(LOADER_DATA, l.size() + extra, &mut p) } != SUCCESS {
            return null_mut();
        }
        if extra == 0 {
            return p;
        }
        let a = (p as usize + l.align()) & !(l.align() - 1);
        unsafe { ((a - 8) as *mut *mut u8).write(p) };
        a as *mut u8
    }

    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        if !BOOT_SERVICES.load(Ordering::Relaxed) {
            return;
        }
        let p = if l.align() > 8 { unsafe { ((p as usize - 8) as *const *mut u8).read() } } else { p };
        unsafe { (bs().free_pool)(p) };
    }
}

pub enum Where {
    Below(u64),
    At(u64),
}

/// `pages` 4 KiB pages of type `kind`.
pub fn allocate_pages(at: Where, kind: u32, pages: usize) -> Result<u64, Status> {
    let (how, mut addr) = match at {
        Where::Below(max) => (1, max - 1),
        Where::At(a) => (2, a),
    };
    // SAFETY: boot services are up.
    match unsafe { (bs().allocate_pages)(how, kind, pages, &mut addr) } {
        SUCCESS => Ok(addr),
        e => Err(e),
    }
}

/// # Safety
/// `h` must be a handle; `T` the protocol's interface for `guid`.
pub unsafe fn protocol<T>(h: Handle, guid: &Guid) -> Option<*mut T> {
    let mut p = null_mut();
    (unsafe { (bs().handle_protocol)(h, guid, &mut p) } == SUCCESS && !p.is_null()).then_some(p as *mut T)
}

/// Every handle that has the protocol `guid`.
pub fn handles(guid: &Guid) -> Vec<Handle> {
    let (mut n, mut buf) = (0usize, null_mut());
    // SAFETY: ByProtocol search; the firmware allocates the buffer, freed below.
    if unsafe { (bs().locate_handle_buffer)(2, guid, null(), &mut n, &mut buf) } != SUCCESS || buf.is_null() {
        return Vec::new();
    }
    let v = unsafe { core::slice::from_raw_parts(buf, n) }.to_vec();
    unsafe { (bs().free_pool)(buf as *mut u8) };
    v
}

/// Connects every driver to every device, recursively, as a firmware's full boot does; firmware that boots fast
/// connects only the boot device's path.
pub fn connect_all() {
    let (mut n, mut buf) = (0usize, null_mut());
    // SAFETY: AllHandles search; the firmware allocates the buffer, freed below.
    if unsafe { (bs().locate_handle_buffer)(0, null(), null(), &mut n, &mut buf) } != SUCCESS || buf.is_null() {
        return;
    }
    for &h in unsafe { core::slice::from_raw_parts(buf, n) } {
        unsafe { (bs().connect_controller)(h, null(), null(), 1) };
    }
    unsafe { (bs().free_pool)(buf as *mut u8) };
}

pub fn locate<T>(guid: &Guid) -> Option<*mut T> {
    let mut p = null_mut();
    // SAFETY: boot services are up.
    (unsafe { (bs().locate_protocol)(guid, null(), &mut p) } == SUCCESS && !p.is_null()).then_some(p as *mut T)
}

pub fn configuration_table(guid: &Guid) -> Option<u64> {
    let st = system_table();
    // SAFETY: the firmware's table of `number_of_table_entries` entries.
    let t = unsafe { core::slice::from_raw_parts(st.configuration_table, st.number_of_table_entries) };
    t.iter().find(|e| e.vendor_guid == *guid).map(|e| e.vendor_table as u64)
}

pub fn stall_ms(ms: usize) {
    unsafe { (bs().stall)(ms * 1000) };
}

/// Reads variable `name` into `buf`: its length, `Ok(None)` if there is none.
pub fn get_variable(name: &str, guid: &Guid, buf: &mut [u8]) -> Result<Option<usize>, Status> {
    let n = ucs2(name);
    let mut size = buf.len();
    match unsafe { (rt().get_variable)(n.as_ptr(), guid, null_mut(), &mut size, buf.as_mut_ptr()) } {
        SUCCESS => Ok(Some(size)),
        NOT_FOUND => Ok(None),
        e => Err(e),
    }
}

pub fn set_variable(name: &str, guid: &Guid, attributes: u32, data: &[u8]) -> Result<(), Status> {
    let n = ucs2(name);
    match unsafe { (rt().set_variable)(n.as_ptr(), guid, attributes, data.len(), data.as_ptr()) } {
        SUCCESS => Ok(()),
        e => Err(e),
    }
}

/// Whether the firmware enforces Secure Boot: the `SecureBoot` variable is 1, or cannot be read (fails closed).
pub fn secure_boot() -> bool {
    let mut b = [0u8; 1];
    !matches!(get_variable("SecureBoot", &GLOBAL_VARIABLE, &mut b), Ok(None) | Ok(Some(1)) if b[0] == 0)
}

/// The memory map: (buffer, number of descriptors, descriptor size, map key). `buf` is reused when big enough.
pub fn memory_map(buf: &mut Vec<u8>) -> Result<(usize, usize, usize), Status> {
    loop {
        let (mut size, mut key, mut dsize, mut ver) = (buf.len(), 0usize, 0usize, 0u32);
        match unsafe { (bs().get_memory_map)(&mut size, buf.as_mut_ptr(), &mut key, &mut dsize, &mut ver) } {
            SUCCESS => return Ok((size / dsize.max(1), dsize, key)),
            BUFFER_TOO_SMALL => buf.resize(size + 16 * dsize.max(48), 0),
            e => return Err(e),
        }
    }
}

/// One memory descriptor of a map from [`memory_map`]: (type, physical start, pages, attributes).
pub fn descriptor(map: &[u8], dsize: usize, i: usize) -> (u32, u64, u64, u64) {
    let d = &map[i * dsize..];
    let u64_at = |o: usize| u64::from_le_bytes(d[o..o + 8].try_into().unwrap());
    (u32::from_le_bytes(d[..4].try_into().unwrap()), u64_at(8), u64_at(24), u64_at(32))
}

/// ExitBootServices with the map `key`; on success nothing may allocate or print any more.
pub fn exit_boot_services(key: usize) -> Result<(), Status> {
    match unsafe { (bs().exit_boot_services)(image(), key) } {
        SUCCESS => {
            BOOT_SERVICES.store(false, Ordering::Relaxed);
            Ok(())
        }
        e => Err(e),
    }
}

/// The root directory of a volume.
pub struct Volume(*mut File);

impl Drop for Volume {
    /// Closes the root directory: the firmware's FAT driver should have nothing of ours open when the next loader
    /// (systemd-boot, which writes the partition) starts. Never runs after ExitBootServices (the jump never returns).
    fn drop(&mut self) {
        // SAFETY: the directory this volume opened, boot services up.
        unsafe { ((*self.0).close)(self.0) };
    }
}

/// A file's contents and its modification time (`EFI_TIME`).
pub struct FileData {
    pub data: Vec<u8>,
    pub mtime: [u8; 16],
}

impl Volume {
    pub fn open(device: Handle) -> Option<Volume> {
        // SAFETY: the device's file system protocol.
        let fs = unsafe { protocol::<SimpleFileSystem>(device, &SIMPLE_FILE_SYSTEM)? };
        let mut root = null_mut();
        (unsafe { ((*fs).open_volume)(fs, &mut root) } == SUCCESS).then_some(Volume(root))
    }

    /// The file at `path` (`/` separated, from the root), at most `max` bytes: `Ok(None)` if there is none.
    pub fn read(&self, path: &str, max: usize) -> Result<Option<FileData>, String> {
        let name = ucs2(&alloc::format!("\\{}", path.replace('/', "\\")));
        let mut f = null_mut();
        // SAFETY: an open directory; read-only.
        match unsafe { ((*self.0).open)(self.0, &mut f, name.as_ptr(), 1, 0) } {
            SUCCESS => {}
            NOT_FOUND => return Ok(None),
            e => return Err(alloc::format!("/{path}: {}", St(e))),
        }
        let r = read_open(f, max).map_err(|e| alloc::format!("/{path}: {e}"));
        unsafe { ((*f).close)(f) };
        r.map(Some)
    }

    pub fn exists(&self, path: &str) -> bool {
        let name = ucs2(&alloc::format!("\\{}", path.replace('/', "\\")));
        let mut f = null_mut();
        // SAFETY: an open directory; read-only.
        if unsafe { ((*self.0).open)(self.0, &mut f, name.as_ptr(), 1, 0) } != SUCCESS {
            return false;
        }
        unsafe { ((*f).close)(f) };
        true
    }
}

fn read_open(f: *mut File, max: usize) -> Result<FileData, String> {
    let mut info = alloc::vec![0u8; 512];
    loop {
        let mut size = info.len();
        match unsafe { ((*f).get_info)(f, &FILE_INFO, &mut size, info.as_mut_ptr()) } {
            SUCCESS => break,
            BUFFER_TOO_SMALL => info.resize(size, 0),
            e => return Err(alloc::format!("{}", St(e))),
        }
    }
    let len = u64::from_le_bytes(info[8..16].try_into().unwrap());
    let attr = u64::from_le_bytes(info[72..80].try_into().unwrap());
    if attr & 0x10 != 0 {
        return Err("a directory".into());
    }
    if len > max as u64 {
        return Err(alloc::format!("{len} bytes, more than {max}"));
    }
    let mut data = alloc::vec![0u8; len as usize];
    let mut at = 0;
    while at < data.len() {
        let mut n = data.len() - at;
        let e = match unsafe { ((*f).read)(f, &mut n, data.as_mut_ptr().add(at)) } {
            SUCCESS if n > 0 => {
                at += n;
                continue;
            }
            SUCCESS => "short read".into(),
            e => alloc::format!("{}", St(e)),
        };
        // The file may be a key.
        k10_mesh::crypto::zeroize(&mut data);
        return Err(e);
    }
    Ok(FileData { data, mtime: info[56..72].try_into().unwrap() })
}

/// The nodes of `device`'s device path, without its end node.
pub fn device_path(device: Handle) -> Option<Vec<u8>> {
    // SAFETY: a device path protocol is a sequence of nodes (type, subtype, u16 length) ending in an end node.
    let dp = unsafe { protocol::<u8>(device, &DEVICE_PATH)? };
    let mut v = Vec::new();
    let mut p = dp as *const u8;
    loop {
        let (t, len) = unsafe { (*p, u16::from_le_bytes([*p.add(2), *p.add(3)]) as usize) };
        if t == 0x7f || len < 4 {
            break;
        }
        v.extend_from_slice(unsafe { core::slice::from_raw_parts(p, len) });
        p = unsafe { p.add(len) };
    }
    Some(v)
}

/// The handle nearest to `path`'s end (its own, or an ancestor's) that has protocol `guid`.
pub fn handle_on_path(guid: &Guid, path: &[u8]) -> Option<Handle> {
    let mut dp = path.to_vec();
    dp.extend_from_slice(&[0x7f, 0xff, 4, 0]);
    let (mut p, mut h): (*const u8, Handle) = (dp.as_ptr(), null_mut());
    // SAFETY: a well-formed device path (with its end node) that outlives the call.
    (unsafe { (bs().locate_device_path)(guid, &mut p, &mut h) } == SUCCESS && !h.is_null()).then_some(h)
}

/// The device path of `device` with a file path node for `path` appended (for LoadImage).
pub fn file_device_path(device: Handle, path: &str) -> Option<Vec<u8>> {
    let mut v = device_path(device)?;
    let name = ucs2(&alloc::format!("\\{}", path.replace('/', "\\")));
    let len = 4 + 2 * name.len();
    // Media device path, file path subtype.
    v.extend_from_slice(&[4, 4]);
    v.extend_from_slice(&(len as u16).to_le_bytes());
    name.iter().for_each(|c| v.extend_from_slice(&c.to_le_bytes()));
    v.extend_from_slice(&[0x7f, 0xff, 4, 0]);
    Some(v)
}
