//! Landlock: the node's files are its data directory (read-write) and a few read-only system files.

use super::Policy;
use crate::sys::Fd;
use std::ffi::CString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

const CREATE_RULESET_VERSION: u32 = 1 << 0;
const RULE_PATH_BENEATH: u32 = 1;

const FS_EXECUTE: u64 = 1 << 0;
const FS_WRITE_FILE: u64 = 1 << 1;
const FS_READ_FILE: u64 = 1 << 2;
const FS_READ_DIR: u64 = 1 << 3;
const FS_REMOVE_DIR: u64 = 1 << 4;
const FS_REMOVE_FILE: u64 = 1 << 5;
const FS_MAKE_DIR: u64 = 1 << 7;
const FS_MAKE_REG: u64 = 1 << 8;
/// ABI 2 (Linux 5.19).
const FS_REFER: u64 = 1 << 13;
/// ABI 3 (Linux 6.2).
const FS_TRUNCATE: u64 = 1 << 14;
/// ABI 5 (Linux 6.10).
const FS_IOCTL_DEV: u64 = 1 << 15;
/// ABI 6 (Linux 6.12).
const SCOPE_ABSTRACT_UNIX_SOCKET: u64 = 1 << 0;
const SCOPE_SIGNAL: u64 = 1 << 1;

/// Data directories: regular files and directories only (no exec, special files or symlinks).
const DATA_RW: u64 =
    FS_READ_FILE | FS_WRITE_FILE | FS_READ_DIR | FS_REMOVE_DIR | FS_REMOVE_FILE | FS_MAKE_DIR | FS_MAKE_REG | FS_REFER | FS_TRUNCATE;
const RO: u64 = FS_READ_FILE | FS_READ_DIR;
const FILE_RIGHTS: u64 = FS_EXECUTE | FS_WRITE_FILE | FS_READ_FILE | FS_TRUNCATE | FS_IOCTL_DEV;

/// `struct landlock_ruleset_attr`; older kernels accept it whole as long as the fields they do not know are 0.
#[repr(C)]
struct RulesetAttr {
    handled_access_fs: u64,
    handled_access_net: u64,
    scoped: u64,
}

#[repr(C, packed)]
struct PathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
}

/// The Landlock ABI of the running kernel (0: none).
pub fn abi() -> u32 {
    // SAFETY: the documented version query (NULL attr, size 0).
    let r = unsafe { libc::syscall(libc::SYS_landlock_create_ruleset, std::ptr::null::<RulesetAttr>(), 0usize, CREATE_RULESET_VERSION) };
    r.max(0) as u32
}

/// Every filesystem right this ABI knows.
fn fs_rights(abi: u32) -> u64 {
    let mut r = (1 << 13) - 1;
    if abi >= 2 {
        r |= FS_REFER;
    }
    if abi >= 3 {
        r |= FS_TRUNCATE;
    }
    if abi >= 5 {
        r |= FS_IOCTL_DEV;
    }
    r
}

/// Optional read-only paths: glibc malloc (get_nprocs, the heap-shrink policy) and panic backtraces (the binary).
fn optional_paths() -> Vec<PathBuf> {
    let mut v = vec![PathBuf::from("/sys/devices/system/cpu"), PathBuf::from("/proc/sys/vm/overcommit_memory")];
    v.extend(std::env::current_exe());
    v
}

fn add_rule(rs: &Fd, path: &Path, access: u64, fs: u64) -> io::Result<()> {
    let c = CString::new(path.as_os_str().as_bytes()).map_err(io::Error::other)?;
    // SAFETY: valid C string; O_PATH opens without reading.
    let fd = Fd(unsafe { libc::open(c.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) });
    if fd.0 < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: an all-zero stat is a valid out-parameter of fstat on our fd.
    let is_dir = unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        libc::fstat(fd.0, &mut st) == 0 && st.st_mode & libc::S_IFMT == libc::S_IFDIR
    };
    let allowed = access & fs & if is_dir { u64::MAX } else { FILE_RIGHTS };
    let rule = PathBeneathAttr { allowed_access: allowed, parent_fd: fd.0 };
    // SAFETY: valid ruleset fd and rule pointer.
    if unsafe { libc::syscall(libc::SYS_landlock_add_rule, rs.0, RULE_PATH_BENEATH, &rule as *const PathBeneathAttr, 0u32) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Restricts the calling thread and the threads it creates afterwards. Fails without Landlock or when a required path
/// (data and write directories) cannot be added. Returns the ABI.
pub(super) fn apply(p: &Policy) -> Result<u32, String> {
    let abi = abi();
    if abi == 0 {
        return Err("Landlock is not available (Linux >= 5.13 with landlock in lsm=); --sandbox seccomp-only runs without it".into());
    }
    let fs = fs_rights(abi);
    let scoped = if abi >= 6 { SCOPE_SIGNAL | SCOPE_ABSTRACT_UNIX_SOCKET } else { 0 };
    let attr = RulesetAttr { handled_access_fs: fs, handled_access_net: 0, scoped };
    // SAFETY: attr is valid for its size.
    let rs = Fd(unsafe {
        libc::syscall(libc::SYS_landlock_create_ruleset, &attr as *const RulesetAttr, std::mem::size_of::<RulesetAttr>(), 0u32)
    } as i32);
    if rs.0 < 0 {
        return Err(format!("landlock_create_ruleset: {}", io::Error::last_os_error()));
    }
    for d in p.data_dir.iter().chain(&p.write_dirs) {
        add_rule(&rs, d, DATA_RW, fs).map_err(|e| format!("landlock rule {}: {e}", d.display()))?;
    }
    for f in optional_paths() {
        let _ = add_rule(&rs, &f, RO, fs);
    }
    super::sc(libc::SYS_landlock_restrict_self, [rs.0 as _, 0, 0, 0, 0]).map_err(|e| format!("landlock_restrict_self: {e}"))?;
    Ok(abi)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::tests::{exited, in_child};

    fn no_new_privs() -> bool {
        super::super::sc(libc::SYS_prctl, [libc::PR_SET_NO_NEW_PRIVS as _, 1, 0, 0, 0]).is_ok()
    }

    #[test]
    fn confines_the_filesystem() {
        if abi() == 0 {
            eprintln!("landlock not supported by this kernel: skipped");
            return;
        }
        let tmp = std::env::temp_dir();
        let dir = tmp.join(format!("k10s-landlock-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let outside = tmp.join(format!("k10s-landlock-outside-{}", std::process::id()));
        std::fs::write(&outside, b"secret").unwrap();
        crate::sys::open_proc_self();
        let (d, o) = (dir.clone(), outside.clone());
        let st = in_child(move || {
            if !no_new_privs() || apply(&Policy { data_dir: Some(d.clone()), write_dirs: vec![] }).is_err() {
                return 90;
            }
            if std::fs::write(d.join("new"), b"x").is_err() || std::fs::create_dir(d.join("sub")).is_err() {
                return 1;
            }
            let denied = |p: &Path| std::fs::read(p).err().and_then(|e| e.raw_os_error()) == Some(libc::EACCES);
            if !denied(&o) || !denied(Path::new("/etc/passwd")) {
                return 2;
            }
            // /proc/self is not reachable by path (environ, mem); the descriptors opened before still read.
            if !denied(Path::new("/proc/self/environ")) || !denied(Path::new("/proc/self/statm")) || crate::sys::rss_bytes() == 0 {
                return 3;
            }
            // Nothing can be executed.
            let argv = [c"/bin/true".as_ptr(), std::ptr::null()];
            // SAFETY: valid argv/envp.
            unsafe { libc::execve(c"/bin/true".as_ptr(), argv.as_ptr(), argv[1..].as_ptr()) };
            if io::Error::last_os_error().raw_os_error() != Some(libc::EACCES) {
                return 4;
            }
            0
        });
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(&outside);
        assert!(exited(st, 0), "status {st:#x}");
    }

    #[test]
    fn required_paths_must_exist() {
        if abi() == 0 {
            return;
        }
        let missing = std::env::temp_dir().join(format!("k10s-landlock-missing-{}", std::process::id()));
        let st = in_child(move || {
            let p = Policy { data_dir: None, write_dirs: vec![missing.clone()] };
            match (no_new_privs(), apply(&p)) {
                (true, Err(e)) if e.contains("landlock rule") => 0,
                _ => 1,
            }
        });
        assert!(exited(st, 0), "status {st:#x}");
    }
}
