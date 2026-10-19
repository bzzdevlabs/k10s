# k10s — project status and resume guide

Last updated: 2026-10-19. This file is the single place to learn where the project stands and what comes next; it is
kept current so work can resume in a fresh session.

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
1. **Chaos / soak / abuse campaign** + fixes for the unsafe-code review findings (mesh-thread panic on lossy path
   encoding, uncapped host copies, KV quota, snapshot/mincore, MAP_FIXED fallback, VmCtx aliasing, catch_unwind →
   abort, --mem race, preemption race, peer-link buffering, EMFILE spin, admin limits). Touches `crates/k10-node`,
   `crates/k10s`, `crates/k10-mesh` (wire: path as raw bytes, `encode` returns `Result`).
2. **Metal phase 2** (JIT engine, cluster membership, SMP) — finished in the working tree under `metal/`, NOT yet
   committed because it builds against the uncommitted k10-mesh wire change from (1). Commit both together.
3. **Comparative benchmark harness** `bench/k10s/`, `bench/compare.py`, `bench/run-final.sh` →
   `bench/results/COMPARISON.md` (preliminary).
4. **Research tasks** `docs/TASK-universal-os.md` → `docs/research/universal-os.md` and
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
