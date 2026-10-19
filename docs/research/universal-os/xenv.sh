# Source me: cross-build/test environment for k10s (no root, no system cross toolchains).
# Layout: copy docs/research/universal-os/ to $UOS/scripts (default UOS=/home/qgerard/dev/k10s-research/universal-os);
# the other scripts expect $UOS/{bin,img,kernel,sysroot,wt} as built by the steps in docs/research/universal-os.md.
# Linker: rust-lld (ships with rustup) + the musl crt objects/libc.a that rustup ships per musl target.
# Runner: qemu-user-static binaries extracted from the Debian package (no binfmt needed); i686 runs natively.
UOS=${UOS:-/home/qgerard/dev/k10s-research/universal-os}
LLD=$(rustc --print sysroot)/lib/rustlib/x86_64-unknown-linux-gnu/bin/rust-lld
Q=$UOS/sysroot/usr/bin
for t in AARCH64_UNKNOWN_LINUX_MUSL ARMV7_UNKNOWN_LINUX_MUSLEABIHF I686_UNKNOWN_LINUX_MUSL RISCV64GC_UNKNOWN_LINUX_MUSL; do
  export CARGO_TARGET_${t}_LINKER=$LLD
done
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_RUNNER=$Q/qemu-aarch64-static
export CARGO_TARGET_ARMV7_UNKNOWN_LINUX_MUSLEABIHF_RUNNER=$Q/qemu-arm-static
export CARGO_TARGET_RISCV64GC_UNKNOWN_LINUX_MUSL_RUNNER=$Q/qemu-riscv64-static
export CARGO_TARGET_DIR=$UOS/target-cross
export CARGO_TARGET_RISCV64GC_UNKNOWN_LINUX_MUSL_RUSTFLAGS="-C target-feature=+crt-static"
