#!/usr/bin/env python3
"""QEMU tests of PID 1 mode and of the disk images (docs/OS.md) without root: user networking, a host-side SNTP server
and, for the DHCP test, a host-side DHCP server on a QEMU socket netdev. os/test.sh builds the inputs and runs them.

  test.py boot --kernel K --initrd I --k10s HOST_K10S --examples DIR [--work DIR]
      A GPT disk with K10S-BOOT (k10s.conf, admin key) and K10S-DATA as older images made it (ext4). Boot 1 keeps
      the ext4 (k10s.data=/dev/vda2:ext4) → apply → serve → power button; boot 2 converts it to a k10s store and
      serves the applied services and their KV state; boot 3 serves them from the store. The guest RTC starts in
      2001: the signed apply only succeeds once SNTP has set the clock.
  test.py dhcp --kernel K --initrd I [--work DIR]
      6 s leases from a scripted server: renew (unicast, ciaddr), rebind (broadcast), NAK → a new address, which
      restarts the cluster node so that it advertises the new address.
  test.py image --image IMG --k10s HOST_K10S --examples DIR (--bios | --uefi) [--metal] [--work DIR]
      A disk image (os/mkimage.py) on a 4 GiB disk, configured like an operator would: k10s.conf and the admin key
      copied to K10S-BOOT. Boot 1 grows K10S-DATA and its store to the end of the disk, applies and serves; boot 2
      keeps the state and grows nothing. --metal: q35, NVMe, e1000e and the Intel TCO watchdog (signed modules). On i686:
      the MBR image on SeaBIOS, an Atom N270, an IDE disk and an RTL8139 (8139too, a signed module).
  test.py unsigned --kernel K --kmods DIR --k10s K10S --work DIR
      The metal kernel refuses e1000e once its signature is stripped (no network).
  test.py boottime --image IMG [--runs N]
      QEMU start → first HTTP 200 from the node, BIOS boot of the image (KVM); fails when the median reaches 1 s.
  test.py update --image IMG --uki EFI --image-key KEY --boot DIR --k10s K10S --examples DIR (--bios | --uefi)
      A/B update of a single-node cluster to a re-signed image, with the power cut at every step (verified, slot
      written, armed, trial boot, commit request, committed): each boot serves on a good image and the KV state
      survives. Then an image that panics (the loaders fall back after 3 boots) and one that never commits (init
      falls back after os_trial); both are recorded as failed. On i686 (BIOS): an IDE disk and an e1000 (a signed
      module).
  test.py rollout --image IMG --uki EFI --image-key KEY --boot DIR --k10s K10S --examples DIR [--nodes 20]
      A cluster of BIOS VMs: an image that panics stops after the first wave (one node), then a good image rolls
      through every node in waves of 1, 2, 4, … with each wave committed before the next starts.
  test.py pi --image IMG --uki FAT --image-key KEY --k10s K10S --examples DIR
      The Raspberry Pi image on QEMU virt behind a model of the Pi firmware (PiNode): K10S-BOOT and K10S-DATA found
      by label on the MBR disk, A/B update through autoboot.txt and tryboot with the power cut at every step, an image
      that panics (tried 3 times), one that never commits.
  test.py pi3 --image IMG --uki FAT --image-key KEY --k10s K10S --examples DIR
      The same on QEMU raspi3b (a Raspberry Pi 3: BCM2837, its devicetree from the slot, the SD card on SDHOST, no
      RTC: the clock comes from a host-side SNTP server), with QEMU's USB CDC NIC standing in for the LAN9514: it
      appears on the dwc2 bus after boot. The Pi 3 has no power button: each run ends in a power cut. No watchdog:
      QEMU's raspi3b resets as soon as it is armed.
  test.py mixed --image IMG --k10s K10S --host-k10s X86_K10S --examples DIR
      A Pi (the Pi image, as above) joins a cluster whose first member is an x86-64 k10s on the host; both serve
      services placed on either node.
  test.py secureboot --image IMG --uki EFI --image-key KEY --shim DIR --k10s K10S --examples DIR
      docs/OS.md "Secure Boot" under the firmware build with Microsoft's keys enrolled (OVMF/AAVMF .ms.fd), a fresh
      disk and variable store per case: the released image is refused; it serves with Secure Boot off, with a MOK under
      Debian's shim (--shim: os/bootfiles.sh shim-ARCH) and with own keys (x86-64: k10boot in front of systemd-boot,
      and a native kernel trusted through k10s.conf only is refused); under own keys an update re-signed for Secure
      Boot commits, and one signed by k10s only is refused by the firmware 3 times and rolled back.
  test.py tier --image IMG --image-key KEY --k10s K10S --examples DIR
      The tiers of the x86-64 image through k10boot (docs/OS.md "Tiers"), OVMF: tier=metal on pc and q35 (the native
      kernel, its k10s.conf and keys from K10S-BOOT, joins a hosted node on the host, serves, clears the boot counter),
      tier=auto (legacy virtio-net: native; e1000e: Tier 1), a failing native kernel (2 boots, then Tier 1 until the
      tier file is written again), kernels with an untrusted or broken signature refused, the signed kernel on PVH.
  test.py mb2 --image IMG --image-key KEY --k10s K10S --examples DIR [--bundle K10B]
      The x86-64 image on SeaBIOS with tier=metal: GRUB boots the native kernel through Multiboot2 with k10s.conf,
      the bundle and the keys as modules; it serves.
--arch aarch64 runs boot, image, update and secureboot on qemu-system-aarch64 (virt; UEFI: AAVMF); pi, pi3 and mixed
are aarch64 only. --arch armv7 runs boot, image and update on qemu-system-arm (virt without highmem, Cortex-A15; UEFI:
AAVMF32), --arch i686 on qemu-system-i386 (pc, an Atom N270; BIOS: SeaBIOS).
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


ARCH = "x86_64"
KVM = os.access("/dev/kvm", os.R_OK | os.W_OK)
OVMF = {"x86_64": [("/usr/share/OVMF/OVMF_CODE_4M.fd", "/usr/share/OVMF/OVMF_VARS_4M.fd"),
                   ("/usr/share/OVMF/OVMF_CODE.fd", "/usr/share/OVMF/OVMF_VARS.fd"),
                   ("/usr/share/edk2/ovmf/OVMF_CODE.fd", "/usr/share/edk2/ovmf/OVMF_VARS.fd")],
        "aarch64": [("/usr/share/AAVMF/AAVMF_CODE.fd", "/usr/share/AAVMF/AAVMF_VARS.fd")],
        "armv7": [("/usr/share/AAVMF/AAVMF32_CODE.fd", "/usr/share/AAVMF/AAVMF32_VARS.fd")]}
QEMU = {"x86_64": "qemu-system-x86_64", "aarch64": "qemu-system-aarch64", "i686": "qemu-system-i386", "armv7": "qemu-system-arm"}
# The serial console and the QEMU machine of the arch (aarch64 and armv7: virt with a PL011, PL031 RTC and gpio-keys;
# armv7 without highmem, which its kernel (no LPAE) cannot reach).
CONSOLE = {"x86_64": "ttyS0", "aarch64": "ttyAMA0", "i686": "ttyS0", "armv7": "ttyAMA0"}
MACHINE = {"aarch64": "virt", "armv7": "virt,highmem=off"}
# A fixed old CPU on i686 (32-bit only, SSE2, PAE and NX), a Cortex-A15 on armv7.
CPU = {"i686": "n270", "armv7": "cortex-a15"}
# The UKI stub name of the arch (systemd-boot's EFI architecture names).
EFI_ARCH = {"x86_64": "x64", "aarch64": "aa64", "i686": "ia32", "armv7": "arm"}


def uefi_firmware(work, name="OVMF_VARS"):
    """pflash arguments for OVMF/AAVMF (Secure Boot off) with a private copy of its variable store, `name`.fd."""
    for code, vars_ in OVMF[ARCH]:
        if os.path.exists(code) and os.path.exists(vars_):
            mine = os.path.join(work, f"{name}.fd")
            shutil.copyfile(vars_, mine)
            return ["-drive", f"if=pflash,format=raw,readonly=on,file={code}", "-drive", f"if=pflash,format=raw,file={mine}"]
    sys.exit("test.py: UEFI firmware not found (package ovmf, qemu-efi-aarch64 or qemu-efi-arm)")


class Guest:
    """One QEMU run: serial console to a file, QMP for the power button. Boots `kernel` + `initrd`, else the disk in
    `extra`."""

    def __init__(self, work, name, kernel, initrd, append, extra, reboot=False, mem="512M", smp=2, cut=None):
        """`cut`: a pattern on the console at which QEMU is killed at once (a power cut)."""
        self.log = os.path.join(work, f"{name}.log")
        self.qmp = os.path.join(work, f"{name}.qmp")
        open(self.log, "w").close()
        boot = ["-kernel", kernel, "-initrd", initrd, "-append", f"console={CONSOLE[ARCH]} panic=-1 " + append] if kernel else []
        # The firmware's debug port (SeaBIOS): what happened before the kernel, should a disk boot fail silently.
        self.fwlog = os.path.join(work, f"{name}.fw.log")
        if not kernel and ARCH in ("x86_64", "i686"):
            boot = ["-chardev", f"file,id=fw,path={self.fwlog}", "-device", "isa-debugcon,iobase=0x402,chardev=fw"]
        machine = ["-M", MACHINE[ARCH]] if ARCH in MACHINE and "-M" not in extra else []
        # The Pi 3 has its own CPU model and RNG, and no PCI bus.
        pi3 = "raspi3b" in extra
        cpu = [] if pi3 else ["-cpu", CPU.get(ARCH, "host" if KVM else "max")]
        rng = [] if pi3 else ["-device", "virtio-rng-pci"]
        cmd = [tool(QEMU[ARCH]), "-nodefaults", "-no-user-config", "-display", "none",
               *([] if reboot else ["-no-reboot"]), *machine,
               "-m", mem, "-smp", str(smp), "-accel", "kvm" if KVM else "tcg", *cpu,
               *boot, "-serial", f"file:{self.log}", "-qmp", f"unix:{self.qmp},server=on,wait=off", *rng] + extra
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


def mbr_parts(disk):
    """(start, size) in bytes of the primary partitions of an MBR disk."""
    with open(disk, "rb") as f:
        m = f.read(512)
    return [(struct.unpack_from("<I", m, 0x1BE + 16 * i + 8)[0] * 512, struct.unpack_from("<I", m, 0x1BE + 16 * i + 12)[0] * 512)
            for i in range(4)]


def mtools(cmd, disk, offset, *args, **kw):
    return subprocess.run([tool(cmd), "-i", f"{disk}@@{offset}", *args], check=True, env=dict(os.environ, MTOOLS_SKIP_CHECK="1"), **kw)


def boot_offset(disk):
    """Byte offset of K10S-BOOT: the GPT partition of that name, else partition 1 of an MBR disk (i686, Pi)."""
    with open(disk, "rb") as f:
        f.seek(512)
        h = f.read(92)
        if h[:8] == b"EFI PART":
            ents_lba, n, esz = struct.unpack_from("<QII", h, 72)
            f.seek(ents_lba * 512)
            ents = f.read(n * esz)
            for i in range(n):
                e = ents[i * esz:(i + 1) * esz]
                if e[56:74].decode("utf-16-le") == "K10S-BOOT":
                    return struct.unpack_from("<Q", e, 32)[0] * 512
            fail(f"{disk}: no K10S-BOOT partition")
    return mbr_parts(disk)[0][0]


class PiNode:
    """A Raspberry Pi on QEMU behind a model of its boot firmware, as far as k10s relies on it: each boot reads
    autoboot.txt from partition 1 of the MBR disk and boots the partition of its [all] section, or once of its
    [tryboot] section when the previous boot ended in `reboot "0 tryboot"` (the kernel logs the command); it loads
    kernel8.img, initramfs8 and cmdline.txt from that FAT partition and passes the partition and the tryboot flag in
    /chosen/bootloader of the devicetree. A reboot of the guest starts the next boot; a power cut (`cut`, kill) or a
    power-off ends the run. Has the interface of Guest; its console is that of every boot so far. On QEMU virt (the
    Pi 4/5 model, whose SoCs QEMU 10 has no Ethernet for) the devicetree is virt's; with `pi3` the machine is raspi3b
    and the devicetree the slot's bcm2710-rpi-3-b.dtb, as the firmware loads it."""

    def __init__(self, work, name, disk, extra, cut=None, mem="512M", smp=2, pi3=False):
        self.work, self.name, self.disk, self.extra, self.cutpat, self.pi3 = work, name, disk, extra, cut, pi3
        self.mem, self.smp = ("1G", 4) if pi3 else (mem, smp)
        self.log = os.path.join(work, f"{name}.log")
        self.boots, self.tryboot, self.stopped = [], False, False
        self.cut, self.done = threading.Event(), threading.Event()
        self.timeout = 600 if pi3 else 300
        if not pi3:
            dtb = os.path.join(work, f"{name}.virt.dtb")
            subprocess.run([tool("qemu-system-aarch64"), "-M", f"virt,dumpdtb={dtb}", "-m", mem, "-smp", str(smp), "-accel",
                            "kvm" if KVM else "tcg", "-cpu", "host" if KVM else "max", "-nodefaults", "-display", "none"],
                           check=True, capture_output=True)
            self.dts = subprocess.run([tool("dtc"), "-q", "-I", "dtb", "-O", "dts", dtb], check=True, capture_output=True, text=True).stdout
        self.boot()
        threading.Thread(target=self.run, daemon=True).start()

    def boot(self):
        n = len(self.boots) + 1
        parts = mbr_parts(self.disk)
        auto = mtools("mtype", self.disk, parts[0][0], "::/autoboot.txt", capture_output=True, text=True).stdout
        section = auto.split("[tryboot]" if self.tryboot else "[all]")[1].split("[")[0]
        part = int(re.search(r"boot_partition=(\d+)", section).group(1))
        d = os.path.join(self.work, f"{self.name}.{n}")
        os.makedirs(d, exist_ok=True)
        files = ("kernel8.img", "initramfs8", "cmdline.txt") + (("bcm2710-rpi-3-b.dtb",) if self.pi3 else ())
        for f in files:
            mtools("mcopy", self.disk, parts[part - 1][0], f"::/{f}", os.path.join(d, f), "-o")
        dts = self.dts if not self.pi3 else subprocess.run(
            [tool("dtc"), "-q", "-I", "dtb", "-O", "dts", os.path.join(d, "bcm2710-rpi-3-b.dtb")], check=True, capture_output=True, text=True).stdout
        dtb = os.path.join(d, "pi.dtb")
        subprocess.run([tool("dtc"), "-q", "-I", "dts", "-O", "dtb", "-o", dtb, "-"], check=True, text=True,
                       input=dts + f"/ {{ chosen {{ bootloader {{ partition = <{part}>; tryboot = <{int(self.tryboot)}>; }}; }}; }};\n")
        self.tryboot = False
        cmdline = open(os.path.join(d, "cmdline.txt")).read().strip()
        machine = ["-M", "raspi3b"] if self.pi3 else []
        g = Guest(self.work, f"{self.name}.{n}", os.path.join(d, "kernel8.img"), os.path.join(d, "initramfs8"),
                  f"{cmdline} console=ttyAMA0", machine + ["-dtb", dtb] + self.extra, mem=self.mem, smp=self.smp, cut=self.cutpat)
        self.boots.append(g)

    def run(self):
        while True:
            g = self.boots[-1]
            g.proc.wait()
            t = g.text()
            if g.cut.is_set() or self.stopped or "reboot: Power down" in t:
                self.cut.set() if g.cut.is_set() else None
                self.done.set()
                return
            self.tryboot = "with command '0 tryboot'" in t
            self.boot()

    @property
    def proc(self):
        return self.boots[-1].proc

    def text(self):
        return "".join(g.text() for g in self.boots)

    def wait_for(self, pattern, what=None, since=0, timeout=None):
        rx = re.compile(pattern)
        end = time.time() + (timeout or self.timeout)
        while time.time() < end:
            m = rx.search(self.text(), since)
            if m:
                return m
            if self.done.is_set():
                break
            time.sleep(0.05)
        stopped = self.done.is_set()
        self.kill()
        with open(self.log, "w") as f:
            f.write(self.text())
        fail(f"{'the Pi stopped' if stopped else 'timeout'} waiting for: {what or pattern}", self.log)

    def power_button(self):
        self.boots[-1].power_button()

    def kill(self):
        self.stopped = True
        self.boots[-1].kill()
        self.done.wait(10)

    def wait_exit(self):
        if not self.done.wait(self.timeout) or self.boots[-1].proc.returncode != 0:
            fail("the Pi did not power off", self.log)


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
    """GPT: K10S-BOOT (FAT, k10s.conf + admin.key) and K10S-DATA (ext4, as images before the k10s store)."""
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
    for run in (1, 2, 3):
        port = free_port()
        ext4 = " k10s.data=/dev/vda2:ext4" if run == 1 else ""
        g = Guest(work, f"boot{run}", a.kernel, a.initrd, f"k10s.ntp=10.0.2.2:{ntp}{ext4}",
                  ["-rtc", "base=2001-01-01T00:00:00", "-device", "i6300esb",
                   "-drive", f"file={disk},if=virtio,format=raw",
                   "-netdev", f"user,id=n0,hostfwd=tcp:127.0.0.1:{port}-:80", "-device", "virtio-net-pci,netdev=n0"])
        g.wait_for(r"k10s-init\[ *[\d.]+\]: k10s \S+ as PID 1", "init banner")
        g.wait_for(r"config: /dev/vda1 \(K10S-BOOT\): k10s.conf merged", "k10s.conf from K10S-BOOT")
        g.wait_for(r"net: eth0 DHCP 10\.0\.2\.15/24 gateway 10\.0\.2\.2", "DHCP lease")
        if run == 1:
            g.wait_for(r"data: /dev/vda2 \(ext4\) on /data", "data partition")
        else:
            if run == 2:
                g.wait_for(r"data: /dev/vda2 converted from ext4 to a k10s store", "ext4 converted")
            g.wait_for(r"data: /dev/vda2 \(k10s store\) for the node", "data partition")
            g.wait_for(r"k10s: data store fd 4: 61 MiB, \d+ keys in \d+ namespaces", "store opened")
        g.wait_for(r"watchdog: armed", "watchdog")
        g.wait_for(r"sandbox mode=on user=65532\(65532:65532\) caps=none landlock=abi\d+ seccomp=kill", "node sandbox")
        g.wait_for(r"ready: the node answers", "node ready")
        g.wait_for(r"time: stepped by \+\d{9}\.\d+ s from 10\.0\.2\.2", "SNTP step from 2001")
        if run == 1:
            subprocess.run([a.k10s, "apply", os.path.join(a.examples, "app.k10s"), "--node", f"127.0.0.1:{port}",
                            "--admin-token-file", key], check=True)
        if http(port, "/hello/") != (200, "hello"):
            fail("GET /hello/", g.log)
        want = [f"{n}\n" for n in ((1, 2) if run == 1 else (run + 1,))]
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
    1 MiB boundary before it (MBR: partition 2 ending at the last 1 MiB boundary of the disk), and its k10s store
    filling it. Returns an error or None."""
    total = os.path.getsize(disk) // 512
    with open(disk, "rb") as f:
        def read(lba, n=512):
            f.seek(lba * 512)
            return f.read(n)

        if read(1, 8) != b"EFI PART":
            start, size = (x // 512 for x in mbr_parts(disk)[1])
            if start + size != total // 2048 * 2048:
                return f"K10S-DATA ends at LBA {start + size} of {total}"
            return store_fills(read, start, start + size - 1)

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
        return store_fills(read, start, end)


def store_superblock(b):
    """(generation, segments) of the current k10s store superblock in the store's first 8 KiB, or None."""
    best = None
    for at in (0, 4096):
        sb = b[at:at + 512]
        if sb[:8] == b"K10SDATA" and zlib.crc32(sb[:508]) == struct.unpack_from("<I", sb, 508)[0]:
            gen, segments = struct.unpack_from("<Q", sb, 16)[0], struct.unpack_from("<I", sb, 68)[0]
            if best is None or gen > best[0]:
                best = (gen, segments)
    return best


def store_fills(read, start, end):
    """None if the k10s store at LBA `start` covers the partition up to LBA `end` (to a 4 MiB segment), else an
    error."""
    sb = store_superblock(read(start, 8192))
    if sb is None:
        return "K10S-DATA holds no k10s store"
    size = (33 + 4 * sb[1]) << 20
    if not 0 <= (end - start + 1) * 512 - size < 4 << 20:
        return f"the store covers {size >> 20} MiB of {((end - start + 1) * 512) >> 20}"
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
    mtools("mcopy", disk, boot_offset(disk), "-o", conf, key, "::/")
    if a.metal:
        dev, nic = "nvme0n1p", "e1000e"
        hw = ["-M", "q35", "-drive", f"file={disk},if=none,id=d0,format=raw", "-device", "nvme,drive=d0,serial=k10s"]
    elif ARCH == "i686":
        # An old PC: IDE disk (ata_piix), RTL8139 (8139too, a signed module).
        dev, nic = "sda", "rtl8139"
        hw = ["-drive", f"file={disk},if=ide,format=raw", "-device", "i6300esb"]
    else:
        dev, nic = "vda", "virtio-net-pci"
        hw = ["-drive", f"file={disk},if=virtio,format=raw", "-device", "i6300esb"]
    # x86-64: p1 is GRUB's BIOS boot partition.
    pboot, pdata = (2, 3) if ARCH == "x86_64" else (1, 2)
    # K10S-DATA grown to the end of the 4 GiB disk: GPT keeps its backup header in the last MiB, MBR does not.
    grown = 3839 if ARCH == "i686" else 3837
    fw = uefi_firmware(work) if a.uefi else []
    for run in (1, 2):
        port = free_port()
        g = Guest(work, f"image{run}", None, None, None,
                  fw + hw + ["-rtc", "base=2001-01-01T00:00:00", "-netdev", f"user,id=n0,hostfwd=tcp:127.0.0.1:{port}-:80",
                             "-device", f"{nic},netdev=n0"])
        if a.uefi and ARCH == "x86_64":
            g.wait_for(r"k10boot: tier linux \(from /k10s/tier\)", "k10boot boots Tier 1")
        g.wait_for(r"k10s-init\[ *[\d.]+\]: k10s \S+ as PID 1", "init banner")
        if a.metal or ARCH == "i686":
            # The only NIC (e1000e, 8139too) and the watchdog (iTCO_wdt behind lpc_ich, i6300esb) are modules: the lease
            # and "armed" below.
            g.wait_for(r"modules: [3-9] loaded by modalias", "signed modules")
        g.wait_for(rf"config: /dev/{dev}{pboot} \(K10S-BOOT\): k10s.conf merged", "k10s.conf from K10S-BOOT")
        g.wait_for(r"net: eth0 DHCP 10\.0\.2\.15/24 gateway 10\.0\.2\.2", "DHCP lease")
        if run == 1:
            g.wait_for(rf"data: /dev/{dev}{pdata} grown to {grown} MiB \(end of disk\)", "K10S-DATA grown")
        g.wait_for(rf"data: /dev/{dev}{pdata} \(k10s store\) for the node", "data partition")
        g.wait_for(r"k10s: data store fd 4: 3837 MiB", "the store fills K10S-DATA")
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

    def __init__(self, a, work, pi=False):
        sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
        sys.dont_write_bytecode = True
        import mkimage
        self.k10s, self.key = a.k10s, a.image_key
        self.pub = subprocess.run([a.k10s, "image", "pubkey", a.image_key], check=True, capture_output=True, text=True).stdout.strip()
        blob = open(a.uki, "rb").read()
        self.a = a.uki
        self.ids = {"a": hashlib.sha256(blob).hexdigest()}
        payload = unsigned_payload(blob)
        # The signed header after the payload: "K10S-IMG", format, then the arch and profile, each with its length.
        h = blob[len(payload) + 9:]
        self.profile = h[h[0] + 2:h[0] + 2 + h[h[0] + 1]].decode()
        self.files = {}
        if pi:
            # The slot image is a FAT filesystem: the variants change its cmdline.txt.
            for name, extra in (("b", None), ("bad", " rdinit=/missing"), ("slow", " k10s.os-commit-grace=100000")):
                fat = os.path.join(work, f"{name}.unsigned.fat")
                open(fat, "wb").write(payload)
                if extra:
                    cmdline = mtools("mtype", fat, 0, "::/cmdline.txt", capture_output=True, text=True).stdout.strip()
                    open(os.path.join(work, "cmdline.txt"), "w").write(cmdline + extra + "\n")
                    mtools("mcopy", fat, 0, "-o", os.path.join(work, "cmdline.txt"), "::/cmdline.txt")
                self.sign(a, fat, name, self.profile)
            return
        secs = pe_sections(payload)
        cmdline = secs[".cmdline"].rstrip(b"\0").decode().strip()
        stub = os.path.join(a.boot, f"usr/lib/systemd/boot/efi/linux{EFI_ARCH[ARCH]}.efi.stub")
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
            self.sign(a, uki, name, self.profile)

    def sign(self, a, payload, name, profile):
        out = os.path.join(os.path.dirname(payload), f"{name}.img")
        subprocess.run([a.k10s, "image", "sign", "--key", a.image_key, "--arch", ARCH, "--profile", profile,
                        "--version", f"test-{name}", payload, out], check=True, stderr=subprocess.DEVNULL)
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
    mtools("mcopy", disk, boot_offset(disk), "-o", conf, os.path.join(work, "admin.key"), os.path.join(work, "cluster.key"), "::/")


def keys(work, k10s):
    for k in ("admin.key", "cluster.key"):
        if not os.path.exists(os.path.join(work, k)):
            subprocess.run([k10s, "keygen", "-o", os.path.join(work, k)], check=True)
    return os.path.join(work, "admin.key")


def apply(a, key, port, manifest):
    subprocess.run([a.k10s, "apply", manifest, "--node", f"127.0.0.1:{port}", "--admin-token-file", key], check=True,
                   stdout=subprocess.DEVNULL)


BOOTED = r"update: (?:Uefi|Bios|Pi) boot of slot ([ab]) \(\w+-\w+, ([0-9a-f]{8})(, trial)?\)"


def update_test(a, work, pi=False, pi3=False):
    im = Images(a, work, pi)
    key = keys(work, a.k10s)
    disk = os.path.join(work, "disk.img")
    fw_args = uefi_firmware(work) if a.uefi else []
    runs = [0]
    # The Pi 3 has no RTC: its clock (and with it the signed admin requests) waits for SNTP. QEMU's raspi3b resets as soon
    # as the watchdog is armed (it models the reset configuration, not the countdown): no watchdog there.
    ntp, watchdog = "ntp=none", "watchdog=1"
    if pi3:
        port = free_port(socket.SOCK_DGRAM)
        sntp_server(port)
        ntp, watchdog = f"ntp=10.0.2.2:{port}", "watchdog=0"

    def fresh_disk():
        subprocess.run(["cp", "--sparse=always", a.image, disk], check=True)
        with open(disk, "r+b") as f:
            f.truncate(2 << 30)
        configure(disk, work, a.k10s, ["admin-token-file=/boot/admin.key", "cluster-key-file=/boot/cluster.key",
                                       "gossip=0.0.0.0:7946", ntp, watchdog, f"os_key={im.pub}", "os_trial=40",
                                       "os-commit-grace=3"])

    def power_on(cut=None):
        runs[0] += 1
        port = free_port()
        net = ["-netdev", f"user,id=n0,hostfwd=tcp:127.0.0.1:{port}-:80"]
        if pi3:
            # The SD card, and QEMU's USB CDC NIC on the dwc2 bus in place of the LAN9514.
            hw = ["-drive", f"file={disk},if=sd,format=raw"] + net + ["-device", "usb-net,netdev=n0"]
        elif ARCH == "i686":
            hw = ["-drive", f"file={disk},if=ide,format=raw", "-device", "i6300esb"] + net + ["-device", "e1000,netdev=n0"]
        else:
            hw = ["-drive", f"file={disk},if=virtio,format=raw", "-device", "i6300esb"] + net + ["-device", "virtio-net-pci,netdev=n0"]
        if pi:
            g = PiNode(work, f"update{runs[0]}", disk, hw, cut=cut, pi3=pi3)
        else:
            g = Guest(work, f"update{runs[0]}", None, None, None, fw_args + hw, reboot=True, cut=cut)
        g.port = port
        return g

    def serving(g, since=0, path="/hello/"):
        """The boot after `since` comes up on a good image (A or B, as committed or trial) and serves."""
        m = g.wait_for(BOOTED, "booted slot", since)
        if m.group(2) not in (im.ids["a"][:8], im.ids["b"][:8]):
            fail(f"booted an unknown image {m.group(2)}", g.log)
        g.wait_for(r"ready: the node answers", "node ready", m.end())
        if pi3:
            # Init reports the slot once it has hashed the image, alongside the network and SNTP: the step may come
            # first, so it is looked for from the start of this boot.
            boot = g.text().rfind(" as PID 1 on Linux ", 0, m.start())
            g.wait_for(r"time: stepped by \+\d+\.\d+ s from 10\.0\.2\.2", "SNTP step", max(boot, since))
        # A cluster node restores its services from the persisted desired state once it is up.
        end = time.time() + 30
        while True:
            try:
                if http(g.port, path)[0] == 200:
                    return m
            except OSError:
                pass
            if time.time() > end:
                fail(f"GET {path}", g.log)
            time.sleep(0.2)

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
    if pi3:
        g.kill()
    else:
        g.power_button()
        g.wait_for(r"powering off", "power off")
        g.wait_exit()
    print(f"test.py: update ({'pi3' if pi3 else 'pi' if pi else 'uefi' if a.uefi else 'bios'}): ok", file=sys.stderr)


def pi_test(a, work):
    update_test(a, work, pi=True)


def pi3_test(a, work):
    update_test(a, work, pi=True, pi3=True)


def mixed_test(a, work):
    """A Pi joins a cluster whose first member is an x86-64 node on the host: gossip and peer links between the two
    architectures (the host node advertises 10.0.2.2, the Pi's address through QEMU's forwards; the Pi's 127.0.0.1 is
    its forwarded ports on the host)."""
    with open(a.host_k10s, "rb") as f:
        if f.read(20)[18:20] != struct.pack("<H", 62):
            fail(f"{a.host_k10s} is not an x86-64 k10s")
    key = keys(work, a.k10s)
    gp, rp, hp, vgp, vrp, vhp = (free_port(socket.SOCK_DGRAM), free_port(), free_port(), free_port(socket.SOCK_DGRAM), free_port(), free_port())
    data = os.path.join(work, "host-data")
    host_log = os.path.join(work, "host.log")
    host = subprocess.Popen([a.host_k10s, "node", "--name", "x86", "--data", data, "--http", f"127.0.0.1:{hp}",
                             "--gossip", f"127.0.0.1:{gp}", "--rpc", f"127.0.0.1:{rp}", "--advertise", "10.0.2.2",
                             "--cluster-key-file", os.path.join(work, "cluster.key"), "--admin-token-file", key, "--sandbox", "off"],
                            stdout=open(host_log, "w"), stderr=subprocess.STDOUT)
    try:
        disk = os.path.join(work, "pi.img")
        subprocess.run(["cp", "--sparse=always", a.image, disk], check=True)
        with open(disk, "r+b") as f:
            f.truncate(2 << 30)
        configure(disk, work, a.k10s, ["admin-token-file=/boot/admin.key", "cluster-key-file=/boot/cluster.key",
                                       f"gossip=0.0.0.0:{vgp}", f"rpc=0.0.0.0:{vrp}", "advertise=127.0.0.1",
                                       f"join=10.0.2.2:{gp}", "ntp=none", "name=pi"])
        fwd = f"hostfwd=udp:127.0.0.1:{vgp}-:{vgp},hostfwd=tcp:127.0.0.1:{vrp}-:{vrp},hostfwd=tcp:127.0.0.1:{vhp}-:80"
        g = PiNode(work, "mixed", disk, ["-drive", f"file={disk},if=virtio,format=raw", "-netdev", f"user,id=n0,{fwd}",
                                          "-device", "virtio-net-pci,netdev=n0"])
        g.wait_for(r"update: Pi boot of slot a \(aarch64-pi, ", "Pi boot")
        g.wait_for(r"ready: the node answers", "node ready")
        status = lambda port: subprocess.run([a.k10s, "status", "--node", f"127.0.0.1:{port}", "--admin-token-file", key],
                                             capture_output=True, text=True).stdout
        # One cluster, and the Pi's OS report (gossip) reached the x86 node.
        end = time.time() + 120
        while not (all("members: 2 live" in status(p) for p in (hp, vhp)) and "    os aarch64-pi " in status(hp)):
            if time.time() > end:
                fail(f"the x86 node and the Pi did not form a cluster: {status(hp)}", g.log)
            time.sleep(1)
        apply(a, key, hp, os.path.join(a.examples, "app.k10s"))
        end = time.time() + 60
        while True:
            try:
                got = [http(p, "/hello/") for p in (hp, vhp)] + [http(p, "/counter/")[0] for p in (hp, vhp)]
                if got == [(200, "hello")] * 2 + [200, 200]:
                    break
            except OSError:
                pass
            if time.time() > end:
                fail("the services do not answer on both nodes", g.log)
            time.sleep(0.5)
        print("test.py: mixed: a Pi (aarch64, interpreter) and an x86-64 node serve one cluster: ok", file=sys.stderr)
        g.power_button()
        g.wait_for(r"powering off", "power off")
        g.wait_exit()
    finally:
        host.kill()


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


TIER1_READY = r"ready: the node answers"
METAL_READY = r"k10-metal ready [\d.]+ms"


def tier_file(path, tier, cmdline=""):
    """/k10s/tier as os/mkimage.py writes it: a GRUB environment block (k10-bootproto `tier`)."""
    with open(path, "w") as f:
        f.write(f"# GRUB Environment Block\ntier={tier}\nmetal_cmdline={cmdline}\n")
    return path


class TierDisks:
    """Fresh copies of the x86-64 image with a tier, k10s.conf, the admin and cluster keys, the native kernel's
    signing key pinned (os_key: in releases the image's kernel is signed by the release key, here maybe by the test
    key) and more files on K10S-BOOT."""

    def __init__(self, a, work):
        self.a, self.work = a, work
        self.key = keys(work, a.k10s)
        self.pub = subprocess.run([a.k10s, "image", "pubkey", a.image_key], check=True, capture_output=True, text=True).stdout.strip()
        self.kernel = os.path.join(work, "metal.elf")
        mtools("mcopy", a.image, boot_offset(a.image), "-o", "::/k10s/metal.elf", self.kernel)

    def disk(self, name, tier, cmdline="", conf=(), files=()):
        d = os.path.join(self.work, f"{name}.img")
        subprocess.run(["cp", "--sparse=always", self.a.image, d], check=True)
        with open(d, "r+b") as f:
            f.truncate(2 << 30)
        configure(d, self.work, self.a.k10s, ["admin-token-file=/boot/admin.key", "ntp=none", f"os_key={self.pub}", *conf], name)
        self.set_tier(d, tier, cmdline)
        for src, dst in files:
            mtools("mcopy", d, boot_offset(d), "-o", src, f"::/{dst}")
        return d

    def set_tier(self, d, tier, cmdline=""):
        # As an editor writes it: with the current time (mtools stamps files with SOURCE_DATE_EPOCH when set; k10boot's
        # boot counter tells a rewritten tier file by its modification time).
        env = {k: v for k, v in os.environ.items() if k != "SOURCE_DATE_EPOCH"}
        subprocess.run([tool("mcopy"), "-i", f"{d}@@{boot_offset(d)}", "-o", tier_file(os.path.join(self.work, "tier"), tier, cmdline),
                        "::/k10s/tier"], check=True, env=dict(env, MTOOLS_SKIP_CHECK="1"))

    def power_on(self, name, d, machine="pc", nic="virtio-net-pci,disable-modern=on", fwd="", fw=None, port=None, extra=()):
        """UEFI (OVMF, Secure Boot off) unless `fw` says otherwise ([] for SeaBIOS); HTTP forwarded from `port`."""
        port = port or free_port()
        fw = uefi_firmware(self.work, name) if fw is None else fw
        g = Guest(self.work, name, None, None, None, fw + ["-M", machine, "-drive", f"file={d},if=virtio,format=raw",
                  "-netdev", f"user,id=n0,hostfwd=tcp:127.0.0.1:{port}-:80{fwd}", "-device", f"{nic},netdev=n0,romfile=", *extra])
        g.port = port
        return g

    def sign(self, payload, name, key=None):
        out = os.path.join(self.work, name)
        subprocess.run([self.a.k10s, "image", "sign", "--key", key or self.a.image_key, "--arch", "x86_64", "--profile", "native",
                        "--version", "test", payload, out], check=True, capture_output=True)
        return out


def until(what, check, log, limit=180):
    end = time.time() + limit
    while True:
        try:
            if check():
                return
        except OSError:
            pass
        if time.time() > end:
            fail(what, log)
        time.sleep(1)


def tier_test(a, work):
    """docs/OS.md "Tiers": the x86-64 image under OVMF through k10boot (its Tier 1 path, tier=linux, is what every
    other UEFI test of the image boots). tier=metal on pc and q35: the native kernel gets k10s.conf and its key files
    from K10S-BOOT, joins a cluster with a hosted node on the host and serves the cluster's services; once healthy it
    clears k10boot's boot counter. tier=auto: a legacy virtio-net boots the native kernel, an e1000e alone Tier 1. A
    kernel that fails its boots (a bad command line) is booted twice, then Tier 1 is pinned until /k10s/tier is
    written again; the next healthy boots count from zero. Kernels signed by an untrusted key, or changed after
    signing, are refused (Tier 1 boots). The signed kernel file also boots as it is through PVH (-kernel)."""
    t = TierDisks(a, work)
    clu = os.path.join(work, "cluster.key")
    status = lambda port: subprocess.run([a.k10s, "status", "--node", f"127.0.0.1:{port}", "--admin-token-file", t.key],
                                         capture_output=True, text=True).stdout
    gp, rp, hp = free_port(socket.SOCK_DGRAM), free_port(), free_port()
    host_log = os.path.join(work, "host.log")
    host = subprocess.Popen([a.k10s, "node", "--name", "host", "--data", os.path.join(work, "host-data"), "--http", f"127.0.0.1:{hp}",
                             "--gossip", f"127.0.0.1:{gp}", "--rpc", f"127.0.0.1:{rp}", "--advertise", "10.0.2.2",
                             "--cluster-key-file", clu, "--admin-token-file", t.key, "--sandbox", "off"],
                            stdout=open(host_log, "w"), stderr=subprocess.STDOUT)
    try:
        # q35 with a display: its GOP mode goes to the kernel too.
        for machine, extra, fb in (("pc", (), ""), ("q35", ("-device", "VGA"), "k10s.framebuffer, ")):
            vgp, vrp, vhp = free_port(socket.SOCK_DGRAM), free_port(), free_port()
            d = t.disk(f"metal-{machine}", "metal", conf=["cluster-key-file=/boot/cluster.key", f"gossip=0.0.0.0:{vgp}", f"rpc=0.0.0.0:{vrp}",
                                                       f"http=0.0.0.0:{vhp}", "advertise=127.0.0.1", f"join=10.0.2.2:{gp}"])
            g = t.power_on(f"metal-{machine}", d, machine, fwd=f",hostfwd=udp:127.0.0.1:{vgp}-:7946,hostfwd=tcp:127.0.0.1:{vrp}-:7947",
                           port=vhp, extra=extra)
            g.wait_for(r"k10boot: tier metal \(from /k10s/tier\)", "k10boot reads /k10s/tier")
            g.wait_for(r"k10boot: native boot 1 of 2 before Tier 1 is pinned", "the boot counted")
            g.wait_for(rf"k10-metal: boot loader modules: k10s.conf, {fb}k10s.efi, /boot/admin.key, /boot/cluster.key; ACPI RSDP at 0x[1-9a-f]",
                       "modules from K10S-BOOT")
            g.wait_for(METAL_READY, "native kernel ready")
            if machine == "pc":
                until("the native kernel did not join the hosted node", lambda: "members: 2 live" in status(hp), g.log)
                apply(a, t.key, hp, os.path.join(a.examples, "app.k10s"))
            # The services of the cluster, through both nodes (the q35 guest has to fetch them from the cluster).
            until("the services do not answer on both nodes",
                  lambda: [http(p, "/hello/") for p in (hp, g.port)] == [(200, "hello")] * 2 and http(g.port, "/counter/")[0] == 200, g.log)
            if "k10-metal" not in status(g.port):
                fail("the native kernel does not answer requests signed with the admin key of k10s.conf", g.log)
            g.wait_for(r"efi: healthy: k10boot's boot counter cleared", "boot counter cleared", timeout=240)
            g.kill()
            print(f"test.py: tier: metal on {machine}: joins the hosted node, serves, clears the boot counter: ok", file=sys.stderr)
    finally:
        host.kill()

    d = t.disk("auto-virtio", "auto")
    g = t.power_on("auto-virtio", d, "pc")
    g.wait_for(r"k10boot: tier auto: virtio-net 1af4:1000 at \S+ is supported: native kernel", "auto: native")
    g.wait_for(METAL_READY, "native kernel ready")
    until("GET /_k10s/health", lambda: http(g.port, "/_k10s/health")[0] == 200, g.log)
    g.kill()
    d = t.disk("auto-e1000e", "auto")
    g = t.power_on("auto-e1000e", d, "q35", "e1000e")
    g.wait_for(r"k10boot: tier auto: no supported network adapter \(8086:10d3 at \S+\): linux", "auto: linux")
    g.wait_for(TIER1_READY, "Tier 1 ready")
    g.kill()
    print("test.py: tier: auto: native with a legacy virtio-net, Tier 1 with an e1000e: ok", file=sys.stderr)

    d = t.disk("fallback", "metal", "k10.engine=bogus")
    fw = uefi_firmware(work, "fallback")
    for n in (1, 2):
        g = t.power_on(f"fallback{n}", d, fw=fw)
        g.wait_for(rf"k10boot: native boot {n} of 2 before Tier 1 is pinned", f"native boot {n} counted")
        g.wait_for(r"k10-metal: k10.engine=bogus: want jit\|interp", "the kernel fails")
        g.kill()
    g = t.power_on("fallback3", d, fw=fw)
    g.wait_for(r"k10boot: native kernel not booted: pinned to Tier 1: the last 2 boots of this kernel never got healthy", "pinned")
    g.wait_for(TIER1_READY, "Tier 1 ready")
    g.kill()
    # Written again (another modification time): tried again; healthy, so the next boot counts from zero again.
    time.sleep(2)
    t.set_tier(d, "metal")
    for n in (1, 2):
        g = t.power_on(f"retry{n}", d, fw=fw)
        g.wait_for(r"k10boot: native boot 1 of 2 before Tier 1 is pinned", "native boot counted from zero")
        g.wait_for(METAL_READY, "native kernel ready")
        until("GET /_k10s/health", lambda: http(g.port, "/_k10s/health")[0] == 200, g.log)
        g.wait_for(r"efi: healthy: k10boot's boot counter cleared", "boot counter cleared", timeout=240)
        g.kill()
    print("test.py: tier: a failing native kernel: 2 boots, then Tier 1 until the tier file is written again: ok", file=sys.stderr)

    payload = unsigned_payload(open(t.kernel, "rb").read())
    raw = os.path.join(work, "metal.payload")
    open(raw, "wb").write(payload)
    other = os.path.join(work, "other.key")
    if not os.path.exists(other):
        subprocess.run([a.k10s, "keygen", "-o", other], check=True, capture_output=True)
    changed = bytearray(open(t.kernel, "rb").read())
    changed[0x3000] ^= 1
    open(os.path.join(work, "changed.elf"), "wb").write(changed)
    for name, kernel, why in (("untrusted", t.sign(raw, "untrusted.elf", other), "image signed by an untrusted key"),
                              ("changed", os.path.join(work, "changed.elf"), "the image payload does not match its signed hash")):
        d = t.disk(name, "metal", files=[(kernel, "k10s/metal.elf")])
        g = t.power_on(name, d)
        g.wait_for(rf"k10boot: native kernel not booted: /k10s/metal.elf refused: {why}", f"{name} kernel refused")
        g.wait_for(TIER1_READY, "Tier 1 ready")
        g.kill()
    print("test.py: tier: kernels signed by an untrusted key or changed after signing are refused: ok", file=sys.stderr)

    port = free_port()
    g = Guest(work, "pvh", None, None, None, ["-kernel", t.kernel, "-netdev", f"user,id=n0,hostfwd=tcp:127.0.0.1:{port}-:80",
                                             "-device", "virtio-net-pci,disable-modern=on,netdev=n0,romfile="])
    g.wait_for(METAL_READY, "native kernel ready (PVH)")
    until("GET /_k10s/health", lambda: http(port, "/_k10s/health")[0] == 200, g.log)
    g.kill()
    print("test.py: tier: the signed kernel file boots through PVH (-kernel): ok", file=sys.stderr)


def mb2_test(a, work):
    """BIOS: SeaBIOS boots the x86-64 image's GRUB, which loads the native kernel through Multiboot2 since /k10s/tier
    says tier=metal, with its command line, k10s.conf, the bundle (--bundle) and the key files of K10S-BOOT as
    modules: the bundle's services answer (else an applied app's), and requests signed with the admin key of k10s.conf."""
    t = TierDisks(a, work)
    files = [(a.bundle, "k10s/bundle.k10b")] if a.bundle else []
    d = t.disk("mb2", "metal", "k10.verbose", files=files)
    g = t.power_on("mb2", d, fw=[])
    m = g.wait_for(r"k10-metal: boot loader modules: ([^;]*); ACPI RSDP at (0x[0-9a-f]+)", "modules from GRUB")
    want = "k10s.conf, " + ("k10s.bundle, " if a.bundle else "") + "/boot/admin.key, /boot/cluster.key"
    if m.group(1) != want or int(m.group(2), 16) == 0:
        fail(f"modules {m.group(1)!r}, RSDP {m.group(2)}: want {want!r} and an RSDP", g.log)
    g.wait_for(r'cmdline "k10.verbose"', "the command line of /k10s/tier")
    m = g.wait_for(METAL_READY + r" vm->kmain=\S+ svc=(\d+) src=(\w+)", "native kernel ready")
    if a.bundle and (m.group(2) != "loader" or m.group(1) == "0"):
        fail(f"the bundle module was not applied: {m.group(0)}", g.log)
    if not a.bundle:
        apply(a, t.key, g.port, os.path.join(a.examples, "app.k10s"))
    until("GET /hello/", lambda: http(g.port, "/hello/")[0] == 200, g.log)
    status = subprocess.run([a.k10s, "status", "--node", f"127.0.0.1:{g.port}", "--admin-token-file", t.key], capture_output=True, text=True)
    if "k10-metal" not in status.stdout:
        fail(f"a request signed with the admin key of k10s.conf: {status.stdout} {status.stderr}", g.log)
    g.kill()
    print("test.py: mb2: SeaBIOS, GRUB, Multiboot2: the native kernel serves: ok", file=sys.stderr)


# OVMF and AAVMF with Secure Boot on and Microsoft's keys enrolled (Debian and Ubuntu name them .ms.fd).
MS_FIRMWARE = {"x86_64": ("/usr/share/OVMF/OVMF_CODE_4M.ms.fd", "/usr/share/OVMF/OVMF_VARS_4M.ms.fd"),
               "aarch64": ("/usr/share/AAVMF/AAVMF_CODE.ms.fd", "/usr/share/AAVMF/AAVMF_VARS.ms.fd")}
# Owner GUID of the test's own keys in the variable store.
OWNER = "6b10c5a0-0000-4000-8000-00000000000e"


def ms_firmware(work, name, *edits):
    """pflash arguments for the Microsoft-key firmware with a private copy of its variable store, changed by
    virt-fw-vars `edits` (what enrolling keys or the firmware setup's Secure Boot switch writes there)."""
    code, vars_ = MS_FIRMWARE[ARCH]
    if not (os.path.exists(code) and os.path.exists(vars_)):
        sys.exit(f"test.py: {code} or {vars_} not found (package ovmf or qemu-efi-aarch64)")
    mine = os.path.join(work, f"{name}.vars.fd")
    subprocess.run([tool("virt-fw-vars"), "-i", vars_, *edits, "-o", mine], check=True, capture_output=True)
    # x86-64: the variable store is protected by SMM, which Secure Boot builds of OVMF require.
    machine = ["-M", "q35,smm=on", "-global", "driver=cfi.pflash01,property=secure,value=on"] if ARCH == "x86_64" else []
    return machine + ["-drive", f"if=pflash,format=raw,readonly=on,file={code}", "-drive", f"if=pflash,format=raw,file={mine}"]


def secureboot_test(a, work):
    """docs/OS.md "Secure Boot", under the Microsoft-key firmware: the image as released is refused; with Secure Boot
    turned off it serves; with own keys (PK, KEK, db enrolled next to Microsoft's, the loaders and the UKI signed with
    the db key) and with a MOK (Debian's Microsoft-signed shim, the loaders and the UKI signed with the MOK) it serves
    with Secure Boot on. The loaders: systemd-boot, and on x86-64 k10boot in front of it, which chain-loads it through
    the firmware (or shim) and refuses a native kernel signed by an os_key of k10s.conf only. Under own keys, an update
    re-signed the same way commits, and one not signed for Secure Boot is refused by the firmware and rolled back by boot
    counting."""
    efi = "x64" if ARCH == "x86_64" else "aa64"
    loader = f"EFI/BOOT/BOOT{efi.upper()}.EFI"
    # x86-64: k10boot is the firmware's loader, systemd-boot its Tier 1 (os/mkimage.py).
    sd_path = "EFI/systemd/systemd-bootx64.efi" if ARCH == "x86_64" else loader
    key = keys(work, a.k10s)
    pub = subprocess.run([a.k10s, "image", "pubkey", a.image_key], check=True, capture_output=True, text=True).stdout.strip()
    run_ = lambda *cmd: subprocess.run([tool(cmd[0]), *cmd[1:]], check=True, capture_output=True)

    def cert(name, bits):
        k, c = os.path.join(work, f"{name}.key"), os.path.join(work, f"{name}.crt")
        run_("openssl", "req", "-new", "-x509", "-newkey", f"rsa:{bits}", "-nodes", "-days", "3650", "-subj", f"/CN=k10s test {name}/",
             "-keyout", k, "-out", c)
        return k, c

    def sbsign(k, c, src, name):
        out = os.path.join(work, name)
        run_("sbsign", "--key", k, "--cert", c, "--output", out, src)
        return out

    def os_image(src, version):
        """`k10s image sign` with the test key (pinned with os_key), as the operator signs with theirs."""
        out = os.path.join(work, f"{version}.img")
        run_(a.k10s, "image", "sign", "--key", a.image_key, "--arch", ARCH, "--profile", "vm", "--version", version, src, out)
        verified = subprocess.run([a.k10s, "image", "verify", out, "--trust", pub], check=True, capture_output=True, text=True).stdout
        if ("signed for Secure Boot" in verified) != (src != payload):
            fail(f"k10s image verify: {verified}")
        return out, hashlib.sha256(open(out, "rb").read()).hexdigest()

    def disk(name, files=(), conf=()):
        """The released image on a fresh 2 GiB disk, `files` [(source, path)] copied to K10S-BOOT."""
        d = os.path.join(work, f"{name}.img")
        subprocess.run(["cp", "--sparse=always", a.image, d], check=True)
        with open(d, "r+b") as f:
            f.truncate(2 << 30)
        configure(d, work, a.k10s, ["admin-token-file=/boot/admin.key", "ntp=none", *conf], name)
        for src, dst in files:
            mtools("mcopy", d, 2 << 20, "-o", src, f"::/{dst}")
        return d

    def power_on(name, d, fw):
        port = free_port()
        g = Guest(work, name, None, None, None, fw + ["-drive", f"file={d},if=virtio,format=raw", "-device", "i6300esb",
                  "-netdev", f"user,id=n0,hostfwd=tcp:127.0.0.1:{port}-:80", "-device", "virtio-net-pci,netdev=n0"], reboot=True)
        g.port = port
        return g

    def serves(g, state, since=0):
        m = g.wait_for(r"efi: Secure Boot (\w+)", "the Secure Boot state", since)
        if m.group(1) != state:
            fail(f"Secure Boot {m.group(1)}, want {state}", g.log)
        m = g.wait_for(BOOTED, "booted slot", m.end())
        g.wait_for(r"ready: the node answers", "node ready", m.end())
        return m

    def serve_and_stop(g):
        apply(a, key, g.port, os.path.join(a.examples, "app.k10s"))
        if http(g.port, "/hello/") != (200, "hello"):
            fail("GET /hello/", g.log)
        g.power_button()
        g.wait_for(r"powering off", "power off")
        g.wait_exit()

    payload = os.path.join(work, "payload.efi")
    run_(a.k10s, "image", "payload", a.uki, payload)
    sd = os.path.join(work, "systemd-boot.efi")
    mtools("mcopy", a.image, 2 << 20, "-o", f"::/{sd_path}", sd)
    k10boot = os.path.join(work, "k10boot.efi")
    if ARCH == "x86_64":
        mtools("mcopy", a.image, 2 << 20, "-o", f"::/{loader}", k10boot)

    def loaders(k, c, name):
        """The image's loaders signed with key `k`: [(file, path on K10S-BOOT)]: systemd-boot, and k10boot on x86-64."""
        signed = [(sbsign(k, c, sd, f"systemd-boot.{name}.efi"), sd_path)]
        if ARCH == "x86_64":
            signed.append((sbsign(k, c, k10boot, f"k10boot.{name}.efi"), loader))
        return signed

    # As released: neither the loaders nor the UKI are signed for Secure Boot, and the firmware refuses to start them.
    g = power_on("refused", disk("refused"), ms_firmware(work, "refused"))
    g.wait_for(r"Access Denied", "the firmware's refusal")
    time.sleep(2)
    if "k10s-init" in g.text():
        fail("the unsigned image booted with Secure Boot on", g.log)
    g.kill()
    print("test.py: secureboot: the released image is refused with Secure Boot on: ok", file=sys.stderr)

    # Secure Boot turned off in the firmware's setup (its SecureBootEnable switch; the keys stay enrolled).
    g = power_on("off", disk("off"), ms_firmware(work, "off", "--set-false", "SecureBootEnable"))
    serves(g, "off")
    serve_and_stop(g)
    print("test.py: secureboot: Secure Boot turned off: serves: ok", file=sys.stderr)

    # MOK: Debian's shim (signed by Microsoft's UEFI CA) starts the image's first loader as grubX.efi (on x86-64
    # k10boot, which starts systemd-boot through shim's LoadImage); they and the UKI are signed with the MOK, which is
    # in shim's MokList (as MokManager's "Enroll key from disk" leaves it).
    mok_key, mok_crt = cert("MOK", 2048)
    mok_cer = os.path.join(work, "mok.cer")
    run_("openssl", "x509", "-in", mok_crt, "-outform", "DER", "-out", mok_cer)
    shim = os.path.join(a.shim, "usr/lib/shim")
    mok_a, _ = os_image(sbsign(mok_key, mok_crt, payload, "mok-a.efi"), "test-mok-a")
    mok_loaders = [(f, f"EFI/BOOT/grub{efi}.efi" if p == loader else p) for f, p in loaders(mok_key, mok_crt, "mok")]
    d = disk("mok", [(os.path.join(shim, f"shim{efi}.efi.signed"), loader), (os.path.join(shim, f"mm{efi}.efi.signed"), f"EFI/BOOT/mm{efi}.efi"),
                     *mok_loaders, (mok_cer, "mok.cer"), (mok_a, "EFI/Linux/k10s-a.efi")], [f"os_key={pub}"])
    g = power_on("mok", d, ms_firmware(work, "mok", "--add-mok", OWNER, mok_crt))
    serves(g, "on")
    serve_and_stop(g)
    print("test.py: secureboot: MOK under Debian's shim: serves with Secure Boot on: ok", file=sys.stderr)

    # Own keys enrolled next to Microsoft's (what `sbctl enroll-keys --microsoft` writes in setup mode; RSA-4096 like
    # sbctl's), the loaders and the UKI signed with the db key, then the UKI with the operator's image key.
    (_, pk), (_, kek), (db_key, db) = cert("PK", 4096), cert("KEK", 4096), cert("db", 4096)
    own_keys = ("--set-pk", OWNER, pk, "--add-kek", OWNER, kek, "--add-db", OWNER, db)
    own_loaders = loaders(db_key, db, "own")
    own_a, own_a_id = os_image(sbsign(db_key, db, payload, "own-a.efi"), "test-own-a")
    own_b, own_b_id = os_image(sbsign(db_key, db, payload, "own-b.efi"), "test-own-b")
    unsigned, unsigned_id = os_image(payload, "test-unsigned")
    if ARCH == "x86_64":
        # tier=metal with a native kernel signed by the operator's image key: k10s.conf is not signed, so with Secure
        # Boot on k10boot does not take its os_key and boots Tier 1 instead.
        native = os.path.join(work, "native.elf")
        mtools("mcopy", a.image, 2 << 20, "-o", "::/k10s/metal.elf", native)
        open(os.path.join(work, "native.payload"), "wb").write(unsigned_payload(open(native, "rb").read()))
        run_(a.k10s, "image", "sign", "--key", a.image_key, "--arch", "x86_64", "--profile", "native", "--version", "test",
             os.path.join(work, "native.payload"), os.path.join(work, "native.os-key.elf"))
        d = disk("own-metal", [*own_loaders, (own_a, "EFI/Linux/k10s-a.efi"), (os.path.join(work, "native.os-key.elf"), "k10s/metal.elf"),
                               (tier_file(os.path.join(work, "tier"), "metal"), "k10s/tier")], [f"os_key={pub}"])
        g = power_on("own-metal", d, ms_firmware(work, "own-metal", *own_keys))
        g.wait_for(r"k10boot: Secure Boot is on: the os_key of k10s.conf is not trusted for the kernel", "os_key ignored")
        g.wait_for(r"k10boot: native kernel not booted: /k10s/metal.elf refused: image signed by an untrusted key", "kernel refused")
        serves(g, "on")
        g.kill()
        print("test.py: secureboot: own keys: a native kernel signed by an os_key only is refused, Tier 1 serves: ok", file=sys.stderr)
    d = disk("own", [*own_loaders, (own_a, "EFI/Linux/k10s-a.efi")],
             ["cluster-key-file=/boot/cluster.key", "gossip=0.0.0.0:7946", "watchdog=1", f"os_key={pub}", "os-commit-grace=3"])
    g = power_on("own", d, ms_firmware(work, "own", *own_keys))
    m = serves(g, "on")
    if (m.group(1), m.group(2), m.group(3)) != ("a", own_a_id[:8], None):
        fail(f"first boot: {m.group(0)}", g.log)
    print("test.py: secureboot: own keys: serves with Secure Boot on: ok", file=sys.stderr)

    def manifest(image, name):
        p = os.path.join(work, f"{name}.k10s")
        ex = os.path.abspath(a.examples)
        open(p, "w").write(f'service hello {{ code = "{ex}/hello.ten" memory = 64KiB }}\n'
                           f'os {{ images = ["{os.path.abspath(image)}"] wave = 1 }}\n')
        return p

    # An update signed for Secure Boot like the running image installs, boots as a trial and commits.
    mark = len(g.text())
    apply(a, key, g.port, manifest(own_b, "to-b"))
    g.wait_for(r"update: slot b armed", "image armed", mark)
    m = serves(g, "on", mark)
    if (m.group(1), m.group(2), m.group(3)) != ("b", own_b_id[:8], ", trial"):
        fail(f"trial boot: {m.group(0)}", g.log)
    g.wait_for(r"update: committed slot b", "commit", m.end(), 180)
    print("test.py: secureboot: own keys: an update signed for Secure Boot commits: ok", file=sys.stderr)
    # One signed by the operator only: the firmware refuses it on each of its 3 trial boots, systemd-boot reboots
    # (reboot-on-error), and the committed slot comes back and records it as failed.
    mark = len(g.text())
    apply(a, key, g.port, manifest(unsigned, "to-unsigned"))
    g.wait_for(r"update: slot a armed", "image armed", mark)
    g.wait_for(rf"update: slot a \({unsigned_id[:8]}\) did not come up in 3 boots; back on slot b", "fallback", mark, 600)
    refusals = len(re.findall(r"Error loading .*k10s-a\+\S*: Access denied", g.text()[mark:], re.IGNORECASE))
    if refusals != 3:
        fail(f"the firmware refused the image {refusals} times, want 3", g.log)
    m = serves(g, "on", mark)
    if (m.group(1), m.group(2), m.group(3)) != ("b", own_b_id[:8], None):
        fail(f"after the refused image: {m.group(0)}", g.log)
    g.wait_for(rf"os: rollout of {unsigned_id[:8]} stopped: node \w+ failed to commit it", "rollout halted", mark)
    if http(g.port, "/hello/") != (200, "hello"):
        fail("GET /hello/", g.log)
    g.power_button()
    g.wait_for(r"powering off", "power off")
    g.wait_exit()
    print("test.py: secureboot: own keys: an update not signed for Secure Boot is refused and rolled back: ok", file=sys.stderr)


def main():
    ap = argparse.ArgumentParser()
    tests = {"boot": boot_test, "dhcp": dhcp_test, "image": image_test, "unsigned": unsigned_test, "boottime": boottime_test,
             "update": update_test, "rollout": rollout_test, "pi": pi_test, "pi3": pi3_test, "mixed": mixed_test,
             "secureboot": secureboot_test, "tier": tier_test, "mb2": mb2_test}
    ap.add_argument("test", choices=tests)
    ap.add_argument("--arch", choices=["x86_64", "aarch64", "i686", "armv7"], default="x86_64")
    ap.add_argument("--host-k10s")
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
    ap.add_argument("--shim")
    ap.add_argument("--bundle")
    ap.add_argument("--boot")
    ap.add_argument("--nodes", type=int, default=20)
    ap.add_argument("--k10s")
    ap.add_argument("--examples")
    ap.add_argument("--work")
    a = ap.parse_args()
    global ARCH, KVM
    ARCH = "aarch64" if a.test in ("pi", "pi3", "mixed") else a.arch
    host = os.uname().machine
    KVM = os.access("/dev/kvm", os.R_OK | os.W_OK) and (ARCH == host or (ARCH, host) == ("i686", "x86_64"))
    work = a.work or tempfile.mkdtemp(prefix="k10s-os-test.")
    os.makedirs(work, exist_ok=True)
    tests[a.test](a, work)


if __name__ == "__main__":
    main()
