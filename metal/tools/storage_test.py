#!/usr/bin/env python3
"""K10S-DATA on the native kernel under QEMU, for each disk controller (virtio-blk, NVMe, AHCI):

  1. a GPT disk with a blank K10S-DATA partition: the kernel grows it to the end of the disk, formats the store,
     serves the `kv` service; KV written over HTTP is there after a power cut (QEMU killed) and a new boot;
  2. power cuts at random moments while a client writes as fast as it can (sequential PUTs of 64 keys round robin,
     values carrying their sequence number, 4 KiB each so that checkpoints run all the time): after each cut and
     reboot the store holds exactly a prefix of the acknowledged writes (every key at its last value up to one
     sequence number, nothing newer, nothing torn) that includes every write acknowledged a second before the cut;
     the writes then continue from there;
  3. a clean power off (signed POST /_k10s/power off): every acknowledged write is there after the next boot.

    storage_test.py --kernel ELF --bundle K10B [--admin-key FILE] [--controllers virtio,nvme,ahci] [--cycles N]
                    [--port P] [--out DIR] [--accel kvm|tcg] [--mem 128M] [--seed N]

The bundle must have the `kv` service (metal/mkbundle's default bundle does); without --admin-key a new key is
written to the output directory.

The kernel's QEMU disk is cache=writeback: a kill loses only what the guest had not written; ordering inside the
guest and the store's recovery are what is under test (k10_kvlog's crash model covers volatile write caches).
"""
import argparse
import http.client
import os
import random
import re
import struct
import subprocess
import sys
import threading
import time
import uuid
import zlib

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", "bench", "k10s"))
from k10s import admin_auth  # noqa: E402

MIB = 1 << 20
K10S_DATA = "6b10c5a0-da7a-4000-8000-000000000001"
KEYS = 64
VALUE = 4096


def gpt_disk(path, disk_mib, start_mib, size_mib):
    """A GPT disk of `disk_mib` with one partition K10S-DATA (blank) at `start_mib`."""
    total = disk_mib * MIB // 512
    entries = bytearray(128 * 128)
    first = start_mib * MIB // 512
    struct.pack_into("<16s16sQQQ", entries, 0, uuid.UUID(K10S_DATA).bytes_le, uuid.uuid4().bytes_le, first,
                     first + size_mib * MIB // 512 - 1, 0)
    name = "K10S-DATA".encode("utf-16-le")
    entries[56:56 + len(name)] = name
    disk_guid = uuid.uuid4().bytes_le

    def header(my, alt, at):
        h = bytearray(512)
        struct.pack_into("<8sIIIIQQQQ16sQIII", h, 0, b"EFI PART", 0x10000, 92, 0, 0, my, alt, 34, total - 34,
                         disk_guid, at, 128, 128, zlib.crc32(entries))
        struct.pack_into("<I", h, 16, zlib.crc32(h[:92]))
        return h

    mbr = bytearray(512)
    struct.pack_into("<BBBBBBBBII", mbr, 446, 0, 0, 2, 0, 0xEE, 0xFF, 0xFF, 0xFF, 1, min(total - 1, 0xFFFFFFFF))
    mbr[510:512] = b"\x55\xaa"
    with open(path, "wb") as f:
        f.truncate(disk_mib * MIB)
        for lba, data in ((0, mbr), (1, header(1, total - 1, 2)), (2, entries), (total - 33, entries),
                          (total - 1, header(total - 1, 1, total - 33))):
            f.seek(lba * 512)
            f.write(data)


def controller_args(kind, disk):
    drive = f"file={disk},if=none,id=d0,format=raw,cache=writeback"
    return {
        "virtio": ["-M", "pc", "-drive", drive, "-device", "virtio-blk-pci,drive=d0"],
        "nvme": ["-M", "pc", "-drive", drive, "-device", "nvme,drive=d0,serial=k10s"],
        "ahci": ["-M", "q35", "-drive", drive, "-device", "ide-hd,drive=d0,bus=ide.0"],
    }[kind]


class Guest:
    def __init__(self, a, kind, disk, n):
        self.log = os.path.join(a.out, f"storage-{kind}-{n}.log")
        if os.path.exists(self.log):
            os.remove(self.log)
        accel = ["-enable-kvm", "-cpu", "host"] if a.accel == "kvm" else ["-accel", "tcg,thread=multi", "-cpu", "max"]
        cmd = ["qemu-system-x86_64", *accel, *controller_args(kind, disk), "-m", a.mem, "-smp", "1", "-nodefaults",
               "-display", "none", "-no-reboot", "-netdev", f"user,id=n0,hostfwd=tcp:127.0.0.1:{a.port}-:80",
               "-device", "virtio-net-pci,netdev=n0,romfile=", "-fw_cfg", f"name=opt/k10s/admin_key,file={a.admin_key}",
               "-kernel", a.kernel, "-initrd", a.bundle, "-serial", f"file:{self.log}", "-append", ""]
        self.proc = subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        self.port = a.port

    def text(self):
        try:
            return open(self.log, errors="replace", newline="").read()
        except FileNotFoundError:
            return ""

    def wait_ready(self, timeout=120):
        t = time.time()
        while time.time() - t < timeout:
            if re.search(r"k10-metal ready .*\r", self.text()):
                return
            if self.proc.poll() is not None:
                break
            time.sleep(0.05)
        fail("no ready line", self)

    def kill(self):
        self.proc.kill()
        self.proc.wait()


def fail(msg, g=None):
    print(f"storage_test.py: FAIL: {msg}", file=sys.stderr)
    if g:
        print(f"--- {g.log} (tail)", file=sys.stderr)
        print("\n".join(g.text().splitlines()[-40:]), file=sys.stderr)
        g.kill()
    sys.exit(1)


def value(seq):
    head = f"{seq:010d}".encode()
    return head + bytes((seq + i) & 0xFF for i in range(VALUE - len(head)))


def connect(port):
    return http.client.HTTPConnection("127.0.0.1", port, timeout=10)


def read_all(port):
    """{key index: sequence number} of the keys present; values are checked whole."""
    c, got = connect(port), {}
    for k in range(KEYS):
        c.request("GET", f"/kv/k{k}")
        r = c.getresponse()
        body = r.read()
        if r.status == 404:
            continue
        if r.status != 200:
            raise RuntimeError(f"GET /kv/k{k}: {r.status}")
        seq = int(body[:10])
        if body != value(seq) or seq % KEYS != k:
            raise RuntimeError(f"k{k}: a torn or foreign value (sequence {seq}, {len(body)} bytes)")
        got[k] = seq
    return got


def check_prefix(got, acked, durable, what, g):
    """`got` must be the state after the writes 0..p for some p with durable <= p < acked."""
    p = max(got.values(), default=-1)
    if p >= acked:
        fail(f"{what}: sequence {p} found but only {acked} writes were acknowledged", g)
    if p < durable:
        fail(f"{what}: the store ends at write {p}, write {durable} was acknowledged well before the cut", g)
    for k in range(KEYS):
        want = None if p < k else p - (p - k) % KEYS
        if got.get(k) != want:
            fail(f"{what}: k{k} holds {got.get(k)}, the prefix up to {p} has {want}", g)
    return p + 1


def writer(port, start, stop, acked):
    """Sequential PUTs from `start` until `stop` is set or the connection dies; acked[0] = next unacknowledged,
    acked[1] = time of each acknowledgement (by sequence)."""
    seq = start
    try:
        c = connect(port)
        while not stop.is_set():
            c.request("PUT", f"/kv/k{seq % KEYS}", value(seq))
            r = c.getresponse()
            r.read()
            if r.status != 200:
                return
            seq += 1
            acked[0] = seq
            acked[1].append(time.time())
    except (OSError, http.client.HTTPException):
        pass


def run(a, kind):
    disk = os.path.join(a.out, f"storage-{kind}.img")
    gpt_disk(disk, 128, 1, 64)
    g = Guest(a, kind, disk, 0)
    g.wait_ready()
    t = g.text()
    if not re.search(r"storage: K10S-DATA grown to 12\d MiB", t) or "is blank: formatting" not in t:
        fail("K10S-DATA not grown and formatted on the first boot", g)
    if not re.search(r"storage: K10S-DATA on (virtio-blk|nvme|ahci)", t):
        fail("no store on the disk", g)
    print(f"storage_test.py: {kind}: grown, formatted", file=sys.stderr)
    nxt, n = 0, 1
    rng = random.Random(a.seed)
    for cycle in range(a.cycles):
        stop, acked = threading.Event(), [nxt, []]
        w = threading.Thread(target=writer, args=(a.port, nxt, stop, acked))
        w.start()
        time.sleep(rng.uniform(0.3, 3.0))
        cut = time.time()
        g.kill()
        stop.set()
        w.join()
        # Writes acknowledged a second before the cut are durable (2 ms window, then the batch's I/O).
        durable = nxt + sum(1 for x in acked[1] if x < cut - a.bound) - 1
        g = Guest(a, kind, disk, n)
        n += 1
        g.wait_ready()
        if not re.search(r"storage: K10S-DATA on .*records replayed", g.text()):
            fail("the store did not open after the cut", g)
        try:
            got = read_all(a.port)
        except (OSError, RuntimeError, http.client.HTTPException) as e:
            fail(f"cut {cycle}: {e}", g)
        written = acked[0] - nxt
        nxt = check_prefix(got, acked[0], durable, f"cut {cycle}", g)
        print(f"storage_test.py: {kind}: cut {cycle + 1}/{a.cycles}: {written} writes acknowledged, store ends at "
              f"{nxt - 1} (acknowledged up to {acked[0] - 1})", file=sys.stderr)
    # A clean power off keeps every acknowledged write.
    stop, acked = threading.Event(), [nxt, []]
    w = threading.Thread(target=writer, args=(a.port, nxt, stop, acked))
    w.start()
    time.sleep(1)
    stop.set()
    w.join()
    os.environ["K10S_ADMIN_TOKEN_FILE"] = a.admin_key
    c = connect(a.port)
    c.request("POST", "/_k10s/power", b"off", {"Authorization": admin_auth(f"127.0.0.1:{a.port}", "POST", "/_k10s/power", b"off")})
    if c.getresponse().status != 202:
        fail("power off refused", g)
    if g.proc.wait(timeout=60) != 0:
        fail("no clean power off", g)
    g = Guest(a, kind, disk, n)
    g.wait_ready()
    got = read_all(a.port)
    if max(got.values(), default=-1) != acked[0] - 1:
        fail(f"after a clean power off the store ends at {max(got.values(), default=-1)}, {acked[0] - 1} was acknowledged", g)
    s = connect(a.port)
    s.request("GET", "/_k10s/status", headers={"Authorization": admin_auth(f"127.0.0.1:{a.port}", "GET", "/_k10s/status", b"")})
    status = s.getresponse().read().decode()
    if '"durable":true' not in status or not re.search(r'"checkpoints":\d+', status):
        fail(f"status: {status[-300:]}", g)
    g.kill()
    print(f"storage_test.py: {kind}: clean power off kept all {acked[0]} writes: ok", file=sys.stderr)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--kernel", required=True)
    ap.add_argument("--bundle", required=True)
    ap.add_argument("--admin-key")
    ap.add_argument("--controllers", default="virtio,nvme,ahci")
    ap.add_argument("--cycles", type=int, default=5)
    ap.add_argument("--port", type=int, default=18190)
    ap.add_argument("--out", default="out/test")
    ap.add_argument("--accel", default="kvm" if os.access("/dev/kvm", os.R_OK | os.W_OK) else "tcg")
    ap.add_argument("--mem", default="128M")
    ap.add_argument("--seed", type=int, default=int(time.time()))
    ap.add_argument("--bound", type=float, default=1.0, help="seconds after which an acknowledged write is durable")
    a = ap.parse_args()
    os.makedirs(a.out, exist_ok=True)
    if not a.admin_key:
        a.admin_key = os.path.join(a.out, "storage-admin.key")
        fd = os.open(a.admin_key, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
        os.write(fd, os.urandom(32).hex().encode() + b"\n")
        os.close(fd)
    print(f"storage_test.py: seed {a.seed}", file=sys.stderr)
    for kind in a.controllers.split(","):
        run(a, kind)
    print("storage_test.py: ok", file=sys.stderr)


if __name__ == "__main__":
    main()
