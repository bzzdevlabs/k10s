#!/usr/bin/env bash
# kind load the workload images into every node of the cluster.
source "$(dirname "$0")/lib.sh"
check_disk
IMGS="k10s-bench/echo:v1 k10s-bench/echo:v2 k10s-bench/chain:v1 k10s-bench/compute:v1 k10s-bench/counter:v1"
for i in $IMGS; do docker image inspect "$i" >/dev/null 2>&1 || { "$BENCH_DIR/workloads/build.sh" >/dev/null; break; }; done
kind load docker-image --name "$CLUSTER" $IMGS
