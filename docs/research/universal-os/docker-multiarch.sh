#!/usr/bin/env bash
# Does Docker Desktop run foreign-arch static binaries? FROM scratch images (nothing pulled), removed afterwards.
cd "$(dirname "$0")/.."
for pa in "linux/arm64 aarch64" "linux/arm/v7 armv7" "linux/386 i686" "linux/riscv64 riscv64gc"; do
  set -- $pa; cp bin/k10s-$2 docker-ctx/k10s
  printf 'FROM scratch\nCOPY k10s hello.ten compute.ten /\nENTRYPOINT ["/k10s"]\n' > docker-ctx/Dockerfile
  tag=k10s-uos-research:$2; echo "== $1"
  timeout 300 docker build -q --platform $1 -t $tag docker-ctx >/dev/null 2>&1 || echo "build failed"
  t0=$(date +%s.%N)
  timeout 120 docker run --rm --platform $1 $tag run /compute.ten --path '?n=100' 2>&1 | tail -1
  echo "   ($(echo "$(date +%s.%N) - $t0" | bc) s incl. container start)"
  docker rmi -f $tag >/dev/null 2>&1
done
docker images --format '{{.Repository}}' | grep -c k10s-uos-research || true
