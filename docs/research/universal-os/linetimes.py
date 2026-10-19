#!/usr/bin/env python3
"""Host-side arrival time of every serial line (ms after QEMU start), to split boot time into phases.
  linetimes.py SECONDS -- qemu args... (the script adds -serial file:LOG)"""
import os, subprocess, sys, tempfile, time
secs = float(sys.argv[1]); args = sys.argv[sys.argv.index("--") + 1:]
d = tempfile.mkdtemp(dir=os.environ.get("UOS", "/home/qgerard/dev/k10s-research/universal-os") + "/runs"); log = os.path.join(d, "s.log")
t0 = time.monotonic()
p = subprocess.Popen(args + ["-serial", "file:" + log], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
seen = 0; buf = b""
while time.monotonic() - t0 < secs and p.poll() is None:
    try:
        with open(log, "rb") as f:
            f.seek(seen); new = f.read()
    except OSError:
        new = b""
    if new:
        seen += len(new); buf += new
        while b"\n" in buf:
            line, buf = buf.split(b"\n", 1)
            print("%8.1f  %s" % ((time.monotonic() - t0) * 1e3, line.decode(errors="replace")[:150]), flush=True)
    time.sleep(0.001)
p.kill(); p.wait()
print("load", open("/proc/loadavg").read().split()[:3])
