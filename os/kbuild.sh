#!/usr/bin/env bash
# Builds a k10s Linux kernel (Tier 1 driver layer) without root: tinyconfig + os/kconfig fragments.
#   os/kbuild.sh [vm|metal] [OUT_DIR]   prints OUT_DIR (default: target/os/kernel-x86_64-PROFILE), which holds
#   arch/x86/boot/bzImage and, for metal, the signed module tree in modroot/lib/modules/VERSION (with depmod output).
# vm: QEMU/KVM guests, every driver built in. metal: real machines, NIC/USB-network/watchdog drivers as modules signed
# with a per-build RSA key that is deleted after `modules_install` (K10S_MODULE_KEY=PEM signs with a kept key+cert
# instead, which makes the build reproducible). Needs make, gcc, flex, bison, bc, openssl, zstd, depmod and the
# libelf and libssl headers. The pinned kernel.org tarball is downloaded once and checked against its SHA-256; a
# build whose version and fragments are unchanged is reused.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
VERSION=6.18.57
SHA256=5bd9acdc82ec5efc3790ae840e5aaa40dfde7efff261cf6dde902dc570acf98c
profile=${1:-vm}
[[ $profile == vm || $profile == metal ]] || { echo "kbuild: unknown profile $profile" >&2; exit 2; }
cache=${K10S_OS_CACHE:-$here/../target/os}
out=${2:-$cache/kernel-x86_64-$profile}
frags=("$here/kconfig/common.config" "$here/kconfig/x86-$profile.config" "$here/kconfig/x86_64.config")
image=$out/arch/x86/boot/bzImage
stamp=$( (echo "$VERSION"; cat "${frags[@]}"; if [[ -n ${K10S_MODULE_KEY:-} ]]; then sha256sum <"$K10S_MODULE_KEY"; fi) | sha256sum | cut -c1-16)
if [[ -f $image && $(cat "$out/.k10s-stamp" 2>/dev/null) == "$stamp" ]]; then
    echo "$out"
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
export PATH=$PATH:/sbin:/usr/sbin
mk() { make -C "$src" O="$out" ARCH=x86_64 "$@"; }
rm -f "$out/.k10s-stamp"
mkdir -p "$out"
extra=()
if [[ $profile == metal ]]; then
    key=${K10S_MODULE_KEY:-}
    if [[ -z $key ]]; then
        rm -rf "$out/signing" && (umask 077 && mkdir "$out/signing")
        key=$out/signing/key.pem
        cat >"$out/signing/x509.genkey" <<'EOF'
[ req ]
default_bits = 4096
distinguished_name = req_distinguished_name
prompt = no
string_mask = utf8only
x509_extensions = myexts
[ req_distinguished_name ]
O = k10s
CN = k10s kernel module signing key (per build)
[ myexts ]
basicConstraints=critical,CA:FALSE
keyUsage=digitalSignature
subjectKeyIdentifier=hash
authorityKeyIdentifier=keyid
EOF
        openssl req -new -nodes -utf8 -sha512 -days 36500 -batch -x509 -config "$out/signing/x509.genkey" \
            -outform PEM -out "$key" -keyout "$key" 2>/dev/null
    fi
    echo "CONFIG_MODULE_SIG_KEY=\"$(realpath "$key")\"" >"$out/k10s-signing.config"
    extra=("$out/k10s-signing.config")
fi
mk -s tinyconfig >/dev/null
"$src/scripts/kconfig/merge_config.sh" -m -O "$out" "$out/.config" "${frags[@]}" "${extra[@]}" >/dev/null
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
if [[ $profile == metal ]]; then
    mk -s -j"$(nproc)" bzImage modules >&2
    rm -rf "$out/modroot"
    mk -s INSTALL_MOD_PATH="$out/modroot" INSTALL_MOD_STRIP=1 modules_install >&2
    rm -f "$out"/modroot/lib/modules/*/build
    # Without the per-build key no other module can ever be signed for this kernel.
    rm -rf "$out/signing"
else
    mk -s -j"$(nproc)" bzImage >&2
fi
echo "$stamp" >"$out/.k10s-stamp"
echo "$out"
