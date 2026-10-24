#!/usr/bin/env bash
# Fetches the pinned boot files of the disk images (os/mkimage.py) and prints DIR, the packages extracted:
#   os/bootfiles.sh [x86_64]   DIR/usr/lib/grub/i386-pc (GRUB modules, boot.img), DIR/usr/bin/grub-mkimage (BIOS),
#                              DIR/usr/lib/systemd/boot/efi/{systemd-bootx64.efi,linuxx64.efi.stub} (UEFI loader, UKI stub)
#   os/bootfiles.sh aarch64    DIR/usr/lib/systemd/boot/efi/{systemd-bootaa64.efi,linuxaa64.efi.stub} and the Raspberry
#                              Pi firmware DIR/usr/lib/raspi-firmware (start4cd.elf, fixup4cd.dat; licence in
#                              DIR/usr/share/doc/raspi-firmware/copyright)
# Debian packages, each checked against its SHA-256 (deb.debian.org, else snapshot.debian.org).
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
cache=${K10S_OS_CACHE:-$here/../target/os}
# name  sha256  snapshot sha1  pool path
case ${1:-x86_64} in
x86_64)
    PKGS=(
        "grub-pc-bin_2.06-13+deb12u2_amd64.deb db6aced5a89f02e10e150140b4df9621196f311c26203baf8558b4c6808e1298 60659aaa4bba685bf505a6baea2e775d2c9436ba main/g/grub2"
        "grub-common_2.06-13+deb12u2_amd64.deb fa2a91e48bb85effb1d9e22e16d83774e6de0ae6167268e8150e76ae963ec671 8bab2583c19be89ffec50d3622acb4a7d00cdb05 main/g/grub2"
        "systemd-boot-efi_252.39-1~deb12u2_amd64.deb 8f2b81bdcfafc466882a0cae460272dbf10d8d97c6a3c1c6ea33038c155a2f5e d4654e8e612a2fe3a9f912e00c8dcb3af76a5307 main/s/systemd"
    )
    ;;
aarch64)
    PKGS=(
        "systemd-boot-efi_252.39-1~deb12u2_arm64.deb d75fbbfa90d37d409e7e65f1149afd770bc17daa3f15f49ef651f608fe7e1dbc fa9986d79069ff389dfe9219d334605ab9b662be main/s/systemd"
        "raspi-firmware_1.20260408+ds-1_all.deb 9e215abb588e9adadf69a67107736fc2fa108ef3f17fa93589919c5e66ffe209 c0af61df71c0b3d1d7c411dfe24c5f7355e4db3e non-free-firmware/r/raspi-firmware"
    )
    ;;
*)
    echo "bootfiles: unknown arch $1" >&2
    exit 2
    ;;
esac
stamp=$(printf '%s\n' "${PKGS[@]}" | sha256sum | cut -c1-16)
dir=$cache/boot-$stamp
if [[ ! -f $dir/.done ]]; then
    mkdir -p "$cache"
    rm -rf "$dir" && mkdir -p "$dir"
    for p in "${PKGS[@]}"; do
        read -r name sha256 sha1 pool <<<"$p"
        deb=$cache/$name
        if [[ ! -f $deb ]]; then
            for u in "https://deb.debian.org/debian/pool/$pool/$name" "https://snapshot.debian.org/file/$sha1"; do
                curl -fsSL --retry 3 -o "$deb.part" "$u" && break
            done
            mv "$deb.part" "$deb"
        fi
        echo "$sha256  $deb" | sha256sum -c --quiet
        dpkg-deb -x "$deb" "$dir"
    done
    touch "$dir/.done"
fi
echo "$dir"
