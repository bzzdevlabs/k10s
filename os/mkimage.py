#!/usr/bin/env python3
"""Rootless, reproducible disk image builder for k10s as PID 1 of a Linux image (docs/OS.md): one GPT disk that
boots on BIOS and on UEFI (x86-64), a GPT disk for UEFI (arm64, armv7), an MBR disk for BIOS (i686), or an MBR disk
for the Raspberry Pi 3, 4 and 5.

  mkimage.py OUT.img --kernel KERNEL --initrd CPIO [--initrd CPIO ...] --cmdline "..." --boot DIR
      [--arch x86_64|aarch64|i686|armv7] [--pi --dtbs DIR] [--conf FILE] [--uki OUT] [--version V]
      [--sign KEY --k10s K10S --profile P] [--k10boot EFI --metal ELF] [--esp-mib 256] [--data-mib 256]

--boot is the output of os/bootfiles.sh for the arch; initramfs archives are concatenated in order. --sign signs the
UKI (the Pi: the slot image) with `K10S image sign` (an OS image for updates, docs/OS.md); unsigned images boot but
cannot update; --uki keeps a copy of it. --k10boot and --metal (x86-64): the UEFI loader and tier selector of
metal/boot and the native kernel signed as a k10s image (profile native). x86-64 layout (the shared ESP of
docs/research/universal-os.md §8.4, with the A/B slots of crates/k10s/src/pid1/update.rs); arm64 and armv7 have the
same without the BIOS parts and the native kernel (p1 K10S-BOOT, p2 K10S-DATA, systemd-boot as BOOTAA64.EFI or
BOOTARM.EFI); i686 has its BIOS parts only, on an MBR disk (old BIOSes may not boot GPT): GRUB's core.img in the
sectors before p1, p1 K10S-BOOT (FAT32, active: grub.cfg, grubenv, k10s/a/, the slot's signed UKI as
EFI/Linux/k10s-a.efi, the image's record of the slot), p2 K10S-DATA:

  MBR  GRUB boot.img (first 440 bytes) in front of the protective MBR
  p1   BIOS boot, 1 MiB: GRUB core.img (with multiboot2)
  p2   K10S-BOOT, the EFI system partition (FAT32):
         /EFI/BOOT/BOOTX64.EFI   k10boot: the tier selector (systemd-boot without --k10boot)
         /EFI/systemd/systemd-bootx64.efi  systemd-boot, Tier 1's loader (with --k10boot)
         /EFI/Linux/k10s-a.efi   slot A, a UKI: systemd-stub + kernel + initramfs + command line (+ signature)
         /loader/loader.conf     no menu, no command-line editor, default k10s-a.efi, reboot-on-error auto
         /boot/grub/grub.cfg     BIOS: the native kernel through Multiboot2 if /k10s/tier says tier=metal; else
                                 slot k10s_slot of grubenv, or a trial slot (k10s_try) counted down
         /boot/grub/grubenv      k10s_slot=a
         /k10s/a/                vmlinuz, initrd and boot.cfg (GRUB's linux/initrd lines): the UKI's payload
         /k10s/metal.elf         the native kernel (--metal)
         /k10s/tier              tier=auto and metal_cmdline=, a GRUB environment block (k10boot reads it too)
         /k10s.conf              node configuration, for both tiers
  p3   K10S-DATA, a k10s store (crates/k10-kvlog: the node's KV and files, the native kernel's A/B slots), shared
       by both tiers; it grows to the end of the disk on first boot

--pi (KERNEL is the arm64 Image.gz, --dtbs the kernel's arch/arm64/boot/dts/broadcom):

  MBR (the Pi firmware reads FAT partitions by number)
  p1   K10S-BOOT (FAT32): autoboot.txt (slot A, tryboot: slot B), k10s.conf, bootcode.bin (the Pi 3's second stage,
       which must be on partition 1) and an empty start.elf (bootcode.bin reads autoboot.txt only from a partition
       that has one)
  p2   slot A: the signed slot image, a FAT16 filesystem followed by its signature:
         start_cd.elf, fixup_cd.dat (Pi 3 firmware), start4cd.elf, fixup4cd.dat (Pi 4; the Pi 5 has it in EEPROM),
         config.txt and tryboot.txt (the same: a Pi 3 whose bootcode.bin ignores tryboot_a_b reads tryboot.txt in a
         trial), cmdline.txt, kernel8.img, initramfs8, the Pi 3, 4 and 5 devicetrees, LICENCE.broadcom
  p3   slot B (empty until an update)
  p4   K10S-DATA, a k10s store (MBR type 0xda); it grows to the end of the disk on first boot

GUIDs, volume ids, the store id and every timestamp (SOURCE_DATE_EPOCH, else 0) are fixed, so the same inputs give
the same image. Needs sfdisk, mkfs.fat and mtools.
"""
import argparse
import os
import shutil
import struct
import subprocess
import sys
import tempfile
import time
import zlib

PATH = os.environ.get("PATH", "") + ":/sbin:/usr/sbin"
EPOCH = max(int(os.environ.get("SOURCE_DATE_EPOCH", "0")), 315532800)  # FAT dates start in 1980
MIB = 1 << 20
DISK_GUID = "6B10C5A0-0000-4000-8000-000000000001"
PARTS = [  # name, type GUID, unique GUID
    ("bios-boot", "21686148-6449-6E6F-744E-656564454649", "6B10C5A0-0000-4000-8000-000000000011"),
    ("K10S-BOOT", "C12A7328-F81F-11D2-BA4B-00A0C93EC93B", "6B10C5A0-0000-4000-8000-000000000012"),
    ("K10S-DATA", "6B10C5A0-DA7A-4000-8000-000000000001", "6B10C5A0-0000-4000-8000-000000000013"),
]
ESP_VOLID = "6B10C5A0"
# The store id of K10S-DATA (it only salts the log's checksums: images may share it).
STORE_ID = bytes.fromhex("6b10c5a0000040008000000000000da7")
# Pi slot (MiB): room for any signed image (MAX_IMAGE_BYTES is 64 MiB).
PI_SLOT_MIB = 128
PI_BOOT_MIB = 64
PI_DTBS = [("bcm2837-rpi-3-b.dtb", "bcm2710-rpi-3-b.dtb"), ("bcm2837-rpi-3-b-plus.dtb", "bcm2710-rpi-3-b-plus.dtb"),
           ("bcm2711-rpi-4-b.dtb",) * 2, ("bcm2711-rpi-400.dtb",) * 2, ("bcm2711-rpi-cm4-io.dtb",) * 2,
           ("bcm2712-rpi-5-b.dtb",) * 2, ("bcm2712-d-rpi-5-b.dtb", "bcm2712d0-rpi-5-b.dtb")]
# The firmware's own names: bcm2710-* for the Pi 3 with a 64-bit kernel (mainline: bcm2837-*), bcm2712d0-rpi-5-b.dtb
# for the D0 stepping of the Pi 5 (mainline: bcm2712-d-rpi-5-b.dtb).
PI_CONFIG = """# k10s (docs/OS.md): one arm64 kernel and initramfs for the Raspberry Pi 3, 4 and 5, mainline devicetrees.
arm_64bit=1
kernel=kernel8.img
initramfs initramfs8 followkernel
enable_uart=1
gpu_mem=16
disable_splash=1
[pi3]
start_file=start_cd.elf
fixup_file=fixup_cd.dat
[pi4]
start_file=start4cd.elf
fixup_file=fixup4cd.dat
[all]
"""
LOADER_CONF = "timeout 0\ndefault k10s-a.efi\neditor no\nauto-entries no\nauto-firmware no\nconsole-mode keep\nreboot-on-error auto\n"
# BIOS: the native kernel through Multiboot2 when /k10s/tier says tier=metal (x86-64), with k10s.conf, the bundle and
# the key files of fixed names as modules (named as k10s.conf refers to them, k10-bootproto); else slot k10s_slot,
# unless a trial (k10s_try=X3) is armed: counted down, slot X booted until X0 (GRUB has no arithmetic, hence the
# chain); init commits or clears it (crates/k10s/src/pid1/update.rs models the same).
# GRUB's own messages (errors only: no menu) go to the screen and the first serial port.
GRUB_METAL = """if [ -f /k10s/tier ]; then load_env --file /k10s/tier tier metal_cmdline; fi
if [ "${tier}" = metal -a -f /k10s/metal.elf ]; then
multiboot2 /k10s/metal.elf ${metal_cmdline}
if [ -f /k10s.conf ]; then module2 /k10s.conf k10s.conf; fi
if [ -f /k10s/bundle.k10b ]; then module2 /k10s/bundle.k10b k10s.bundle; fi
for f in admin.key cluster.key cluster-next.key; do if [ -f /$f ]; then module2 /$f /boot/$f; fi; done
boot
fi
"""
GRUB_HEAD = "serial --unit=0 --speed=115200\nterminal_output --append serial\nset timeout=0\n"
GRUB_SLOTS = "load_env\nset slot=\"${k10s_slot}\"\n" + "".join(
    f'{"if" if (s, n) == ("a", 3) else "elif"} [ "${{k10s_try}}" = {s}{n} ]; then set k10s_try={s}{n - 1}; set slot={s}; save_env k10s_try\n'
    for s in "ab" for n in (3, 2, 1)) + "fi\nif [ \"${slot}\" != b ]; then set slot=a; fi\nsource /k10s/${slot}/boot.cfg\nboot\n"
# /k10s/tier: a GRUB environment block (GRUB reads it on BIOS, k10boot on UEFI; k10-bootproto `tier`).
TIER = """# GRUB Environment Block
# This machine's tier (docs/OS.md "Tiers"), read by k10boot (UEFI) and GRUB (BIOS):
#   tier=auto   UEFI: the native kernel if it drives one of the network adapters and K10S-DATA's disk, else
#               Tier 1; BIOS: Tier 1 (as shipped)
#   tier=linux  Tier 1, Linux
#   tier=metal  the native kernel, /k10s/metal.elf
# metal_cmdline= is the native kernel's command line. Edit lines in place: no blank lines, a newline at the end.
tier=auto
metal_cmdline=
"""


def grubenv(**kv):
    """A GRUB environment block (1024 bytes, `#` padded)."""
    text = "# GRUB Environment Block\n" + "".join(f"{k}={v}\n" for k, v in kv.items())
    return (text + "#" * (1024 - len(text))).encode()
CONF = """# k10s node configuration: KEY=VALUE lines with the keys of `k10s node` and of init (docs/OS.md).
# k10s.KEY=VALUE on the kernel command line wins over this file.
# admin-token-file=/boot/admin.key
# join=10.0.0.10:7946
"""


def run(*cmd, **kw):
    exe = shutil.which(cmd[0], path=PATH) or cmd[0]
    return subprocess.run([exe, *cmd[1:]], check=True, **kw)


def uki(stub, sections, out):
    """systemd-stub plus sections appended after its own, in order (.linux last), as ukify does: section headers in
    the stub's spare header room, data at its file alignment, addresses at its section alignment."""
    b = bytearray(open(stub, "rb").read())
    pe = struct.unpack_from("<I", b, 0x3C)[0]
    nsec, optsz = struct.unpack_from("<H", b, pe + 6)[0], struct.unpack_from("<H", b, pe + 20)[0]
    opt = pe + 24
    # PE32+ (64-bit) or PE32 (ia32, arm): the fields used below are at the same offsets in both.
    if struct.unpack_from("<H", b, opt)[0] not in (0x20B, 0x10B):
        sys.exit("mkimage: the stub is not a PE32 or PE32+ image")
    salign, falign = struct.unpack_from("<II", b, opt + 32)
    table, room = opt + optsz, struct.unpack_from("<I", b, opt + 60)[0]
    if table + 40 * (nsec + len(sections)) > room:
        sys.exit("mkimage: no room for the UKI's section headers in the stub")
    heads = [struct.unpack_from("<IIII", b, table + 40 * i + 8) for i in range(nsec)]
    end = max(va + vsize for vsize, va, _, _ in heads)
    del b[max(ptr + raw for _, _, raw, ptr in heads):]
    up = lambda n, a: -(-n // a) * a
    for name, path in sections:
        data = open(path, "rb").read()
        b += bytes(up(len(b), falign) - len(b))
        va = up(end, salign)
        # IMAGE_SCN_CNT_INITIALIZED_DATA | IMAGE_SCN_MEM_READ
        b[table + 40 * nsec:table + 40 * (nsec + 1)] = struct.pack("<8sIIIIIIHHI", name.encode(), len(data), va,
                                                                    up(len(data), falign), len(b), 0, 0, 0, 0, 0x40000040)
        b += data + bytes(up(len(data), falign) - len(data))
        nsec, end = nsec + 1, va + len(data)
    struct.pack_into("<H", b, pe + 6, nsec)
    struct.pack_into("<I", b, pe + 8, EPOCH & 0xFFFFFFFF)
    struct.pack_into("<I", b, opt + 56, up(end, salign))
    struct.pack_into("<I", b, opt + 64, 0)
    c = sum(struct.unpack(f"<{len(b) // 2}H", b[: len(b) // 2 * 2])) + (b[-1] if len(b) % 2 else 0)
    while c >> 16:
        c = (c & 0xFFFF) + (c >> 16)
    struct.pack_into("<I", b, opt + 64, c + len(b))
    open(out, "wb").write(b)


def dos_time(epoch):
    t = time.gmtime(epoch)
    return ((t.tm_year - 1980) << 9 | t.tm_mon << 5 | t.tm_mday, t.tm_hour << 11 | t.tm_min << 5 | t.tm_sec // 2)


def fat_fix_times(img, epoch):
    """Sets every directory entry's timestamps of a FAT16 or FAT32 image to `epoch` (mtools stamps directories with
    now)."""
    date, tod = dos_time(epoch)
    with open(img, "r+b") as f:
        bs = f.read(512)
        bps, spc = struct.unpack_from("<H", bs, 11)[0], bs[13]
        reserved, nfats = struct.unpack_from("<H", bs, 14)[0], bs[16]
        roots, fat16 = struct.unpack_from("<H", bs, 17)[0], struct.unpack_from("<H", bs, 22)[0]
        fatsz = fat16 or struct.unpack_from("<I", bs, 36)[0]
        root_dir = (reserved + nfats * fatsz) * bps
        data = root_dir + roots * 32
        csize = spc * bps

        def chain(c):
            while 2 <= c < (0xFFF8 if fat16 else 0x0FFFFFF8):
                yield c
                f.seek(reserved * bps + (2 if fat16 else 4) * c)
                c = struct.unpack("<H" if fat16 else "<I", f.read(2 if fat16 else 4))[0] & 0x0FFFFFFF

        def walk(first):
            # FAT16's root directory is a fixed area before the clusters (cluster number 0).
            for off, size in ([(root_dir, roots * 32)] if first == 0 else [(data + (c - 2) * csize, csize) for c in list(chain(first))]):
                f.seek(off)
                buf = bytearray(f.read(size))
                subdirs = []
                for i in range(0, size, 32):
                    e = buf[i:i + 32]
                    if e[0] == 0:
                        break
                    if e[0] == 0xE5 or e[11] == 0x0F:
                        continue
                    struct.pack_into("<BHHH", buf, i + 13, 0, tod, date, date)
                    struct.pack_into("<HH", buf, i + 22, tod, date)
                    if e[11] & 0x10 and e[0] != ord("."):
                        subdirs.append(struct.unpack_from("<H", e, 20)[0] << 16 | struct.unpack_from("<H", e, 26)[0])
                f.seek(off)
                f.write(buf)
                for d in subdirs:
                    walk(d)

        walk(0 if fat16 else struct.unpack_from("<I", bs, 44)[0])


def fat_image(path, label, volid, size_kib, files, env, fat=32, spc=4):
    """A FAT filesystem image holding `files` [(source, path in the image)], timestamps fixed; `spc` 512-byte
    sectors per cluster (FAT32 needs at least 65525 clusters, FAT16 at least 4085)."""
    run("mkfs.fat", "-F", str(fat), "-s", str(spc), "-n", label, "-i", volid, "-C", path, str(size_kib), stdout=subprocess.DEVNULL)
    dirs = sorted({"/".join(d.split("/")[:n]) for _, d in files for n in range(1, d.count("/") + 1)})
    if dirs:
        run("mmd", "-i", path, *[f"::/{d}" for d in dirs], env=env)
    for src, dst in files:
        run("mcopy", "-i", path, src, f"::/{dst}", env=env)
    fat_fix_times(path, EPOCH)


def copy_sparse(src, dst, offset):
    with open(src, "rb") as s, open(dst, "r+b") as d:
        pos = offset
        while chunk := s.read(MIB):
            if chunk.count(0) != len(chunk):
                d.seek(pos)
                d.write(chunk)
            pos += len(chunk)


def grub_bios(boot, disk, tmp, mbr=False):
    """GRUB for BIOS without grub-install: core.img in the BIOS boot partition p1 (LBA 2048) of a GPT disk, or right
    after the MBR (LBA 1, the gap before p1 at 1 MiB) of an MBR disk; boot.img in the MBR, sector numbers patched."""
    mods = os.path.join(boot, "usr/lib/grub/i386-pc")
    core = os.path.join(tmp, "core.img")
    run(os.path.join(boot, "usr/bin/grub-mkimage"), "-d", mods, "-O", "i386-pc", "-o", core, "-p",
        "(,msdos1)/boot/grub" if mbr else "(,gpt2)/boot/grub", "biosdisk", "part_msdos" if mbr else "part_gpt", "fat",
        "normal", "linux", "configfile", "loadenv", "test", "serial", "terminal", *([] if mbr else ["multiboot2"]))
    img = bytearray(open(core, "rb").read())
    # The BIOS boot partition (1 MiB), or the sectors between the MBR and p1 at 1 MiB.
    lba, room = (1, MIB - 512) if mbr else (2048, MIB)
    if len(img) > room:
        sys.exit("mkimage: no room for core.img")
    struct.pack_into("<Q", img, 0x200 - 12, lba + 1)  # diskboot.img's blocklist: the rest of core.img
    mbr = bytearray(open(os.path.join(mods, "boot.img"), "rb").read()[:440])
    struct.pack_into("<Q", mbr, 0x5C, lba)  # GRUB_BOOT_MACHINE_KERNEL_SECTOR
    mbr[0x66:0x68] = b"\x90\x90"  # like grub-bios-setup on a hard disk: trust no BIOS boot drive number below 0x80
    with open(disk, "r+b") as f:
        f.write(mbr)
        f.seek(lba * 512)
        f.write(img)


def sign(a, payload, out):
    """`K10S image sign`s `payload` into `out` (a copy when not signing)."""
    if not a.sign:
        shutil.copyfile(payload, out)
        return
    if not (a.k10s and a.profile):
        sys.exit("mkimage: --sign needs --k10s and --profile")
    run(a.k10s, "image", "sign", "--key", a.sign, "--arch", a.arch, "--profile", a.profile, "--version", a.version, payload, out)


def store_format(f, size, store_id=STORE_ID, offset=0):
    """Formats `size` bytes of file `f` at `offset` as an empty k10s store, byte for byte what
    k10_kvlog::store::Store::format writes (superblocks of generations 1 and 2, the first segment's SEGMENT record)."""
    unit, log_offset, seg, slot_offset, slot_bytes, reserve = 4096, 33 * MIB, 4 * MIB, MIB, 16 * MIB, 1024
    segments = min((size - log_offset) // seg, 1 << 22)
    if segments < 5:
        raise SystemExit(f"mkimage: a store needs {(log_offset + 5 * seg) >> 20} MiB")
    mask = zlib.crc32(struct.pack("<Q", 1), zlib.crc32(store_id))

    def record(op, value):
        body = bytes([op, 0, 0]) + value
        return struct.pack("<II", len(body), zlib.crc32(body) ^ mask) + body

    first = record(6, store_id + struct.pack("<QI", 1, 0))
    first += record(4, bytes(unit - len(first) - 11))

    def superblock(gen):
        b = bytearray(unit)
        b[0:8] = b"K10SDATA"
        struct.pack_into("<IIQ", b, 8, 1, 512, gen)
        b[24:40] = store_id
        struct.pack_into("<QQQIIIIQQ", b, 40, slot_offset, slot_bytes, log_offset, seg, segments, 0, 0, 1, 1 + reserve)
        b[176:178] = b"\xff\xff"
        struct.pack_into("<I", b, 508, zlib.crc32(bytes(b[:508])))
        return bytes(b)

    for at, data in ((log_offset, first), (unit, superblock(1)), (0, superblock(2))):
        f.seek(offset + at)
        f.write(data)


def data_store(a, tmp):
    data = os.path.join(tmp, "data.img")
    with open(data, "wb") as f:
        f.truncate(a.data_mib * MIB)
        store_format(f, a.data_mib * MIB)
    return data


def disk(out, label, parts, total):
    """Writes the partition table (sfdisk lines) and the partitions' images [(image or None, start, sfdisk line)]."""
    if os.path.exists(out):
        os.remove(out)
    with open(out, "wb") as f:
        f.truncate(total)
    run("sfdisk", "-q", out, input=(label + "".join(line for _, _, line in parts)).encode())
    for img, start, _ in parts:
        if img:
            copy_sparse(img, out, start)


def pi(a, tmp, env, put, initrd):
    """The Raspberry Pi image: MBR, K10S-BOOT, the signed slot A, an empty slot B, K10S-DATA."""
    fw = os.path.join(a.boot, "usr/lib/raspi-firmware")
    files = [(os.path.join(fw, f), f) for f in ("start_cd.elf", "fixup_cd.dat", "start4cd.elf", "fixup4cd.dat")]
    files += [(os.path.join(a.boot, "usr/share/doc/raspi-firmware/copyright"), "LICENCE.broadcom"),
              (put("config.txt", PI_CONFIG), "config.txt"), (put("config.txt", PI_CONFIG), "tryboot.txt"),
              (put("cmdline.txt", a.cmdline + "\n"), "cmdline.txt"), (a.kernel, "kernel8.img"), (initrd, "initramfs8")]
    files += [(os.path.join(a.dtbs, src), dst) for src, dst in PI_DTBS]
    # Clusters of 1 KiB, at least 1 MiB of slack; FAT16 needs 4085 clusters.
    kib = max((sum(-(-os.path.getsize(f) // 1024) for f, _ in files) + 2048) // 1024 * 1024, 8192)
    slot = os.path.join(tmp, "slot.img")
    fat_image(slot, "K10S-SLOT", ESP_VOLID, kib, files, env, fat=16, spc=2)
    signed = os.path.join(tmp, "slot.signed")
    sign(a, slot, signed)
    if a.uki:
        shutil.copyfile(signed, a.uki)
    boot = os.path.join(tmp, "boot.img")
    autoboot = "[all]\ntryboot_a_b=1\nboot_partition=2\n[tryboot]\nboot_partition=3\n"
    fat_image(boot, "K10S-BOOT", ESP_VOLID, PI_BOOT_MIB * 1024,
              [(put("autoboot.txt", autoboot), "autoboot.txt"), (a.conf or put("k10s.conf", CONF), "k10s.conf"),
               (os.path.join(fw, "bootcode.bin"), "bootcode.bin"), (put("start.elf", b""), "start.elf")], env, spc=1)
    data = data_store(a, tmp)
    at = [MIB]
    for mib in (PI_BOOT_MIB, PI_SLOT_MIB, PI_SLOT_MIB, a.data_mib):
        at.append(at[-1] + mib * MIB)
    line = lambda n, t: f"start={at[n] // 512}, size={(at[n + 1] - at[n]) // 512}, type={t}\n"
    disk(a.out, f"label: dos\nlabel-id: 0x{ESP_VOLID}\n",
         [(boot, at[0], line(0, "c")), (signed, at[1], line(1, "e")), (None, at[2], line(2, "e")), (data, at[3], line(3, "da"))],
         at[4] + MIB)
    print(f"mkimage: {a.out}: {(at[4] + MIB) >> 20} MiB (slots {PI_SLOT_MIB} MiB, data {a.data_mib} MiB), "
          f"slot image {os.path.getsize(signed)} bytes", file=sys.stderr)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("out")
    ap.add_argument("--kernel", required=True)
    ap.add_argument("--initrd", action="append", required=True)
    ap.add_argument("--cmdline", required=True)
    ap.add_argument("--boot", required=True)
    ap.add_argument("--arch", choices=["x86_64", "aarch64", "i686", "armv7"], default="x86_64")
    ap.add_argument("--pi", action="store_true")
    ap.add_argument("--dtbs")
    ap.add_argument("--conf")
    ap.add_argument("--uki")
    ap.add_argument("--version", default="dev")
    ap.add_argument("--sign")
    ap.add_argument("--k10s")
    ap.add_argument("--profile")
    ap.add_argument("--k10boot")
    ap.add_argument("--metal")
    ap.add_argument("--esp-mib", type=int, default=256)
    ap.add_argument("--data-mib", type=int, default=256)
    a = ap.parse_args()
    if a.pi and (a.arch != "aarch64" or not a.dtbs):
        sys.exit("mkimage: --pi needs --arch aarch64 and --dtbs")
    if (a.k10boot or a.metal) and not (a.k10boot and a.metal and a.arch == "x86_64"):
        sys.exit("mkimage: --k10boot and --metal go together, on x86_64")
    # GRUB for BIOS (x86); systemd-boot for UEFI (all but the BIOS-only i686 image, an MBR disk).
    bios, uefi = a.arch in ("x86_64", "i686"), a.arch != "i686"
    efi_arch = {"x86_64": "x64", "aarch64": "aa64", "i686": "ia32", "armv7": "arm"}[a.arch]

    env = dict(os.environ, MTOOLS_SKIP_CHECK="1")
    with tempfile.TemporaryDirectory() as tmp:
        def put(name, data):
            p = os.path.join(tmp, name)
            with open(p, "wb") as f:
                f.write(data if isinstance(data, bytes) else data.encode())
            return p

        initrd = put("initrd", b"".join(open(p, "rb").read() for p in a.initrd))
        if a.pi:
            return pi(a, tmp, env, put, initrd)
        osrel = put("osrel", f'NAME="k10s"\nID=k10s\nVERSION_ID="{a.version}"\nPRETTY_NAME="k10s {a.version}"\n')
        unsigned, efi = os.path.join(tmp, "k10s-a.unsigned"), os.path.join(tmp, "k10s-a.efi")
        uki(os.path.join(a.boot, f"usr/lib/systemd/boot/efi/linux{efi_arch}.efi.stub"),
            [(".osrel", osrel), (".cmdline", put("cmdline", a.cmdline)), (".initrd", initrd), (".linux", a.kernel)], unsigned)
        sign(a, unsigned, efi)
        if a.uki:
            shutil.copyfile(efi, a.uki)

        esp = os.path.join(tmp, "esp.img")
        files = []
        sdboot = os.path.join(a.boot, f"usr/lib/systemd/boot/efi/systemd-boot{efi_arch}.efi")
        if a.k10boot:
            files += [(a.k10boot, "EFI/BOOT/BOOTX64.EFI"), (sdboot, "EFI/systemd/systemd-bootx64.efi"), (a.metal, "k10s/metal.elf")]
        elif uefi:
            files.append((sdboot, f"EFI/BOOT/BOOT{efi_arch.upper()}.EFI"))
        files.append((efi, "EFI/Linux/k10s-a.efi"))
        if uefi:
            files.append((put("loader.conf", LOADER_CONF), "loader/loader.conf"))
        files.append((a.conf or put("k10s.conf", CONF), "k10s.conf"))
        if bios:
            files += [
                (put("grub.cfg", GRUB_HEAD + (GRUB_METAL if a.arch == "x86_64" else "") + GRUB_SLOTS), "boot/grub/grub.cfg"),
                (put("grubenv", grubenv(k10s_slot="a")), "boot/grub/grubenv"),
                (a.kernel, "k10s/a/vmlinuz"),
                (initrd, "k10s/a/initrd"),
                (put("boot.cfg", f"linux /k10s/a/vmlinuz {a.cmdline} k10s.slot=a\ninitrd /k10s/a/initrd\n"), "k10s/a/boot.cfg"),
            ]
        if a.arch == "x86_64":
            files.append((put("tier", TIER), "k10s/tier"))
        # 2 KiB clusters: GRUB reads files one cluster at a time, and BIOS disk reads are slow.
        fat_image(esp, "K10S-BOOT", ESP_VOLID, a.esp_mib * 1024, files, env)
        data = data_store(a, tmp)

        if not uefi:
            at = [MIB, MIB + a.esp_mib * MIB, MIB + (a.esp_mib + a.data_mib) * MIB]
            disk(a.out, f"label: dos\nlabel-id: 0x{ESP_VOLID}\n",
                 [(esp, at[0], f"start={at[0] // 512}, size={a.esp_mib * MIB // 512}, type=c, bootable\n"),
                  (data, at[1], f"start={at[1] // 512}, size={a.data_mib * MIB // 512}, type=da\n")], at[2] + MIB)
            grub_bios(a.boot, a.out, tmp, mbr=True)
        else:
            parts = PARTS if bios else PARTS[1:]
            sizes = [MIB, a.esp_mib * MIB, a.data_mib * MIB][-len(parts):]
            at = [MIB if bios else 2 * MIB]
            for size in sizes:
                at.append(at[-1] + size)
            lines = [f'start={at[i] // 512}, size={size // 512}, type={ptype}, name="{name}", uuid={uuid}\n'
                     for i, ((name, ptype, uuid), size) in enumerate(zip(parts, sizes))]
            images = ([None] if bios else []) + [esp, data]
            disk(a.out, f"label: gpt\nlabel-id: {DISK_GUID}\n", [(img, at[i], lines[i]) for i, img in enumerate(images)], at[-1] + MIB)
            if bios:
                grub_bios(a.boot, a.out, tmp)
        print(f"mkimage: {a.out}: {os.path.getsize(a.out) >> 20} MiB (K10S-BOOT {a.esp_mib} MiB, data {a.data_mib} MiB), "
              f"UKI {os.path.getsize(efi)} bytes", file=sys.stderr)


if __name__ == "__main__":
    main()
