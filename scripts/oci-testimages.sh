#!/usr/bin/env bash
# Builds the OCI test images from sources we control, without Docker (docs/research/oci.md §14 P0.2, §17):
#   k10s-test/lxprobe:v1          static musl lxprobe (bench/oci/lxprobe), the syscall probes of docs/LX.md
#   k10s-test/lxprobe-dynamic:v1  the same program linked dynamically, with musl's loader in /lib
#   k10s-test/shell:v1            busybox (static, built here) running an /entrypoint.sh that execs httpd on :8080
#   k10s-test/echo:v1             bench/workloads echo, CGO_ENABLED=0, linux/amd64 and linux/arm64 (an image index)
#   k10s-test/{compute,chain,counter}:v1   the other bench workloads (amd64)
# Without Go, echo:v1 is bench/oci/lxprobe/echo.c (static musl, amd64) and the other workloads are skipped.
# Output (target/oci/testimages): layout/ (one OCI layout, gzip layers), archives/NAME.tar (`docker save` form).
# musl 1.2.5 and busybox 1.37.0 are fetched once (pinned SHA-256) and built with the host's gcc. The script then
# runs both lxprobe builds on this host (no failure, identical output) and pulls every image through
# os/tools/registry.py. Deterministic: rebuilding gives the same digests for the same compilers.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
out=${K10S_TESTIMAGES:-$root/target/oci/testimages}
src=$out/src build=$out/build
MUSL=musl-1.2.5 MUSL_SHA256=a9a118bbe84d8764da0ea0d28b3ab3fae8477fc7e4085d90102b8596fc7c75e4
BUSYBOX=busybox-1.37.0 BUSYBOX_SHA256=3311dff32e746499f4df0d5df04d7eb396382d7e108bb9250e7b519b837043a4
APPLETS="sh ash echo printf test [ true false cat ls mkdir rm rmdir cp mv ln touch chmod sed grep egrep awk sort head
tail wc date sleep env id seq tr cut uname basename dirname find xargs ps kill hostname nproc md5sum sha256sum tar gzip
gunzip httpd wget nc mktemp stat yes expr uptime free df"
mkdir -p "$src" "$build"

fetch() { # URL SHA256
    local f=$src/${1##*/}
    if [[ ! -f $f ]]; then
        curl -fsSL --retry 3 -o "$f.part" "$1"
        mv "$f.part" "$f"
    fi
    echo "$2  $f" | sha256sum -c --quiet
}

fetch "https://musl.libc.org/releases/$MUSL.tar.gz" $MUSL_SHA256
fetch "https://busybox.net/downloads/$BUSYBOX.tar.bz2" $BUSYBOX_SHA256

musl=$build/musl
if [[ ! -x $musl/bin/musl-gcc ]]; then
    rm -rf "$build/$MUSL" && tar -C "$build" -xzf "$src/$MUSL.tar.gz"
    (cd "$build/$MUSL" && ./configure --prefix="$musl" --syslibdir="$musl/lib" &&
        make -j"$(nproc)" && make install) >"$build/musl.log" 2>&1 || { tail -20 "$build/musl.log" >&2; exit 1; }
fi
CC=$musl/bin/musl-gcc
CFLAGS=(-std=c11 -O2 -Wall -Wextra -Werror)

bb=$build/$BUSYBOX
if [[ ! -x $bb/busybox ]]; then
    rm -rf "$bb" && tar -C "$build" -xjf "$src/$BUSYBOX.tar.bz2"
    # Kernel UAPI headers for the few applets that include <linux/...>; musl-gcc hides the host's include dir.
    mkdir -p "$build/uapi"
    ln -sfn /usr/include/linux "$build/uapi/linux"
    ln -sfn "/usr/include/$(gcc -print-multiarch)/asm" "$build/uapi/asm"
    ln -sfn /usr/include/asm-generic "$build/uapi/asm-generic"
    (cd "$bb" && make allnoconfig >/dev/null &&
        for o in STATIC ASH SH_IS_ASH ASH_ECHO ASH_PRINTF ASH_TEST ASH_GETOPTS ASH_ALIAS ASH_BASH_COMPAT ASH_CMDCMD \
                 ASH_JOB_CONTROL ASH_EXPAND_PRMT FEATURE_SH_MATH ECHO PRINTF TEST TEST1 TEST2 TRUE FALSE CAT LS MKDIR \
                 RM RMDIR CP MV LN TOUCH CHMOD SED GREP EGREP AWK SORT HEAD TAIL WC DATE SLEEP ENV ID SEQ TR CUT UNAME \
                 BASENAME DIRNAME FIND XARGS PS KILL HOSTNAME NPROC MD5SUM SHA256SUM TAR GZIP GUNZIP HTTPD WGET NC \
                 MKTEMP STAT YES EXPR UPTIME FREE DF; do
            sed -i "s/^# CONFIG_$o is not set/CONFIG_$o=y/" .config
        done && { yes "" || true; } | make oldconfig >/dev/null 2>&1 &&
        make -j"$(nproc)" CC="$CC -isystem $build/uapi" HOSTCC=gcc >"$build/busybox.log" 2>&1) ||
        { tail -20 "$build/busybox.log" >&2; exit 1; }
fi

bin=$build/bin
mkdir -p "$bin/static" "$bin/dynamic" "$bin/host-dynamic" "$bin/arm64"
probe=$root/bench/oci/lxprobe
"$CC" "${CFLAGS[@]}" -static -o "$bin/static/lxprobe" "$probe/lxprobe.c"
"$CC" "${CFLAGS[@]}" -Wl,--dynamic-linker=/lib/ld-musl-x86_64.so.1 -o "$bin/dynamic/lxprobe" "$probe/lxprobe.c"
"$CC" "${CFLAGS[@]}" -Wl,--dynamic-linker="$musl/lib/ld-musl-x86_64.so.1" -o "$bin/host-dynamic/lxprobe" \
    "$probe/lxprobe.c"
strip "$bin/static/lxprobe" "$bin/dynamic/lxprobe"

# The same probes, static and dynamic, on this host: no failure and the same output.
"$bin/static/lxprobe" >"$build/lxprobe-static.out" || { cat "$build/lxprobe-static.out" >&2; exit 1; }
"$bin/host-dynamic/lxprobe" >"$build/lxprobe-dynamic.out" || { cat "$build/lxprobe-dynamic.out" >&2; exit 1; }
cmp -s "$build/lxprobe-static.out" "$build/lxprobe-dynamic.out" ||
    { diff "$build/lxprobe-static.out" "$build/lxprobe-dynamic.out" >&2; exit 1; }
echo "lxprobe: $(grep -c '^ok' "$build/lxprobe-static.out") probes ok, static and dynamic outputs identical"

cat >"$build/entrypoint.sh" <<'EOF'
#!/bin/sh
# k10s-test/shell: an entrypoint-style script, then busybox httpd in the foreground.
set -eu
mkdir -p /www
printf '<h1>%s</h1>\n' "${GREETING:-hello from k10s-test/shell}" >/www/index.html
sed -i 's/h1/h2/g' /www/index.html
echo "serving $(wc -c </www/index.html) bytes on :${PORT}"
exec httpd -f -v -p "$PORT" -h /www
EOF
printf 'root:x:0:0:root:/root:/bin/sh\nnobody:x:65534:65534:nobody:/:/bin/false\n' >"$build/passwd"
printf 'root:x:0:\nnogroup:x:65534:\n' >"$build/group"

workloads=()
if command -v go >/dev/null; then
    for w in echo compute chain counter; do
        (cd "$root/bench/workloads" && CGO_ENABLED=0 GOOS=linux GOARCH=amd64 go build -trimpath -ldflags="-s -w" \
            -o "$bin/static/$w" "./$w")
        workloads+=("$w")
    done
    (cd "$root/bench/workloads" && CGO_ENABLED=0 GOOS=linux GOARCH=arm64 go build -trimpath -ldflags="-s -w" \
        -o "$bin/arm64/echo" ./echo)
else
    "$CC" "${CFLAGS[@]}" -static -o "$bin/static/echo" "$probe/echo.c"
    strip "$bin/static/echo"
    workloads=(echo)
fi

export BIN=$bin BUILD=$build MUSL=$musl BUSYBOX_BIN=$bb/busybox APPLETS WORKLOADS="${workloads[*]}"
python3 - >"$build/spec.json" <<'EOF'
import json, os
b, build, musl = os.environ["BIN"], os.environ["BUILD"], os.environ["MUSL"]
env = ["PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"]
tmp = [["/tmp", 0o1777]]
images = [
    {"name": "k10s-test/lxprobe", "tag": "v1", "arch": "amd64", "dirs": tmp,
     "files": [["/lxprobe", f"{b}/static/lxprobe", 0o755]], "config": {"Entrypoint": ["/lxprobe"], "Env": env}},
    {"name": "k10s-test/lxprobe-dynamic", "tag": "v1", "arch": "amd64", "dirs": tmp,
     "files": [["/lxprobe", f"{b}/dynamic/lxprobe", 0o755],
               ["/lib/ld-musl-x86_64.so.1", f"{musl}/lib/libc.so", 0o755]],
     "symlinks": [["/lib/libc.musl-x86_64.so.1", "ld-musl-x86_64.so.1"]],
     "config": {"Entrypoint": ["/lxprobe"], "Env": env}},
    {"name": "k10s-test/shell", "tag": "v1", "arch": "amd64", "dirs": tmp + [["/www", 0o755], ["/root", 0o700]],
     "files": [["/bin/busybox", os.environ["BUSYBOX_BIN"], 0o755],
               ["/entrypoint.sh", f"{build}/entrypoint.sh", 0o755], ["/etc/passwd", f"{build}/passwd", 0o644],
               ["/etc/group", f"{build}/group", 0o644]],
     "symlinks": [[f"/bin/{a}", "busybox"] for a in os.environ["APPLETS"].split()],
     "config": {"Entrypoint": ["/entrypoint.sh"], "Env": env + ["PORT=8080"], "ExposedPorts": {"8080/tcp": {}},
                "WorkingDir": "/"}},
]
for w in os.environ["WORKLOADS"].split():
    dirs = tmp + ([["/data", 0o755]] if w == "counter" else [])
    cfg = {"Entrypoint": ["/app"], "Env": env + ["PORT=8080"], "ExposedPorts": {"8080/tcp": {}}}
    images.append({"name": f"k10s-test/{w}", "tag": "v1", "arch": "amd64", "dirs": dirs,
                   "files": [["/app", f"{b}/static/{w}", 0o755]], "config": cfg})
    if w == "echo" and os.path.exists(f"{b}/arm64/echo"):
        images.append(dict(images[-1], arch="arm64", files=[["/app", f"{b}/arm64/echo", 0o755]]))
print(json.dumps({"images": images}, indent=1))
EOF
rm -rf "$out/layout" "$out/archives"
python3 "$root/scripts/ocilayout.py" "$build/spec.json" "$out"
python3 "$root/os/tools/registry.py" --check-layout "$out/layout"
