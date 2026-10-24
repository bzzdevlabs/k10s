#!/usr/bin/env python3
"""DHCP client test of k10-metal against a scripted server (os/test.py's Dhcpd, 6 s leases, Ethernet frames over a
QEMU UDP socket netdev): the first lease, a renewal (unicast, ciaddr set), a rebinding (broadcast) after the server
stays silent, a NAK that drops the address, discovery of a new address, and its renewal. The guest is a cluster
member that advertises the interface's address, so the new address makes it leave and reboot, and it comes back
on the new address.

  tools/dhcp_test.py --kernel out/k10-metal.elf [--log FILE]      (ACCEL_ARGS: QEMU accelerator options)
"""
import argparse
import importlib.util
import os
import re
import socket
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.dont_write_bytecode = True  # no __pycache__ next to os/test.py
spec = importlib.util.spec_from_file_location("os_test", os.path.join(HERE, "..", "..", "os", "test.py"))
os_test = importlib.util.module_from_spec(spec)
spec.loader.exec_module(os_test)


def free_udp_port():
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--kernel", required=True)
    ap.add_argument("--log", default="out/test/serial-dhcp.log")
    ap.add_argument("--timeout", type=float, default=120)
    a = ap.parse_args()
    sp, qp = free_udp_port(), free_udp_port()
    d = os_test.Dhcpd(sp, qp)
    accel = os.environ.get("ACCEL_ARGS", "-enable-kvm -cpu host").split()
    if os.path.exists(a.log):
        os.unlink(a.log)
    q = subprocess.Popen(["qemu-system-x86_64", *accel, "-M", "pc", "-m", "64M", "-nodefaults", "-display", "none",
                          "-netdev", f"socket,id=n0,udp=127.0.0.1:{sp},localaddr=127.0.0.1:{qp}",
                          "-device", "virtio-net-pci,netdev=n0,mac=52:54:00:12:34:56,romfile=",
                          "-kernel", a.kernel, "-serial", f"file:{a.log}",
                          "-append", "k10.cluster k10.insecure_unauthenticated_cluster"])
    deadline = time.time() + a.timeout

    def wait_for(pattern, what):
        while time.time() < deadline:
            log = open(a.log, errors="replace").read() if os.path.exists(a.log) else ""
            if re.search(pattern, log):
                return
            if q.poll() is not None:
                break
            time.sleep(0.1)
        q.kill()
        sys.exit(f"dhcp_test: no {what} (/{pattern}/)\n{open(a.log, errors='replace').read()[-2000:]}")

    try:
        wait_for(r"net: DHCP 10\.0\.3\.15/24 gateway 10\.0\.3\.1 from 10\.0\.3\.2, lease 6 s", "first lease")
        wait_for(r"k10-metal ready .* ip=10\.0\.3\.15", "ready line with the leased address")
        wait_for(r"net: DHCP lease of 10\.0\.3\.15 renewed, lease 6 s", "renewal")
        wait_for(r"net: DHCP lease of 10\.0\.3\.15 lost", "loss of the lease (NAK)")
        wait_for(r"net: DHCP 10\.0\.3\.16/24", "new lease")
        wait_for(r"address changed from 10\.0\.3\.15 to 10\.0\.3\.16: rebooting", "reboot for the new address")
        wait_for(r"k10-metal: reboot[\s\S]*k10-metal ready .* ip=10\.0\.3\.16", "the node back on the new address")
        if not d.done.wait(max(1, deadline - time.time())):
            sys.exit(f"dhcp_test: no renewal of 10.0.3.16 (server saw {d.log})")
        if d.error:
            sys.exit(f"dhcp_test: {d.error}")
        if not (d.log[0] == ("10.0.3.15", "unicast") and ("10.0.3.15", "broadcast") in d.log):
            sys.exit(f"dhcp_test: want a unicast renewal then a broadcast rebinding: {d.log}")
    finally:
        q.kill()
        q.wait()
    print(f"lease, unicast renewal, broadcast rebinding, NAK, new address 10.0.3.16 (cluster member rebooted onto it) and its renewal ({len(d.log)} renewals)")


if __name__ == "__main__":
    main()
