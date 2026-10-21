#!/usr/bin/env bash
# Benchmark workload images (FROM scratch + static binary, as bench/workloads/build.sh) from built binaries.
# usage: bench-images.sh BIN_DIR PREFIX TAG... [--push]  -> images PREFIX<echo|compute|chain|counter>:TAG
# Prints the image references, or with --push "<sha256 hex>  <image name>" lines (attestation subjects).
# env: K10S_VERSION_FULL, K10S_REVISION, GITHUB_SERVER_URL/GITHUB_REPOSITORY (OCI labels; source links GHCR to the repo)
set -euo pipefail
usage="usage: bench-images.sh BIN_DIR PREFIX TAG... [--push]"
bin=${1:?$usage}
prefix=${2:?$usage}
shift 2
push=0
tags=()
for a in "$@"; do
    if [[ $a == --push ]]; then push=1; else tags+=("$a"); fi
done
((${#tags[@]})) || { echo "$usage" >&2; exit 2; }
source_url=${GITHUB_SERVER_URL:-https://github.com}/${GITHUB_REPOSITORY:-bzzdevlabs/k10s}
ctx=$(mktemp -d)
trap 'rm -rf "$ctx"' EXIT
printf 'FROM scratch\nCOPY app /app\nENTRYPOINT ["/app"]\n' >"$ctx/Dockerfile"

for w in echo compute chain counter; do
    cp "$bin/$w" "$ctx/app"
    args=()
    for t in "${tags[@]}"; do args+=(-t "$prefix$w:$t"); done
    # One plain manifest per image: no BuildKit attestation manifests (provenance is attested by the workflow).
    docker build -q --provenance=false --sbom=false "${args[@]}" \
        --label "org.opencontainers.image.source=$source_url" \
        --label "org.opencontainers.image.title=k10s-bench-$w" \
        --label "org.opencontainers.image.description=k10s benchmark workload '$w' (bench/workloads/$w)" \
        --label "org.opencontainers.image.version=${K10S_VERSION_FULL:-${tags[0]}}" \
        --label "org.opencontainers.image.revision=${K10S_REVISION:-}" \
        --label "org.opencontainers.image.licenses=Apache-2.0" \
        "$ctx" >/dev/null
    if ((push)); then
        for t in "${tags[@]}"; do docker push -q "$prefix$w:$t" >/dev/null; done
        digest=$(docker inspect --format '{{index .RepoDigests 0}}' "$prefix$w:${tags[0]}")
        echo "${digest#*@sha256:}  $prefix$w"
    else
        printf "$prefix$w:%s\n" "${tags[@]}"
    fi
done
