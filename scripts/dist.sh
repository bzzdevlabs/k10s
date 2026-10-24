#!/usr/bin/env bash
# Builds all release archives into dist/ (locally and in CI), versioned once by scripts/version.sh.
# usage: dist.sh [--dev|--stable] [--only k10s,musl,arm64,i686,armv7,examples,metal,bench,src]
# env: DIST, K10S_BUILD_VERSION/_COMMIT, GLIBC_MAX (highest glibc symbol version the gnu k10s may need, 2.34)
# Needs cargo (targets x86_64-unknown-linux-musl, x86_64-unknown-none, aarch64-unknown-linux-musl,
# i686-unknown-linux-musl, armv7-unknown-linux-musleabihf), go, git, binutils, GNU tar; on other hosts than arm64,
# aarch64-linux-gnu-strip and qemu-aarch64 for the arm64 build, arm-linux-gnueabihf-strip and qemu-arm for armv7 (both
# linked by rust-lld; i686 too, which an x86-64 host runs).
# Output (V = version without +build): k10s-V-x86_64-unknown-linux-{gnu,musl}.tar.gz,
#   k10s-V-aarch64-unknown-linux-musl.tar.gz, k10s-V-i686-unknown-linux-musl.tar.gz,
#   k10s-V-armv7-unknown-linux-musleabihf.tar.gz, k10s-examples-V.tar.gz, k10s-metal-V-x86_64.tar.gz,
#   k10s-bench-V-linux-amd64.tar.gz, k10s-V-src.tar.gz, SHA256SUMS (see README.md).
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT=$PWD

mode=()
parts=(k10s musl arm64 i686 armv7 examples metal bench src)
while (($#)); do
    case $1 in
    --dev | --stable) mode=("$1") ;;
    --only)
        IFS=, read -r -a parts <<<"${2:?--only needs a list}"
        shift
        ;;
    -h | --help)
        sed -n '2,/^set -euo/{/^set -euo/d;s/^# \{0,1\}//;p}' "$0"
        exit 0
        ;;
    *)
        echo "dist.sh: unknown option '$1'" >&2
        exit 2
        ;;
    esac
    shift
done
want() { [[ " ${parts[*]} " == *" $1 "* ]]; }

# One version for every build below (cargo build scripts and the Go build read these).
envs=$(scripts/version.sh "${mode[@]}" --strict --env)
while IFS='=' read -r k v; do export "$k=$v"; done <<<"$envs"
V=$K10S_BUILD_VERSION
echo "== k10s $K10S_VERSION_FULL ($K10S_CHANNEL)"

DIST=${DIST:-$ROOT/dist}
mkdir -p "$DIST"
DIST=$(cd "$DIST" && pwd)
STAGE=$DIST/.stage
rm -rf "$STAGE"
rm -f "$DIST"/*.tar.gz "$DIST/SHA256SUMS"
mkdir -p "$STAGE"
# Reproducible archives: fixed owner, order and mtime (commit time).
EPOCH=$(git log -1 --format=%ct 2>/dev/null || date +%s)
pack() { # STAGE/<name> -> DIST/<name>.tar.gz
    tar -C "$STAGE" --sort=name --owner=0 --group=0 --numeric-owner --mtime="@$EPOCH" -cf - "$1" | gzip -9n >"$DIST/$1.tar.gz"
}
check_version() { # <binary and args> <expected line>
    local got
    got=$("${@:1:$#-1}")
    [[ $got == "${!#}" ]] || {
        echo "dist.sh: '${*:1:$#-1}' says '$got', expected '${!#}'" >&2
        exit 1
    }
    echo "   $got"
}
DOCS=(LICENSE NOTICE README.md SECURITY.md)
GLIBC_MAX=${GLIBC_MAX:-2.34}

# k10s for one target triple.
build_k10s() {
    local triple=$1 name="k10s-$V-$1" strip=strip emu=()
    echo "== k10s ($triple)"
    # Cross builds: rust-lld links the target's self-contained musl; QEMU runs the version check.
    if [[ $triple == aarch64-* && $(uname -m) != aarch64 ]]; then
        export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld
        strip=aarch64-linux-gnu-strip
        emu=(qemu-aarch64)
    elif [[ $triple == i686-* ]]; then
        export CARGO_TARGET_I686_UNKNOWN_LINUX_MUSL_LINKER=rust-lld
        [[ $(uname -m) == x86_64 ]] || { strip=i686-linux-gnu-strip; emu=(qemu-i386); }
    elif [[ $triple == armv7-* ]]; then
        export CARGO_TARGET_ARMV7_UNKNOWN_LINUX_MUSLEABIHF_LINKER=rust-lld
        strip=arm-linux-gnueabihf-strip
        emu=(qemu-arm)
    fi
    cargo build --release --locked -p k10s --target "$triple"
    mkdir -p "$STAGE/$name"
    "$strip" --strip-debug -o "$STAGE/$name/k10s" "target/$triple/release/k10s"
    cp "${DOCS[@]}" "$STAGE/$name/"
    check_version "${emu[@]}" "$STAGE/$name/k10s" --version "k10s $K10S_VERSION_FULL"
    if [[ $triple == *-gnu ]]; then
        local glibc
        glibc=$(objdump -T "$STAGE/$name/k10s" | grep -o 'GLIBC_[0-9.]*' | sed 's/GLIBC_//' | sort -uV | tail -1)
        echo "   needs glibc >= $glibc"
        if [[ $(printf '%s\n' "$glibc" "$GLIBC_MAX" | sort -V | tail -1) != "$GLIBC_MAX" ]]; then
            echo "dist.sh: the gnu k10s needs glibc $glibc > GLIBC_MAX=$GLIBC_MAX (README.md promises $GLIBC_MAX)" >&2
            exit 1
        fi
    fi
    pack "$name"
}
if want k10s; then build_k10s x86_64-unknown-linux-gnu; fi
static() {
    if readelf -lW "$STAGE/k10s-$V-$1/k10s" | grep -q 'program interpreter'; then
        echo "dist.sh: the $1 k10s is not static" >&2
        exit 1
    fi
}
if want musl; then
    build_k10s x86_64-unknown-linux-musl
    static x86_64-unknown-linux-musl
fi
if want arm64; then
    build_k10s aarch64-unknown-linux-musl
    static aarch64-unknown-linux-musl
fi
if want i686; then
    build_k10s i686-unknown-linux-musl
    static i686-unknown-linux-musl
fi
if want armv7; then
    build_k10s armv7-unknown-linux-musleabihf
    static armv7-unknown-linux-musleabihf
fi

if want examples; then
    echo "== examples"
    name=k10s-examples-$V
    mkdir -p "$STAGE/$name"
    bin=target/x86_64-unknown-linux-gnu/release/k10s
    [[ -x $bin ]] || cargo build --release --locked -p k10s --target x86_64-unknown-linux-gnu
    for f in examples/*.ten; do
        b=$(basename "$f" .ten)
        cp "$f" "$STAGE/$name/"
        "$bin" build "$f" -o "$STAGE/$name/$b.k10" >/dev/null
    done
    cp examples/app.k10s LICENSE NOTICE "$STAGE/$name/"
    pack "$name"
fi

if want metal; then
    echo "== metal"
    name=k10s-metal-$V-x86_64
    # metal/build.sh expects metal/target: no foreign CARGO_TARGET_DIR.
    env -u CARGO_TARGET_DIR EMBED=1 metal/build.sh
    (cd metal && env -u CARGO_TARGET_DIR cargo build --release --locked -p mkbundle --target x86_64-unknown-linux-musl)
    mkdir -p "$STAGE/$name/examples"
    cp metal/out/k10-metal.elf metal/out/k10-metal-embedded.elf metal/out/bundle.k10b "$STAGE/$name/"
    strip --strip-debug -o "$STAGE/$name/mkbundle" metal/target/x86_64-unknown-linux-musl/release/mkbundle
    cp examples/*.ten "$STAGE/$name/examples/"
    cp metal/README.md LICENSE NOTICE "$STAGE/$name/"
    check_version "$STAGE/$name/mkbundle" --version "mkbundle $K10S_VERSION_FULL"
    for k in k10-metal.elf k10-metal-embedded.elf; do
        grep -qaF "$K10S_VERSION_FULL" "$STAGE/$name/$k" || { echo "dist.sh: $k does not embed $K10S_VERSION_FULL" >&2; exit 1; }
    done
    pack "$name"
fi

if want bench; then
    echo "== bench (Go)"
    name=k10s-bench-$V-linux-amd64
    NO_DOCKER=1 bench/workloads/build.sh
    mkdir -p "$STAGE/$name"
    for t in echo compute chain counter compute-purego loadgen; do cp "bench/bin/$t" "$STAGE/$name/"; done
    cp LICENSE NOTICE "$STAGE/$name/"
    check_version "$STAGE/$name/loadgen" -version "loadgen $K10S_VERSION_FULL"
    check_version "$STAGE/$name/echo" -version "echo $K10S_VERSION_FULL"
    pack "$name"
fi

if want src; then
    echo "== sources"
    name=k10s-$V-src
    # The archive holds HEAD, not local changes: no `.dirty` in its stamp.
    git archive --format=tar --prefix="$name/" HEAD >"$STAGE/src.tar"
    mkdir -p "$STAGE/stamp/$name"
    printf 'K10S_BUILD_VERSION=%s\nK10S_BUILD_COMMIT=%s\n' "$V" "${K10S_BUILD_COMMIT%.dirty}" >"$STAGE/stamp/$name/.k10s-version"
    tar -C "$STAGE/stamp" --owner=0 --group=0 --numeric-owner --mtime="@$EPOCH" -rf "$STAGE/src.tar" "$name/.k10s-version"
    gzip -9n <"$STAGE/src.tar" >"$DIST/$name.tar.gz"
fi

rm -rf "$STAGE"
(cd "$DIST" && sha256sum -- *.tar.gz >SHA256SUMS)
echo "== $DIST"
(cd "$DIST" && ls -l -- *.tar.gz SHA256SUMS | awk '{printf "  %10d  %s\n", $5, $9}')
