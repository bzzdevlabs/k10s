#!/usr/bin/env bash
# Strips the per-target k10s binaries (LLVM objcopy from the Rust toolchain handles every arch) and prints sizes.
cd "$(dirname "$0")/.."
SR=$(rustc --print sysroot)
for t in x86_64-unknown-linux-musl i686-unknown-linux-musl aarch64-unknown-linux-musl armv7-unknown-linux-musleabihf riscv64gc-unknown-linux-musl; do
  a=${t%%-*}
  LD_LIBRARY_PATH=$SR/lib $SR/lib/rustlib/x86_64-unknown-linux-gnu/bin/rust-objcopy --strip-all target-pid1/$t/release/k10s bin/k10s-pid1-$a
  s=$(stat -c %s bin/k10s-pid1-$a); x=$(xz -9 -c bin/k10s-pid1-$a | wc -c); z=$(zstd -19 -q -c bin/k10s-pid1-$a | wc -c)
  printf "%-32s %9d B stripped, zstd-19 %8d, xz %8d\n" $t $s $z $x
done
