# k10s — project status and resume guide

PROJECT STATE: COMPLETE

Last updated: 2026-10-20. All planned steps are done; remaining items below are the roadmap (docs/REPORT.md §8). This file is the single place to learn where the project stands and what comes next; it is
kept current so work can resume in a fresh session.

Keep-alive: the desktop scheduled task `k10s-session-watchdog` (17 and 47 past each hour) wakes the build session
(`local_96d994a5-1881-4af5-9a73-ee7953fae801`) when it has been idle for 25+ minutes, e.g. after a usage limit. It
disables itself once the state line at the top is switched from IN PROGRESS to COMPLETE — do that only when
everything is done.

## Security round 2 (2026-10-20) — resume here, one item per session
(Leave the state line above as is: the old watchdog task keys on it.) Local `main` (not pushed; origin/main = c1c5234) holds
fixes #1–#5 of the security review, the release pipeline (SemVer: tag vX.Y.Z → release, main → vX.Y.Z-dev.N
pre-release), the KV durability fix. All suites + metal were green at 5b6eb94. Independent reviews then found defects;
reworks were started by agents and stopped mid-way (token budget). Each lives on a branch whose tip may be a
"WIP (stopped…)" commit: finish it, run its tests, merge into main. Rules: production-ready (no workarounds), comments only
where needed as English one-liners, no perf regression (A/B with `bench/k10s/ab.py` vs 08f2921), no pushes/tags until
step 8, never experiment with namespaces/privileges on this machine.

1. DONE (merged 93ada5f) — **Calls allowlist**: index-based `calls` format 2, canonical `Calls`, strict decode bounded
   by service count, warnings at apply/reload, `Fiber::caller` nulled in `finish`; status shows per-service grants as
   `may_call` (`calls` stays the call counter). All suites both backends, metal test (294) and cluster-test (114) green.
2. DONE (merged e1888a7) — **Reservations / rejoin**: degraded (over-committed) reload and cluster installs, retiring
   units, fail-fast waiters, relief list, placement version in gossip, reconnect set with per-address backoff and
   DATA/peers fallback seeds, persisted incarnation (decided at bind). `k10-mesh` is built at opt-level 2 in debug
   (unoptimized crypto missed gossip deadlines in mesh_abuse). All suites both backends, chaos_short ×3 seeds,
   chaos_long 20 min (1631 rounds), metal test (294; needs `bench/bin/loadgen`, else its loadgen checks are skipped)
   and cluster-test (110; varies with placement) green.
3. **Release pipeline** — `worktree-agent-abc9097adc759323d` (d857328, committed; final validation pending). Review
   items: fail on dev-tag collision; never republish/move `:dev` backwards; stable tags must be on main; skip dev publish
   when HEAD has v$base; metal/test.sh non-blocking in PR CI; build provenance attestations; `--latest=false` backports,
   orphan drafts, GHCR dev pruning, glibc floor, e2e tests on the dist musl binary; build.rs decoupled from metal, no
   process-wide version global (NodeConfig field); CONTRIBUTING lists tag/branch protection for the user to apply.
   Validate YAML, run dist.sh + ci-test.sh, merge.
4. **Spectre** — `worktree-agent-a164c4392bf6288f3` (3be341f WIP: compiled, suites not run). Items: shared no_std host
   crate (one copy-out semantics hosted/metal + parity test; `nospec`/`clock` moved out, k10-ir back to
   forbid(unsafe_code)); per-node immutable guest clock (no `inv`, no process-wide atomics); SipHash-keyed fuzzy edges
   with domain separation, keys from fill_random/RDRAND without fallback; metal invalid knobs `fatal()`; no panic on
   br_table length; aarch64 `nospec` or explicit x86-only; dead code (`check_range`, `load_r11` arm, double IrqLock);
   SECURITY.md wording (raises cost; retpoline AMD/CET caveats). Clean option names (no aliases needed). Finish, test, merge.
5. **Sandbox + SFI verifier** — `worktree-agent-a57497ca666c45a0a` (ee37a7b WIP; verifier holes a–f committed before it).
   Sandbox items: two-phase start (bind + read secrets → setres[ug]id → caps check → Landlock → seccomp → KV, bundle
   reload, threads); refuse root without `--user`; socket type STREAM/DGRAM only; required Landlock rules fail loudly;
   no-Landlock = fail unless `--sandbox seccomp-only`; drop NET_BIND_TCP rules; pre-opened /proc fds; accurate `log` mode;
   cfg-gate x86-64 Linux; musl tkill(SIGABRT); benign syscalls (poll, rmdir, get*id, uname); `--user` buffer/fchown
   fixes; SIGHUP handling; split into sandbox/{mod,seccomp,landlock,user}.rs with trimmed comments. Finish, test, merge.
6. **Auth** — `worktree-agent-a34a5562a6c3410ee` (db963af WIP, largest). Items: key confirmation before serving
   (pre-auth record cap, `Hello` first, hold records, cap unauthenticated links); one `k10_mesh::secure::Link` shared by
   hosted and metal; exact-match boolean flags (no fail-open `=false`); signed admin requests (HMAC over
   method‖target‖t‖n‖sha256(body), skew + nonce cache), node always requires a key (auto `DATA/admin.key` 0600), no
   loopback/X-Forwarded heuristic, `metrics` open only on `--admin`; metal secrets via fw_cfg not cmdline; gossip
   replay-state GC fix, no skew-0 mode, sender id in AAD; metal wall-clock resync; cluster key rotation (current + next,
   key id on wire); zeroize keys; RDRAND failure fails the link; dead code + comment trims; NODE.md claims match code.
7. **Integrate**: merge order 1→6 into main (resolve main.rs/node.rs/mesh.rs conflicts), full suites both backends,
   `metal/test.sh`, `metal/cluster-test.sh`, quiet-machine `bench/k10s/ab.py` (≥ 4 rounds) vs a 08f2921 build; update this
   file and README/REPORT numbers.
8. **Push main** (publishes the first dev pre-release); watch the release workflow; fix what the first real run shows
   (sandbox under the GitHub runners' kernel, musl suite).
9. Optional perf (after 6): shard-owned peer links (each shard dials/reads its own link per peer; mesh keeps gossip,
   fetch, accept; expected 7 → 3 wake-ups per forwarded request, ≈ −40 % CPU at 1 conn). A shard-does-mesh-work
   prototype regressed c=64 and was dropped.
10. User: delete the test GHCR images `k10s-bench-*` (0.1.0, 0.1.0-dev.46, latest, dev) or grant `gh` packages scope.

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
3. DONE (committed 412ad4f) — **Comparative benchmark harness** `bench/k10s/`, `bench/compare.py`, `bench/run-final.sh` →
   `bench/results/COMPARISON.md` (preliminary).
4. DONE — native-hardware (docs/research/native-hardware.md) and universal-os (docs/research/universal-os.md); prototypes on local branches research/native-hardware and research/universal-os (not merged, not pushed) — **Research tasks** `docs/TASK-universal-os.md` → `docs/research/universal-os.md` and
   `docs/TASK-native-hardware.md` → `docs/research/native-hardware.md` (experiments live outside the repo in
   `/home/qgerard/dev/k10s-research/`).

## Remaining steps, in order
1. DONE — When (1) lands: run all suites (`CARGO_TARGET_DIR=target/node cargo test -p k10-ir -p k10-mesh -p k10-lang -p k10-jit
   -p k10-node -p k10s`, and `K10S_BACKEND=interp` for node/k10s), `metal/test.sh`, `metal/cluster-test.sh`; commit
   crates + metal; push.
2. DONE — Commit the benchmark harness; commit the research reports.
3. DONE — final quiet run of `bench/run-final.sh` (2026-10-19 21:35 → 2026-10-20 00:52) → `bench/results/COMPARISON.md`.
4. DONE — Write `docs/REPORT.md` (final results: what was built, measured gains vs Kubernetes, test evidence, limitations,
   roadmap) and refresh `README.md` numbers; commit; push.

## Conventions
- Commit as `Quentin Gerard <quentingerard8@gmail.com>` via `git -c user.name=… -c user.email=…`, NO trailer
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
