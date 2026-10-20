#!/usr/bin/env bash
# usage: set-version.sh X.Y.Z — sets the version of both Cargo workspaces and their lock files (CI builds --locked).
set -euo pipefail
cd "$(dirname "$0")/.."
v=${1:?usage: set-version.sh X.Y.Z}
[[ $v =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]] || {
    echo "set-version.sh: '$v' is not X.Y.Z" >&2
    exit 2
}
for m in Cargo.toml metal/Cargo.toml; do
    awk -v v="$v" '
        /^[[:space:]]*\[/ { s = $1 }
        s == "[workspace.package]" && /^[[:space:]]*version[[:space:]]*=/ && !done { sub(/"[^"]*"/, "\"" v "\""); done = 1 }
        { print }' "$m" >"$m.tmp"
    mv "$m.tmp" "$m"
done
# Workspace members carry the version in the lock files too.
cargo update --workspace --offline --quiet
(cd metal && cargo update --workspace --offline --quiet)
scripts/version.sh --check
echo "version $v: Cargo.toml, metal/Cargo.toml, Cargo.lock, metal/Cargo.lock"
