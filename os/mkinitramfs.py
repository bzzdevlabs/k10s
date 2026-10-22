#!/usr/bin/env python3
"""Rootless initramfs builder for k10s as PID 1 of a Linux image (newc cpio, optionally compressed; docs/OS.md).

  mkinitramfs.py OUT.cpio[.gz|.xz|.zst] --init K10S_BINARY
      [--kmods /lib/modules/VER] [--mod PATTERN ...]     # module globs relative to kernel/ (deps added automatically)
      [--firmware DIR] [--fw GLOB ...]                    # firmware files relative to DIR, shipped as /lib/firmware/...
      [--conf FILE]                                       # becomes /etc/k10s.conf
      [--file SRC:DST[:MODE] ...]                         # extra files, e.g. admin.key:/etc/admin.key:0600

No root needed: device nodes and directories are just cpio headers. Timestamps are fixed (SOURCE_DATE_EPOCH or 0)
and entries are sorted, so the output is reproducible. modules.alias / modules.dep are filtered to the shipped
modules (k10s's PID 1 mode parses them for modalias coldplug).
"""
import argparse
import fnmatch
import gzip
import lzma
import os
import subprocess
import sys

MTIME = int(os.environ.get("SOURCE_DATE_EPOCH", "0"))


class Cpio:
    def __init__(self):
        self.out = bytearray()
        self.ino = 1
        self.dirs = set()

    def entry(self, name, mode, data=b"", rmaj=0, rmin=0):
        self.ino += 1
        fields = [self.ino, mode, 0, 0, 1, MTIME, len(data), 0, 0, rmaj, rmin, len(name) + 1, 0]
        hdr = b"070701" + "".join("%08X" % f for f in fields).encode() + name.encode() + b"\0"
        hdr += b"\0" * (-len(hdr) % 4)
        self.out += hdr + data + b"\0" * (-len(data) % 4)

    def mkdirs(self, path):
        parts = path.split("/")
        for i in range(1, len(parts) + 1):
            d = "/".join(parts[:i])
            if d and d not in self.dirs:
                self.dirs.add(d)
                self.entry(d, 0o040755)

    def file(self, name, data, mode=0o100644):
        self.mkdirs(os.path.dirname(name))
        self.entry(name, mode, data)

    def finish(self):
        self.entry("TRAILER!!!", 0)
        return bytes(self.out)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("out")
    ap.add_argument("--init", required=True)
    ap.add_argument("--kmods")
    ap.add_argument("--mod", action="append", default=[])
    ap.add_argument("--firmware")
    ap.add_argument("--fw", action="append", default=[])
    ap.add_argument("--conf")
    ap.add_argument("--file", action="append", default=[])
    a = ap.parse_args()

    c = Cpio()
    for d in ["dev", "proc", "sys", "data", "etc", "run"]:
        c.mkdirs(d)
    c.entry("dev/console", 0o020600, rmaj=5, rmin=1)
    c.file("init", open(a.init, "rb").read(), 0o100755)
    if a.conf:
        c.file("etc/k10s.conf", open(a.conf, "rb").read())
    for spec in a.file:
        src, dst, *mode = spec.split(":")
        c.file(dst.lstrip("/"), open(src, "rb").read(), 0o100000 | int(mode[0] if mode else "644", 8))

    nmods = 0
    mod_bytes = 0
    if a.kmods:
        kver = os.path.basename(a.kmods.rstrip("/"))
        deps = {}
        for line in open(os.path.join(a.kmods, "modules.dep")):
            path, _, rest = line.partition(":")
            deps[path] = rest.split()
        want = set()
        for pat in a.mod:
            hits = [p for p in deps if fnmatch.fnmatch(p, "kernel/" + pat)]
            if not hits:
                print(f"mkinitramfs: no module matches {pat}", file=sys.stderr)
            want.update(hits)
        for p in list(want):
            want.update(deps[p])

        def modname(p):
            b = os.path.basename(p)
            return b[: b.index(".ko")].replace("-", "_")

        names = {modname(p) for p in want}
        base = f"lib/modules/{kver}"
        dep_lines = [f"{p}: {' '.join(deps[p])}\n" for p in sorted(want)]
        c.file(f"{base}/modules.dep", "".join(dep_lines).encode())
        alias_lines = []
        for line in open(os.path.join(a.kmods, "modules.alias")):
            parts = line.split()
            if len(parts) == 3 and parts[0] == "alias" and parts[2].replace("-", "_") in names:
                alias_lines.append(line)
        c.file(f"{base}/modules.alias", "".join(alias_lines).encode())
        sd = os.path.join(a.kmods, "modules.softdep")
        if os.path.exists(sd):
            c.file(f"{base}/modules.softdep", open(sd, "rb").read())
        for p in sorted(want):
            data = open(os.path.join(a.kmods, p), "rb").read()
            mod_bytes += len(data)
            nmods += 1
            c.file(f"{base}/{p}", data)

    fw_bytes = 0
    if a.firmware:
        for root, _, files in os.walk(a.firmware):
            for f in sorted(files):
                rel = os.path.relpath(os.path.join(root, f), a.firmware)
                if any(fnmatch.fnmatch(rel, g) for g in a.fw):
                    data = open(os.path.join(root, f), "rb").read()
                    fw_bytes += len(data)
                    c.file(f"lib/firmware/{rel}", data)

    blob = c.finish()
    raw = len(blob)
    if a.out.endswith(".gz"):
        blob = gzip.compress(blob, 9, mtime=0)
    elif a.out.endswith(".xz"):
        blob = lzma.compress(blob, check=lzma.CHECK_CRC32, preset=9)  # the kernel's xz decoder wants CRC32
    elif a.out.endswith(".zst"):
        blob = subprocess.run(["zstd", "-19", "-q", "-c"], input=blob, stdout=subprocess.PIPE, check=True).stdout
    with open(a.out, "wb") as f:
        f.write(blob)
    print(
        f"mkinitramfs: {a.out}: {len(blob)} bytes ({raw} uncompressed; init {os.path.getsize(a.init)}, "
        f"{nmods} modules {mod_bytes}, firmware {fw_bytes})",
        file=sys.stderr,
    )


if __name__ == "__main__":
    main()
