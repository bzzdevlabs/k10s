#!/usr/bin/env bash
# k10s SemVer, the single source of truth: X.Y.Z from [workspace.package] (Cargo.toml = metal/Cargo.toml);
# stable on tag vX.Y.Z, else X.Y.Z-dev.N (N = commit count; next patch once vX.Y.Z exists); +<sha7>[.dirty].
# usage: version.sh [--dev|--stable] [--strict] [--version|--full|--base|--env|--watch|--check]...
# --stable: HEAD tagged, clean tree; --strict: fail without git/full history. K10S_BUILD_VERSION/_COMMIT override.
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)
die() {
    echo "version.sh: $*" >&2
    exit 1
}

mode=auto
strict=0
outs=()
for a in "$@"; do
    case $a in
    --dev) mode=dev ;;
    --stable) mode=stable ;;
    --strict) strict=1 ;;
    --version | --full | --base | --env | --watch | --check) outs+=("$a") ;;
    -h | --help)
        sed -n '2,/^set -euo/{/^set -euo/d;s/^# \{0,1\}//;p}' "${BASH_SOURCE[0]}"
        exit 0
        ;;
    *) die "unknown option '$a' (see --help)" ;;
    esac
done
((${#outs[@]})) || outs=(--version)

# [workspace.package] version of a Cargo.toml.
cargo_version() {
    awk '/^[[:space:]]*\[/ { s = $1; next }
         s == "[workspace.package]" && /^[[:space:]]*version[[:space:]]*=/ {
             v = $0; sub(/^[^"]*"/, "", v); sub(/".*/, "", v); print v; exit }' "$1"
}
base=$(cargo_version "$ROOT/Cargo.toml")
[[ $base =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]] ||
    die "Cargo.toml: [workspace.package] version '$base' is not X.Y.Z"
if [[ -f $ROOT/metal/Cargo.toml ]]; then
    metal=$(cargo_version "$ROOT/metal/Cargo.toml")
    [[ $metal == "$base" ]] ||
        die "version mismatch: Cargo.toml says $base but metal/Cargo.toml says '${metal}'; give both workspaces the same X.Y.Z"
fi
[[ ${outs[*]} == --check ]] && exit 0

g() { git -C "$ROOT" "$@"; }
tag_exists() { g rev-parse -q --verify "refs/tags/$1" >/dev/null; }

if [[ -n ${K10S_BUILD_VERSION:-} ]]; then
    source=env
elif command -v git >/dev/null && top=$(g rev-parse --show-toplevel 2>/dev/null) && [[ $(cd "$top" && pwd -P) == "$ROOT" ]]; then
    source=git # not when ROOT is merely unpacked inside another repository
elif [[ -f $ROOT/.k10s-version ]]; then
    source=stamp
else
    source=none
fi

case $source in
env | stamp)
    if [[ $source == stamp ]]; then
        K10S_BUILD_VERSION=$(sed -n 's/^K10S_BUILD_VERSION=//p' "$ROOT/.k10s-version")
        K10S_BUILD_COMMIT=$(sed -n 's/^K10S_BUILD_COMMIT=//p' "$ROOT/.k10s-version")
        [[ -n $K10S_BUILD_VERSION ]] || die "$ROOT/.k10s-version: no K10S_BUILD_VERSION"
    fi
    version=${K10S_BUILD_VERSION%%+*}
    build=${K10S_BUILD_COMMIT:-}
    if [[ -z $build && $K10S_BUILD_VERSION == *+* ]]; then build=${K10S_BUILD_VERSION#*+}; fi
    build=${build:-unknown}
    ;;
git)
    if [[ $(g rev-parse --is-shallow-repository) == true ]]; then
        ((!strict)) || die "shallow clone: the build number would be wrong; fetch the full history (actions/checkout: fetch-depth: 0)"
        echo "version.sh: warning: shallow clone, build number unknown (0)" >&2
        number=0
    else
        number=$(g rev-list --count HEAD)
    fi
    build=$(g rev-parse --short=7 HEAD)
    # GIT_OPTIONAL_LOCKS=0: no index refresh, which would retrigger the build scripts.
    if [[ -n $(GIT_OPTIONAL_LOCKS=0 g status --porcelain --untracked-files=no) ]]; then build+=.dirty; fi
    if [[ $mode != dev && $(g rev-parse -q --verify "refs/tags/v$base^{commit}" || true) == "$(g rev-parse HEAD)" ]]; then
        version=$base
    else
        next=$base # first unreleased patch >= X.Y.Z
        while tag_exists "v$next"; do next=${next%.*}.$((${next##*.} + 1)); done
        version=$next-dev.$number
    fi
    ;;
none)
    ((!strict)) || die "no git repository at $ROOT and no K10S_BUILD_VERSION: cannot tell the version"
    version=$base-dev.0
    build=unknown
    ;;
esac
[[ $version == *-* ]] && channel=dev || channel=stable
if [[ $mode == stable ]]; then
    [[ $channel == stable ]] || die "HEAD is not tagged v$base (the Cargo workspace version): version $version"
    [[ $build != *.dirty ]] || die "the tree has uncommitted changes"
fi

watch() {
    local p
    for p in "$ROOT/Cargo.toml" "$ROOT/metal/Cargo.toml" "$ROOT/scripts/version.sh" "$ROOT/.k10s-version"; do
        if [[ -e $p ]]; then echo "$p"; fi
    done
    [[ $source == git ]] || return 0
    # HEAD, its reflog, the branch and the tags (--git-path resolves worktrees).
    for p in HEAD logs/HEAD packed-refs refs/tags $(g symbolic-ref -q HEAD || true); do
        p=$(g rev-parse --git-path "$p")
        [[ $p == /* ]] || p=$ROOT/$p
        if [[ -e $p ]]; then echo "$p"; fi
    done
}

for o in "${outs[@]}"; do
    case $o in
    --version) echo "$version" ;;
    --full) echo "$version+$build" ;;
    --base) echo "$base" ;;
    --env)
        echo "K10S_BUILD_VERSION=$version"
        echo "K10S_BUILD_COMMIT=$build"
        echo "K10S_VERSION_FULL=$version+$build"
        echo "K10S_VERSION_TAG=v$version"
        echo "K10S_CHANNEL=$channel"
        ;;
    --watch) watch ;;
    --check) ;;
    esac
done
