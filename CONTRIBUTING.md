# Contributing to k10s

Thanks for your interest! k10s questions every layer of today's orchestration stack, so proposals that challenge an
existing design choice are welcome — bring measurements.

## Getting started
```bash
cargo build --release -p k10s
```
```bash
cargo test -p k10-nospec -p k10-ir -p k10-host -p k10-mesh -p k10-lang -p k10-jit -p k10-node -p k10s
```
The node and CLI suites run on the JIT by default; run them on the interpreter too:
```bash
K10S_BACKEND=interp cargo test -p k10-node -p k10s
```
The bare-metal kernel is a separate workspace: `metal/build.sh` then `metal/test.sh` (needs QEMU with KVM).
Benchmarks: see `bench/README.md`.

## Where things live
| Path | What |
|---|---|
| `docs/` | design (`DESIGN.md`), specs (`TIR.md`, `HOSTAPI.md`, `JIT-ABI.md`, `TEN.md`), runtime (`NODE.md`), status |
| `crates/k10-ir` | TIR types, codec, validator, reference interpreter, program generator |
| `crates/k10-nospec` | branch-free clamps for host code indexed by guest values (Spectre v1) |
| `crates/k10-host` | the host API, shared by the hosted node and the bare-metal kernel |
| `crates/k10-lang` | Ten compiler and prelude |
| `crates/k10-jit` | TIR → x86-64 compiler |
| `crates/k10-mesh` | gossip, manifests, bundles, placement, wire protocol |
| `crates/k10-node`, `crates/k10s` | node runtime and the `k10s` binary |
| `metal/` | the node as a bare-metal kernel |
| `bench/` | Kubernetes / bare-process / k10s benchmarks |
| `editors/vscode` | VS Code extension for Ten sources and `.k10s` manifests |
| `website/` | the documentation site (GitHub Pages) |
| `scripts/` | versioning, release builds and CI test suites (below) |
| `.github/` | workflows (`ci.yml`, `release.yml`, `pages.yml`, `vscode.yml`) and the pinned toolchains (`actions/toolchains`) |

## CI, versions and releases
- **CI** (`.github/workflows/ci.yml`, every pull request) runs `scripts/ci-test.sh unit musl metal-selftest os`,
  `scripts/dist.sh` then `scripts/ci-test.sh dist`; the same commands work locally. `unit` = all crates in release
  mode on the JIT, the node and CLI also on the interpreter, `#[ignore]`d long tests excluded; `musl` = the same for
  the static binary's target; `os` = PID 1 mode under QEMU on the Debian and k10s kernels (os/test.sh, docs/OS.md);
  `dist` = the k10s integration tests against the archived static binary (`K10S_BIN`).
  Non-blocking: `metal` (metal/test.sh, latency-bound) and the chaos/soak `campaign` (~7 min). Toolchains are pinned
  in `.github/actions/toolchains/action.yml` (Rust 1.98.1, Go 1.25.1); change them there.
- **Releases** (`.github/workflows/release.yml`): a push to `main` publishes the pre-release `vX.Y.Z-dev.N` (only
  for the current head of `main`, and not for a commit that carries the stable tag), a pushed tag `vX.Y.Z` the stable
  release, once `unit`, `musl`, `dist`, `metal-selftest` and `os` pass. Artifacts and images get build-provenance
  attestations; the newest 10 dev pre-releases and dev images are kept. See README.md, "Releases and versions".
- **Repository settings** (to apply once, by an admin): stable tags immutable and admin-only, `main` without force
  pushes or deletion (the build number is the commit count). The workflow creates and prunes dev tags itself, so
  the tag ruleset excludes them. On GHCR, make the `k10s-bench-*` packages public and give this repository the
  *Admin* role under "Manage Actions access" (needed to prune old dev images).
  ```sh
  gh api -X POST repos/bzzdevlabs/k10s/rulesets --input - <<'EOF'
  {"name": "stable tags", "target": "tag", "enforcement": "active",
   "conditions": {"ref_name": {"include": ["refs/tags/v*"], "exclude": ["refs/tags/v*-dev.*"]}},
   "bypass_actors": [{"actor_id": 5, "actor_type": "RepositoryRole", "bypass_mode": "always"}],
   "rules": [{"type": "creation"}, {"type": "update"}, {"type": "deletion"}]}
  EOF
  gh api -X POST repos/bzzdevlabs/k10s/rulesets --input - <<'EOF'
  {"name": "main", "target": "branch", "enforcement": "active",
   "conditions": {"ref_name": {"include": ["~DEFAULT_BRANCH"], "exclude": []}},
   "rules": [{"type": "deletion"}, {"type": "non_fast_forward"}]}
  EOF
  ```
- **The version** has one source: `scripts/version.sh`. Never edit the version of only one workspace: use
  `scripts/set-version.sh X.Y.Z`, which also updates both lock files (CI builds with `--locked`). Binaries embed it
  through their build scripts (`scripts/buildinfo.rs`, included by `crates/k10s/build.rs`, `metal/kernel/build.rs`,
  `metal/mkbundle/build.rs`). They rerun only when HEAD, the branch, the tags or the manifests change, or when the
  crate itself is rebuilt: the commit hash is always exact, while `.dirty` reflects the tree at that point (edits
  elsewhere never trigger rebuilds, and neither workspace rebuilds the other). `K10S_BUILD_VERSION` /
  `K10S_BUILD_COMMIT` override it (CI computes it once per run and passes it to every build).
  A new binary that should report the version includes the same helper.
- Do not hand-edit release artifacts: `scripts/dist.sh` builds them all into `dist/` (gitignored) with the version
  checked inside each binary.

## Rules of thumb
- **The validator and the JIT are the security boundary.** Any change to `crates/k10-ir/src/validate.rs` or
  `crates/k10-jit` must keep the differential test (`tests/diff.rs`) and the red-team suite (`tests/redteam.rs`)
  green, and should run a release soak (`soak` in `tests/diff.rs`) before merging.
- Minimal dependencies: the core crates have none besides `libc`; justify any new one in the PR.
- Every `unsafe` block states its invariants in a comment.
- `crates/k10-ir/src/types.rs` (TIR) and `docs/JIT-ABI.md` are frozen interfaces: changes need a design discussion.
- Keep docs in sync with behaviour; numbers in docs must come from a reproducible command.
- `cargo clippy` clean, `rustfmt` formatted.

## Licensing
k10s is licensed under Apache-2.0. By contributing you agree that your contributions are licensed under the same
terms (inbound = outbound). Do not copy code from GPL-licensed projects (e.g. Linux drivers) into this repository.
