#!/usr/bin/env python3
"""QEMU tests of PID 1 mode and of the disk images (docs/OS.md) without root: user networking, a host-side SNTP server
and, for the DHCP test, a host-side DHCP server on a QEMU socket netdev. os/test.sh builds the inputs and runs them.

  test.py boot --kernel K --initrd I --k10s HOST_K10S --examples DIR [--work DIR]
      A GPT disk with K10S-BOOT (k10s.conf, admin key) and K10S-DATA (ext4). Boot → apply → serve → power button,
      then a second boot that serves the applied services and their KV state → power button. The guest RTC starts in
      2001: the signed apply only succeeds once SNTP has set the clock.
  test.py dhcp --kernel K --initrd I [--work DIR]
      6 s leases from a scripted server: renew (unicast, ciaddr), rebind (broadcast), NAK → a new address, which
      restarts the cluster node so that it advertises the new address.
  test.py image --image IMG --k10s HOST_K10S --examples DIR (--bios | --uefi) [--metal] [--work DIR]
      A disk image (os/mkimage.py) on a 4 GiB disk, configured like an operator would: k10s.conf and the admin key
      copied to K10S-BOOT. Boot 1 grows K10S-DATA and its ext4 to the end of the disk, applies and serves; boot 2 keeps
      the state and grows nothing. --metal: q35, NVMe, e1000e and the Intel TCO watchdog (signed modules).
  test.py unsigned --kernel K --kmods DIR --k10s K10S --work DIR
      The metal kernel refuses e1000e once its signature is stripped (no network).
  test.py boottime --image IMG [--runs N]
      QEMU start → first HTTP 200 from the node, BIOS boot of the image (KVM); fails when the median reaches 1 s.
"""
import argparse
import json
import os
import re
import shutil
import socket
import statistics
import struct
import subprocess
import sys
import tempfile
import threading
import time
import urllib.request
import zlib

PATH = os.environ.get("PATH", "") + ":/sbin:/usr/sbin"


def tool(name):
    p = shutil.which(name, path=PATH)
    if not p:
        sys.exit(f"test.py: {name} not found")
    return p


def free_port(kind=socket.SOCK_STREAM):
    s = socket.socket(socket.AF_INET, kind)
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


def fail(msg, log=None):
    if log and os.path.exists(log):
        sys.stderr.write(open(log, errors="replace").read()[-6000:])
    sys.exit(f"test.py: FAIL: {msg}")


KVM = os.access("/dev/kvm", os.R_OK | os.W_OK)
OVMF = [("/usr/share/OVMF/OVMF_CODE_4M.fd", "/usr/share/OVMF/OVMF_VARS_4M.fd"),
        ("/usr/share/OVMF/OVMF_CODE.fd", "/usr/share/OVMF/OVMF_VARS.fd"),
        ("/usr/share/edk2/ovmf/OVMF_CODE.fd", "/usr/share/edk2/ovmf/OVMF_VARS.fd")]


def uefi_firmware(work):
    """pflash arguments for OVMF (Secure Boot off) with a private copy of its variable store."""
    for code, vars_ in OVMF:
        if os.path.exists(code) and os.path.exists(vars_):
            mine = os.path.join(work, "OVMF_VARS.fd")
            shutil.copyfile(vars_, mine)
            return ["-drive", f"if=pflash,format=raw,readonly=on,file={code}", "-drive", f"if=pflash,format=raw,file={mine}"]
    sys.exit("test.py: OVMF not found (package ovmf)")


class Guest:
    """One QEMU run: serial console to a file, QMP for the power button. Boots `kernel` + `initrd`, else the disk in
    `extra`."""

    def __init__(self, work, name, kernel, initrd, append, extra):
        self.log = os.path.join(work, f"{name}.log")
        self.qmp = os.path.join(work, f"{name}.qmp")
        open(self.log, "w").close()
        boot = ["-kernel", kernel, "-initrd", initrd, "-append", "console=ttyS0 panic=-1 " + append] if kernel else []
        cmd = [tool("qemu-system-x86_64"), "-nodefaults", "-no-user-config", "-display", "none", "-no-reboot",
               "-m", "512M", "-smp", "2", "-accel", "kvm" if KVM else "tcg", "-cpu", "host" if KVM else "max",
               *boot, "-serial", f"file:{self.log}", "-qmp", f"unix:{self.qmp},server=on,wait=off",
               "-device", "virtio-rng-pci"] + extra
        self.timeout = 90 if KVM else 300
        self.proc = subprocess.Popen(cmd, stdin=subprocess.DEVNULL)

    def wait_for(self, pattern, what=None):
        """Waits for console output matching `pattern`; returns the match."""
        rx = re.compile(pattern)
        end = time.time() + self.timeout
        while time.time() < end:
            text = open(self.log, errors="replace").read()
            m = rx.search(text)
            if m:
                return m
            if self.proc.poll() is not None:
                fail(f"QEMU exited ({self.proc.returncode}) before: {what or pattern}", self.log)
            time.sleep(0.05)
        self.proc.kill()
        fail(f"timeout waiting for: {what or pattern}", self.log)

    def power_button(self):
        s = socket.socket(socket.AF_UNIX)
        s.connect(self.qmp)
        f = s.makefile("rw")
        f.readline()
        for c in ("qmp_capabilities", "system_powerdown"):
            f.write(json.dumps({"execute": c}) + "\n")
            f.flush()
            while "return" not in json.loads(f.readline()):
                pass
        s.close()

    def wait_exit(self):
        try:
            rc = self.proc.wait(self.timeout)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            fail("the guest did not power off", self.log)
        if rc != 0:
            fail(f"QEMU exit status {rc}", self.log)


def sntp_server(port):
    """Answers SNTP requests with the host's clock (stratum 2)."""
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.bind(("127.0.0.1", port))

    def ntp_now():
        t = time.time() + 2208988800
        return (int(t) << 32) | int((t % 1) * 2**32)

    def serve():
        while True:
            req, peer = s.recvfrom(512)
            if len(req) < 48 or req[0] & 7 != 3:
                continue
            rx = ntp_now()
            hdr = bytes([(4 << 3) | 4, 2, 6, 0xEC]) + bytes(8) + b"LOCL"
            s.sendto(hdr + struct.pack(">Q", rx) + req[40:48] + struct.pack(">QQ", rx, ntp_now()), peer)

    threading.Thread(target=serve, daemon=True).start()


def http(port, path, method="GET"):
    r = urllib.request.urlopen(urllib.request.Request(f"http://127.0.0.1:{port}{path}", method=method), timeout=10)
    return r.status, r.read().decode()


def make_disk(work, k10s):
    """GPT: K10S-BOOT (FAT, k10s.conf + admin.key) and K10S-DATA (ext4)."""
    key = os.path.join(work, "admin.key")
    subprocess.run([k10s, "keygen", "-o", key], check=True)
    conf = os.path.join(work, "k10s.conf")
    with open(conf, "w") as f:
        f.write("# PID 1 test node\nadmin-token-file=/boot/admin.key\nwatchdog=1\n")
    esp, data, disk = (os.path.join(work, n) for n in ("esp.img", "data.img", "disk.img"))
    for p in (esp, data):
        if os.path.exists(p):
            os.remove(p)
    subprocess.run([tool("mkfs.fat"), "-C", "-n", "K10S-BOOT", esp, str(32 * 1024)], check=True, stdout=subprocess.DEVNULL)
    env = dict(os.environ, MTOOLS_SKIP_CHECK="1")
    subprocess.run([tool("mcopy"), "-i", esp, conf, key, "::/"], check=True, env=env)
    subprocess.run([tool("mkfs.ext4"), "-q", "-F", "-L", "K10S-DATA", data, "64M"], check=True)
    with open(disk, "wb") as f:
        f.truncate((1 + 32 + 64 + 1) << 20)
    layout = ("label: gpt\n"
              'start=2048, size=65536, type=C12A7328-F81F-11D2-BA4B-00A0C93EC93B, name="K10S-BOOT"\n'
              'start=67584, size=131072, type=0FC63DAF-8483-4772-8E79-3D69D8477DE4, name="K10S-DATA"\n')
    subprocess.run([tool("sfdisk"), "-q", disk], input=layout.encode(), check=True)
    for img, mib in ((esp, 1), (data, 33)):
        subprocess.run(["dd", f"if={img}", f"of={disk}", "bs=1M", f"seek={mib}", "conv=notrunc", "status=none"], check=True)
    return disk, key


def boot_test(a, work):
    disk, key = make_disk(work, a.k10s)
    ntp = free_port(socket.SOCK_DGRAM)
    sntp_server(ntp)
    for run in (1, 2):
        port = free_port()
        g = Guest(work, f"boot{run}", a.kernel, a.initrd, f"k10s.ntp=10.0.2.2:{ntp}",
                  ["-rtc", "base=2001-01-01T00:00:00", "-device", "i6300esb",
                   "-drive", f"file={disk},if=virtio,format=raw",
                   "-netdev", f"user,id=n0,hostfwd=tcp:127.0.0.1:{port}-:80", "-device", "virtio-net-pci,netdev=n0"])
        g.wait_for(r"k10s-init\[ *[\d.]+\]: k10s \S+ as PID 1", "init banner")
        g.wait_for(r"config: /dev/vda1 \(K10S-BOOT\): k10s.conf merged", "k10s.conf from K10S-BOOT")
        g.wait_for(r"net: eth0 DHCP 10\.0\.2\.15/24 gateway 10\.0\.2\.2", "DHCP lease")
        g.wait_for(r"data: /dev/vda2 \(ext4\) on /data", "data partition")
        g.wait_for(r"watchdog: armed", "watchdog")
        g.wait_for(r"sandbox mode=on user=65532\(65532:65532\) caps=none landlock=abi\d+ seccomp=kill", "node sandbox")
        g.wait_for(r"ready: the node answers", "node ready")
        g.wait_for(r"time: stepped by \+\d{9}\.\d+ s from 10\.0\.2\.2", "SNTP step from 2001")
        if run == 1:
            subprocess.run([a.k10s, "apply", os.path.join(a.examples, "app.k10s"), "--node", f"127.0.0.1:{port}",
                            "--admin-token-file", key], check=True)
        if http(port, "/hello/") != (200, "hello"):
            fail("GET /hello/", g.log)
        want = [f"{n}\n" for n in ((1, 2) if run == 1 else (3,))]
        got = [http(port, "/counter/")[1] for _ in want]
        if got != want:
            fail(f"counter: got {got}, want {want}", g.log)
        g.power_button()
        g.wait_for(r"shutdown: node stopped \(exit status 0\)", "node drained")
        g.wait_for(r"powering off", "power off")
        g.wait_exit()
        print(f"test.py: boot {run}: ok", file=sys.stderr)


def csum(b):
    s = sum(struct.unpack(f">{len(b) // 2}H", b))
    s = (s & 0xFFFF) + (s >> 16)
    return ~((s & 0xFFFF) + (s >> 16)) & 0xFFFF


class Dhcpd:
    """A scripted DHCP server speaking Ethernet frames over QEMU's UDP socket netdev; also answers ARP for itself."""

    MAC = bytes.fromhex("525400000002")
    IP = bytes([10, 0, 3, 2])

    def __init__(self, port, peer):
        self.s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self.s.bind(("127.0.0.1", port))
        self.peer = ("127.0.0.1", peer)
        self.log = []  # (ciaddr, unicast | broadcast) of every renewal
        self.done = threading.Event()
        self.error = None
        threading.Thread(target=self.serve, daemon=True).start()

    def send_ip(self, dst_mac, dst_ip, payload):
        udp = struct.pack(">HHHH", 67, 68, 8 + len(payload), 0) + payload
        ip = struct.pack(">BBHHHBBH4s4s", 0x45, 0, 20 + len(udp), 0, 0, 64, 17, 0, self.IP, dst_ip)
        ip = ip[:10] + struct.pack(">H", csum(ip)) + ip[12:]
        self.s.sendto(dst_mac + self.MAC + b"\x08\x00" + ip + udp, self.peer)

    def reply(self, req, kind, yiaddr, lease=6):
        xid, flags, ciaddr, chaddr = req[4:8], req[10:12], req[12:16], req[28:34]
        p = bytearray(236)
        p[0:4] = b"\x02\x01\x06\x00"
        p[4:8], p[10:12], p[16:20], p[28:34] = xid, flags, bytes(yiaddr) if kind != 6 else bytes(4), chaddr
        opts = [(53, bytes([kind])), (54, self.IP)]
        if kind != 6:
            opts += [(51, struct.pack(">I", lease)), (1, bytes([255, 255, 255, 0])), (3, bytes([10, 0, 3, 1]))]
        p += bytes([99, 130, 83, 99]) + b"".join(bytes([c, len(v)]) + v for c, v in opts) + b"\xff"
        unicast = ciaddr != bytes(4) and kind != 6
        self.send_ip(chaddr if unicast else b"\xff" * 6, ciaddr if unicast else b"\xff" * 4, bytes(p))

    def serve(self):
        # Renewals of 10.0.3.15: the 1st (unicast) is ACKed; then silence until the client rebinds (broadcast),
        # which is ACKed; the next is NAKed; discovery then gets 10.0.3.16, whose first renewal ends the script.
        phase = 0
        while True:
            f, _ = self.s.recvfrom(2048)
            eth = f[12:14]
            if eth == b"\x08\x06" and f[20:22] == b"\x00\x01" and f[38:42] in (self.IP, bytes([10, 0, 3, 1])):
                arp = f[14:20] + b"\x00\x02" + self.MAC + f[38:42] + f[22:28] + f[28:32]
                self.s.sendto(f[6:12] + self.MAC + b"\x08\x06" + arp, self.peer)
                continue
            ihl = (f[14] & 15) * 4
            if eth != b"\x08\x00" or f[23] != 17 or struct.unpack(">H", f[14 + ihl + 2:14 + ihl + 4])[0] != 67:
                continue
            unicast = f[30:34] == self.IP
            m = f[14 + ihl + 8:]
            opts, i = {}, 240
            while i < len(m) and m[i] != 255:
                if m[i] == 0:
                    i += 1
                    continue
                opts[m[i]] = m[i + 2:i + 2 + m[i + 1]]
                i += 2 + m[i + 1]
            kind, ciaddr = opts[53][0], socket.inet_ntoa(m[12:16])
            if kind == 1:
                self.reply(m, 2, [10, 0, 3, 16 if phase == 3 else 15])
            elif kind == 3 and ciaddr == "0.0.0.0":
                self.reply(m, 5, list(opts[50]))
            elif kind == 3:
                if 50 in opts or 54 in opts or m[10] & 0x80:
                    self.error = f"renewal with option 50/54 or the broadcast flag: {sorted(opts)}"
                self.log.append((ciaddr, "unicast" if unicast else "broadcast"))
                if ciaddr == "10.0.3.16":
                    self.reply(m, 5, list(m[12:16]))
                    self.done.set()
                elif phase == 0 and unicast or phase == 1 and not unicast:
                    self.reply(m, 5, list(m[12:16]))
                    phase += 1
                elif phase == 2:
                    self.reply(m, 6, [0, 0, 0, 0])
                    phase = 3


def dhcp_test(a, work):
    sp, qp = free_port(socket.SOCK_DGRAM), free_port(socket.SOCK_DGRAM)
    d = Dhcpd(sp, qp)
    # Cluster mode without `advertise`: the node advertises the lease's address, so a new address restarts it.
    g = Guest(work, "dhcp", a.kernel, a.initrd,
              "k10s.ntp=none k10s.data=none k10s.gossip=0.0.0.0:7946 k10s.insecure-unauthenticated-cluster=1",
              ["-netdev", f"socket,id=n0,udp=127.0.0.1:{sp},localaddr=127.0.0.1:{qp}",
               "-device", "virtio-net-pci,netdev=n0,mac=52:54:00:12:34:56"])
    g.wait_for(r"net: eth0 DHCP 10\.0\.3\.15/24 gateway 10\.0\.3\.1 from 10\.0\.3\.2, lease 6 s", "first lease")
    g.wait_for(r"ready: the node answers", "node ready")
    g.wait_for(r"lease of 10\.0\.3\.15 renewed", "renewal")
    g.wait_for(r"lease of 10\.0\.3\.15 rebound", "rebinding")
    g.wait_for(r"dhcp: lease of 10\.0\.3\.15 lost \(Nak\)", "NAK")
    g.wait_for(r"net: eth0 DHCP 10\.0\.3\.16/24", "new lease")
    g.wait_for(r"address changed from 10\.0\.3\.15 to 10\.0\.3\.16", "address change")
    g.wait_for(r"node: restarting to advertise the new address[\s\S]*ready: the node answers", "node restart")
    if not d.done.wait(g.timeout):
        fail("no renewal of 10.0.3.16", g.log)
    if d.error:
        fail(d.error, g.log)
    if not (d.log[0] == ("10.0.3.15", "unicast") and ("10.0.3.15", "broadcast") in d.log):
        fail(f"want a unicast renewal then a broadcast rebinding: {d.log}", g.log)
    g.power_button()
    g.wait_for(r"powering off", "power off")
    g.wait_exit()
    print("test.py: dhcp: ok", file=sys.stderr)


def data_partition_check(disk):
    """After the first boot: both GPT headers valid, the backup in the last sector, K10S-DATA ending at the last
    1 MiB boundary before it, and ext4 filling K10S-DATA. Returns an error or None."""
    total = os.path.getsize(disk) // 512
    with open(disk, "rb") as f:
        def read(lba, n=512):
            f.seek(lba * 512)
            return f.read(n)

        for lba, other in ((1, total - 1), (total - 1, 1)):
            h = read(lba, 92)
            if h[:8] != b"EFI PART" or struct.unpack_from("<QQ", h, 24) != (lba, other):
                return f"no GPT header at LBA {lba}"
            if zlib.crc32(h[:16] + bytes(4) + h[20:]) != struct.unpack_from("<I", h, 16)[0]:
                return f"GPT header checksum at LBA {lba}"
            ents_lba, n, esz, crc = struct.unpack_from("<QIII", h, 72)
            ents = read(ents_lba, n * esz)
            if zlib.crc32(ents) != crc:
                return f"GPT entries checksum (header at LBA {lba})"
        for i in range(n):
            e = ents[i * esz:(i + 1) * esz]
            if e[56:74].decode("utf-16-le") == "K10S-DATA":
                start, end = struct.unpack_from("<QQ", e, 32)
                break
        else:
            return "no K10S-DATA partition"
        if end != (total - 33) // 2048 * 2048 - 1:
            return f"K10S-DATA ends at LBA {end} of {total}"
        sb = read(start + 2, 1024)
        hi = struct.unpack_from("<I", sb, 0x150)[0] if struct.unpack_from("<I", sb, 0x60)[0] & 0x80 else 0
        blocks = hi << 32 | struct.unpack_from("<I", sb, 4)[0]
        if blocks * (1024 << struct.unpack_from("<I", sb, 0x18)[0]) != (end - start + 1) * 512:
            return f"ext4 has {blocks} blocks"
    return None


def image_test(a, work):
    disk = os.path.join(work, "disk.img")
    subprocess.run(["cp", "--sparse=always", a.image, disk], check=True)
    with open(disk, "r+b") as f:
        f.truncate(4 << 30)
    key = os.path.join(work, "admin.key")
    subprocess.run([a.k10s, "keygen", "-o", key], check=True)
    ntp = free_port(socket.SOCK_DGRAM)
    sntp_server(ntp)
    conf = os.path.join(work, "k10s.conf")
    with open(conf, "w") as f:
        f.write(f"# image test node\nadmin-token-file=/boot/admin.key\nwatchdog=1\nntp=10.0.2.2:{ntp}\n")
    subprocess.run([tool("mcopy"), "-o", "-i", f"{disk}@@{2 << 20}", conf, key, "::/"], check=True,
                   env=dict(os.environ, MTOOLS_SKIP_CHECK="1"))
    if a.metal:
        dev, nic = "nvme0n1p", "e1000e"
        hw = ["-M", "q35", "-drive", f"file={disk},if=none,id=d0,format=raw", "-device", "nvme,drive=d0,serial=k10s"]
    else:
        dev, nic = "vda", "virtio-net-pci"
        hw = ["-drive", f"file={disk},if=virtio,format=raw", "-device", "i6300esb"]
    fw = uefi_firmware(work) if a.uefi else []
    for run in (1, 2):
        port = free_port()
        g = Guest(work, f"image{run}", None, None, None,
                  fw + hw + ["-rtc", "base=2001-01-01T00:00:00", "-netdev", f"user,id=n0,hostfwd=tcp:127.0.0.1:{port}-:80",
                             "-device", f"{nic},netdev=n0"])
        g.wait_for(r"k10s-init\[ *[\d.]+\]: k10s \S+ as PID 1", "init banner")
        if a.metal:
            # The only NIC (e1000e) and watchdog (iTCO_wdt behind lpc_ich) are modules: the lease and "armed" below.
            g.wait_for(r"modules: [3-9] loaded by modalias", "signed modules")
        g.wait_for(rf"config: /dev/{dev}2 \(K10S-BOOT\): k10s.conf merged", "k10s.conf from K10S-BOOT")
        g.wait_for(r"net: eth0 DHCP 10\.0\.2\.15/24 gateway 10\.0\.2\.2", "DHCP lease")
        if run == 1:
            g.wait_for(rf"data: /dev/{dev}3 grown to 3837 MiB \(end of disk\)", "K10S-DATA grown")
        g.wait_for(rf"data: /dev/{dev}3 \(ext4\) on /data", "data partition")
        if run == 1:
            g.wait_for(r"data: ext4 grown to 3837 MiB", "ext4 grown")
        g.wait_for(r"watchdog: armed", "watchdog")
        g.wait_for(r"sandbox mode=on user=65532\(65532:65532\) caps=none landlock=abi\d+ seccomp=kill", "node sandbox")
        g.wait_for(r"ready: the node answers", "node ready")
        g.wait_for(r"time: stepped by \+\d{9}\.\d+ s from 10\.0\.2\.2", "SNTP step from 2001")
        if run == 1:
            subprocess.run([a.k10s, "apply", os.path.join(a.examples, "app.k10s"), "--node", f"127.0.0.1:{port}",
                            "--admin-token-file", key], check=True)
        if http(port, "/hello/") != (200, "hello"):
            fail("GET /hello/", g.log)
        want = [f"{n}\n" for n in ((1, 2) if run == 1 else (3,))]
        got = [http(port, "/counter/")[1] for _ in want]
        if got != want:
            fail(f"counter: got {got}, want {want}", g.log)
        g.power_button()
        g.wait_for(r"shutdown: node stopped \(exit status 0\)", "node drained")
        g.wait_for(r"powering off", "power off")
        g.wait_exit()
        if run == 1 and (err := data_partition_check(disk)):
            fail(err, g.log)
        if run == 2 and "grown" in open(g.log, errors="replace").read():
            fail("the second boot grew the data partition again", g.log)
        print(f"test.py: image {run}: ok", file=sys.stderr)


def unsigned_test(a, work):
    kmods = os.path.join(work, os.path.basename(a.kmods.rstrip("/")))
    shutil.copytree(a.kmods, kmods, symlinks=True)
    ko = next(os.path.join(r, f) for r, _, fs in os.walk(kmods) for f in fs if f == "e1000e.ko.zst")
    raw = subprocess.run([tool("zstd"), "-dqc", ko], stdout=subprocess.PIPE, check=True).stdout
    marker = b"~Module signature appended~\n"
    if not raw.endswith(marker):
        fail(f"{ko} is not signed")
    siglen = struct.unpack(">I", raw[-len(marker) - 4:-len(marker)])[0]
    raw = raw[:-(len(marker) + 12 + siglen)]
    subprocess.run([tool("zstd"), "-19qfo", ko], input=raw, check=True)
    initrd = os.path.join(work, "unsigned.cpio.zst")
    subprocess.run([sys.executable, os.path.join(os.path.dirname(os.path.abspath(__file__)), "mkinitramfs.py"), initrd,
                    "--init", a.k10s, "--kmods", kmods, "--mod", "*"], check=True)
    g = Guest(work, "unsigned", a.kernel, initrd, "k10s.netwait=2",
              ["-M", "q35", "-netdev", "user,id=n0", "-device", "e1000e,netdev=n0"])
    g.wait_for(r"e1000e\.ko\.zst: Key was rejected by service", "unsigned e1000e refused")
    g.wait_for(r"ready: the node answers", "node ready")
    if "DHCP" in open(g.log, errors="replace").read():
        fail("network configured without e1000e", g.log)
    g.power_button()
    g.wait_for(r"powering off", "power off")
    g.wait_exit()
    print("test.py: unsigned: ok", file=sys.stderr)


def boottime_test(a, work):
    if not KVM:
        print("test.py: boottime: skipped (needs KVM)", file=sys.stderr)
        return
    times = []
    for i in range(a.runs):
        port = free_port()
        cmd = [tool("qemu-system-x86_64"), "-nodefaults", "-no-user-config", "-display", "none", "-m", "512M", "-smp", "2",
               "-accel", "kvm", "-cpu", "host", "-snapshot", "-drive", f"file={a.image},if=virtio,format=raw",
               "-netdev", f"user,id=n0,hostfwd=tcp:127.0.0.1:{port}-:80", "-device", "virtio-net-pci,netdev=n0",
               "-device", "virtio-rng-pci", "-serial", f"file:{os.path.join(work, f'boottime{i}.log')}"]
        t0 = time.monotonic()
        p = subprocess.Popen(cmd, stdin=subprocess.DEVNULL)
        while True:
            try:
                if urllib.request.urlopen(f"http://127.0.0.1:{port}/_k10s/health", timeout=0.1).status == 200:
                    break
            except OSError:
                pass
            if time.monotonic() - t0 > 30 or p.poll() is not None:
                p.kill()
                fail("no HTTP 200 within 30 s", os.path.join(work, f"boottime{i}.log"))
            time.sleep(0.005)
        times.append(time.monotonic() - t0)
        p.kill()
        p.wait()
    med = statistics.median(times)
    print(f"test.py: boottime: {' '.join(f'{t * 1e3:.0f}' for t in times)} ms, median {med * 1e3:.0f} ms", file=sys.stderr)
    if med >= 1.0:
        fail(f"boot to first HTTP 200 takes {med * 1e3:.0f} ms (limit 1000 ms)")


def main():
    ap = argparse.ArgumentParser()
    tests = {"boot": boot_test, "dhcp": dhcp_test, "image": image_test, "unsigned": unsigned_test, "boottime": boottime_test}
    ap.add_argument("test", choices=tests)
    ap.add_argument("--kernel")
    ap.add_argument("--initrd")
    ap.add_argument("--image")
    ap.add_argument("--kmods")
    fw = ap.add_mutually_exclusive_group()
    fw.add_argument("--bios", action="store_true")
    fw.add_argument("--uefi", action="store_true")
    ap.add_argument("--metal", action="store_true")
    ap.add_argument("--runs", type=int, default=5)
    ap.add_argument("--k10s")
    ap.add_argument("--examples")
    ap.add_argument("--work")
    a = ap.parse_args()
    work = a.work or tempfile.mkdtemp(prefix="k10s-os-test.")
    os.makedirs(work, exist_ok=True)
    tests[a.test](a, work)


if __name__ == "__main__":
    main()
