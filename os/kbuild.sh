#!/usr/bin/env bash
# Builds the k10s Linux kernel (Tier 1 driver layer, `vm` profile) without root: tinyconfig + os/kconfig fragments.
#   os/kbuild.sh [OUT_DIR]   prints the bzImage path (default OUT_DIR: target/os/kernel-x86_64-vm)
# Needs make, gcc, flex, bison, bc and the libelf headers (objtool). The pinned kernel.org tarball is downloaded once
# and checked against its SHA-256; a build whose version and fragments are unchanged is reused.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
VERSION=6.18.57
SHA256=5bd9acdc82ec5efc3790ae840e5aaa40dfde7efff261cf6dde902dc570acf98c
cache=${K10S_OS_CACHE:-$here/../target/os}
out=${1:-$cache/kernel-x86_64-vm}
frags=("$here/kconfig/common.config" "$here/kconfig/x86-vm.config" "$here/kconfig/x86_64.config")
image=$out/arch/x86/boot/bzImage
stamp=$( (echo "$VERSION"; cat "${frags[@]}") | sha256sum | cut -c1-16)
if [[ -f $image && $(cat "$out/.k10s-stamp" 2>/dev/null) == "$stamp" ]]; then
    echo "$image"
    exit 0
fi
src=$cache/linux-$VERSION
if [[ ! -f $src/Makefile ]]; then
    tar=$cache/linux-$VERSION.tar.xz
    mkdir -p "$cache"
    if [[ ! -f $tar ]]; then
        curl -fsSL --retry 3 -o "$tar.part" "https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-$VERSION.tar.xz"
        mv "$tar.part" "$tar"
    fi
    echo "$SHA256  $tar" | sha256sum -c --quiet
    tar -C "$cache" -xf "$tar"
fi
# Reproducible build metadata.
export KBUILD_BUILD_TIMESTAMP="@${SOURCE_DATE_EPOCH:-0}" KBUILD_BUILD_USER=k10s KBUILD_BUILD_HOST=k10s
mk() { make -C "$src" O="$out" ARCH=x86_64 "$@"; }
mkdir -p "$out"
mk -s tinyconfig >/dev/null
"$src/scripts/kconfig/merge_config.sh" -m -O "$out" "$out/.config" "${frags[@]}" >/dev/null
mk -s olddefconfig >/dev/null
# Every fragment option must survive olddefconfig (a missing dependency silently drops it).
bad=0
for f in "${frags[@]}"; do
    while IFS='=' read -r k v; do
        got=$(grep -E "^$k=|^# $k is not set" "$out/.config" | head -1 || true)
        if [[ $v == n ]]; then
            [[ -z $got || $got == "# $k is not set" ]] && continue
        else
            [[ $got == "$k=$v" ]] && continue
        fi
        echo "kbuild: $k: wanted $v, got ${got:-unset}" >&2
        bad=1
    done < <(grep -E '^CONFIG_' "$f")
done
[[ $bad == 0 ]] || exit 1
mk -s -j"$(nproc)" bzImage >&2
echo "$stamp" >"$out/.k10s-stamp"
echo "$image"
