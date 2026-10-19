#!/usr/bin/env bash
# Builds the kernel image, the host tool and the default test bundle into metal/out/.
#   out/k10-metal.elf          kernel (PVH-bootable ELF64: qemu-system-x86_64 -kernel out/k10-metal.elf)
#   out/bundle.k10b            all test services (pass with -initrd, or POST to /_k10s/apply)
#   out/k10-metal-embedded.elf kernel with out/bundle.k10b baked in (EMBED=1)
set -euo pipefail
cd "$(dirname "$0")"

cargo build --release -p mkbundle
cargo build --release -p k10-metal --target x86_64-unknown-none
mkdir -p out
cp target/x86_64-unknown-none/release/k10-metal out/k10-metal.elf
target/release/mkbundle -o out/bundle.k10b

if [[ "${EMBED:-0}" == 1 ]]; then
    # Separate target dir so the two kernel variants do not keep invalidating each other.
    K10_EMBED_BUNDLE="$PWD/out/bundle.k10b" CARGO_TARGET_DIR=target/embedded \
        cargo build --release -p k10-metal --target x86_64-unknown-none
    cp target/embedded/x86_64-unknown-none/release/k10-metal out/k10-metal-embedded.elf
fi

size() { stat -c %s "$1"; }
echo "out/k10-metal.elf: $(size out/k10-metal.elf) bytes (ELF file); loaded image:"
readelf -lW out/k10-metal.elf | awk '/LOAD/ {f+=strtonum($5); m+=strtonum($6)} END {printf "  file-backed %d bytes, in memory %d bytes (incl. .bss)\n", f, m}'
echo "out/bundle.k10b: $(size out/bundle.k10b) bytes"
