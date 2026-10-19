#!/usr/bin/env bash
# Final benchmark run (quiet machine): k8s (kind) suite, native suite, k10s suite, sequentially, then COMPARISON.md.
# Before each suite it waits until the 1-min load average is below LOAD_MAX (1.0) and the host CPU busy over 10 s is below
# BUSY_MAX (1.0 core; Docker Desktop's idle VM alone uses ~0.4), checking every 15 s, and aborts after LOAD_WAIT_MAX
# seconds (default 2 h). Total ~2.5-3 h. Results overwrite bench/results/{k8s,native,k10s}/*.json.
# Every latency run also records `other_cpu_cores` (CPU used by anything else during its window) and is flagged noisy
# above NOISY_CPU (0.5 cores): check COMPARISON.md's noise line before using the numbers.
#   SUITES="k8s native k10s"   subset/order        REPEATS=3   LOAD_MAX=1.0   LOAD_WAIT_MAX=7200
#   SKIP_BUILD=1               do not rebuild k10s / workloads first
# Requirements: docker (+ kind, kubectl), Go, cargo, python3, systemd user session (k10s cgroup scopes), >= 3 GB free on /home.
# Only kind clusters named k10s-bench* are created/deleted; no other Docker container or image is touched.
set -euo pipefail
B="$(cd "$(dirname "$0")" && pwd)"; REPO="$(cd "$B/.." && pwd)"
LOAD_MAX="${LOAD_MAX:-1.0}"; BUSY_MAX="${BUSY_MAX:-1.0}"; LOAD_WAIT_MAX="${LOAD_WAIT_MAX:-7200}"
SUITES="${SUITES:-k8s native k10s}"
export REPEATS="${REPEATS:-3}" NOISY_LOAD="${NOISY_LOAD:-$LOAD_MAX}"
# extensions the comparison needs (all suites): compute in the closed-loop latency, open-loop runs at fixed rates
export LAT_TARGETS_K8S="${LAT_TARGETS_K8S:-echo chain compute}" LAT_TARGETS_NATIVE="${LAT_TARGETS_NATIVE:-echo chain compute compute-purego}"
export OPEN_LOOP=1
LOG="$B/results/.final.log"; mkdir -p "$B/results"
log() { echo "[$(date +%F' '%T)] $*" | tee -a "$LOG" >&2; }
free_gb() { df -BG --output=avail /home | tail -1 | tr -dc 0-9; }
busy10() { # host CPU busy (cores) over 10 s, from /proc/stat
  python3 - <<'PY'
import os,time
def b():
    f=[int(x) for x in open("/proc/stat").readline().split()[1:]]; return sum(f[:8])-f[3]-f[4]
a=b(); time.sleep(10); print(round((b()-a)/os.sysconf("SC_CLK_TCK")/10, 2))
PY
}
gate() { # wait for a quiet machine: 1-min load average < LOAD_MAX and < BUSY_MAX cores busy over 10 s
  local t0 l c; t0=$(date +%s)
  while :; do
    l=$(cut -d' ' -f1 /proc/loadavg)
    if python3 -c "import sys; sys.exit(0 if float('$l') < float('$LOAD_MAX') else 1)"; then
      c=$(busy10)
      if python3 -c "import sys; sys.exit(0 if float('$c') < float('$BUSY_MAX') else 1)"; then log "load $l, busy $c cores: starting $1"; return 0; fi
    fi
    if [ $(( $(date +%s) - t0 )) -gt "$LOAD_WAIT_MAX" ]; then log "ABORT: machine still busy (load $l) after ${LOAD_WAIT_MAX}s (before $1)"; exit 4; fi
    sleep 15
  done
}
[ "$(free_gb)" -ge 3 ] || { log "ABORT: less than 3 GB free on /home"; exit 3; }
if [ "${SKIP_BUILD:-0}" != 1 ]; then
  log "building k10s (release) and the workloads"
  ( cd "$REPO" && CARGO_TARGET_DIR=target/node cargo build --release -p k10s ) >>"$LOG" 2>&1
  "$B/workloads/build.sh" >>"$LOG" 2>&1
fi
for s in $SUITES; do
  gate "$s"
  case $s in
    k8s)    LAT_TARGETS="$LAT_TARGETS_K8S" "$B/k8s/run-all.sh" >>"$LOG" 2>&1 ;;
    native) LAT_TARGETS="$LAT_TARGETS_NATIVE" "$B/native/run-all.sh" >>"$LOG" 2>&1 ;;
    k10s)   SKIP_BUILD=1 "$B/k10s/run-all.sh" >>"$LOG" 2>&1 ;;
    *) log "unknown suite $s"; exit 2 ;;
  esac
  log "$s done"
done
python3 "$B/compare.py" | tee -a "$LOG"
log "all done: $B/results/COMPARISON.md"
