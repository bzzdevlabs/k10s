#!/usr/bin/env python3
"""Boot-time measurement: QEMU process start -> "ready" line on the serial console -> first HTTP 200.

Runs QEMU N times (serial on stdout), timestamps the moment the ready marker appears, then (optionally) polls an
HTTP URL through the slirp host forward until it answers 200 and timestamps that. Prints one JSON object with the
raw samples and the median/min/max of each.

  boottime.py --runs 10 --marker 'k10-metal ready' --url http://127.0.0.1:18090/hello/ -- qemu-system-x86_64 ...

Without --url only the ready line is measured (use e.g. -append k10.exit_on_ready so QEMU exits by itself).
Polling starts when the ready line is seen (polling earlier would race slirp's SYN retransmission timer against
the guest's boot and measure slirp, not the guest).
"""
import argparse
import json
import socket
import statistics
import subprocess
import sys
import time


def http_ok(host, port, path, timeout):
    try:
        with socket.create_connection((host, port), timeout=timeout) as s:
            s.settimeout(timeout)
            s.sendall(f"GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n".encode())
            data = b""
            while b"\r\n" not in data:
                chunk = s.recv(4096)
                if not chunk:
                    break
                data += chunk
            return data.startswith(b"HTTP/1.1 200")
    except OSError:
        return False


def one_run(cmd, marker, url, deadline_s):
    host = port = path = None
    if url:
        rest = url.split("://", 1)[1]
        hp, _, p = rest.partition("/")
        host, _, port = hp.partition(":")
        port, path = int(port or 80), "/" + p
    t0 = time.monotonic()
    proc = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, stdin=subprocess.DEVNULL)
    ready = http = None
    lines = []
    try:
        buf = b""
        while time.monotonic() - t0 < deadline_s:
            chunk = proc.stdout.read1(4096) if hasattr(proc.stdout, "read1") else proc.stdout.read(1)
            if not chunk:
                break
            buf += chunk
            if marker.encode() in buf:
                ready = time.monotonic() - t0
                break
        lines = buf.decode(errors="replace").splitlines()[-3:]
        if ready is not None and url:
            while time.monotonic() - t0 < deadline_s:
                if http_ok(host, port, path, 1.0):
                    http = time.monotonic() - t0
                    break
                time.sleep(0.001)
    finally:
        proc.kill()
        proc.wait()
    return ready, http, lines


def summary(xs):
    xs = [x * 1000 for x in xs if x is not None]
    if not xs:
        return None
    return {"median_ms": round(statistics.median(xs), 2), "min_ms": round(min(xs), 2), "max_ms": round(max(xs), 2), "n": len(xs)}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--runs", type=int, default=10)
    ap.add_argument("--marker", default="k10-metal ready")
    ap.add_argument("--url")
    ap.add_argument("--deadline", type=float, default=30.0)
    ap.add_argument("--label", default="")
    ap.add_argument("cmd", nargs=argparse.REMAINDER)
    a = ap.parse_args()
    cmd = a.cmd[1:] if a.cmd and a.cmd[0] == "--" else a.cmd
    readies, https, last = [], [], []
    for _ in range(a.runs):
        r, h, lines = one_run(cmd, a.marker, a.url, a.deadline)
        readies.append(r)
        https.append(h)
        last = lines
        if r is None:
            print(f"boottime: no ready marker; last output: {lines}", file=sys.stderr)
    out = {
        "label": a.label,
        "runs": a.runs,
        "ready": summary(readies),
        "first_http_200": summary(https) if a.url else None,
        "samples_ready_ms": [round(x * 1000, 2) if x else None for x in readies],
        "samples_http_ms": [round(x * 1000, 2) if x else None for x in https] if a.url else None,
        "last_lines": last,
    }
    print(json.dumps(out))


if __name__ == "__main__":
    main()
