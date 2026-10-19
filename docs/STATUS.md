# k10s — project status and resume guide

PROJECT STATE: IN PROGRESS

Last updated: 2026-10-19. This file is the single place to learn where the project stands and what comes next; it is
kept current so work can resume in a fresh session.

Keep-alive: the desktop scheduled task `k10s-session-watchdog` (17 and 47 past each hour) wakes the build session
(`local_96d994a5-1881-4af5-9a73-ee7953fae801`) when it has been idle for 25+ minutes, e.g. after a usage limit. It
disables itself once the state line at the top is switched from IN PROGRESS to COMPLETE — do that only when
everything is done.

## Built and verified (on `main`)
| Component | Where | State |
|---|---|---|
| Design, specs | `docs/DESIGN.md`, `TIR.md`, `HOSTAPI.md`, `JIT-ABI.md`, `TEN.md`, `NODE.md`, `research.md` | done |
| TIR core: codec, validator, reference interpreter, builder, generator | `crates/k10-ir` | done, 81 tests |
| Cluster layer: SWIM gossip, manifests, bundles, deterministic placement, wire framing, simulator | `crates/k10-mesh` | done |
| Ten compiler + prelude + examples | `crates/k10-lang`, `examples/` | done, 87 tests |
| x86-64 JIT (single pass, SFI masking, safepoints) | `crates/k10-jit` | done; 1M-module differential soak, red-team suite |
| Node runtime: shards, fibers, slots, CoW snapshots, host API, KV, HTTP ingress, admin API, cluster mode, JIT backend | `crates/k10-node`, `crates/k10s` | done |
| Bare-metal node (PVH, virtio-net, smoltcp, interpreter) | `metal/` | done (phase 1) |
| Kubernetes + bare-process baselines, Go loadgen | `bench/k8s`, `bench/native`, `bench/loadgen` | done (preliminary numbers) |

## In flight (uncommitted work may exist in the working tree)
1. DONE (committed) — **Chaos / soak / abuse campaign** + fixes for the unsafe-code review findings (mesh-thread panic on lossy path
   encoding, uncapped host copies, KV quota, snapshot/mincore, MAP_FIXED fallback, VmCtx aliasing, catch_unwind →
   abort, --mem race, preemption race, peer-link buffering, EMFILE spin, admin limits). Touches `crates/k10-node`,
   `crates/k10s`, `crates/k10-mesh` (wire: path as raw bytes, `encode` returns `Result`).
2. DONE (committed) — **Metal phase 2** (JIT engine, cluster membership, SMP) — finished in the working tree under `metal/`, NOT yet
   committed because it builds against the uncommitted k10-mesh wire change from (1). Commit both together.
3. RUNNING (resumed after usage limit) — **Comparative benchmark harness** `bench/k10s/`, `bench/compare.py`, `bench/run-final.sh` →
   `bench/results/COMPARISON.md` (preliminary).
4. DONE — native-hardware (docs/research/native-hardware.md) and universal-os (docs/research/universal-os.md); prototypes on local branches research/native-hardware and research/universal-os (not merged, not pushed) — **Research tasks** `docs/TASK-universal-os.md` → `docs/research/universal-os.md` and
   `docs/TASK-native-hardware.md` → `docs/research/native-hardware.md` (experiments live outside the repo in
   `/home/qgerard/dev/k10s-research/`).

## Remaining steps, in order
1. When (1) lands: run all suites (`CARGO_TARGET_DIR=target/node cargo test -p k10-ir -p k10-mesh -p k10-lang -p k10-jit
   -p k10-node -p k10s`, and `K10S_BACKEND=interp` for node/k10s), `metal/test.sh`, `metal/cluster-test.sh`; commit
   crates + metal; push.
2. Commit the benchmark harness; commit the research reports.
3. On a quiet machine (no other agents): `bench/run-final.sh` → final `bench/results/COMPARISON.md`; commit.
4. Write `docs/REPORT.md` (final results: what was built, measured gains vs Kubernetes, test evidence, limitations,
   roadmap) and refresh `README.md` numbers; commit; push.

## Conventions
- Commit as `Quentin Gerard <quentin.gerard@telespazio.com>` via `git -c user.name=… -c user.email=…`, trailer
  `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`; push `main` to `origin` after each verified commit.
- Use separate cargo target dirs per concurrent agent (`target/node`, `target/chaos`, …) to avoid lock contention.
- Never touch Docker containers/images not created by k10s scripts (the machine has the user's own).

## Follow-ups found by the reliability campaign — DONE (heap_reset in examples, atomic kv_add/kv_cas, mesh read/write caps, accept backoff, --conn-buffer-max); still open: per-service memory reservation, chaos faulting the seed node, multi-hour soak
- examples: add `heap_reset()` at the top of `handle` in chain/compute/nbody (per-request heap leak → trap every ~4k
  requests); counter/todo lose updates under concurrency → add atomic KV host functions (`kv_add`, `kv_cas`) in
  HOSTAPI + node + metal + Ten prelude, or document `max_instances = 1`.
- Review items 9 (peer-link buffering caps) and 10 (EMFILE listener spin); node-wide buffered-bytes budget; per-service
  memory reservation; chaos driver should also fault the seed node; multi-hour soak to rule out the 40 KB/min RSS drift
  seen on one cluster node.
- Native-hardware open questions for the user are listed at the end of docs/research/native-hardware.md.
