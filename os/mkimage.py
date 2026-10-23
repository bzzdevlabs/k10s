#!/usr/bin/env python3
"""Rootless, reproducible disk image builder for k10s as PID 1 of a Linux image (docs/OS.md): one GPT disk that
boots on BIOS and on UEFI.

  mkimage.py OUT.img --kernel BZIMAGE --initrd CPIO [--initrd CPIO ...] --cmdline "..." --boot DIR
      [--conf FILE] [--uki OUT.efi] [--version V] [--esp-mib 256] [--data-mib 256]

--boot is the output of os/bootfiles.sh; initramfs archives are concatenated in order. Layout (the shared ESP of
docs/research/universal-os.md §8.4):

  MBR  GRUB boot.img (first 440 bytes) in front of the protective MBR
  p1   BIOS boot, 1 MiB: GRUB core.img
  p2   K10S-BOOT, the EFI system partition (FAT32):
         /EFI/BOOT/BOOTX64.EFI   systemd-boot
         /EFI/Linux/k10s-a.efi   slot A, a UKI: systemd-stub + kernel + initramfs + command line
         /loader/loader.conf     no menu, no command-line editor
         /boot/grub/grub.cfg     BIOS: GRUB boots /k10s/a/vmlinuz and /k10s/a/initrd, the UKI's payload
         /k10s/tier              auto (read by the Tier 2 loader)
         /k10s.conf              node configuration
  p3   K10S-DATA (ext4); init grows it to the end of the disk on first boot

GUIDs, volume ids, the ext4 hash seed and every timestamp (SOURCE_DATE_EPOCH, else 0) are fixed, so the same inputs
give the same image. Needs objcopy, sfdisk, mkfs.fat, mtools and mkfs.ext4.
"""
import argparse
import os
import shutil
import struct
import subprocess
import sys
import tempfile
import time

PATH = os.environ.get("PATH", "") + ":/sbin:/usr/sbin"
EPOCH = max(int(os.environ.get("SOURCE_DATE_EPOCH", "0")), 315532800)  # FAT dates start in 1980
MIB = 1 << 20
DISK_GUID = "6B10C5A0-0000-4000-8000-000000000001"
PARTS = [  # name, type GUID, unique GUID
    ("bios-boot", "21686148-6449-6E6F-744E-656564454649", "6B10C5A0-0000-4000-8000-000000000011"),
    ("K10S-BOOT", "C12A7328-F81F-11D2-BA4B-00A0C93EC93B", "6B10C5A0-0000-4000-8000-000000000012"),
    ("K10S-DATA", "0FC63DAF-8483-4772-8E79-3D69D8477DE4", "6B10C5A0-0000-4000-8000-000000000013"),
]
ESP_VOLID = "6B10C5A0"
DATA_UUID = "6b10c5a0-0000-4000-8000-0000000000da"
DATA_SEED = "6b10c5a0-0000-4000-8000-0000000000db"
LOADER_CONF = "timeout 0\ndefault k10s-*\neditor no\nauto-entries no\nauto-firmware no\nconsole-mode keep\n"
CONF = """# k10s node configuration: KEY=VALUE lines with the keys of `k10s node` and of init (docs/OS.md).
# k10s.KEY=VALUE on the kernel command line wins over this file.
# admin-token-file=/boot/admin.key
# join=10.0.0.10:7946
"""


def run(*cmd, **kw):
    exe = shutil.which(cmd[0], path=PATH) or cmd[0]
    return subprocess.run([exe, *cmd[1:]], check=True, **kw)


def uki(stub, sections, out):
    """systemd-stub plus sections placed after the stub's own, in order, at its section alignment (.linux last)."""
    b = open(stub, "rb").read()
    pe = struct.unpack_from("<I", b, 0x3C)[0]
    nsec, optsz = struct.unpack_from("<H", b, pe + 6)[0], struct.unpack_from("<H", b, pe + 20)[0]
    opt = pe + 24
    if struct.unpack_from("<H", b, opt)[0] != 0x20B:
        sys.exit("mkimage: the stub is not PE32+")
    base, align = struct.unpack_from("<Q", b, opt + 24)[0], struct.unpack_from("<I", b, opt + 32)[0]
    end = max(sum(struct.unpack_from("<II", b, opt + optsz + 40 * i + 8)) for i in range(nsec))
    args = []
    for name, path in sections:
        vma = -(-end // align) * align
        args += ["--add-section", f"{name}={path}", "--change-section-vma", f"{name}={base + vma:#x}"]
        end = vma + os.path.getsize(path)
    run("objcopy", *args, stub, out)
    # objcopy stamps the PE header with the current time: fixed time, checksum recomputed.
    img = bytearray(open(out, "rb").read())
    struct.pack_into("<I", img, pe + 8, EPOCH & 0xFFFFFFFF)
    struct.pack_into("<I", img, opt + 64, 0)
    c = sum(struct.unpack(f"<{len(img) // 2}H", img[: len(img) // 2 * 2])) + (img[-1] if len(img) % 2 else 0)
    while c >> 16:
        c = (c & 0xFFFF) + (c >> 16)
    struct.pack_into("<I", img, opt + 64, c + len(img))
    open(out, "wb").write(img)


def dos_time(epoch):
    t = time.gmtime(epoch)
    return ((t.tm_year - 1980) << 9 | t.tm_mon << 5 | t.tm_mday, t.tm_hour << 11 | t.tm_min << 5 | t.tm_sec // 2)


def fat_fix_times(img, epoch):
    """Sets every directory entry's timestamps of a FAT32 image to `epoch` (mtools stamps directories with now)."""
    date, tod = dos_time(epoch)
    with open(img, "r+b") as f:
        bs = f.read(512)
        bps, spc = struct.unpack_from("<H", bs, 11)[0], bs[13]
        reserved, nfats = struct.unpack_from("<H", bs, 14)[0], bs[16]
        fatsz, root = struct.unpack_from("<I", bs, 36)[0], struct.unpack_from("<I", bs, 44)[0]
        data = (reserved + nfats * fatsz) * bps
        csize = spc * bps

        def chain(c):
            while 2 <= c < 0x0FFFFFF8:
                yield c
                f.seek(reserved * bps + 4 * c)
                c = struct.unpack("<I", f.read(4))[0] & 0x0FFFFFFF

        def walk(first):
            for c in list(chain(first)):
                off = data + (c - 2) * csize
                f.seek(off)
                buf = bytearray(f.read(csize))
                subdirs = []
                for i in range(0, csize, 32):
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

        walk(root)


def copy_sparse(src, dst, offset):
    with open(src, "rb") as s, open(dst, "r+b") as d:
        pos = offset
        while chunk := s.read(MIB):
            if chunk.count(0) != len(chunk):
                d.seek(pos)
                d.write(chunk)
            pos += len(chunk)


def grub_bios(boot, disk, tmp):
    """GRUB for BIOS without grub-install: core.img in p1 (LBA 2048), boot.img in the MBR, sector numbers patched."""
    mods = os.path.join(boot, "usr/lib/grub/i386-pc")
    core = os.path.join(tmp, "core.img")
    run(os.path.join(boot, "usr/bin/grub-mkimage"), "-d", mods, "-O", "i386-pc", "-o", core, "-p", "(,gpt2)/boot/grub",
        "biosdisk", "part_gpt", "fat", "normal", "linux", "configfile")
    img = bytearray(open(core, "rb").read())
    if len(img) > MIB:
        sys.exit("mkimage: core.img does not fit the BIOS boot partition")
    lba = 2048
    struct.pack_into("<Q", img, 0x200 - 12, lba + 1)  # diskboot.img's blocklist: the rest of core.img
    mbr = bytearray(open(os.path.join(mods, "boot.img"), "rb").read()[:440])
    struct.pack_into("<Q", mbr, 0x5C, lba)  # GRUB_BOOT_MACHINE_KERNEL_SECTOR
    mbr[0x66:0x68] = b"\x90\x90"  # like grub-bios-setup on a hard disk: trust no BIOS boot drive number below 0x80
    with open(disk, "r+b") as f:
        f.write(mbr)
        f.seek(lba * 512)
        f.write(img)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("out")
    ap.add_argument("--kernel", required=True)
    ap.add_argument("--initrd", action="append", required=True)
    ap.add_argument("--cmdline", required=True)
    ap.add_argument("--boot", required=True)
    ap.add_argument("--conf")
    ap.add_argument("--uki")
    ap.add_argument("--version", default="dev")
    ap.add_argument("--esp-mib", type=int, default=256)
    ap.add_argument("--data-mib", type=int, default=256)
    a = ap.parse_args()

    # mke2fs takes a fake time of 0 for "unset", hence the 1980 floor of EPOCH.
    env = dict(os.environ, MTOOLS_SKIP_CHECK="1", E2FSPROGS_FAKE_TIME=str(EPOCH))
    with tempfile.TemporaryDirectory() as tmp:
        def put(name, data):
            p = os.path.join(tmp, name)
            with open(p, "wb") as f:
                f.write(data if isinstance(data, bytes) else data.encode())
            return p

        initrd = put("initrd", b"".join(open(p, "rb").read() for p in a.initrd))
        osrel = put("osrel", f'NAME="k10s"\nID=k10s\nVERSION_ID="{a.version}"\nPRETTY_NAME="k10s {a.version}"\n')
        efi = os.path.join(tmp, "k10s-a.efi")
        uki(os.path.join(a.boot, "usr/lib/systemd/boot/efi/linuxx64.efi.stub"),
            [(".osrel", osrel), (".cmdline", put("cmdline", a.cmdline)), (".initrd", initrd), (".linux", a.kernel)], efi)
        if a.uki:
            shutil.copyfile(efi, a.uki)

        esp = os.path.join(tmp, "esp.img")
        # 2 KiB clusters: GRUB reads files one cluster at a time, and BIOS disk reads are slow.
        run("mkfs.fat", "-F", "32", "-s", "4", "-n", "K10S-BOOT", "-i", ESP_VOLID, "-C", esp, str(a.esp_mib * 1024), stdout=subprocess.DEVNULL)
        files = [
            (os.path.join(a.boot, "usr/lib/systemd/boot/efi/systemd-bootx64.efi"), "EFI/BOOT/BOOTX64.EFI"),
            (efi, "EFI/Linux/k10s-a.efi"),
            (put("loader.conf", LOADER_CONF), "loader/loader.conf"),
            (put("grub.cfg", f"set timeout=0\nlinux /k10s/a/vmlinuz {a.cmdline}\ninitrd /k10s/a/initrd\nboot\n"), "boot/grub/grub.cfg"),
            (a.kernel, "k10s/a/vmlinuz"),
            (initrd, "k10s/a/initrd"),
            (put("tier", "auto\n"), "k10s/tier"),
            (a.conf or put("k10s.conf", CONF), "k10s.conf"),
        ]
        dirs = sorted({"/".join(d.split("/")[:n]) for _, d in files for n in range(1, d.count("/") + 1)})
        run("mmd", "-i", esp, *[f"::/{d}" for d in dirs], env=env)
        for src, dst in files:
            run("mcopy", "-i", esp, src, f"::/{dst}", env=env)
        fat_fix_times(esp, EPOCH)

        data = os.path.join(tmp, "data.img")
        run("mkfs.ext4", "-q", "-F", "-L", "K10S-DATA", "-U", DATA_UUID, "-E", f"hash_seed={DATA_SEED},root_owner=0:0",
            "-b", "4096", "-T", "default", "-J", "size=64", data, f"{a.data_mib}M", env=env, stdout=subprocess.DEVNULL)

        esp_start, data_start = 2 * MIB, (2 + a.esp_mib) * MIB
        total = data_start + (a.data_mib + 1) * MIB
        if os.path.exists(a.out):
            os.remove(a.out)
        with open(a.out, "wb") as f:
            f.truncate(total)
        layout = [f"label: gpt\nlabel-id: {DISK_GUID}\n"]
        for (name, ptype, uuid), (start, size) in zip(PARTS, [(MIB, MIB), (esp_start, a.esp_mib * MIB), (data_start, a.data_mib * MIB)]):
            layout.append(f'start={start // 512}, size={size // 512}, type={ptype}, name="{name}", uuid={uuid}\n')
        run("sfdisk", "-q", a.out, input="".join(layout).encode())
        copy_sparse(esp, a.out, esp_start)
        copy_sparse(data, a.out, data_start)
        grub_bios(a.boot, a.out, tmp)
        print(f"mkimage: {a.out}: {total >> 20} MiB (ESP {a.esp_mib} MiB, data {a.data_mib} MiB), "
              f"UKI {os.path.getsize(efi)} bytes", file=sys.stderr)


if __name__ == "__main__":
    main()
