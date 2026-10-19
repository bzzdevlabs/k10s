#!/usr/bin/env bash
# Open-loop latency through the NodePort (fixed request rates; tail latency under a fixed load is a fairer comparison
# than the closed-loop maximum): latency.sh with LAT_MODE=open -> results/k8s/latency-open.json.
# OPEN_RATES_echo / OPEN_RATES_chain (default "5000 20000"), OPEN_RATES_compute ("500 2000"), OPEN_CONNS (64) bounds
# in-flight requests; LAT_TARGETS (default "echo chain compute"), LAT_PLACES (host node), REPEATS, LAT_DURATION 20, LAT_WARMUP 3.
export LAT_MODE=open LAT_TARGETS="${LAT_TARGETS:-echo chain compute}"
exec "$(dirname "$0")/latency.sh"
