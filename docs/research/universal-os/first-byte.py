#!/usr/bin/env python3
"""QEMU start -> first byte the guest kernel writes on the serial console (kernel entry + early setup), N runs."""
import os, subprocess, sys, tempfile, time, statistics
kernel, initrd, n = sys.argv[1], sys.argv[2], int(sys.argv[3])
extra = sys.argv[4:]
res = []
for i in range(n):
    d = tempfile.mkdtemp(dir=os.environ.get("UOS", "/home/qgerard/dev/k10s-research/universal-os") + "/runs")
    log = os.path.join(d, "s.log")
    t0 = time.monotonic()
    p = subprocess.Popen(["qemu-system-x86_64", "-M", "pc", "-enable-kvm", "-cpu", "host", "-m", "256M", "-nodefaults",
                          "-display", "none", "-serial", "file:" + log, "-kernel", kernel, "-initrd", initrd,
                          "-append", "console=ttyS0 earlyprintk=serial,ttyS0 panic=-1 k10s.net=none"] + extra,
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    first = None
    while time.monotonic() - t0 < 20:
        if first is None and os.path.exists(log) and os.path.getsize(log) > 0:
            first = (time.monotonic() - t0) * 1e3
            break
        time.sleep(0.001)
    p.kill(); p.wait()
    res.append(round(first, 1) if first else None)
print("first serial byte ms:", res, "median", statistics.median([r for r in res if r]), "load", open("/proc/loadavg").read().split()[:3])
