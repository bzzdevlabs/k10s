# k10s — project status and resume guide

PROJECT STATE: HARDENING

Last updated: 2026-10-21. This file is the single place to learn where the project stands and what comes next.

## State of `main`
Local `main` contains `origin/main` plus the security hardening below; it is not pushed yet (pushing publishes a dev
pre-release, step 8).
- Isolation: SFI-masked JIT and interpreter, validator, red-team and differential suites, objdump encoder fuzzing and
  SFI verifier tests.
- Control plane: bearer-token admin API, sealed gossip and encrypted, authenticated peer links (shared cluster key).
- Authority: per-service `calls` allowlist (manifest format 2, index-based, canonical).
- Side channels: `br_table` compare trees and clamped jump tables, opt-in lfence/retpoline, speculation-safe host
  bounds (`k10-nospec`), per-node SipHash-keyed fuzzy guest clock; one host ABI crate (`k10-host`) for hosted and metal.
- Defense in depth: two-phase start (bind, then `--user` drop, capability check, Landlock, seccomp, then workloads),
  root refused without `--user`, `--sandbox on|seccomp-only|log|off`.
- DoS: per-service memory reservations, bounded overshoot, placement versioned in gossip, rejoin through tombstone
  addresses and `DATA/peers`, KV store with ordered flushes and rewrite-from-memory on I/O errors.
- Releases: SemVer; tag `vX.Y.Z` on main → stable release, every push to main → `vX.Y.Z-dev.N` pre-release.

## Remaining steps, in order (one per session)
Rules: production-ready (no workarounds), comments only where needed as English one-liners, docs (incl. website/)
updated with every change and written as the current state, no perf regression (`bench/k10s/ab.py` vs 08f2921), no
push or tag before step 9, never create namespaces or seek privileges on this machine.

6. **Auth** — branch `worktree-agent-a34a5562a6c3410ee` (worktree under `.claude/worktrees/`; merge main first). Key
   confirmation before serving (pre-auth record cap, `Hello` first, records held until the peer's first record opens,
   cap on unauthenticated links); one `k10_mesh::secure::Link` for hosted and metal; exact-match boolean flags; signed
   admin requests (HMAC over method‖target‖t‖n‖sha256(body), skew window, nonce cache), node always requires a key
   (`DATA/admin.key`, 0600, when none is given), `metrics` open only on `--admin`; metal secrets via fw_cfg; gossip
   replay state kept until max(ts, last) + skew + margin, sender id in AAD, no skew-0 mode; metal wall-clock resync;
   cluster key rotation (current + next key, key id on the wire); zeroized key material; RDRAND failure fails the link.
7. **Performance** — main is 2–12 % below 08f2921 in `ab.py` runs (noisy) and a trivial invoke costs ~19 ns vs ~10 ns.
   Bisect 08f2921..main with `cargo bench -p k10-jit` / `crates/k10-node/examples/measure.rs` and `ab.py`, fix the
   causes without weakening any security property. `ab.py` gains an option to give the base the same manifest
   (`calls`) and cluster key when the base supports them.
8. **Integrate** — full suites on both backends, `metal/test.sh`, `metal/cluster-test.sh`; fix the load-sensitive
   flakes (`mesh_abuse::peer_that_never_reads_is_cut_off` on the interpreter, `placement::speed_1000_nodes_10k_replicas`)
   at their cause; quiet-machine `ab.py` (≥ 4 rounds) vs 08f2921; check README, docs/, SECURITY.md, docs/REPORT.md and
   every website/ page against the code, current state only.
9. **Push** — rewrite the unpushed commits to the commit convention below
   (`git filter-branch --env-filter … --msg-filter "sed '/^Co-Authored-By:/d'" origin/main..main`, which keeps trees
   and merges), push main, watch the release workflow, fix what the first real run shows.
10. **Optional perf** — shard-owned peer links: each shard dials and reads its own link per peer, the mesh thread keeps
    gossip, fetch and accept; expected 7 → 3 wake-ups per forwarded request (≈ −40 % CPU at 1 conn).
11. **Decisions (user)** — answer docs/REPORT.md §9 and the open questions of docs/research/universal-os.md §10 and
    docs/research/native-hardware.md; record the answers in docs/REPORT.md.
12. **Tier 1 universal image** (docs/TASK-universal-os.md, plan in docs/research/universal-os.md §9; prototypes on
    branch `research/universal-os`): milestones U0 → U1 → U2 → U3 → U4 (= Tier 2 M6) → U5 → U6, one session each.
13. **Tier 2 native hardware** (docs/TASK-native-hardware.md, plan in docs/research/native-hardware.md §9; prototypes
    on branch `research/native-hardware`): milestones M1 → M2 → M5 → M6 → M7 → M3 → M4, one session each.
14. **Final audit** — adversarial in-depth audit of the whole code base (isolation, JIT, host ABI, crypto and peer
    protocol, admin API, sandbox, KV durability, metal, release pipeline), every finding verified by independent
    refutation before it is reported, then fixed.
15. **User** — delete the GHCR images `k10s-bench-*` tagged `0.1.0`, `0.1.0-dev.46`, `latest`, `dev`, or grant `gh`
    the packages scope.

## Components
| Component | Where | Tests |
|---|---|---|
| Design, specs | `docs/DESIGN.md`, `TIR.md`, `HOSTAPI.md`, `JIT-ABI.md`, `TEN.md`, `NODE.md`, `research.md` | — |
| TIR: codec, validator, reference interpreter, builder, generator | `crates/k10-ir` | unit + property |
| Cluster layer: SWIM gossip, crypto, manifests, bundles, placement, wire, simulator | `crates/k10-mesh` | unit + simulator |
| Ten compiler, prelude, examples | `crates/k10-lang`, `examples/` | unit + examples |
| x86-64 JIT (single pass, SFI masking, safepoints, speculation hardening) | `crates/k10-jit` | differential soak, red-team, objdump |
| Node runtime, sandbox, CLI | `crates/k10-node`, `crates/k10s` | unit, cluster, chaos, soak, abuse, auth |
| Bare-metal node (PVH, virtio-net, smoltcp, JIT, SMP, cluster member) | `metal/` | `test.sh`, `cluster-test.sh` |
| Baselines (Kubernetes, bare processes), loadgen, k10s harness, A/B driver | `bench/` | — |
| Editor extension, documentation site | `editors/vscode`, `website/` | — |
| CI and releases | `.github/`, `scripts/` | — |

## Conventions
- Commit as `Quentin Gerard <quentingerard8@gmail.com>` via `git -c user.name=… -c user.email=…`, NO trailer
  `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`; push `main` to `origin` after each verified commit (from
  step 8 on).
- Use separate cargo target dirs per concurrent agent (`target/node`, `target/chaos`, …) to avoid lock contention.
- Never touch Docker containers/images not created by k10s scripts (the machine has the user's own).

## Open items
- Multi-hour soak to rule out the ~40 KB/min RSS drift seen once on a cluster node.
- Metal has no per-service memory reservations (it shares placement, not the accounting).
- Native-hardware open questions for the user: end of docs/research/native-hardware.md; decisions list in
  docs/REPORT.md §9.
