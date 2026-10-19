#!/usr/bin/env bash
# Rootless QEMU 10 (x86 + aarch64) and AAVMF from Debian bookworm-backports, as used by the native-hardware research.
# Needs the bookworm-backports apt source (present on the dev machine); no root: apt-get download + dpkg -x.
#   DEST=/path ./get-qemu10.sh      (default ../../../../k10s-research/native-hardware/tools/qemu10)
# Run the binaries with QEMU_MODULE_DIR pointing at the extracted modules (otherwise QEMU 10 tries the system's
# 7.2 modules and aborts), and with `-display none -audio none` (no GUI/audio modules were extracted):
#   QEMU_MODULE_DIR=$DEST/usr/lib/x86_64-linux-gnu/qemu $DEST/usr/bin/qemu-system-aarch64 -L $DEST/usr/share/qemu ...
set -euo pipefail
DEST="${DEST:-$(cd "$(dirname "$0")" && pwd)/../../../../k10s-research/native-hardware/tools/qemu10}"
work="$(mktemp -d "${TMPDIR:-$HOME}/qemu10-debs.XXXXXX")"
trap 'rm -rf "$work"' EXIT
cd "$work"
apt-get download qemu-system-x86/bookworm-backports qemu-system-arm/bookworm-backports \
    qemu-system-common/bookworm-backports qemu-system-data/bookworm-backports qemu-efi-aarch64
mkdir -p "$DEST"
for f in *.deb; do dpkg -x "$f" "$DEST"; done
rm -f "$DEST"/usr/share/AAVMF/*.ms.fd "$DEST"/usr/share/AAVMF/*snakeoil*   # keep the plain AAVMF code/vars
"$DEST/usr/bin/qemu-system-aarch64" --version | head -1
du -sh "$DEST"
