//! `--user`: the unprivileged identity of a node started as root, and the capability drop.

use super::sc;
use crate::sys::Fd;
use std::ffi::{CStr, CString};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct User {
    pub name: String,
    pub uid: libc::uid_t,
    pub gid: libc::gid_t,
}

/// Calls a `get*nam_r` function, growing its buffer on ERANGE.
fn lookup<T>(f: impl Fn(&mut T, &mut [u8], &mut *mut T) -> libc::c_int) -> Option<T> {
    let mut buf = vec![0u8; 4096];
    loop {
        // SAFETY: an all-zero passwd/group is a valid out-parameter.
        let mut out: T = unsafe { std::mem::zeroed() };
        let mut res: *mut T = std::ptr::null_mut();
        match f(&mut out, &mut buf, &mut res) {
            0 if !res.is_null() => return Some(out),
            libc::ERANGE if buf.len() < 1 << 24 => buf.resize(buf.len() * 4, 0),
            _ => return None,
        }
    }
}

fn getpw(name: &CStr) -> Option<(libc::uid_t, libc::gid_t)> {
    // SAFETY: valid C string, out-pointers and buffer of the given length.
    lookup(|pw: &mut libc::passwd, buf, res| unsafe { libc::getpwnam_r(name.as_ptr(), pw, buf.as_mut_ptr().cast(), buf.len(), res) })
        .map(|pw| (pw.pw_uid, pw.pw_gid))
}

fn getgr(name: &CStr) -> Option<libc::gid_t> {
    // SAFETY: as in getpw.
    lookup(|gr: &mut libc::group, buf, res| unsafe { libc::getgrnam_r(name.as_ptr(), gr, buf.as_mut_ptr().cast(), buf.len(), res) })
        .map(|gr| gr.gr_gid)
}

impl User {
    /// `USER[:GROUP]`, names or numeric ids; the group defaults to the user's primary group (required for a bare uid).
    pub fn resolve(spec: &str) -> Result<User, String> {
        let (u, g) = spec.split_once(':').map_or((spec, None), |(u, g)| (u, Some(g)));
        let cstr = |s: &str| CString::new(s).map_err(|_| format!("--user `{spec}`: NUL byte"));
        if u.is_empty() {
            return Err(format!("--user `{spec}`: empty user"));
        }
        let (uid, primary) = match u.parse::<libc::uid_t>() {
            Ok(uid) => (uid, None),
            Err(_) => getpw(&cstr(u)?).map(|(uid, gid)| (uid, Some(gid))).ok_or_else(|| format!("--user `{spec}`: unknown user `{u}`"))?,
        };
        let gid = match (g, primary) {
            (Some(g), _) => match g.parse::<libc::gid_t>() {
                Ok(gid) => gid,
                Err(_) => getgr(&cstr(g)?).ok_or_else(|| format!("--user `{spec}`: unknown group `{g}`"))?,
            },
            (None, Some(gid)) => gid,
            (None, None) => return Err(format!("--user `{spec}`: a numeric uid needs a group (UID:GID)")),
        };
        if uid == 0 || gid == 0 {
            return Err(format!("--user `{spec}`: refusing to run as uid/gid 0"));
        }
        Ok(User { name: u.to_string(), uid, gid })
    }

    /// While still root: `dir` must belong to the user; a missing one is created and handed over (never through a
    /// symlink).
    pub fn own_dir(&self, dir: &Path) -> Result<(), String> {
        let created = !dir.exists();
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let c = CString::new(dir.as_os_str().as_bytes()).map_err(|_| "bad data dir path".to_string())?;
        // SAFETY: valid C string.
        let fd = Fd(unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC) });
        if fd.0 < 0 {
            return Err(format!("{}: {} (a directory, not a symlink)", dir.display(), io::Error::last_os_error()));
        }
        if created {
            sc(libc::SYS_fchown, [fd.0 as _, self.uid as _, self.gid as _, 0, 0]).map_err(|e| format!("chown {}: {e}", dir.display()))?;
        }
        // SAFETY: an all-zero stat is a valid out-parameter of fstat on our fd.
        let owner = unsafe {
            let mut st: libc::stat = std::mem::zeroed();
            (libc::fstat(fd.0, &mut st) == 0).then_some(st.st_uid)
        };
        match owner {
            Some(o) if o == self.uid => Ok(()),
            o => Err(format!("{} belongs to uid {o:?}, not to {} ({}): `chown -R {}: {}`", dir.display(), self.name, self.uid, self.name, dir.display())),
        }
    }

    /// Single-threaded: supplementary groups, then real/effective/saved gid and uid become the user's.
    pub(super) fn switch(&self) -> Result<(), String> {
        let groups = [self.gid];
        // SAFETY: one valid gid_t.
        if unsafe { libc::syscall(libc::SYS_setgroups, 1usize, groups.as_ptr()) } != 0 {
            return Err(format!("setgroups: {}", io::Error::last_os_error()));
        }
        let (u, g) = (self.uid as libc::c_long, self.gid as libc::c_long);
        sc(libc::SYS_setresgid, [g, g, g, 0, 0]).map_err(|e| format!("setresgid({g}): {e}"))?;
        sc(libc::SYS_setresuid, [u, u, u, 0, 0]).map_err(|e| format!("setresuid({u}): {e}"))?;
        let (ru, rg) = (resids(libc::SYS_getresuid), resids(libc::SYS_getresgid));
        if ru != [self.uid; 3] || rg != [self.gid; 3] {
            return Err(format!("ids are uid {ru:?} gid {rg:?} after the switch"));
        }
        Ok(())
    }
}

fn resids(nr: libc::c_long) -> [u32; 3] {
    let (mut a, mut b, mut c) = (u32::MAX, u32::MAX, u32::MAX);
    // SAFETY: three valid out-pointers.
    unsafe { libc::syscall(nr, &mut a, &mut b, &mut c) };
    [a, b, c]
}

/// The effective uid.
pub(super) fn euid() -> u32 {
    sc(libc::SYS_geteuid, [0; 5]).map_or(u32::MAX, |v| v as u32)
}

#[repr(C)]
struct CapHeader {
    version: u32,
    pid: i32,
}

#[repr(C)]
#[derive(Default, Clone, Copy, PartialEq, Debug)]
struct CapData {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

const CAP_V3: u32 = 0x2008_0522;

/// Clears the ambient, effective, permitted and inheritable capabilities (none is needed once the listeners are
/// bound), then checks they are all empty: a service manager's KEEP_CAPS/NO_SETUID_FIXUP securebits cannot leave any.
pub(super) fn drop_capabilities() -> Result<(), String> {
    let _ = sc(libc::SYS_prctl, [libc::PR_CAP_AMBIENT as _, libc::PR_CAP_AMBIENT_CLEAR_ALL as _, 0, 0, 0]);
    let mut hdr = CapHeader { version: CAP_V3, pid: 0 };
    let zero = [CapData::default(); 2];
    // SAFETY: valid header and two data structs (version 3).
    if unsafe { libc::syscall(libc::SYS_capset, &mut hdr, zero.as_ptr()) } != 0 {
        return Err(format!("capset: {}", io::Error::last_os_error()));
    }
    let mut now = [CapData { effective: !0, permitted: !0, inheritable: !0 }; 2];
    // SAFETY: as above, written by the kernel.
    if unsafe { libc::syscall(libc::SYS_capget, &mut hdr, now.as_mut_ptr()) } != 0 || now != zero {
        return Err(format!("capabilities still held after the drop: {now:?}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::tests::{exited, in_child};

    #[test]
    fn user_spec_parsing() {
        assert!(User::resolve("").is_err());
        assert!(User::resolve("0:0").is_err(), "root is refused");
        assert!(User::resolve("1234").is_err(), "numeric uid needs a group");
        assert_eq!(User::resolve("1234:5678").unwrap(), User { name: "1234".into(), uid: 1234, gid: 5678 });
        assert!(User::resolve("no-such-user-k10s").is_err());
        if let Some((uid, gid)) = getpw(c"nobody") {
            assert_eq!(User::resolve("nobody").map(|u| (u.uid, u.gid)), Ok((uid, gid)));
        }
    }

    #[test]
    fn capabilities_dropped_and_checked() {
        assert!(exited(in_child(|| drop_capabilities().is_err() as i32), 0));
    }

    #[test]
    fn data_dir_must_belong_to_the_user() {
        let d = std::env::temp_dir().join(format!("k10s-user-dir-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let me = User { name: "me".into(), uid: euid(), gid: 0 };
        assert_eq!(me.own_dir(&d), Ok(()));
        let other = User { name: "other".into(), uid: euid() + 1, gid: 1 };
        assert!(other.own_dir(&d).unwrap_err().contains("belongs to uid"));
        let link = d.with_extension("link");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&d, &link).unwrap();
        assert!(me.own_dir(&link).unwrap_err().contains("not a symlink"));
        let _ = std::fs::remove_file(&link);
        let _ = std::fs::remove_dir_all(&d);
    }
}
