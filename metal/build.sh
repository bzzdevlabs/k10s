#!/usr/bin/env bash
# Builds the kernel image, its UEFI loader, the host tool and the default test bundle into metal/out/.
#   out/k10-metal.elf          kernel (PVH-bootable ELF64: qemu-system-x86_64 -kernel out/k10-metal.elf; Multiboot2)
#   out/k10boot.efi            UEFI loader and tier selector (metal/boot; README.md "Booting")
#   out/bundle.k10b            all test services (pass with -initrd, or POST to /_k10s/apply)
#   out/k10-metal-embedded.elf kernel with out/bundle.k10b baked in (EMBED=1)
# CARGO_TARGET_DIR (default metal/target) is honoured.
set -euo pipefail
cd "$(dirname "$0")"
T=${CARGO_TARGET_DIR:-target}

T="${CARGO_TARGET_DIR:-target}"
cargo build --release -p mkbundle
cargo build --release -p k10-metal --target x86_64-unknown-none
cargo build --release -p k10boot --target x86_64-unknown-uefi
mkdir -p out
cp "$T/x86_64-unknown-none/release/k10-metal" out/k10-metal.elf
cp "$T/x86_64-unknown-uefi/release/k10boot.efi" out/k10boot.efi
"$T/release/mkbundle" -o out/bundle.k10b

if [[ "${EMBED:-0}" == 1 ]]; then
    # Separate target dir so the two kernel variants do not keep invalidating each other.
    K10_EMBED_BUNDLE="$PWD/out/bundle.k10b" CARGO_TARGET_DIR="$T/embedded" \
        cargo build --release -p k10-metal --target x86_64-unknown-none
    cp "$T/embedded/x86_64-unknown-none/release/k10-metal" out/k10-metal-embedded.elf
fi

size() { stat -c %s "$1"; }
echo "out/k10-metal.elf: $(size out/k10-metal.elf) bytes (ELF file); loaded image:"
readelf -lW out/k10-metal.elf | awk '/LOAD/ {print $5, $6}' | {
    f=0 m=0
    while read -r a b; do f=$((f + a)) m=$((m + b)); done
    echo "  file-backed $f bytes, in memory $m bytes (incl. .bss)"
}
echo "out/k10boot.efi: $(size out/k10boot.efi) bytes"
echo "out/bundle.k10b: $(size out/bundle.k10b) bytes"
