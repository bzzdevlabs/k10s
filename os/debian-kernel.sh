#!/usr/bin/env bash
# Fetches the pinned Debian 13 kernel (the Tier 1 bootstrap and fallback flavour) and prepares its module tree.
#   os/debian-kernel.sh   prints DIR; DIR/vmlinuz and DIR/lib/modules/VERSION (with modules.dep and modules.alias)
# The .deb is checked against its SHA-256; modules.dep/alias come from depmod, which the package leaves to postinst.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
VERSION=6.12.111+deb13-amd64
DEB=linux-image-${VERSION}_6.12.111-1_amd64.deb
SHA256=792e7a407f8f39fa0db3deeb6cda4e4bb56042963376133a072856b1c685d4f3
URLS=("https://deb.debian.org/debian/pool/main/l/linux-signed-amd64/$DEB"
      "https://snapshot.debian.org/file/e66b1c8998bfdb94943a2c08843215016b837a9a")
cache=${K10S_OS_CACHE:-$here/../target/os}
dir=$cache/debian-$VERSION
if [[ ! -f $dir/lib/modules/$VERSION/modules.alias ]]; then
    mkdir -p "$cache"
    deb=$cache/$DEB
    if [[ ! -f $deb ]]; then
        for u in "${URLS[@]}"; do
            curl -fsSL --retry 3 -o "$deb.part" "$u" && break
        done
        mv "$deb.part" "$deb"
    fi
    echo "$SHA256  $deb" | sha256sum -c --quiet
    rm -rf "$dir" && mkdir -p "$dir"
    dpkg-deb -x "$deb" "$dir"
    # Debian 13 is merged-/usr: the modules ship in usr/lib/modules.
    if [[ -d $dir/usr/lib/modules ]]; then
        mkdir -p "$dir/lib" && mv "$dir/usr/lib/modules" "$dir/lib/modules"
    fi
    cp "$dir/boot/vmlinuz-$VERSION" "$dir/vmlinuz"
    PATH=$PATH:/sbin:/usr/sbin depmod -b "$dir" "$VERSION"
fi
echo "$dir"
