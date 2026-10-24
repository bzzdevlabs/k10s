#!/usr/bin/env bash
# Downloads the pinned Firecracker and Cloud Hypervisor releases (static x86-64 binaries, no root needed) into
# DIR (default target/vmm) and checks their SHA-256 digests. Prints the directory.
set -euo pipefail
dir="${1:-target/vmm}"
mkdir -p "$dir"
FC=v1.17.0
FC_SHA256=06094a1108ae9e82aa4c23a775aa92758f53f1175d422270d9d6162cb9ade558
CH=v53.0
CH_SHA256=448af3d4e59b22c2987f7df94c213ad40fb53a10d437e42b5ee6c4fce7c29ecc
get() { # URL FILE SHA256
    [[ -f "$2" ]] && echo "$3  $2" | sha256sum -c --quiet - 2>/dev/null && return
    curl -fsSL --retry 3 -o "$2.part" "$1"
    echo "$3  $2.part" | sha256sum -c --quiet -
    mv "$2.part" "$2"
}
get "https://github.com/firecracker-microvm/firecracker/releases/download/$FC/firecracker-$FC-x86_64.tgz" "$dir/firecracker.tgz" "$FC_SHA256"
tar -xzf "$dir/firecracker.tgz" -C "$dir" "release-$FC-x86_64/firecracker-$FC-x86_64"
ln -sf "release-$FC-x86_64/firecracker-$FC-x86_64" "$dir/firecracker"
get "https://github.com/cloud-hypervisor/cloud-hypervisor/releases/download/$CH/cloud-hypervisor-static" "$dir/cloud-hypervisor" "$CH_SHA256"
chmod +x "$dir/cloud-hypervisor" "$dir/firecracker"
echo "$dir"
