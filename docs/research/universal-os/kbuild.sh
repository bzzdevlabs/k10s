#!/usr/bin/env bash
# Builds a k10s-oriented minimal kernel without root: kbuild.sh ARCH OUTDIR FRAGMENT... (ARCH = x86_64 | i386 | arm64)
set -euo pipefail
UOS=${UOS:-/home/qgerard/dev/k10s-research/universal-os}
SRC=$UOS/kernel/linux-6.18.57
arch=$1 out=$2; shift 2
export PATH=$UOS/sysroot/usr/bin:$PATH BISON_PKGDATADIR=$UOS/sysroot/usr/share/bison
# objtool needs libelf headers: a pkg-config shim pointing at the extracted libelf-dev
mkdir -p $UOS/tools && cat > $UOS/tools/pkg-config <<PC
#!/bin/sh
case "\$*" in *libelf*cflags*) echo "-I$UOS/sysroot/usr/include";; *libelf*libs*) echo "-L$UOS/sysroot/usr/lib/x86_64-linux-gnu -lelf";; *) exec /usr/bin/pkg-config "\$@";; esac
PC
chmod +x $UOS/tools/pkg-config
export HOSTPKG_CONFIG=$UOS/tools/pkg-config
extra=()
[ "$arch" = arm64 ] && extra=(CROSS_COMPILE=${CROSS_COMPILE:-aarch64-linux-gnu-})
mk() { nice -n 10 make -C "$SRC" O="$out" ARCH=$arch "${extra[@]}" HOSTCFLAGS="-I$UOS/sysroot/usr/include" HOSTLDFLAGS="-L$UOS/sysroot/usr/lib/x86_64-linux-gnu" "$@"; }
mkdir -p "$out"
mk tinyconfig >/dev/null
"$SRC/scripts/kconfig/merge_config.sh" -m -O "$out" "$out/.config" "$@" | tail -3
mk olddefconfig >/dev/null
# report fragment options that did not stick
for f in "$@"; do grep -E '^CONFIG_' "$f" | while IFS== read -r k v; do
  got=$(grep -E "^$k=|^# $k is not set" "$out/.config" | head -1 || true)
  case "$v" in n) [[ -z "$got" || "$got" == "# $k is not set" ]] || echo "kbuild: $k wanted n, got $got";;
               *) [[ "$got" == "$k=$v" ]] || echo "kbuild: $k wanted $v, got ${got:-unset}";; esac
done; done
case $arch in arm64) target=Image;; *) target=bzImage;; esac
t0=$(date +%s)
mk -j${JOBS:-6} $target 2>&1 | grep -E "error|Error|warning: unmet" | head -20 || true
echo "kbuild: $arch $target built in $(( $(date +%s) - t0 )) s (load $(cut -d' ' -f1-3 /proc/loadavg))"
