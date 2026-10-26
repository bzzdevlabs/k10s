#!/usr/bin/env bash
# Reproduces the syscall evidence behind docs/LX.md: bench/oci/syscalls.json and bench/oci/syscalls.md.
#   bench/oci/trace.sh [NAME ...]    NAME from bench/oci/corpus.json (default: every image)
# Needs cargo with the x86_64-unknown-linux-musl target, qemu-system-x86_64 with KVM, curl, python3 and network
# access for the pinned images (cached under target/oci, ~550 MB) and the Debian kernel (os/debian-kernel.sh).
# No root, no Docker. About 3 minutes once the images are cached.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$root/target/oci-lxtrace}
(cd "$here/lxtrace" && cargo build --release --locked --target x86_64-unknown-linux-musl)
lxtrace=$CARGO_TARGET_DIR/x86_64-unknown-linux-musl/release/lxtrace
kernel=$("$root/os/debian-kernel.sh")/vmlinuz
python3 "$here/trace.py" --kernel "$kernel" --lxtrace "$lxtrace" "$@"
python3 "$here/table.py" collect
python3 "$here/table.py" render
python3 "$here/table.py" --check
