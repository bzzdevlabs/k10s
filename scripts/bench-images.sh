#!/usr/bin/env bash
# Benchmark workload images (FROM scratch + static binary, as bench/workloads/build.sh) from built binaries.
# usage: bench-images.sh BIN_DIR PREFIX TAG... [--push]   -> PREFIX<echo|compute|chain|counter>:TAG
# env: K10S_VERSION_FULL, K10S_REVISION, GITHUB_SERVER_URL/GITHUB_REPOSITORY (OCI labels; source links GHCR to the repo)
set -euo pipefail
bin=${1:?usage: bench-images.sh BIN_DIR PREFIX TAG [TAG...] [--push]}
prefix=${2:?usage: bench-images.sh BIN_DIR PREFIX TAG [TAG...] [--push]}
shift 2
push=0
tags=()
for a in "$@"; do
    if [[ $a == --push ]]; then push=1; else tags+=("$a"); fi
done
((${#tags[@]})) || { echo "bench-images.sh: no tag" >&2; exit 2; }
source_url=${GITHUB_SERVER_URL:-https://github.com}/${GITHUB_REPOSITORY:-bzzdevlabs/k10s}

for w in echo compute chain counter; do
    ctx=$(mktemp -d)
    trap 'rm -rf "$ctx"' EXIT
    cp "$bin/$w" "$ctx/app"
    printf 'FROM scratch\nCOPY app /app\nENTRYPOINT ["/app"]\n' >"$ctx/Dockerfile"
    args=()
    for t in "${tags[@]}"; do args+=(-t "$prefix$w:$t"); done
    docker build -q "${args[@]}" \
        --label "org.opencontainers.image.source=$source_url" \
        --label "org.opencontainers.image.title=k10s-bench-$w" \
        --label "org.opencontainers.image.description=k10s benchmark workload '$w' (bench/workloads/$w)" \
        --label "org.opencontainers.image.version=${K10S_VERSION_FULL:-${tags[0]}}" \
        --label "org.opencontainers.image.revision=${K10S_REVISION:-}" \
        --label "org.opencontainers.image.licenses=Apache-2.0" \
        "$ctx" >/dev/null
    rm -rf "$ctx"
    trap - EXIT
    for t in "${tags[@]}"; do
        echo "$prefix$w:$t"
        if ((push)); then docker push -q "$prefix$w:$t" >/dev/null; fi
    done
done
