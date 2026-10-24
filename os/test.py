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
  test.py update --image IMG --uki EFI --image-key KEY --boot DIR --k10s K10S --examples DIR (--bios | --uefi)
      A/B update of a single-node cluster to a re-signed image, with the power cut at every step (verified, slot
      written, armed, trial boot, commit request, committed): each boot serves on a good image and the KV state
      survives. Then an image that panics (the loaders fall back after 3 boots) and one that never commits (init
      falls back after os_trial); both are recorded as failed.
  test.py rollout --image IMG --uki EFI --image-key KEY --boot DIR --k10s K10S --examples DIR [--nodes 20]
      A cluster of BIOS VMs: an image that panics stops after the first wave (one node), then a good image rolls
      through every node in waves of 1, 2, 4, … with each wave committed before the next starts.
"""
import argparse
import hashlib
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

    def __init__(self, work, name, kernel, initrd, append, extra, reboot=False, mem="512M", smp=2, cut=None):
        """`cut`: a pattern on the console at which QEMU is killed at once (a power cut)."""
        self.log = os.path.join(work, f"{name}.log")
        self.qmp = os.path.join(work, f"{name}.qmp")
        open(self.log, "w").close()
        boot = ["-kernel", kernel, "-initrd", initrd, "-append", "console=ttyS0 panic=-1 " + append] if kernel else []
        # The firmware's debug port (SeaBIOS): what happened before the kernel, should a disk boot fail silently.
        self.fwlog = os.path.join(work, f"{name}.fw.log")
        if not kernel:
            boot = ["-chardev", f"file,id=fw,path={self.fwlog}", "-device", "isa-debugcon,iobase=0x402,chardev=fw"]
        cmd = [tool("qemu-system-x86_64"), "-nodefaults", "-no-user-config", "-display", "none",
               *([] if reboot else ["-no-reboot"]),
               "-m", mem, "-smp", str(smp), "-accel", "kvm" if KVM else "tcg", "-cpu", "host" if KVM else "max",
               *boot, "-serial", f"file:{self.log}", "-qmp", f"unix:{self.qmp},server=on,wait=off",
               "-device", "virtio-rng-pci"] + extra
        self.timeout = 90 if KVM else 300
        self.cmd = cmd
        self.proc = subprocess.Popen(cmd, stdin=subprocess.DEVNULL)
        self.cut = threading.Event()
        if cut:
            threading.Thread(target=self.cutter, args=(re.compile(cut.encode()),), daemon=True).start()

    def cutter(self, rx):
        """Kills QEMU within about a millisecond of `rx` appearing on the console (QEMU writes it synchronously)."""
        seen = b""
        with open(self.log, "rb") as log:
            while self.proc.poll() is None:
                chunk = log.read()
                if not chunk:
                    time.sleep(0.0005)
                    continue
                seen = seen[-4096:] + chunk
                if rx.search(seen):
                    self.proc.kill()
                    self.cut.set()
                    return

    def text(self):
        return open(self.log, errors="replace").read()

    def wait_for(self, pattern, what=None, since=0, timeout=None):
        """Waits for console output matching `pattern` (after offset `since`); returns the match."""
        rx = re.compile(pattern)
        end = time.time() + (timeout or self.timeout)
        while time.time() < end:
            text = self.text()
            m = rx.search(text, since)
            if m:
                return m
            if self.proc.poll() is not None:
                fail(f"QEMU exited ({self.proc.returncode}) before: {what or pattern}", self.log)
            time.sleep(0.05)
        self.proc.kill()
        if not self.text() and os.path.exists(self.fwlog):
            sys.stderr.write(open(self.fwlog, errors="replace").read()[-1500:])
            disk = next((d.split("=", 1)[1].split(",")[0] for d in self.cmd if d.startswith("file=")), None)
            if disk:
                env = dict(os.environ, MTOOLS_SKIP_CHECK="1")
                for c in (["mdir", "-/", "-i", f"{disk}@@{2 << 20}", "::/"], ["mtype", "-i", f"{disk}@@{2 << 20}", "::/boot/grub/grubenv"]):
                    sys.stderr.write(subprocess.run([tool(c[0]), *c[1:]], capture_output=True, text=True, env=env).stdout[:3000])
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

    def kill(self):
        """Power cut."""
        self.proc.kill()
        self.proc.wait()

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


def unsigned_payload(blob):
    """The UKI of a signed OS image (crates/k10-mesh/src/osimage.rs: the trailer ends in its length and K10S-END)."""
    if blob[-8:] != b"K10S-END":
        fail("not a signed OS image")
    return blob[:len(blob) - 12 - struct.unpack("<I", blob[-12:-8])[0]]


def pe_sections(pe):
    lfanew = struct.unpack_from("<I", pe, 0x3C)[0]
    nsec, optsz = struct.unpack_from("<HH", pe, lfanew + 6)[0], struct.unpack_from("<H", pe, lfanew + 20)[0]
    out = {}
    for i in range(nsec):
        h = lfanew + 24 + optsz + 40 * i
        name = pe[h:h + 8].rstrip(b"\0").decode()
        vsize, raw, ptr = struct.unpack_from("<I", pe, h + 8)[0], struct.unpack_from("<I", pe, h + 16)[0], struct.unpack_from("<I", pe, h + 20)[0]
        out[name] = pe[ptr:ptr + min(vsize, raw)]
    return out


class Images:
    """OS images for the update tests, from the image's own slot-A UKI: a re-signed copy (another version, so another
    hash), one whose kernel finds no init (it panics: `panic=5` reboots), and one whose node never asks to commit."""

    def __init__(self, a, work):
        sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
        import mkimage
        self.k10s, self.key = a.k10s, a.image_key
        self.pub = subprocess.run([a.k10s, "image", "pubkey", a.image_key], check=True, capture_output=True, text=True).stdout.strip()
        blob = open(a.uki, "rb").read()
        self.a = a.uki
        self.ids = {"a": hashlib.sha256(blob).hexdigest()}
        payload = unsigned_payload(blob)
        secs = pe_sections(payload)
        cmdline = secs[".cmdline"].rstrip(b"\0").decode().strip()
        stub = os.path.join(a.boot, "usr/lib/systemd/boot/efi/linuxx64.efi.stub")
        self.files = {}
        for name, extra in (("b", None), ("bad", " rdinit=/missing"), ("slow", " k10s.os-commit-grace=100000")):
            uki = os.path.join(work, f"{name}.unsigned.efi")
            if extra is None:
                open(uki, "wb").write(payload)
            else:
                parts = []
                for sec, data in ((".osrel", secs[".osrel"]), (".cmdline", (cmdline + extra).encode()),
                                  (".initrd", secs[".initrd"]), (".linux", secs[".linux"])):
                    parts.append((sec, os.path.join(work, f"{name}{sec}")))
                    open(parts[-1][1], "wb").write(data)
                mkimage.uki(stub, parts, uki)
            out = os.path.join(work, f"{name}.efi")
            subprocess.run([a.k10s, "image", "sign", "--key", a.image_key, "--arch", "x86_64", "--profile", "vm",
                            "--version", f"test-{name}", uki, out], check=True, stderr=subprocess.DEVNULL)
            self.files[name] = out
            self.ids[name] = hashlib.sha256(open(out, "rb").read()).hexdigest()

    def manifest(self, work, a, image, name):
        p = os.path.join(work, f"{name}.k10s")
        ex = os.path.abspath(a.examples)
        with open(p, "w") as f:
            f.write(f'service hello {{ code = "{ex}/hello.ten" memory = 64KiB replicas = 3 }}\n'
                    f'service counter {{ code = "{ex}/counter.ten" memory = 64KiB }}\n'
                    f'os {{ images = ["{os.path.abspath(self.files[image])}"] wave = 1 }}\n')
        return p


def configure(disk, work, k10s, conf_lines, name="node"):
    """k10s.conf, the admin key and the cluster key on K10S-BOOT of a disk image, like an operator does."""
    d = os.path.join(work, name)
    os.makedirs(d, exist_ok=True)
    conf = os.path.join(d, "k10s.conf")
    open(conf, "w").write("".join(f"{l}\n" for l in conf_lines))
    subprocess.run([tool("mcopy"), "-o", "-i", f"{disk}@@{2 << 20}", conf, os.path.join(work, "admin.key"),
                    os.path.join(work, "cluster.key"), "::/"], check=True, env=dict(os.environ, MTOOLS_SKIP_CHECK="1"))


def keys(work, k10s):
    for k in ("admin.key", "cluster.key"):
        if not os.path.exists(os.path.join(work, k)):
            subprocess.run([k10s, "keygen", "-o", os.path.join(work, k)], check=True)
    return os.path.join(work, "admin.key")


def apply(a, key, port, manifest):
    subprocess.run([a.k10s, "apply", manifest, "--node", f"127.0.0.1:{port}", "--admin-token-file", key], check=True,
                   stdout=subprocess.DEVNULL)


BOOTED = r"update: (?:Uefi|Bios) boot of slot ([ab]) \(x86_64-vm, ([0-9a-f]{8})(, trial)?\)"


def update_test(a, work):
    im = Images(a, work)
    key = keys(work, a.k10s)
    disk = os.path.join(work, "disk.img")
    fw_args = uefi_firmware(work) if a.uefi else []
    runs = [0]

    def fresh_disk():
        subprocess.run(["cp", "--sparse=always", a.image, disk], check=True)
        with open(disk, "r+b") as f:
            f.truncate(2 << 30)
        configure(disk, work, a.k10s, ["admin-token-file=/boot/admin.key", "cluster-key-file=/boot/cluster.key",
                                       "gossip=0.0.0.0:7946", "ntp=none", "watchdog=1", f"os_key={im.pub}", "os_trial=40",
                                       "os-commit-grace=3"])

    def power_on(cut=None):
        runs[0] += 1
        port = free_port()
        g = Guest(work, f"update{runs[0]}", None, None, None,
                  fw_args + ["-drive", f"file={disk},if=virtio,format=raw", "-device", "i6300esb",
                             "-netdev", f"user,id=n0,hostfwd=tcp:127.0.0.1:{port}-:80", "-device", "virtio-net-pci,netdev=n0"],
                  reboot=True, cut=cut)
        g.port = port
        return g

    def serving(g, since=0, path="/hello/"):
        """The boot after `since` comes up on a good image (A or B, as committed or trial) and serves."""
        m = g.wait_for(BOOTED, "booted slot", since)
        if m.group(2) not in (im.ids["a"][:8], im.ids["b"][:8]):
            fail(f"booted an unknown image {m.group(2)}", g.log)
        g.wait_for(r"ready: the node answers", "node ready", m.end())
        if http(g.port, path)[0] != 200:
            fail(f"GET {path}", g.log)
        return m

    def committed_b(g, since=0):
        """Boots (QEMU reboots in place) until slot B is committed; returns the offset after the commit."""
        c = g.wait_for(r"update: committed slot b", "commit", since, 180)
        return c.end()

    # A power cut at every step of an update, each on a fresh disk: the QEMU process is killed the moment init or the
    # node logs the step. Every next boot serves on a good image, the update completes, and the KV state survives.
    steps = ((r"update: verified ", "image verified"), (r"update: slot b written", "slot written"),
             (r"update: slot b armed", "trial armed"), (r"update: trial boot of slot b", "trial booted"),
             (r"os: trial image \w+ serves; asking init to commit it", "commit requested"),
             (r"update: committed slot b", "committed"))
    for pat, what in steps:
        fresh_disk()
        g = power_on(cut=pat)
        m = serving(g, path="/_k10s/health")
        if (m.group(1), m.group(2), m.group(3)) != ("a", im.ids["a"][:8], None):
            fail(f"first boot: {m.group(0)}", g.log)
        apply(a, key, g.port, im.manifest(work, a, "b", "to-b"))
        if http(g.port, "/counter/")[1] != "1\n":
            fail("counter before the update", g.log)
        if not g.cut.wait(180):
            fail(f"no power cut at: {what}", g.log)
        g.proc.wait()
        g = power_on()
        m = serving(g)
        state = f"slot {m.group(1)}{' trial' if m.group(3) else ''}"
        if not (m.group(1) == "b" and m.group(3) is None):
            committed_b(g, m.start())
            if http(g.port, "/hello/")[0] != 200:
                fail("GET /hello/ after the commit", g.log)
        if http(g.port, "/counter/")[1] != "2\n":
            fail(f"the KV state did not survive a power cut at: {what}", g.log)
        g.kill()
        print(f"test.py: update: power cut at {what}: next boot {state}, then slot b committed: ok", file=sys.stderr)

    # An image that panics: 3 trial boots, then the loader falls back to B; init records it as failed. Then an image
    # that boots but never asks to commit: init gives up after os_trial and falls back.
    fresh_disk()
    g = power_on()
    serving(g, path="/_k10s/health")
    apply(a, key, g.port, im.manifest(work, a, "b", "to-b"))
    committed_b(g)
    if http(g.port, "/hello/")[0] != 200:
        fail("GET /hello/ after the update", g.log)
    mark = len(g.text())
    apply(a, key, g.port, im.manifest(work, a, "bad", "to-bad"))
    g.wait_for(r"update: slot a armed", "bad image armed", mark)
    g.wait_for(rf"update: slot a \({im.ids['bad'][:8]}\) did not come up in 3 boots; back on slot b", "fallback", mark, 300)
    m = serving(g, mark)
    if m.group(1) != "b":
        fail(f"after the bad image: {m.group(0)}", g.log)
    g.wait_for(rf"os: rollout of {im.ids['bad'][:8]} stopped: node \w+ failed to commit it", "rollout halted", mark)
    print("test.py: update: an image that panics falls back after 3 boots: ok", file=sys.stderr)
    mark = len(g.text())
    apply(a, key, g.port, im.manifest(work, a, "slow", "to-slow"))
    m = g.wait_for(BOOTED, "trial of the slow image", g.wait_for(r"update: slot a armed", "slow image armed", mark).end(), 120)
    if m.group(3) is None or m.group(2) != im.ids["slow"][:8]:
        fail(f"slow image: {m.group(0)}", g.log)
    g.wait_for(r"update: the trial boot did not commit within 40 s; falling back", "trial timeout", m.end(), 120)
    m = serving(g, m.end())
    if (m.group(1), m.group(3)) != ("b", None):
        fail(f"after the slow image: {m.group(0)}", g.log)
    print("test.py: update: an image that never commits is abandoned after os_trial: ok", file=sys.stderr)
    g.power_button()
    g.wait_for(r"powering off", "power off")
    g.wait_exit()
    print(f"test.py: update ({'uefi' if a.uefi else 'bios'}): ok", file=sys.stderr)


def rollout_test(a, work):
    if not KVM:
        print("test.py: rollout: skipped (needs KVM)", file=sys.stderr)
        return
    im = Images(a, work)
    key = keys(work, a.k10s)
    n = a.nodes
    # Distinct ports: every socket stays bound until all are picked.
    held = []
    for kind in [socket.SOCK_DGRAM, socket.SOCK_STREAM, socket.SOCK_STREAM] * n:
        s = socket.socket(socket.AF_INET, kind)
        s.bind(("127.0.0.1", 0))
        held.append(s)
    nums = [s.getsockname()[1] for s in held]
    for s in held:
        s.close()
    ports = [tuple(nums[3 * i:3 * i + 3]) for i in range(n)]
    guests = []
    for i, (gp, rp, hp) in enumerate(ports):
        disk = os.path.join(work, f"disk{i}.img")
        subprocess.run(["cp", "--sparse=always", a.image, disk], check=True)
        # Peers reach each other through the host: QEMU forwards each node's ports from 127.0.0.1, which guests
        # see as 10.0.2.2.
        configure(disk, work, a.k10s, ["admin-token-file=/boot/admin.key", "cluster-key-file=/boot/cluster.key",
                                       f"gossip=0.0.0.0:{gp}", f"rpc=0.0.0.0:{rp}", "advertise=10.0.2.2",
                                       f"join=10.0.2.2:{ports[0][0]}", "ntp=none", f"os_key={im.pub}", "os_trial=120",
                                       "os-commit-grace=3", f"name=n{i}"], f"n{i}")
        fwd = f"hostfwd=udp:127.0.0.1:{gp}-:{gp},hostfwd=tcp:127.0.0.1:{rp}-:{rp},hostfwd=tcp:127.0.0.1:{hp}-:80"
        g = Guest(work, f"n{i}", None, None, None,
                  ["-drive", f"file={disk},if=virtio,format=raw", "-netdev", f"user,id=n0,{fwd}",
                   "-device", "virtio-net-pci,netdev=n0"], reboot=True, mem="192M", smp=1)
        g.timeout = 600
        g.port = hp
        guests.append(g)
    for g in guests:
        g.wait_for(r"ready: the node answers", "node ready")
    status = lambda g: subprocess.run([a.k10s, "status", "--node", f"127.0.0.1:{g.port}", "--admin-token-file", key],
                                      capture_output=True, text=True).stdout
    # Every node sees every member and its OS report.
    end = time.time() + 180
    while not all(f"members: {n} live" in (st := status(g)) and st.count("    os x86_64-vm ") == n for g in guests):
        if time.time() > end:
            fail(f"the membership did not converge: {status(guests[0])}", guests[0].log)
        time.sleep(1)
    print(f"test.py: rollout: {n} nodes up, each sees {n} OS reports", file=sys.stderr)

    def watch(pattern, since, until, wait_all=True, limit=600):
        """Host time of the first match per guest, polled until `until()` or the limit."""
        seen, end = {}, time.time() + limit
        while time.time() < end:
            for i, g in enumerate(guests):
                if i not in seen and re.search(pattern, g.text()[since[i]:]):
                    seen[i] = time.time()
            if until(seen):
                return seen
            time.sleep(0.2)
        fail(f"timeout watching {pattern}: {sorted(seen)}", guests[0].log)

    # A bad image: the first wave (one node) takes it, falls back, and the rollout stops there.
    marks = [len(g.text()) for g in guests]
    apply(a, key, guests[0].port, im.manifest(work, a, "bad", "to-bad"))
    back = watch(rf"did not come up in 3 boots", marks, lambda s: len(s) >= 1)
    canary = next(iter(back))
    stopped = watch(rf"os: rollout of {im.ids['bad'][:8]} stopped", marks, lambda s: len(s) == n, limit=120)
    armed = [i for i, g in enumerate(guests) if re.search(r"update: slot \w armed", g.text()[marks[i]:])]
    if armed != [canary]:
        for i in armed:
            sys.stderr.write("".join(f"n{i}: {l}\n" for l in guests[i].text()[marks[i]:].splitlines() if "os:" in l or "update:" in l))
        fail(f"the bad image was installed on nodes {armed}, want only the first wave [{canary}]")
    print(f"test.py: rollout: bad image stopped after the first wave (node {canary}); {len(stopped)} nodes halted", file=sys.stderr)
    # A good image rolls through every node, a wave at a time.
    marks = [len(g.text()) for g in guests]
    apply(a, key, guests[0].port, im.manifest(work, a, "b", "to-b"))
    events = {}
    end = time.time() + 900
    while len([1 for e in events.values() if "committed" in e]) < n:
        if time.time() > end:
            fail(f"rollout incomplete: {events}", guests[0].log)
        for i, g in enumerate(guests):
            t = g.text()[marks[i]:]
            for ev, pat in (("armed", r"update: slot \w armed"), ("committed", r"update: committed slot")):
                if ev not in events.setdefault(i, {}) and re.search(pat, t):
                    events[i][ev] = time.time()
        time.sleep(0.2)
    # Each node logs its wave when it starts; the waves must have the expected sizes, and a node may start only after
    # every node of the earlier waves committed (host times, polled every 0.2 s: 0.5 s of slack).
    wave = {}
    for i, g in enumerate(guests):
        m = re.search(rf"os: installing {im.ids['b'][:8]} \(wave (\d+)\)", g.text()[marks[i]:])
        if not m:
            fail(f"node {i} logged no wave", g.log)
        wave[i] = int(m.group(1))
    waves = [sum(1 for w in wave.values() if w == k) for k in range(max(wave.values()) + 1)]
    want, size, left = [], 1, n
    while left > 0:
        want.append(min(size, left))
        left -= size
        size *= 2
    for i in events:
        earlier = [events[j]["committed"] for j in events if wave[j] < wave[i]]
        if earlier and events[i]["armed"] < max(earlier) - 0.5:
            fail(f"node {i} (wave {wave[i]}) started {max(earlier) - events[i]['armed']:.1f} s before the earlier waves committed")
    if waves != want:
        fail(f"waves {waves}, want {want}")
    # Every node serves once the links to the last wave's nodes are back.
    end = time.time() + 60
    for g in guests:
        while True:
            try:
                if http(g.port, "/hello/") == (200, "hello"):
                    break
            except OSError:
                pass
            if time.time() > end:
                fail("GET /hello/ after the rollout", g.log)
            time.sleep(0.5)
    for g in guests:
        g.kill()
    print(f"test.py: rollout: good image on all {n} nodes in waves {waves}", file=sys.stderr)


def main():
    ap = argparse.ArgumentParser()
    tests = {"boot": boot_test, "dhcp": dhcp_test, "image": image_test, "unsigned": unsigned_test, "boottime": boottime_test,
             "update": update_test, "rollout": rollout_test}
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
    ap.add_argument("--uki")
    ap.add_argument("--image-key")
    ap.add_argument("--boot")
    ap.add_argument("--nodes", type=int, default=20)
    ap.add_argument("--k10s")
    ap.add_argument("--examples")
    ap.add_argument("--work")
    a = ap.parse_args()
    work = a.work or tempfile.mkdtemp(prefix="k10s-os-test.")
    os.makedirs(work, exist_ok=True)
    tests[a.test](a, work)


if __name__ == "__main__":
    main()
