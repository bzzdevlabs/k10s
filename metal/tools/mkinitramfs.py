#!/usr/bin/env python3
"""Writes an uncompressed newc cpio initramfs for the Linux comparison guest, without root (device nodes are just
cpio headers): /init (static binary), /dev/console (char 5:1), and kernel modules as /lib/NN-name.ko in load order.

  mkinitramfs.py OUT INIT [MODULE.ko ...]
"""
import os
import sys


def entry(name, mode, data=b"", rmaj=0, rmin=0, ino=[1]):
    ino[0] += 1
    fields = [ino[0], mode, 0, 0, 1, 0, len(data), 0, 0, rmaj, rmin, len(name) + 1, 0]
    hdr = b"070701" + "".join("%08X" % f for f in fields).encode() + name.encode() + b"\0"
    hdr += b"\0" * (-len(hdr) % 4)
    return hdr + data + b"\0" * (-len(data) % 4)


def main():
    out, init, mods = sys.argv[1], sys.argv[2], sys.argv[3:]
    blob = entry("dev", 0o040755) + entry("dev/console", 0o020600, rmaj=5, rmin=1) + entry("lib", 0o040755)
    blob += entry("init", 0o100755, open(init, "rb").read())
    for i, m in enumerate(mods):
        blob += entry("lib/%02d-%s" % (i, os.path.basename(m)), 0o100644, open(m, "rb").read())
    blob += entry("TRAILER!!!", 0)
    with open(out, "wb") as f:
        f.write(blob)
    print(f"mkinitramfs: {out}: {len(blob)} bytes, {len(mods)} module(s)", file=sys.stderr)


if __name__ == "__main__":
    main()
