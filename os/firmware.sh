#!/usr/bin/env bash
# Builds the firmware archive of the metal images and prints its path: a zstd newc cpio of /lib/firmware with the
# files of os/firmware.list (each checked against its SHA-256) from the pinned linux-firmware tag, licences included.
# The archive is concatenated to the initramfs; the files keep their own licences (not k10s's Apache-2.0).
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
TAG=20261019
BASE=https://git.kernel.org/pub/scm/linux/kernel/git/firmware/linux-firmware.git/plain
cache=${K10S_OS_CACHE:-$here/../target/os}
stamp=$( (echo "$TAG"; cat "$here/firmware.list") | sha256sum | cut -c1-16)
out=$cache/firmware-$stamp.cpio.zst
if [[ ! -f $out ]]; then
    dir=$cache/firmware-$TAG
    while read -r sha256 path src; do
        [[ $sha256 == \#* ]] && continue
        f=$dir/$path
        if [[ ! -f $f ]] || ! echo "$sha256  $f" | sha256sum -c --quiet 2>/dev/null; then
            mkdir -p "$(dirname "$f")"
            curl -fsSL --retry 3 -o "$f.part" "$BASE/${src:-$path}?h=$TAG"
            echo "$sha256  $f.part" | sha256sum -c --quiet
            mv "$f.part" "$f"
        fi
    done <"$here/firmware.list"
    files=()
    while read -r sha256 path _; do
        [[ $sha256 == \#* ]] || files+=(--fw "$path")
    done <"$here/firmware.list"
    python3 "$here/mkinitramfs.py" "$cache/firmware-part.cpio.zst" --firmware "$dir" "${files[@]}"
    mv "$cache/firmware-part.cpio.zst" "$out"
fi
echo "$out"
