#!/usr/bin/env python3
"""Static syscall scan of an image's ELF files (the second, upper-bound evidence next to the lxtrace runs):

* imports: undefined dynamic symbols that are syscall wrappers (`WRAPPERS`), per object, for every ELF object in the
  image except the C library and its loader (whose exports are the wrappers themselves); the `closure` set is the
  traced programs plus their DT_NEEDED closure, the `all` set every object in the image (dlopen'd modules included);
* inline sites: `syscall` instructions preceded by `mov $nr, %eax` within 12 bytes in executable segments (how
  static musl, Rust and Go binaries and inline assembly issue calls); a heuristic that misses numbers passed in
  registers (Go's syscall.Syscall6, musl's __syscall_cp) and can catch stray byte patterns.

  elfscan.py REPO@DIGEST TRACE.json OUT.json
"""
import json
import os
import re
import struct
import sys
import tarfile
import tempfile

import linuxabi
import oci

LIBC = re.compile(r"^(libc|libm|libpthread|librt|libdl|libutil|libresolv|libanl|ld-linux-x86-64|ld-musl-x86_64|"
                  r"libc\.musl-x86_64)\.so")
SYNC = ["futex"]
DNS = ["socket", "connect", "sendto", "recvfrom", "poll"]
ALIASES = {
    "open64": "open", "__open_2": "open", "__open64_2": "open", "openat64": "openat", "__openat_2": "openat",
    "__openat64_2": "openat", "creat64": "creat", "stat64": "stat", "__xstat": "stat", "__xstat64": "stat",
    "fstat64": "fstat", "__fxstat": "fstat", "__fxstat64": "fstat", "lstat64": "lstat", "__lxstat": "lstat",
    "__lxstat64": "lstat", "fstatat": "newfstatat", "fstatat64": "newfstatat", "__fxstatat": "newfstatat",
    "__fxstatat64": "newfstatat", "pread": "pread64", "__pread_chk": "pread64", "__pread64_chk": "pread64",
    "pwrite": "pwrite64", "preadv64": "preadv", "pwritev64": "pwritev", "preadv64v2": "preadv2",
    "pwritev64v2": "pwritev2", "lseek64": "lseek", "mmap64": "mmap", "truncate64": "truncate",
    "ftruncate64": "ftruncate", "fallocate64": "fallocate", "posix_fallocate": "fallocate",
    "posix_fallocate64": "fallocate", "posix_fadvise": "fadvise64", "posix_fadvise64": "fadvise64",
    "sendfile64": "sendfile", "statfs64": "statfs", "statvfs": "statfs", "statvfs64": "statfs",
    "fstatfs64": "fstatfs", "fstatvfs": "fstatfs", "fstatvfs64": "fstatfs", "readdir": "getdents64",
    "readdir64": "getdents64", "readdir_r": "getdents64", "readdir64_r": "getdents64", "scandir": "getdents64",
    "scandir64": "getdents64", "opendir": "openat", "fdopendir": "fcntl", "getrlimit64": "getrlimit",
    "setrlimit64": "setrlimit", "prlimit": "prlimit64", "wait": "wait4", "waitpid": "wait4", "wait3": "wait4",
    "sigaction": "rt_sigaction", "signal": "rt_sigaction", "sigset": "rt_sigaction", "bsd_signal": "rt_sigaction",
    "__sysv_signal": "rt_sigaction", "sysv_signal": "rt_sigaction", "sigprocmask": "rt_sigprocmask",
    "pthread_sigmask": "rt_sigprocmask", "sigpending": "rt_sigpending", "sigsuspend": "rt_sigsuspend",
    "sigtimedwait": "rt_sigtimedwait", "sigwaitinfo": "rt_sigtimedwait", "sigwait": "rt_sigtimedwait",
    "sigqueue": "rt_sigqueueinfo", "raise": "tgkill", "pthread_kill": "tgkill", "abort": "tgkill",
    "pthread_create": "clone", "posix_spawn": "clone", "posix_spawnp": "clone", "system": "clone", "popen": "clone",
    "_Fork": "clone", "daemon": "clone", "sleep": "nanosleep", "usleep": "nanosleep", "epoll_pwait": "epoll_pwait",
    "signalfd": "signalfd4", "eventfd": "eventfd2", "recv": "recvfrom", "__recv_chk": "recvfrom",
    "__recvfrom_chk": "recvfrom", "send": "sendto", "shm_open": "openat", "shm_unlink": "unlink",
    "mkstemp": "openat", "mkstemp64": "openat", "mkostemp": "openat", "mkostemp64": "openat", "mkstemps": "openat",
    "mkdtemp": "mkdir", "tmpfile": "openat", "tmpfile64": "openat", "mkfifo": "mknodat", "mkfifoat": "mknodat",
    "isatty": "ioctl", "tcgetattr": "ioctl", "tcsetattr": "ioctl", "ttyname": "ioctl", "ttyname_r": "ioctl",
    "getentropy": "getrandom", "arc4random": "getrandom", "klogctl": "syslog", "umount": "umount2",
    "futimens": "utimensat", "lutimes": "utimensat", "futimes": "utimensat", "sched_getcpu": "getcpu",
    "pselect": "pselect6", "closefrom": "close_range", "_exit": "exit_group", "_Exit": "exit_group",
    "exit": "exit_group", "__read_chk": "read", "__readlink_chk": "readlink", "__readlinkat_chk": "readlinkat",
    "__getcwd_chk": "getcwd", "__poll_chk": "poll", "__ppoll_chk": "ppoll", "__fdelt_chk": "select",
    "__getgroups_chk": "getgroups", "initgroups": "setgroups", "seteuid": "setresuid", "setegid": "setresgid",
    "sbrk": "brk", "posix_madvise": "madvise", "cap_get_proc": "capget", "cap_set_proc": "capset",
    "sched_getaffinity": "sched_getaffinity", "get_nprocs": "sched_getaffinity", "if_nametoindex": "ioctl",
    "getifaddrs": "socket", "inotify_init": "inotify_init", "mq_send": "mq_timedsend", "mq_receive": "mq_timedreceive",
    "mq_timedsend": "mq_timedsend", "mq_timedreceive": "mq_timedreceive", "mq_setattr": "mq_getsetattr",
    "mq_getattr": "mq_getsetattr",
}
MULTI = {s: SYNC for s in ("pthread_mutex_lock", "pthread_mutex_timedlock", "pthread_cond_wait",
                            "pthread_cond_timedwait", "pthread_cond_signal", "pthread_cond_broadcast", "pthread_join",
                            "pthread_rwlock_rdlock", "pthread_rwlock_wrlock", "pthread_barrier_wait", "sem_wait",
                            "sem_timedwait", "sem_post")}
MULTI.update({s: DNS for s in ("getaddrinfo", "gethostbyname", "gethostbyname2", "gethostbyname_r",
                                "gethostbyname2_r", "gethostbyaddr", "gethostbyaddr_r", "getnameinfo", "res_query",
                                "res_search", "res_nquery", "res_nsearch", "__res_query", "__res_search")})
NOT_WRAPPERS = {"time", "times", "security", "uselib"}


def wrapper(sym):
    """The syscalls an imported libc symbol stands for ([] when it is not a wrapper)."""
    sym = sym.split("@")[0]
    if sym in MULTI:
        return MULTI[sym]
    if sym in ALIASES:
        return [ALIASES[sym]]
    if sym in linuxabi.NR and sym not in NOT_WRAPPERS:
        return [sym]
    return []


class Elf:
    """Just enough ELF64 little-endian parsing: dynamic imports, DT_NEEDED/RUNPATH, PT_INTERP, executable bytes."""

    def __init__(self, data):
        self.ok = data[:4] == b"\x7fELF" and len(data) >= 64 and data[4] == 2 and data[5] == 1
        if not self.ok:
            return
        self.d = data
        (self.type, machine, _, _, phoff, shoff, _, _, phentsize, phnum, shentsize, shnum,
         _) = struct.unpack_from("<HHIQQQIHHHHHH", data, 16)
        self.ok = machine == 62
        self.phdrs = [struct.unpack_from("<IIQQQQQQ", data, phoff + i * phentsize) for i in range(phnum)
                      if phoff + (i + 1) * phentsize <= len(data)]
        self.shdrs = [struct.unpack_from("<IIQQQQIIQQ", data, shoff + i * shentsize) for i in range(shnum)
                      if shoff + (i + 1) * shentsize <= len(data)]

    def interp(self):
        for p in self.phdrs:
            if p[0] == 3:
                return self.d[p[2]:p[2] + p[5]].rstrip(b"\0").decode("latin-1")
        return None

    def section(self, kind):
        return [s for s in self.shdrs if s[1] == kind]

    def strz(self, off, base, size):
        if off >= size:
            return ""
        end = self.d.find(b"\0", base + off, base + size)
        return self.d[base + off:end if end >= 0 else base + size].decode("latin-1")

    def imports(self):
        out = set()
        for s in self.section(11):  # SHT_DYNSYM
            if s[6] >= len(self.shdrs):
                continue
            strtab = self.shdrs[s[6]]
            for i in range(1, s[5] // 24):
                name_off, info, _, shndx, _, _ = struct.unpack_from("<IBBHQQ", self.d, s[4] + i * 24)
                if shndx == 0 and (info >> 4) in (1, 2):  # undefined GLOBAL/WEAK
                    out.add(self.strz(name_off, strtab[4], strtab[5]))
        return out

    def dynamic(self):
        needed, runpath, self.flags1 = [], [], 0
        for s in self.section(6):  # SHT_DYNAMIC
            if s[6] >= len(self.shdrs):
                continue
            strtab = self.shdrs[s[6]]
            for i in range(s[5] // 16):
                tag, val = struct.unpack_from("<qQ", self.d, s[4] + i * 16)
                if tag == 0:
                    break
                if tag == 1:
                    needed.append(self.strz(val, strtab[4], strtab[5]))
                elif tag in (15, 29):
                    runpath += self.strz(val, strtab[4], strtab[5]).split(":")
                elif tag == 0x6ffffffb:
                    self.flags1 = val
        return needed, runpath

    def inline_syscalls(self):
        """Syscall numbers loaded into eax just before a `syscall` instruction in executable segments."""
        nrs = {}
        for p in self.phdrs:
            if p[0] != 1 or not p[1] & 1:
                continue
            seg = self.d[p[2]:p[2] + p[5]]
            i = seg.find(b"\x0f\x05")
            while i >= 0:
                for back in range(5, 13):
                    j = i - back
                    if j >= 0 and seg[j] == 0xb8:
                        nr = struct.unpack_from("<I", seg, j + 1)[0]
                        if nr in linuxabi.SYSCALLS:
                            nrs[nr] = nrs.get(nr, 0) + 1
                        break
                i = seg.find(b"\x0f\x05", i + 2)
        return nrs


def scan(ref, trace_path):
    trace = json.load(open(trace_path))
    _, m, _ = oci.pull(ref)
    with tempfile.TemporaryDirectory(dir=oci.tmpdir()) as td:
        tfs = [tarfile.open(oci.open_layer(d, td)) for d in m["layers"]]
        tree = oci.merge(tfs)

        def resolve(path):
            """Follows symlinks inside the image; returns the merged-tree key of a regular file or None."""
            parts, done, hops = [x for x in path.split("/") if x not in ("", ".")], [], 0
            while parts:
                x = parts.pop(0)
                if x == "..":
                    done = done[:-1]
                    continue
                hit = tree.get("/".join(done + [x]))
                if hit and hit[1].issym():
                    hops += 1
                    if hops > 40:
                        return None
                    t = hit[1].linkname
                    done = [] if t.startswith("/") else done
                    parts = [y for y in t.split("/") if y not in ("", ".")] + parts
                    continue
                done.append(x)
            key = "/".join(done)
            hit = tree.get(key)
            return key if hit and (hit[1].isreg() or hit[1].islnk()) else None

        cache = {}

        def elf(key):
            if key not in cache:
                tf, ti = tree[key]
                f = tf.extractfile(ti)
                head = f.read(4)
                cache[key] = Elf(head + f.read()) if head == b"\x7fELF" else None
            return cache[key]

        libdirs = ["lib/x86_64-linux-gnu", "usr/lib/x86_64-linux-gnu", "lib64", "usr/lib64", "lib", "usr/lib",
                   "usr/local/lib"]
        for k, (tf, ti) in tree.items():
            if k.startswith("etc/ld.so.conf.d/") and ti.isreg():
                libdirs += [ln.strip().lstrip("/") for ln in tf.extractfile(ti).read().decode("latin-1").splitlines()
                            if ln.strip().startswith("/")]
        closure, todo = set(), []
        for prog in trace["programs"]:
            k = resolve(prog)
            if k:
                todo.append(k)
        while todo:
            k = todo.pop()
            if k in closure or not elf(k) or not elf(k).ok:
                continue
            closure.add(k)
            needed, runpath = elf(k).dynamic()
            origin = os.path.dirname(k)
            dirs = [r.replace("$ORIGIN", "/" + origin).replace("${ORIGIN}", "/" + origin).lstrip("/")
                    for r in runpath] + libdirs
            for n in needed:
                hit = next((r for r in (resolve(os.path.normpath(os.path.join(d, n))) for d in dirs) if r), None)
                if hit:
                    todo.append(hit)
        objects = []
        for k, (tf, ti) in tree.items():
            if ti.isreg() and ti.size >= 64 or ti.islnk() and oci.safe_name(ti.linkname) not in tree:
                e = elf(k)
                if e and e.ok:
                    objects.append(k)
        result = {"objects": len(objects), "closure_objects": len(closure), "closure": {}, "all": {}, "inline": {},
                  "static_programs": [], "syscall_fn": []}
        for k in sorted(objects):
            e = elf(k)
            if LIBC.match(os.path.basename(k)):
                continue
            imps = e.imports()
            if "syscall" in imps:
                result["syscall_fn"].append(k)
            for sym in imps:
                for sc in wrapper(sym):
                    result["all"].setdefault(sc, 0)
                    result["all"][sc] += 1
                    if k in closure:
                        result["closure"].setdefault(sc, [])
                        if len(result["closure"][sc]) < 8:
                            result["closure"][sc].append(f"{os.path.basename(k)}:{sym}")
            static = e.interp() is None and not e.dynamic()[0] and (e.type == 2 or e.flags1 & 0x08000000)
            if static:
                result["static_programs"].append(k)
            if static or k in closure:
                for nr, n in e.inline_syscalls().items():
                    name = linuxabi.SYSCALLS[nr]
                    result["inline"][name] = result["inline"].get(name, 0) + n
        for tf in tfs:
            tf.close()
    return result


def main():
    if len(sys.argv) != 4:
        sys.exit(__doc__)
    json.dump(scan(sys.argv[1], sys.argv[2]), open(sys.argv[3], "w"), indent=1, sort_keys=True)


if __name__ == "__main__":
    main()
