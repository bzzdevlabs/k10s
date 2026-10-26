#!/usr/bin/env python3
r"""The native kernel's platform services under QEMU (metal/README.md, "How it works"):

  lan    a scripted LAN on a QEMU UDP socket netdev (ARP, DHCP, DNS and NTP answered from here): without k10.ntp the
         lease's NTP servers (option 42) are asked in order, an unsynchronized one is skipped, forged replies (from
         another address, or not echoing the request's transmit timestamp) are ignored and a +1000 s offset steps
         the clock; with k10.ntp=NAME,… the names go to the lease's name server (option 6): an unknown name is
         skipped, the next one resolves and its server sets the clock
  clock  user networking with the RTC at 2001-01-01 and no kvmclock: signed admin requests are refused until the
         SNTP server on the host answers and the clock is stepped, then accepted
  button the ACPI power button (QMP system_powerdown) powers off through S5 on pc (PIIX4) and q35 (ICH9): the
         fixed-feature button; then a control-method one: an SSDT whose \_GPE._E01 notifies a PNP0C0C device, and
         a PCI hotplug (QEMU's GPE 1 on pc) standing in for the press
  apply  a single node with K10S-DATA keeps what POST /_k10s/apply installed: after a power cut that follows the
         200, an older boot bundle (-initrd) is refused and the kept services answer; a newer apply, a clean power
         off, and the next boot without a bundle serves it
  ipv6   on the scripted LAN: the SLAAC address of an advertised prefix, SNTP to an IPv6 server, an HTTP GET to the
         guest over IPv6, echo replies over IPv6 and IPv4

    platform_test.py --kernel ELF [--bundle K10B] [--mkbundle BIN] [--tests lan,clock,button,apply,ipv6] [--out DIR]
                     [--accel kvm|tcg] [--port P]
"""
import argparse
import http.client
import json
import os
import queue
import re
import socket
import struct
import subprocess
import sys
import threading
import time

sys.dont_write_bytecode = True  # no __pycache__ next to the tools
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", "bench", "k10s"))
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from k10s import admin_auth  # noqa: E402
from storage_test import gpt_disk  # noqa: E402

NTP_EPOCH = 2208988800


def fail(msg, g=None):
    print(f"platform_test.py: FAIL: {msg}", file=sys.stderr)
    if g:
        print(f"--- {g.log}:\n" + g.text()[-3000:], file=sys.stderr)
        g.kill()
    sys.exit(1)


def say(msg):
    print(f"platform_test.py: {msg}", file=sys.stderr)


def free_port(kind):
    s = socket.socket(socket.AF_INET, kind)
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


def ntp_time(offset=0.0):
    t = time.time() + NTP_EPOCH + offset
    return (int(t) << 32) | int((t % 1) * 2**32)


def ntp_reply(req, offset, synced=True):
    """The answer to an SNTP request: the host's clock plus `offset` seconds (stratum 2), or an unsynchronized one."""
    rx = ntp_time(offset)
    hdr = bytes([(4 << 3) | 4, 2, 6, 0xEC]) if synced else bytes([(3 << 6) | (4 << 3) | 4, 0, 6, 0xEC])
    return hdr + bytes(8) + b"LOCL" + struct.pack(">Q", rx) + req[40:48] + struct.pack(">QQ", rx, ntp_time(offset))


def csum(b):
    if len(b) % 2:
        b += b"\0"
    s = sum(struct.unpack(f">{len(b) // 2}H", b))
    s = (s & 0xFFFF) + (s >> 16)
    return ~((s & 0xFFFF) + (s >> 16)) & 0xFFFF


class Guest:
    """One QEMU run of the kernel: serial console to a file, QMP for the power button."""

    def __init__(self, a, name, net, append, extra=()):
        self.log = os.path.join(a.out, f"platform-{name}.log")
        self.qmp = os.path.join(a.out, f"platform-{name}.qmp")
        for f in (self.log, self.qmp):
            if os.path.exists(f):
                os.remove(f)
        # No kvmclock: the wall clock of bare metal (the RTC, then SNTP).
        accel = ["-enable-kvm", "-cpu", "host,kvmclock=off"] if a.accel == "kvm" else ["-accel", "tcg,thread=multi", "-cpu", "max"]
        machine = [] if "-M" in extra else ["-M", "pc"]
        cmd = ["qemu-system-x86_64", *accel, *machine, "-m", "64M", "-smp", "1", "-nodefaults", "-display", "none",
               "-no-reboot", *net, "-fw_cfg", f"name=opt/k10s/admin_key,file={a.admin_key}", "-kernel", a.kernel,
               "-serial", f"file:{self.log}", "-qmp", f"unix:{self.qmp},server=on,wait=off", *extra, "-append", append]
        self.proc = subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        self.timeout = 60 if a.accel == "kvm" else 180

    def text(self):
        try:
            return open(self.log, errors="replace", newline="").read()
        except FileNotFoundError:
            return ""

    def wait_for(self, pattern, what):
        t = time.time()
        while time.time() - t < self.timeout:
            m = re.search(pattern, self.text())
            if m:
                return m
            if self.proc.poll() is not None:
                break
            time.sleep(0.05)
        fail(f"{what}: no /{pattern}/ on the console", self)

    def qmp_command(self, command, arguments=None):
        s = socket.socket(socket.AF_UNIX)
        s.connect(self.qmp)
        f = s.makefile("rw")
        f.readline()
        for c, args in (("qmp_capabilities", None), (command, arguments)):
            f.write(json.dumps({"execute": c, **({"arguments": args} if args else {})}) + "\n")
            f.flush()
            while True:
                r = json.loads(f.readline())
                if "error" in r:
                    fail(f"QMP {c}: {r['error']}", self)
                if "return" in r:
                    break
        s.close()

    def wait_exit(self, what):
        try:
            st = self.proc.wait(timeout=self.timeout)
        except subprocess.TimeoutExpired:
            fail(f"{what}: QEMU still running", self)
        if st != 0:
            fail(f"{what}: QEMU exit status {st}", self)

    def kill(self):
        if self.proc.poll() is None:
            self.proc.kill()
            self.proc.wait()


class Lan:
    """A scripted LAN: Ethernet frames over QEMU's UDP socket netdev. Answers ARP for the router 10.0.3.1 and every
    server address, DHCP (10.0.3.15/24, router .1, name server .2, NTP servers `ntp`), DNS on .2 (`names`: name →
    address, others NXDOMAIN) and NTP (`offsets`: server address → seconds added to the host's clock, None for an
    unsynchronized server). Before its first answer, the server `forge` gets two forged replies sent first: one
    from another address, one not echoing the request's transmit timestamp, both with a -5000 s offset.
    IPv6: router advertisements of fd00:3::/64 from fe80::5054:ff:fe00:2 (to all nodes, answering solicitations),
    neighbor advertisements for that router and for fd00:3::2, NTP on fd00:3::2 (`offsets`), and from fd00:3::2
    echo requests and an HTTP GET (a minimal TCP client) to the guest's SLAAC address."""

    MAC = bytes.fromhex("525400000002")
    GUEST = bytes([10, 0, 3, 15])
    GUEST_MAC = bytes.fromhex("525400123456")
    LL = socket.inet_pton(socket.AF_INET6, "fe80::5054:ff:fe00:2")
    V6 = socket.inet_pton(socket.AF_INET6, "fd00:3::2")
    GUEST_V6 = socket.inet_pton(socket.AF_INET6, "fd00:3::5054:ff:fe12:3456")

    def __init__(self, ntp=(), names=None, offsets=None, forge=None):
        self.port, self.peer = free_port(socket.SOCK_DGRAM), free_port(socket.SOCK_DGRAM)
        self.s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self.s.bind(("127.0.0.1", self.port))
        self.ntp, self.names, self.offsets, self.forge = list(ntp), names or {}, offsets or {}, forge
        self.events = []
        self.error = None
        self.icmp, self.tcp = queue.Queue(), queue.Queue()
        threading.Thread(target=self.serve, daemon=True).start()

    def netdev(self):
        return ["-netdev", f"socket,id=n0,udp=127.0.0.1:{self.port},localaddr=127.0.0.1:{self.peer}",
                "-device", "virtio-net-pci,netdev=n0,mac=52:54:00:12:34:56,romfile="]

    def ours(self, ip):
        return ip == bytes([10, 0, 3, 1]) or ip == bytes([10, 0, 3, 2]) or socket.inet_ntoa(ip) in self.offsets

    def send_udp(self, dst_mac, src, dst, sport, dport, payload):
        udp = struct.pack(">HHHH", sport, dport, 8 + len(payload), 0) + payload
        ip = struct.pack(">BBHHHBBH4s4s", 0x45, 0, 20 + len(udp), 0, 0, 64, 17, 0, src, dst)
        ip = ip[:10] + struct.pack(">H", csum(ip)) + ip[12:]
        self.s.sendto(dst_mac + self.MAC + b"\x08\x00" + ip + udp, ("127.0.0.1", self.peer))

    def dhcp(self, f, m):
        opts, i = {}, 240
        while i < len(m) and m[i] != 255:
            if m[i] == 0:
                i += 1
                continue
            opts[m[i]] = m[i + 2:i + 2 + m[i + 1]]
            i += 2 + m[i + 1]
        kind = {1: 2, 3: 5}.get(opts.get(53, b"\0")[0])
        if not kind:
            return
        if 42 not in opts.get(55, b""):
            self.error = f"the DHCP request does not ask for NTP servers (option 55: {list(opts.get(55, b''))})"
        p = bytearray(236)
        p[0:4] = b"\x02\x01\x06\x00"
        p[4:8], p[10:12], p[16:20], p[28:34] = m[4:8], m[10:12], self.GUEST, m[28:34]
        o = [(53, bytes([kind])), (54, bytes([10, 0, 3, 2])), (51, struct.pack(">I", 3600)), (1, bytes([255, 255, 255, 0])),
             (3, bytes([10, 0, 3, 1])), (6, bytes([10, 0, 3, 2]))]
        if self.ntp:
            o.append((42, b"".join(socket.inet_aton(n) for n in self.ntp)))
        p += bytes([99, 130, 83, 99]) + b"".join(bytes([c, len(v)]) + v for c, v in o) + b"\xff"
        self.send_udp(b"\xff" * 6, bytes([10, 0, 3, 2]), b"\xff" * 4, 67, 68, bytes(p))

    def dns(self, f, src, sport, q):
        name, i = [], 12
        while q[i]:
            name.append(q[i + 1:i + 1 + q[i]].decode())
            i += 1 + q[i]
        name, end = ".".join(name), i + 5
        self.events.append(f"dns {name}")
        addr = self.names.get(name)
        flags = 0x8180 if addr else 0x8183
        r = q[:2] + struct.pack(">HHHHH", flags, 1, 1 if addr else 0, 0, 0) + q[12:end]
        if addr:
            r += struct.pack(">HHHIH", 0xC00C, 1, 1, 60, 4) + socket.inet_aton(addr)
        self.send_udp(f[6:12], bytes([10, 0, 3, 2]), src, 53, sport, r)

    def ntp_query(self, f, src, dst, sport, req):
        server = socket.inet_ntoa(dst)
        self.events.append(f"ntp {server}")
        offset = self.offsets[server]
        if offset is None:
            self.send_udp(f[6:12], dst, src, 123, sport, ntp_reply(req, 0, synced=False))
            return
        if self.forge == server:
            self.forge = None
            self.send_udp(f[6:12], bytes([10, 0, 3, 9]), src, 123, sport, ntp_reply(req, -5000))
            other = bytearray(req)
            other[47] ^= 1
            self.send_udp(f[6:12], dst, src, 123, sport, ntp_reply(bytes(other), -5000))
            self.events.append("forged")
        self.send_udp(f[6:12], dst, src, 123, sport, ntp_reply(req, offset))

    def send_ip6(self, dst_mac, src, dst, nh, msg, hop=64, csum_at=None):
        """An IPv6 packet; `csum_at`: where the upper layer's checksum goes (pseudo-header included)."""
        if csum_at is not None:
            c = csum(src + dst + struct.pack(">IxxxB", len(msg), nh) + msg)
            msg = msg[:csum_at] + struct.pack(">H", c) + msg[csum_at + 2:]
        ip = struct.pack(">IHBB", 0x60000000, len(msg), nh, hop) + src + dst
        self.s.sendto(dst_mac + self.MAC + b"\x86\xdd" + ip + msg, ("127.0.0.1", self.peer))

    def icmp6(self, dst_mac, src, dst, typ, body, hop=255):
        self.send_ip6(dst_mac, src, dst, 58, bytes([typ, 0, 0, 0]) + body, hop, csum_at=2)

    def advertise(self):
        prefix = bytes([3, 4, 64, 0xC0]) + struct.pack(">III", 86400, 14400, 0) + socket.inet_pton(socket.AF_INET6, "fd00:3::")
        body = struct.pack(">BBHII", 64, 0, 1800, 0, 0) + bytes([1, 1]) + self.MAC + prefix
        self.icmp6(bytes.fromhex("333300000001"), self.LL, socket.inet_pton(socket.AF_INET6, "ff02::1"), 134, body)

    def serve6(self, f):
        ip, mac = f[14:], f[6:12]
        nh, src, dst = ip[6], ip[8:24], ip[24:40]
        m = ip[40:40 + struct.unpack(">H", ip[4:6])[0]]
        if nh == 58 and m[0] == 133:
            self.events.append("rs")
            self.advertise()
        elif nh == 58 and m[0] == 135 and src != bytes(16) and m[8:24] in (self.LL, self.V6):
            self.icmp6(mac, m[8:24], src, 136, bytes([0xE0, 0, 0, 0]) + m[8:24] + bytes([2, 1]) + self.MAC)
        elif nh == 58 and m[0] == 129:
            self.icmp.put(("v6", src, m[4:8]))
        elif nh == 17 and dst == self.V6 and struct.unpack(">H", m[2:4])[0] == 123:
            sport = struct.unpack(">H", m[0:2])[0]
            self.events.append("ntp fd00:3::2")
            r = ntp_reply(m[8:], self.offsets["fd00:3::2"])
            self.send_ip6(mac, dst, src, 17, struct.pack(">HHHH", 123, sport, 8 + len(r), 0) + r, csum_at=6)
        elif nh == 6 and dst == self.V6:
            self.tcp.put(m)

    def ping(self, v6):
        """Echo requests from fd00:3::2 or 10.0.3.2 to the guest, one a second (the first reply may be lost while
        the guest resolves our address); True once one is answered, within 10 s."""
        for _ in range(10):
            ident = os.urandom(4)
            if v6:
                self.icmp6(self.GUEST_MAC, self.V6, self.GUEST_V6, 128, ident + b"k10s", hop=64)
            else:
                msg = bytes([8, 0, 0, 0]) + ident + b"k10s"
                msg = msg[:2] + struct.pack(">H", csum(msg)) + msg[4:]
                ip = struct.pack(">BBHHHBBH4s4s", 0x45, 0, 20 + len(msg), 0, 0, 64, 1, 0, bytes([10, 0, 3, 2]), self.GUEST)
                ip = ip[:10] + struct.pack(">H", csum(ip)) + ip[12:]
                self.s.sendto(self.GUEST_MAC + self.MAC + b"\x08\x00" + ip + msg, ("127.0.0.1", self.peer))
            end = time.time() + 1
            while (left := end - time.time()) > 0:
                try:
                    kind, _, got = self.icmp.get(timeout=left)
                except queue.Empty:
                    break
                if (kind == "v6") == v6 and got == ident:
                    return True
        return False

    def http6_get(self, path):
        """`GET path` over TCP from [fd00:3::2]:40000 to the guest's port 80: the response's bytes."""
        def send(flags, seq, ack, data=b""):
            seg = struct.pack(">HHIIBBHHH", 40000, 80, seq, ack, 5 << 4, flags, 65535, 0, 0) + data
            self.send_ip6(self.GUEST_MAC, self.V6, self.GUEST_V6, 6, seg, csum_at=16)

        def segment():
            m = self.tcp.get(timeout=10)
            seq, ack, off, flags = struct.unpack(">IIBB", m[4:14])
            return seq, flags, m[(off >> 4) * 4:]

        send(0x02, 1000, 0)
        seq, flags, _ = segment()
        if flags & 0x12 != 0x12:
            return b""
        rcv, data = seq + 1, b""
        req = f"GET {path} HTTP/1.1\r\nHost: guest\r\nConnection: close\r\n\r\n".encode()
        send(0x18, 1001, rcv, req)
        while True:
            seq, flags, payload = segment()
            if seq == rcv:
                data, rcv = data + payload, rcv + len(payload)
                if flags & 0x01:
                    send(0x11, 1001 + len(req), rcv + 1)
                    return data
            send(0x10, 1001 + len(req), rcv)

    def serve(self):
        while True:
            f, _ = self.s.recvfrom(4096)
            eth = f[12:14]
            if eth == b"\x86\xdd":
                self.serve6(f)
                continue
            if eth == b"\x08\x06" and f[20:22] == b"\x00\x01" and self.ours(f[38:42]):
                arp = f[14:20] + b"\x00\x02" + self.MAC + f[38:42] + f[22:28] + f[28:32]
                self.s.sendto(f[6:12] + self.MAC + b"\x08\x06" + arp, ("127.0.0.1", self.peer))
                continue
            if eth == b"\x08\x00" and f[23] == 1 and f[14 + (f[14] & 15) * 4] == 0:
                ihl = (f[14] & 15) * 4
                self.icmp.put(("v4", f[26:30], f[14 + ihl + 4:14 + ihl + 8]))
                continue
            if eth != b"\x08\x00" or f[23] != 17:
                continue
            ihl = (f[14] & 15) * 4
            src, dst = f[26:30], f[30:34]
            sport, dport = struct.unpack(">HH", f[14 + ihl:14 + ihl + 4])
            m = f[14 + ihl + 8:]
            if dport == 67:
                self.dhcp(f, m)
            elif dport == 53 and dst == bytes([10, 0, 3, 2]):
                self.dns(f, src, sport, m)
            elif dport == 123 and socket.inet_ntoa(dst) in self.offsets:
                self.ntp_query(f, src, dst, sport, m)


def lan_test(a):
    # The lease's NTP servers in order: .3 is unsynchronized, .2 answers after two forged replies.
    lan = Lan(ntp=["10.0.3.3", "10.0.3.2"], offsets={"10.0.3.3": None, "10.0.3.2": 1000.0}, forge="10.0.3.2")
    g = Guest(a, "lan-lease", lan.netdev(), "")
    m = g.wait_for(r"time: stepped by ([+-][\d.]+) s from (\S+)\r", "SNTP step")
    # The RTC has whole seconds: the kernel's clock starts up to 1 s behind.
    if m.group(2) != "10.0.3.2" or not -0.5 < float(m.group(1)) - 1000 < 1.5:
        fail(f"stepped by {m.group(1)} s from {m.group(2)}: want +1000 s from 10.0.3.2", g)
    events = [e for e in lan.events if e != "rs"]
    if events != ["ntp 10.0.3.3", "ntp 10.0.3.2", "forged"] or lan.error:
        fail(f"want the lease's servers asked once, in order, and the forged replies sent: {events} {lan.error or ''}", g)
    if "-5000" in g.text():
        fail("a forged reply was taken", g)
    g.kill()
    say(f"lan: the lease's NTP servers (option 42): unsynchronized one skipped, forged replies ignored, stepped by {m.group(1)} s: ok")

    # The RTC has whole seconds: the first sync usually steps (offsets of 128 ms or more), else slews.
    lan = Lan(names={"ntp.k10s.test": "10.0.3.2"}, offsets={"10.0.3.2": 0.0})
    g = Guest(a, "lan-names", lan.netdev(), "k10.ntp=nope.k10s.test,ntp.k10s.test:123")
    m = g.wait_for(r"time: (stepped by \S+ s from|in sync with) (\S+)", "SNTP sync")
    if m.group(2) != "ntp.k10s.test:123":
        fail(f"set from {m.group(2)}: want ntp.k10s.test:123", g)
    events = [e for e in lan.events if e != "rs"]
    if events != ["dns nope.k10s.test", "dns ntp.k10s.test", "ntp 10.0.3.2"]:
        fail(f"want both names resolved through the lease's name server, then the server asked: {events}", g)
    g.kill()
    say("lan: names through the lease's name server (option 6), an unknown one skipped: ok")


def ipv6_test(a):
    lan = Lan(offsets={"fd00:3::2": 0.0})
    g = Guest(a, "ipv6", lan.netdev(), "k10.ntp=[fd00:3::2]:123", ["-initrd", a.bundle])
    g.wait_for(r"net: IPv6 fd00:3::5054:ff:fe12:3456/64 \(SLAAC\)", "SLAAC address")
    g.wait_for(r"time: (stepped by \S+ s from|in sync with) \[fd00:3::2\]:123", "SNTP over IPv6")
    if not lan.ping(True) or not lan.ping(False):
        fail("no echo reply over IPv6 or IPv4", g)
    r = lan.http6_get("/hello/").decode(errors="replace")
    if not r.startswith("HTTP/1.1 200") or "hello from k10-metal" not in r:
        fail(f"GET /hello/ over IPv6: {r[:200]!r}", g)
    g.kill()
    say("ipv6: SLAAC address from a router advertisement, SNTP and HTTP over IPv6, echo replies in both families: ok")


def signed_status(a):
    c = http.client.HTTPConnection("127.0.0.1", a.port, timeout=10)
    c.request("GET", "/_k10s/status", headers={"Authorization": admin_auth(f"127.0.0.1:{a.port}", "GET", "/_k10s/status", b"")})
    r = c.getresponse()
    r.read()
    return r.status


def clock_test(a):
    """The RTC says 2001: until SNTP sets the clock, signed admin requests carry a time the kernel refuses."""
    ntp = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    ntp.bind(("127.0.0.1", 0))
    answer = threading.Event()

    def serve():
        while True:
            req, peer = ntp.recvfrom(512)
            if answer.is_set() and len(req) >= 48 and req[0] & 7 == 3:
                ntp.sendto(ntp_reply(req, 0), peer)

    threading.Thread(target=serve, daemon=True).start()
    net = ["-netdev", f"user,id=n0,hostfwd=tcp:127.0.0.1:{a.port}-:80", "-device", "virtio-net-pci,netdev=n0,romfile="]
    g = Guest(a, "clock", net, f"k10.ntp=10.0.2.2:{ntp.getsockname()[1]}", ["-rtc", "base=2001-01-01T00:00:00"])
    g.wait_for(r"k10-metal ready", "ready")
    if (st := signed_status(a)) != 401:
        fail(f"a signed request to a node whose clock says 2001: {st}, want 401", g)
    answer.set()
    g.wait_for(r"time: stepped by \+\d{9}\.\d+ s from 10\.0\.2\.2", "SNTP step from 2001")
    if (st := signed_status(a)) != 200:
        fail(f"a signed request after the step: {st}, want 200", g)
    g.kill()
    say("clock: signed admin requests refused with the RTC at 2001, accepted once SNTP stepped the clock: ok")


def ssdt_gpe_button(path):
    """An SSDT body (QEMU adds the header): Device (\\_SB.PWRB) { Name (_HID, EisaId ("PNP0C0C")) } and
    Scope (\\_GPE) { Method (_E01) { Notify (\\_SB.PWRB, 0x80) } }."""
    pwrb = b"\\\x2e_SB_PWRB"
    dev = pwrb + b"\x08_HID\x0c\x41\xd0\x0c\x0c"
    method = b"_E01\x00\x86" + pwrb + b"\x0a\x80"
    scope = b"\\_GPE" + b"\x14" + bytes([len(method) + 1]) + method
    open(path, "wb").write(b"\x5b\x82" + bytes([len(dev) + 1]) + dev + b"\x10" + bytes([len(scope) + 1]) + scope)


def button_test(a):
    net = ["-netdev", "user,id=n0", "-device", "virtio-net-pci,netdev=n0,romfile="]
    for machine in ("pc", "q35"):
        g = Guest(a, f"button-{machine}", net, "k10.verbose", ["-M", machine])
        g.wait_for(r"power button: fixed-feature, SCI on IRQ 9", "fixed-feature button armed")
        g.wait_for(r"k10-metal ready", "ready")
        g.qmp_command("system_powerdown")
        g.wait_for(r"power: button pressed \(fixed-feature\): leaving the cluster, then powering off", "button press")
        g.wait_exit("power off")
        if "k10-metal: power off" not in g.text():
            fail("no ACPI power off", g)
        say(f"button: {machine}: the fixed-feature power button powers off (S5): ok")
    ssdt = os.path.join(a.out, "platform-ssdt-pwrb.aml")
    ssdt_gpe_button(ssdt)
    g = Guest(a, "button-gpe", net, "k10.verbose", ["-acpitable", f"sig=SSDT,rev=2,oem_id=K10S,oem_table_id=PWRBGPE,data={ssdt}"])
    g.wait_for(r"power button: fixed-feature, GPE 0x01 \(edge\), SCI on IRQ 9", "GPE button armed")
    g.wait_for(r"k10-metal ready", "ready")
    g.qmp_command("device_add", {"driver": "e1000", "id": "hotplug0"})
    g.wait_for(r"power: button pressed \(GPE 0x01\)", "GPE press")
    g.wait_exit("power off")
    say("button: pc: a control-method power button (its GPE found in an SSDT) powers off: ok")


def request(a, method, target, body=b""):
    """(status, body) of a request to the guest; /_k10s/ requests are signed."""
    c = http.client.HTTPConnection("127.0.0.1", a.port, timeout=30)
    h = {"Authorization": admin_auth(f"127.0.0.1:{a.port}", method, target, body)} if target.startswith("/_k10s/") else {}
    c.request(method, target, body, h)
    r = c.getresponse()
    return r.status, r.read().decode(errors="replace")


def apply_test(a):
    disk = os.path.join(a.out, "platform-apply.img")
    gpt_disk(disk, 128, 1, 64)
    v2, v3 = (os.path.join(a.out, f"platform-v{n}.k10b") for n in (2, 3))
    subprocess.run([a.mkbundle, "-o", v2, "--version", "2", "--services", "hello,kv", "--no-ten"], check=True, capture_output=True)
    subprocess.run([a.mkbundle, "-o", v3, "--version", "3"], check=True, capture_output=True)
    net = ["-netdev", f"user,id=n0,hostfwd=tcp:127.0.0.1:{a.port}-:80", "-device", "virtio-net-pci,netdev=n0,romfile=",
           "-drive", f"file={disk},if=none,id=d0,format=raw,cache=writeback", "-device", "virtio-blk-pci,drive=d0"]
    g = Guest(a, "apply-1", net, "")
    g.wait_for(r"k10-metal ready .* svc=0 src=none", "an empty node")
    st, body = request(a, "POST", "/_k10s/apply", open(v2, "rb").read())
    if st != 200:
        fail(f"apply: {st} {body}", g)
    g.kill()  # a power cut right after the 200
    g = Guest(a, "apply-2", net, "", ["-initrd", a.bundle])
    g.wait_for(r"boot bundle \(initrd\) rejected: manifest version 1 is older than the current 2", "older boot bundle refused")
    g.wait_for(r"k10-metal ready .* svc=2 src=data", "the kept bundle")
    if request(a, "GET", "/hello/")[0] != 200 or request(a, "GET", "/sha/?n=1")[0] != 404:
        fail("want hello served and sha (not in version 2) not found", g)
    say("apply: the applied bundle kept through a power cut, an older boot bundle refused: ok")
    st, body = request(a, "POST", "/_k10s/apply", open(v3, "rb").read())
    if st != 200 or request(a, "POST", "/_k10s/power", b"off")[0] != 202:
        fail(f"apply of version 3, then power off: {st} {body}", g)
    g.wait_exit("power off")
    g = Guest(a, "apply-3", net, "")
    g.wait_for(r"k10-metal ready .* svc=20 src=data", "version 3 kept")
    st, body = request(a, "GET", "/sha/?n=1000")
    if st != 200 or not body.startswith("cc2cbdc0"):
        fail(f"sha after the reboot: {st} {body[:80]}", g)
    g.kill()
    say("apply: a newer apply kept through a clean power off and served after the next boot: ok")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--kernel", required=True)
    ap.add_argument("--bundle", default="out/bundle.k10b")
    ap.add_argument("--mkbundle", default=os.path.join(os.environ.get("CARGO_TARGET_DIR", "target"), "release", "mkbundle"))
    ap.add_argument("--tests", default="lan,clock,button,apply,ipv6")
    ap.add_argument("--out", default="out/test")
    ap.add_argument("--port", type=int, default=18190)
    ap.add_argument("--accel", default="kvm" if os.access("/dev/kvm", os.R_OK | os.W_OK) else "tcg")
    a = ap.parse_args()
    os.makedirs(a.out, exist_ok=True)
    a.admin_key = os.path.join(a.out, "platform-admin.key")
    fd = os.open(a.admin_key, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    os.write(fd, os.urandom(32).hex().encode() + b"\n")
    os.close(fd)
    os.environ["K10S_ADMIN_TOKEN_FILE"] = a.admin_key
    tests = {"lan": lan_test, "clock": clock_test, "button": button_test, "apply": apply_test, "ipv6": ipv6_test}
    for t in a.tests.split(","):
        tests[t](a)
    say("ok")


if __name__ == "__main__":
    main()
