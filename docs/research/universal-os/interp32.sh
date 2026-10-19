#!/usr/bin/env bash
# Same CPU, same Ten services: x86_64 (interp + JIT) vs i686 (interp) — the cost of 64-bit TIR values on a 32-bit ISA.
# Prints the min over N interleaved runs of `k10s run` wall time per request (ms).
cd "$(dirname "$0")/.."
N=${N:-7}
declare -A best
for i in $(seq $N); do
  for cfg in "x86_64 jit" "x86_64 interp" "i686 interp"; do
    set -- $cfg
    for w in "compute.ten ?n=1000" "nbody.ten ?n=1000" "sort.ten ?n=20000" "hello.ten /"; do
      set -- $cfg $w
      ms=$(bin/k10s-$1 run wt/examples/$3 --path "$4" --backend $2 2>&1 >/dev/null | sed -n 's/.*bytes, \([0-9.]*\) ms.*/\1/p')
      k="$1/$2/$3"
      if [[ -z "${best[$k]:-}" ]] || (( $(echo "$ms < ${best[$k]}" | bc) )); then best[$k]=$ms; fi
    done
  done
done
for k in $(printf '%s\n' "${!best[@]}" | sort); do echo "$k ${best[$k]}"; done
echo "load $(cut -d' ' -f1-3 /proc/loadavg)"
