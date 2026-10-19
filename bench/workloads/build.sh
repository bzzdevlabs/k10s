#!/usr/bin/env bash
# Build static binaries into bench/bin/ (+ compute-purego) and FROM-scratch docker images k10s-bench/<name>:v1 (+ echo:v2 tag).
set -euo pipefail
cd "$(dirname "$0")"
BIN=../bin; mkdir -p "$BIN"
for w in echo compute chain counter; do
  CGO_ENABLED=0 go build -trimpath -ldflags="-s -w" -o "$BIN/$w" "./$w"
done
# fairness extra (native only): compute with Go's portable SHA-256 (no hand-written assembly), to separate the
# language/codegen gap from the platform gap when comparing with Ten's portable SHA-256
CGO_ENABLED=0 go build -tags purego -trimpath -ldflags="-s -w" -o "$BIN/compute-purego" ./compute
( cd ../loadgen && CGO_ENABLED=0 go build -trimpath -ldflags="-s -w" -o "$BIN/loadgen" . )
[ "${NO_DOCKER:-0}" = 1 ] && exit 0
for w in echo compute chain counter; do
  ctx=$(mktemp -d); cp "$BIN/$w" "$ctx/app"
  printf 'FROM scratch\nCOPY app /app\nENTRYPOINT ["/app"]\n' > "$ctx/Dockerfile"
  docker build -q -t "k10s-bench/$w:v1" "$ctx" >/dev/null
  rm -rf "$ctx"
done
docker tag k10s-bench/echo:v1 k10s-bench/echo:v2
docker images --format '{{.Repository}}:{{.Tag}} {{.Size}}' | grep '^k10s-bench/'
