# Contributing to k10s

Thanks for your interest! k10s questions every layer of today's orchestration stack, so proposals that challenge an
existing design choice are welcome — bring measurements.

## Getting started
```bash
cargo build --release -p k10s
```
```bash
cargo test -p k10-ir -p k10-mesh -p k10-lang -p k10-jit -p k10-node -p k10s
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
| `crates/k10-lang` | Ten compiler and prelude |
| `crates/k10-jit` | TIR → x86-64 compiler |
| `crates/k10-mesh` | gossip, manifests, bundles, placement, wire protocol |
| `crates/k10-node`, `crates/k10s` | node runtime and the `k10s` binary |
| `metal/` | the node as a bare-metal kernel |
| `bench/` | Kubernetes / bare-process / k10s benchmarks |
| `scripts/` | versioning, release builds and CI test suites (below) |
| `.github/` | workflows (`ci.yml`, `release.yml`) and the pinned toolchains (`actions/toolchains`) |

## CI, versions and releases
- **CI** (`.github/workflows/ci.yml`, every pull request) runs `scripts/ci-test.sh unit musl metal-selftest metal`
  and `scripts/dist.sh`; the same commands work locally. `unit` = all crates in release mode on the JIT and on the
  interpreter, `#[ignore]`d long tests excluded; `musl` = the same for the static release binary's target. The
  chaos and soak campaigns (`scripts/ci-test.sh campaign`, ~7 min) also run there but never block. Toolchains are
  pinned in `.github/actions/toolchains/action.yml` (Rust 1.98.1, Go 1.25.1); change them there.
- **Releases** (`.github/workflows/release.yml`): every push to `main` publishes the pre-release `vX.Y.Z-dev.N`, a
  pushed tag `vX.Y.Z` the stable release, once the gating suites pass (`unit`, `musl`, `metal-selftest`). See
  README.md, "Releases and versions", for the scheme and how to cut a release.
- **The version** has one source: `scripts/version.sh`. Never edit the version of only one workspace: use
  `scripts/set-version.sh X.Y.Z`, which also updates both lock files (CI builds with `--locked`). Binaries embed it
  through their build scripts (`scripts/buildinfo.rs`, included by `crates/k10s/build.rs`, `metal/kernel/build.rs`,
  `metal/mkbundle/build.rs`), which rerun when HEAD, the branch, the tags, the manifests or a crate's `src/` change;
  `K10S_BUILD_VERSION` / `K10S_BUILD_COMMIT` override it (CI computes it once per run and passes it to every build).
  A new binary that should report the version includes the same helper.
- Do not hand-edit release artifacts: `scripts/dist.sh` builds them all into `dist/` (gitignored) with the version
  checked inside each binary.

## Rules of thumb
- **The validator and the JIT are the security boundary.** Any change to `crates/k10-ir/src/validate.rs` or
  `crates/k10-jit` must keep the differential test (`tests/diff.rs`) and the red-team suite (`tests/redteam.rs`)
  green, and should run a release soak (see `crates/k10-jit` docs) before merging.
- Minimal dependencies: the core crates have none besides `libc`; justify any new one in the PR.
- Every `unsafe` block states its invariants in a comment.
- `crates/k10-ir/src/types.rs` (TIR) and `docs/JIT-ABI.md` are frozen interfaces: changes need a design discussion.
- Keep docs in sync with behaviour; numbers in docs must come from a reproducible command.
- `cargo clippy` clean, `rustfmt` formatted.

## Licensing
k10s is licensed under Apache-2.0. By contributing you agree that your contributions are licensed under the same
terms (inbound = outbound). Do not copy code from GPL-licensed projects (e.g. Linux drivers) into this repository.
