# k10-ir implementation notes

Decisions taken where docs/TIR.md is silent or ambiguous, and facts other components may rely on.
docs/TIR.md was **not** edited.

## Spec interpretations (validation, §5/§7)

1. **`locals ≤ 65536`** is read as `params + locals ≤ 65536` (total local slots), the quantity a back end
   allocates. A function with `params = 1, locals = 65535` is valid; `locals = 65536` is not.
2. **Nesting depth ≤ 1024** counts simultaneously open `block`/`loop`/`if` labels, *not* counting the function body.
3. **Operand stack depth ≤ 1024** is the maximum operand-stack height reached in a function.
4. **Code ≤ 1 MiB** is the size of the encoded body (`code_len`, including `br_table` immediates). For in-memory
   modules it is computed with `Func::encoded_len` / `codec::op_encoded_len`.
5. **Memory immediates** must be `< 2^31` (§2 says `o < 2^31`; §7 does not list it, so it is enforced as part of
   "every instruction well-formed"). The decoder accepts any `u32` so that encode/decode stay total.
6. **Dead code** (§5): after `unreachable`/`br`/`br_table`/`return` the next instruction must be `end` or `else`;
   the height check at that `end`/`else` is skipped — including the results check at the function's final `end`
   (`[unreachable, end]` is valid for a function with a result). The height after it is 0.
7. **Exports** must name defined functions (exporting an import is an error); duplicate export *names* are errors,
   exporting one function under several names is fine. Duplicate **import** names are allowed (spec is silent).
8. **Data segments**: `offset + len ≤ 2^min_mem_log2` (an empty segment at offset `2^k` is valid). Segments may
   overlap; `init_memory` applies them in order (later wins).
9. Magic and version are checked by `decode` (the in-memory `Module` has no version field).

## Codec (§4)

* The decoder accepts **only canonical (minimal) LEB128**, so every accepted byte string re-encodes to itself
  (`encode(decode(b)?) == b`); overlong encodings → `LebOverlong`, values that do not fit (`u32` for indices,
  depths, offsets, `locals`; `u64` for data offsets; signed 64-bit for `const`) → `LebOverflow`.
* `br_table` tables are numbered in code order on decode (canonical form). `decode(encode(m)) == m` holds for
  canonical modules (what the builder, the generator and the decoder produce). `encode` is total: a `BrTable(t)`
  with no such table (or an empty one) is written as `n = 0, default = u32::MAX`, which stays invalid.
* Allocation is bounded by a constant factor of the input length: every `with_capacity` is capped by the remaining
  bytes divided by the minimum encoded size of an item.

## Interpreter

* **Prepared form**: one linear pass per function turns the stack code into register-style instructions over a
  frame `[locals | operand stack]`; stack positions map to fixed slots because heights are static (§5).
  `local.get`/`const` are folded into consumers, pure ops on constants are folded at prepare time (never ones that
  would trap), `op; local.set x` writes `x` directly, common binary ops have an immediate form, and all branch
  targets are absolute. The result is immutable and `Send + Sync`. The whole crate is `#![forbid(unsafe_code)]`.
* **Calls** use an explicit frame stack; the callee's frame starts at the caller's argument slots (no copying).
  Arguments of a call are the only values that must be materialized.
* **Safepoints**: function entry (including the invoked function, before its first instruction) and every
  execution of a `loop` instruction — a branch to a loop label jumps *to the `loop` instruction*, so each iteration
  passes the safepoint. Host imports are not safepoints. Default: `Host::interrupt` at every safepoint;
  `InterpConfig::safepoint_interval = n` (clamped to 1..=1024) calls it every n-th safepoint of an invocation.
* **Stack budget** (`Trap::StackOverflow`): `max_call_depth` active frames (default 10 000, the invoked function
  counts) and `max_stack_slots` 8-byte slots over all frames (default 4 Mi = 32 MiB, needed because one frame can
  be ~66 000 slots). Order at a call: depth check, slot check, then the callee-entry safepoint.
* **Invocation errors** never panic and run no code: memory length `≠ 2^k + 8` or `k ∉ max(12, min_mem_log2)..=36`
  → `Trap::MemOutOfBounds`; `func` not a defined function or `args.len() ≠ params` → `Trap::Unreachable`.
  `max_call_depth = 0` → `StackOverflow`.
* **Traps** leave memory exactly as it was at the trapping instruction (including writes a host made before
  returning an error). Bulk ops check before writing anything and never touch the 8-byte tail.
* **Host calls** receive the argument slice in parameter order and the whole memory (`2^k + 8` bytes); for an import
  with `results = 0` the returned value is ignored. `sem::check_range(ptr, len, S)` implements the `mem.copy`
  range rule for host-API buffers (docs/HOSTAPI.md).
* **NaNs**: float ops are native `f64` ops, so NaN sign/payload is whatever the CPU and compiler produce — it can
  even differ between two builds of the same code (LLVM may commute `fadd`/`fmul` operands), which the spec allows.
  Differential tests must not compare NaN bits unless programs canonicalize them (the generator does).
* **`f.sqrt` without `std`**: a software, correctly rounded square root (`sem::soft_sqrt`), tested bit-identical to
  the hardware instruction on ~1.5 M inputs including subnormals.

## Generator (feature `gen`)

* Termination: counted loops (`const K; local.set c; block; loop; c == 0 → exit; c -= 1; …`), a DAG call graph
  (function `i` calls only `j > i`) plus at most one self-recursive function whose depth argument is clamped at
  entry, and a static cost model: every export executes at most ≈ `2 × cost_budget` TIR instructions for any
  arguments (max observed over 100 000 modules: 89 347 with the default budget of 100 000).
* `canonical_nans` (default) rewrites every `f.add/sub/mul/div/sqrt` result to `x == x ? x : 0x7FF8…0`, and random
  code never reads the scratch local holding the raw value, so results are bit-comparable across implementations.
* `gen::DiffHost` is a deterministic host for comparing implementations (log of host calls, memory-dependent
  results, occasional `Host` traps, fuel-based interrupts). When comparing a JIT with the interpreter use
  `fuel = u64::MAX` and do not compare interrupt counts (implementations may call `interrupt()` at different rates).

## Testing

* `cargo test -p k10-ir --all-features`: per-op semantics, one negative test per validation rule, codec round-trips,
  ≥ 120 000 random/mutated decoder inputs, end-to-end programs, and a differential test of the prepared interpreter
  against a naive spec-transliteration interpreter (`src/tests/naive.rs`) on ~3 000 generated modules.
* Longer campaigns (use `--release`):
  `K10_SOAK_SEEDS=100000 cargo test -p k10-ir --release --all-features -- --ignored soak` and
  `K10_FUZZ_ITERS=3000000 cargo test -p k10-ir --release --all-features decoder_robustness`.
* Throughput: `cargo test -p k10-ir --release --all-features -- --ignored --nocapture perf`.
  Set `NAIVE_TRACE=1` to trace the naive interpreter when investigating a mismatch.
