#!/usr/bin/env bash
# Fetches the pinned boot loaders of the disk images (os/mkimage.py) and prints DIR, the packages extracted:
#   DIR/usr/lib/grub/i386-pc (GRUB modules, boot.img), DIR/usr/bin/grub-mkimage (BIOS), and
#   DIR/usr/lib/systemd/boot/efi/{systemd-bootx64.efi,linuxx64.efi.stub} (UEFI loader and the UKI stub).
# Debian 12 packages, each checked against its SHA-256 (deb.debian.org, else snapshot.debian.org).
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
cache=${K10S_OS_CACHE:-$here/../target/os}
# name  sha256  snapshot sha1  pool path
PKGS=(
    "grub-pc-bin_2.06-13+deb12u2_amd64.deb db6aced5a89f02e10e150140b4df9621196f311c26203baf8558b4c6808e1298 60659aaa4bba685bf505a6baea2e775d2c9436ba g/grub2"
    "grub-common_2.06-13+deb12u2_amd64.deb fa2a91e48bb85effb1d9e22e16d83774e6de0ae6167268e8150e76ae963ec671 8bab2583c19be89ffec50d3622acb4a7d00cdb05 g/grub2"
    "systemd-boot-efi_252.39-1~deb12u2_amd64.deb 8f2b81bdcfafc466882a0cae460272dbf10d8d97c6a3c1c6ea33038c155a2f5e d4654e8e612a2fe3a9f912e00c8dcb3af76a5307 s/systemd"
)
stamp=$(printf '%s\n' "${PKGS[@]}" | sha256sum | cut -c1-16)
dir=$cache/boot-$stamp
if [[ ! -f $dir/.done ]]; then
    mkdir -p "$cache"
    rm -rf "$dir" && mkdir -p "$dir"
    for p in "${PKGS[@]}"; do
        read -r name sha256 sha1 pool <<<"$p"
        deb=$cache/$name
        if [[ ! -f $deb ]]; then
            for u in "https://deb.debian.org/debian/pool/main/$pool/$name" "https://snapshot.debian.org/file/$sha1"; do
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
