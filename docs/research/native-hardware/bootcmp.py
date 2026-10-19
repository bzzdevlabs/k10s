#!/usr/bin/env python3
"""Interleaved boot-time comparison of k10-metal boot paths (research prototype).

For each run: QEMU process start -> "k10-metal ready" on the serial console (wall clock), plus the kernel's own
"ready X ms" (kmain -> ready) and "vm->kmain=Y ms" (kvmclock: VM creation -> kernel entry = firmware + loader).
Usage: bootcmp.py RUNS name1 'cmd1' name2 'cmd2' ...   (cmds are shell-split)
"""
import json, os, re, shlex, statistics, subprocess, sys, time

def run(cmd, deadline=60.0):
    t0 = time.monotonic()
    p = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, stdin=subprocess.DEVNULL)
    buf = b""; wall = None
    try:
        while time.monotonic() - t0 < deadline:
            c = p.stdout.read1(4096)
            if not c: break
            buf += c
            i = buf.find(b"k10-metal ready")
            if i >= 0 and wall is None:
                wall = time.monotonic() - t0
            if wall is not None and b"\n" in buf[i:]:
                break
    finally:
        p.kill(); p.wait()
    m = re.search(rb"k10-metal ready ([0-9.]+)ms vm->kmain=([0-9.]+)ms", buf)
    return (wall * 1000 if wall else None, float(m.group(1)) if m else None, float(m.group(2)) if m else None)

def med(xs):
    xs = [x for x in xs if x is not None]
    return round(statistics.median(xs), 1) if xs else None

runs = int(sys.argv[1]); cfgs = list(zip(sys.argv[2::2], sys.argv[3::2]))
res = {n: [] for n, _ in cfgs}
for r in range(runs):
    for n, c in cfgs:
        res[n].append(run(shlex.split(c)))
load = os.getloadavg()
out = {}
for n, xs in res.items():
    out[n] = {"wall_ms_median": med([x[0] for x in xs]), "wall_ms_min": min([x[0] for x in xs if x[0]] or [None]),
              "kernel_ms_median": med([x[1] for x in xs]), "vm_to_kmain_ms_median": med([x[2] for x in xs]),
              "samples": xs}
out["_loadavg_end"] = load
print(json.dumps(out, indent=1))
