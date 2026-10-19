//! Executable memory (W^X) and the `CompiledModule` API (std).

use crate::abi::VmCtx;
use crate::blob::{BlobMeta, CodeBlob};
use crate::codegen::{compile_blob, CompileError, CompileOptions};
use k10_ir::{Module, Trap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

/// A page-aligned mapping holding generated code: written while RW, then switched to RX (never both).
struct ExecMem {
    ptr: *mut u8,
    len: usize,
}

// SAFETY: after construction the mapping is read+execute only and never mutated; it is unmapped in Drop when
// the last owner goes away. Sharing pointers to immutable memory across threads is sound.
unsafe impl Send for ExecMem {}
unsafe impl Sync for ExecMem {}

fn os_error() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(-1)
}

impl ExecMem {
    fn new(blob: &CodeBlob) -> Result<ExecMem, CompileError> {
        // SAFETY: sysconf has no preconditions.
        let page = match unsafe { libc::sysconf(libc::_SC_PAGESIZE) } {
            p if p > 0 => p as usize,
            _ => 4096,
        };
        let len = blob.len().max(1).div_ceil(page) * page;
        // SAFETY: anonymous private mapping, no address hint; checked for MAP_FAILED below.
        let p = unsafe {
            libc::mmap(
                core::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if p == libc::MAP_FAILED {
            return Err(CompileError::Memory(os_error()));
        }
        let mem = ExecMem { ptr: p as *mut u8, len };
        {
            // SAFETY: the mapping is `len` bytes of RW memory owned exclusively by `mem`.
            let dst = unsafe { core::slice::from_raw_parts_mut(mem.ptr, len) };
            let ok = blob.write_to(dst);
            debug_assert!(ok);
        }
        // SAFETY: (ptr, len) is exactly the mapping created above.
        if unsafe { libc::mprotect(p, len, libc::PROT_READ | libc::PROT_EXEC) } != 0 {
            return Err(CompileError::Memory(os_error())); // Drop unmaps
        }
        Ok(mem)
    }
}

impl Drop for ExecMem {
    fn drop(&mut self) {
        // SAFETY: (ptr, len) is a mapping we own; no code from it can be running (we hold the last reference,
        // and invoke borrows the module for the whole call).
        unsafe {
            libc::munmap(self.ptr as *mut libc::c_void, self.len);
        }
    }
}

/// A compiled module in executable memory: immutable, `Send + Sync`, shareable via `Arc` by every instance.
pub struct CompiledModule {
    meta: BlobMeta,
    mem: ExecMem,
}

impl core::fmt::Debug for CompiledModule {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CompiledModule")
            .field("funcs", &self.meta.funcs().len())
            .field("code_bytes", &self.meta.code_len())
            .field("code", &self.mem.ptr)
            .finish()
    }
}

/// Validates and compiles a module for this CPU ([`CompileOptions::default`]).
pub fn compile(m: &Module) -> Result<CompiledModule, CompileError> {
    compile_with(m, &CompileOptions::default())
}

/// Validates and compiles a module with explicit options. The code must only run on CPUs that support
/// `opts.features`.
pub fn compile_with(m: &Module, opts: &CompileOptions) -> Result<CompiledModule, CompileError> {
    let blob = compile_blob(m, opts)?;
    if let Some(dir) = dump_dir() {
        dump(dir, &blob);
    }
    let mem = ExecMem::new(&blob)?;
    Ok(CompiledModule { meta: blob.into_meta(), mem })
}

impl CompiledModule {
    /// Function index (imports first) of an export.
    pub fn export(&self, name: &str) -> Option<u32> {
        self.meta.export(name)
    }

    /// `(params, results)` of a function index (imports first).
    pub fn func_type(&self, idx: u32) -> Option<(u8, u8)> {
        self.meta.func_type(idx)
    }

    /// Runs defined function `func` on the current native stack (see [`BlobMeta::invoke`] for the invocation
    /// errors and docs/JIT-ABI.md for the protocol). Re-entrant: a host function may call `invoke` again, with
    /// the same or another context.
    ///
    /// # Safety
    /// `ctx` must satisfy the requirements of [`BlobMeta::invoke`]: valid for the whole call, `mem_base` valid
    /// for `mem_mask + 1 + 8` bytes and not aliased by Rust references, `host_fns` covering every import,
    /// a valid interrupt hook, and stack limits that leave room for host functions.
    pub unsafe fn invoke(&self, ctx: *mut VmCtx, func: u32, args: &[i64]) -> Result<Option<i64>, Trap> {
        // SAFETY: the code was loaded with `write_to` into `mem`, which is now RX and outlives this call; it was
        // compiled for features the caller vouched for (default: this CPU); ctx requirements are forwarded.
        unsafe { self.meta.invoke(self.mem.ptr, ctx, func, args) }
    }

    /// Size of the generated machine code (bytes).
    pub fn code_bytes(&self) -> usize {
        self.meta.code_len()
    }

    /// Start of the executable code.
    pub fn code_ptr(&self) -> *const u8 {
        self.mem.ptr
    }

    pub fn meta(&self) -> &BlobMeta {
        &self.meta
    }
}

fn dump_dir() -> Option<&'static std::path::Path> {
    static DIR: OnceLock<Option<std::path::PathBuf>> = OnceLock::new();
    DIR.get_or_init(|| std::env::var_os("K10_JIT_DUMP").filter(|d| !d.is_empty()).map(Into::into)).as_deref()
}

/// `K10_JIT_DUMP=dir`: writes `k10jit-<pid>-<n>.bin` (raw code, disassemble with
/// `objdump -D -b binary -mi386:x86-64 file.bin`) and a `.txt` map of function offsets. Best effort.
fn dump(dir: &std::path::Path, blob: &CodeBlob) {
    use std::fmt::Write as _;
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let stem = format!("k10jit-{}-{:06}", std::process::id(), n);
    let _ = std::fs::create_dir_all(dir);
    let _ = std::fs::write(dir.join(format!("{stem}.bin")), blob.code());
    let meta = blob.meta();
    let mut map = String::new();
    let _ = writeln!(map, "entry            {:#08x}", meta.entry_offset());
    let _ = writeln!(map, "helper_table     {:#08x}", meta.helper_table_offset());
    for (i, f) in meta.funcs().iter().enumerate() {
        let idx = i + meta.num_imports();
        let names: Vec<&str> =
            meta.exports.iter().filter(|(_, fi)| *fi as usize == idx).map(|(n, _)| n.as_str()).collect();
        let _ = writeln!(map, "func {idx:<5} {:#08x}  params={} results={} {}", f.offset, f.params, f.results, names.join(","));
    }
    let _ = std::fs::write(dir.join(format!("{stem}.txt")), map);
}
