#!/usr/bin/env python3
"""Writes an ACPI WDAT table (Microsoft's Watchdog Action Table) describing the TCO watchdog of QEMU's q35 (ICH9:
ACPI base 0x600, so TCO registers at 0x660; NO_REBOOT in GCS at RCBA 0xFED1C000 + 0x3410), complete with its
header, for `qemu-system-x86_64 -M q35 -acpitable file=FILE` (metal/test.sh). One countdown unit is 1.2 s: the TCO
timer ticks every 0.6 s and resets the machine on its second expiry. The same table is built by
metal/drivers/tests/watchdog.rs.

usage: wdat.py FILE
"""
import struct
import sys

TCO = 0x660
GCS = 0xFED1C000 + 0x3410
RESET, QUERY_COUNTDOWN, SET_COUNTDOWN, QUERY_RUNNING, SET_RUNNING = 1, 5, 6, 8, 9
QUERY_STOPPED, SET_STOPPED, QUERY_REBOOT, SET_REBOOT, QUERY_STATUS, SET_STATUS = 10, 11, 14, 15, 32, 33
READ_VALUE, READ_COUNTDOWN, WRITE_VALUE, WRITE_COUNTDOWN, PRESERVE = 0, 1, 2, 3, 0x80


def io16(port, bit=0):
    return (1, 16, bit, 2, port)


def mem32(addr, bit):
    return (0, 32, bit, 3, addr)


ENTRIES = [
    (RESET, WRITE_VALUE, io16(TCO + 0x00), 1, 0xFFFF),
    (QUERY_COUNTDOWN, READ_COUNTDOWN, io16(TCO + 0x12), 0, 0x3FF),
    (SET_COUNTDOWN, WRITE_COUNTDOWN | PRESERVE, io16(TCO + 0x12), 0, 0x3FF),
    (QUERY_RUNNING, READ_VALUE, io16(TCO + 0x08, 11), 0, 1),
    (SET_RUNNING, WRITE_VALUE | PRESERVE, io16(TCO + 0x08, 11), 0, 1),
    (QUERY_STOPPED, READ_VALUE, io16(TCO + 0x08, 11), 1, 1),
    (SET_STOPPED, WRITE_VALUE | PRESERVE, io16(TCO + 0x08, 11), 1, 1),
    (QUERY_REBOOT, READ_VALUE, mem32(GCS, 5), 0, 1),
    (SET_REBOOT, WRITE_VALUE | PRESERVE, mem32(GCS, 5), 0, 1),
    (QUERY_STATUS, READ_VALUE, io16(TCO + 0x06, 1), 1, 1),
    (SET_STATUS, WRITE_VALUE, io16(TCO + 0x06, 1), 1, 1),
]


def table():
    body = struct.pack("<I", 32)  # watchdog header length
    body += struct.pack("<HBBB3x", 0xFFFF, 0xFF, 0xFF, 0xFF)  # not a PCI device
    body += struct.pack("<IIIB3xI", 1200, 0x3FF, 2, 1, len(ENTRIES))  # period ms, max, min, flags (enabled), count
    for action, instr, (space, bits, off, access, addr), value, mask in ENTRIES:
        body += struct.pack("<BBH", action, instr, 0) + struct.pack("<BBBBQ", space, bits, off, access, addr)
        body += struct.pack("<II", value, mask)
    length = 36 + len(body)
    head = b"WDAT" + struct.pack("<IBB", length, 1, 0) + b"K10S  " + b"K10SWDAT" + struct.pack("<I", 1) + b"K10S" + struct.pack("<I", 1)
    t = bytearray(head + body)
    t[9] = (-sum(t)) & 0xFF
    return bytes(t)


if __name__ == "__main__":
    open(sys.argv[1], "wb").write(table())
