#!/usr/bin/env python3
"""Boots a k10s universal-image guest under QEMU and measures it (no root: user networking + hostfwd).

  boot.py [--arch x86_64|i386|aarch64] --kernel K [--initrd I] [--append "..."] [--mem 256M] [--smp 1]
          [--bios FILE | --uefi CODE.fd] [--drive disk.img] [--apply MANIFEST] [--k10s HOST_K10S]
          [--powerdown] [--idle SECS] [--runs N] [--json OUT] [--no-kvm] [--watchdog] [--serial-log FILE]

Measures: QEMU start -> first HTTP 200 on /_k10s/health (polled every 5 ms through a hostfwd port), the in-guest
"k10s-init" timeline lines, apply + first service answer, memory lines (k10s.meminfo), and powerdown -> exit.
"""
import argparse
import json
import os
import re
import socket
import statistics
import subprocess
import sys
import tempfile
import threading
import time

UOS = os.environ.get("UOS", "/home/qgerard/dev/k10s-research/universal-os")


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


def http_get(port, path, timeout=1.0):
    try:
        s = socket.create_connection(("127.0.0.1", port), timeout=timeout)
        s.settimeout(timeout)
        s.sendall(f"GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n".encode())
        data = b""
        while True:
            b = s.recv(65536)
            if not b:
                break
            data += b
        s.close()
        m = re.match(rb"HTTP/1\.[01] (\d+)", data)
        return (int(m.group(1)) if m else None), data.split(b"\r\n\r\n", 1)[-1]
    except OSError:
        return None, b""


def qemu_cmd(a, port, mon):
    q = {
        "x86_64": "qemu-system-x86_64",
        "i386": "qemu-system-i386",
        "aarch64": f"{UOS}/sysroot/usr/bin/qemu-system-aarch64",
    }[a.arch]
    cmd = [q, "-m", a.mem, "-smp", str(a.smp), "-nodefaults", "-display", "none", "-no-user-config"]
    if a.arch in ("x86_64", "i386"):
        cmd += ["-M", a.machine or "pc"]
        if not a.no_kvm:
            cmd += ["-enable-kvm", "-cpu", a.cpu or ("host" if a.arch == "x86_64" else "qemu32,+sse2")]
        nic = "virtio-net-pci" + (",disable-modern=on" if a.legacy_virtio else "")
    else:
        cmd += ["-M", a.machine or "virt", "-cpu", a.cpu or "cortex-a72"]
        nic = "virtio-net-pci,romfile="  # no iPXE option ROM in the extracted package
    cmd += ["-serial", "file:" + a._serial]
    cmd += ["-qmp", f"unix:{mon},server=on,wait=off"]
    cmd += ["-netdev", f"user,id=n0,hostfwd=tcp:127.0.0.1:{port}-:80", "-device", f"{nic},netdev=n0"]
    if a.uefi:
        vars_copy = a._serial + ".vars.fd"
        src_vars = a.uefi_vars or a.uefi.replace("CODE", "VARS")
        open(vars_copy, "wb").write(open(src_vars, "rb").read())
        cmd += ["-drive", f"if=pflash,format=raw,unit=0,readonly=on,file={a.uefi}"]
        cmd += ["-drive", f"if=pflash,format=raw,unit=1,file={vars_copy}"]
    elif a.bios:
        cmd += ["-bios", a.bios]
    if a.kernel:
        cmd += ["-kernel", a.kernel]
        if a.initrd:
            cmd += ["-initrd", a.initrd]
        if a.append is not None:
            cmd += ["-append", a.append]
    if a.disk:
        cmd += ["-drive", f"file={a.disk},format=raw,if=virtio,snapshot=on"]
    if a.boot_disk:
        cmd += ["-drive", f"file={a.boot_disk},format=raw,if=virtio" + ("" if a.persist else ",snapshot=on")]
    if a.watchdog:
        cmd += ["-device", "i6300esb", "-action", "watchdog=poweroff"]
    if a.rng:
        cmd += ["-device", "virtio-rng-pci"]
    return cmd


def qmp(mon, command):
    s = socket.socket(socket.AF_UNIX)
    s.settimeout(2)
    s.connect(mon)
    f = s.makefile("rwb")
    f.readline()
    for c in ({"execute": "qmp_capabilities"}, {"execute": command}):
        f.write(json.dumps(c).encode() + b"\n")
        f.flush()
        f.readline()
    s.close()


def one_run(a, i):
    port = free_port()
    tmp = tempfile.mkdtemp(prefix="boot-", dir=f"{UOS}/runs")
    a._serial = a.serial_log or os.path.join(tmp, "serial.log")
    mon = os.path.join(tmp, "qmp.sock")
    cmd = qemu_cmd(a, port, mon)
    load = open("/proc/loadavg").read().split()[:3]
    t0 = time.monotonic()
    p = subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    r = {"run": i, "load": load, "cmd": " ".join(cmd)}
    deadline = t0 + a.timeout
    # Like metal/tools/measure.py: wait for the console's ready line, then poll HTTP (a poll that starts while the
    # guest is still booting can sit in slirp's connect to the guest for a while).
    while time.monotonic() < deadline and p.poll() is None:
        try:
            if a.ready in open(a._serial, errors="replace").read():
                r["serial_listening_ms"] = round((time.monotonic() - t0) * 1e3, 1)
                break
        except OSError:
            pass
        time.sleep(0.002)
    while time.monotonic() < deadline and p.poll() is None:
        st, _ = http_get(port, a.health, 0.2)
        if st == 200:
            r["first_200_ms"] = round((time.monotonic() - t0) * 1e3, 1)
            break
        time.sleep(0.005)
    if "first_200_ms" not in r:
        r["error"] = "no HTTP 200" + (f" (qemu exited {p.returncode}: {p.stderr.read().decode()[-500:]})" if p.poll() is not None else "")
    elif a.apply:
        t1 = time.monotonic()
        out = subprocess.run([a.k10s, "apply", a.apply, "--node", f"127.0.0.1:{port}"], capture_output=True, text=True)
        r["apply_ms"] = round((time.monotonic() - t1) * 1e3, 1)
        r["apply_ok"] = out.returncode == 0
        if out.returncode != 0:
            r["apply_err"] = (out.stdout + out.stderr)[-400:]
    if "first_200_ms" in r:
        for path in a.check:
            st, body = http_get(port, path, 5)
            r.setdefault("checks", {})[path] = [st, body[:60].decode(errors="replace")]
    if a.idle and "first_200_ms" in r:
        time.sleep(a.idle)
    if a.powerdown and p.poll() is None:
        t2 = time.monotonic()
        try:
            qmp(mon, "system_powerdown")
            p.wait(timeout=30)
            r["powerdown_to_exit_ms"] = round((time.monotonic() - t2) * 1e3, 1)
        except Exception as e:  # noqa
            r["powerdown_error"] = str(e)
    if p.poll() is None:
        p.kill()
        p.wait()
    serial = open(a._serial, errors="replace").read()
    r["init_lines"] = [l for l in serial.splitlines() if "k10s-init[" in l or l.startswith("k10s:")]
    mem = [l for l in r["init_lines"] if "mem: total" in l]
    if mem:
        r["last_mem"] = mem[-1]
    if a.keep_serial:
        r["serial_tail"] = serial[-3000:]
    return r


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--arch", default="x86_64")
    ap.add_argument("--machine")
    ap.add_argument("--cpu")
    ap.add_argument("--kernel")
    ap.add_argument("--initrd")
    ap.add_argument("--append")
    ap.add_argument("--mem", default="256M")
    ap.add_argument("--smp", type=int, default=1)
    ap.add_argument("--bios")
    ap.add_argument("--uefi")
    ap.add_argument("--uefi-vars")
    ap.add_argument("--disk")
    ap.add_argument("--boot-disk")
    ap.add_argument("--apply")
    ap.add_argument("--check", action="append", default=[])
    ap.add_argument("--k10s", default=f"{UOS}/bin/k10s-x86_64")
    ap.add_argument("--powerdown", action="store_true")
    ap.add_argument("--persist", action="store_true")
    ap.add_argument("--ready", default="listening on")
    ap.add_argument("--health", default="/_k10s/health")
    ap.add_argument("--legacy-virtio", action="store_true")
    ap.add_argument("--watchdog", action="store_true")
    ap.add_argument("--rng", action="store_true")
    ap.add_argument("--idle", type=float, default=0)
    ap.add_argument("--runs", type=int, default=1)
    ap.add_argument("--timeout", type=float, default=60)
    ap.add_argument("--no-kvm", action="store_true")
    ap.add_argument("--json")
    ap.add_argument("--serial-log")
    ap.add_argument("--keep-serial", action="store_true")
    ap.add_argument("--quiet", action="store_true")
    a = ap.parse_args()
    os.makedirs(f"{UOS}/runs", exist_ok=True)
    rs = [one_run(a, i) for i in range(a.runs)]
    ok = [r["first_200_ms"] for r in rs if "first_200_ms" in r]
    summary = {"runs": len(rs), "ok": len(ok)}
    if ok:
        summary.update(min=min(ok), median=statistics.median(ok), max=max(ok))
    out = {"summary": summary, "runs": rs}
    if a.json:
        json.dump(out, open(a.json, "w"), indent=1)
    if a.quiet:
        print(json.dumps(summary), [r.get("error") for r in rs if "error" in r])
    else:
        print(json.dumps(out, indent=1))


if __name__ == "__main__":
    main()
