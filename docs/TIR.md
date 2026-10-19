# TIR — the k10s intermediate representation (v1)

TIR is what services ship as. It is validated and compiled to machine code on each node. Design goals: trivially
validatable, single-pass compilable to good machine code, compact, safe under software fault isolation (SFI).

Rust types: `crates/k10-ir/src/types.rs` (frozen — changes need the architect's approval).

## 1. Values
One value type: **i64**. `f64` values are carried as their IEEE-754 bit patterns in an i64; float instructions
reinterpret their operands. Booleans are i64: comparisons produce 1 or 0; conditions treat non-zero as true.

## 2. Memory
Each instance has one linear memory of size `S = 2^k` bytes (`k` chosen at deployment, `k ≥ module.min_mem_log2`,
`12 ≤ k ≤ 36`), followed by an 8-byte **tail** region `T` that is part of the instance's state.

Effective address of a load/store with address operand `a` and immediate offset `o` (o < 2^31):
`e = ((a + o) mod 2^64) & (S - 1)`. The access touches bytes `e .. e + width - 1`, which may extend into `T`.
All accesses are little-endian. There are **no out-of-bounds traps for loads/stores** — masking makes every address
valid. Memory starts zeroed; data segments are copied at instantiation.

Bulk operations: `mem.copy(dst, src, n)` and `mem.fill(dst, byte, n)` compute `d = dst & (S-1)`, `s = src & (S-1)`;
if `n` (as u64) `> S`, or `d + n > S`, or (copy) `s + n > S` → trap `MemOutOfBounds`; otherwise `memmove` / fill
with `byte & 0xFF`. `mem.size` pushes `S`.

## 3. Module structure
```
Module { min_mem_log2, imports[], funcs[], exports[], data[] }
Import { name, params (0..=5), results (0|1) }          // host functions, resolved by name at load
Func   { params (0..=255), results (0|1), locals, code[], br_tables[] }
Export { name, func }                                     // func must be a defined function (not an import)
Data   { offset, bytes }                                  // offset + len ≤ 2^min_mem_log2
```
Function index space: indices `0 .. imports.len()` are imports, then defined functions in order.
Locals: indices `0 .. params` are the parameters, then `locals` extra locals, all zero-initialized.

## 4. Binary encoding
All integers are unsigned LEB128 (`uleb`) unless noted; `sleb` = signed LEB128.
```
"K10M" version:u8(=1) min_mem_log2:u8
n_imports:uleb  { name_len:uleb name:utf8 params:u8 results:u8 }*
n_funcs:uleb    { params:u8 results:u8 locals:uleb code_len:uleb code:bytes }*
n_exports:uleb  { name_len:uleb name:utf8 func:uleb }*
n_data:uleb     { offset:uleb len:uleb bytes }*
```
Code = sequence of instructions (opcode byte + immediates). Immediates: depths, indices and offsets are `uleb`;
`const` is `sleb` (64-bit). `br_table` is `n:uleb depth_0 … depth_{n-1} default`.

## 5. Instructions
Stack effects are written `[inputs] → [outputs]`; for binary ops `a` is pushed first, `b` second.

### Control (structured)
| op | hex | imm | effect |
|---|---|---|---|
| unreachable | 00 | | trap `Unreachable` |
| nop | 01 | | |
| block | 02 | | open label; `br` to it jumps past its `end` |
| loop | 03 | | open label; `br` to it jumps to the loop start. **Safepoint.** |
| if | 04 | | `[c] → []`; open label; run then-part if `c ≠ 0`, else the else-part |
| else | 05 | | |
| end | 06 | | close innermost block/loop/if, or the function body |
| br | 07 | depth | jump to the label `depth` levels out (0 = innermost) |
| br_if | 08 | depth | `[c] → []`; branch if `c ≠ 0` |
| br_table | 09 | n, depths…, default | `[i] → []`; branch to `depths[i]` if `i` (as u64) `< n` else `default` |
| return | 0A | | return from the function |
| call | 0B | func | `[args…] → [results]`; calls an import (host) or a defined function |

**Operand-stack discipline (the key simplification).** The operand stack height (relative to the function's start)
must be exactly:
- `0` at `block`, `loop`, `else`, `end` (of a block/loop/if) and `br`;
- `1` at `if`, `br_if`, `br_table` (the consumed value), after which it is 0;
- `results` at `return` and at the function's final `end`.

Branches never carry values; values flow through locals. The function body's implicit outer label is **not** a
branch target (use `return`). After `unreachable`, `br`, `br_table` or `return`, the next instruction must be
`end` or `else`, and stack-height checks at that `end`/`else` are skipped (it is reachable only by branches).
Consequence: at every control-flow join the operand stack is empty, so a single-pass compiler never needs to
reconcile register states at merges. Calls may happen with a non-empty stack.

### Stack and locals
| op | hex | imm | effect |
|---|---|---|---|
| drop | 10 | | `[a] → []` |
| select | 11 | | `[a, b, c] → [c ≠ 0 ? a : b]` |
| local.get | 12 | idx | `[] → [local]` |
| local.set | 13 | idx | `[a] → []` |
| local.tee | 14 | idx | `[a] → [a]` and store |
| const | 15 | sleb | `[] → [v]` |

### Memory (imm = offset)
| op | hex | effect |
|---|---|---|
| load8_u, load8_s | 20, 21 | `[a] → [v]` zero/sign-extended |
| load16_u, load16_s | 22, 23 | |
| load32_u, load32_s | 24, 25 | |
| load64 | 26 | |
| store8, store16, store32, store64 | 27, 28, 29, 2A | `[a, v] → []` stores the low bytes of `v` |
| mem.copy | 2B (no imm) | `[dst, src, n] → []` |
| mem.fill | 2C (no imm) | `[dst, byte, n] → []` |
| mem.size | 2D (no imm) | `[] → [S]` |

### Integer (wrapping two's complement)
| op | hex | notes |
|---|---|---|
| add sub mul | 30 31 32 | |
| div_s div_u rem_s rem_u | 33 34 35 36 | divisor 0 → trap `DivByZero`; `div_s(MIN,-1) = MIN`, `rem_s(MIN,-1) = 0` |
| and or xor | 37 38 39 | |
| shl shr_s shr_u rotl rotr | 3A 3B 3C 3D 3E | count is `b & 63` |
| clz ctz popcnt | 3F 40 41 | unary; `clz(0) = ctz(0) = 64` |
| eqz | 42 | unary, → 1/0 |
| eq ne lt_s lt_u gt_s gt_u le_s le_u ge_s ge_u | 43 … 4C | → 1/0 |
| ext8_s ext16_s ext32_s | 4D 4E 4F | unary sign extension of the low 8/16/32 bits |

### Float (operands/results are f64 bit patterns)
| op | hex | notes |
|---|---|---|
| f.add f.sub f.mul f.div | 50 51 52 53 | IEEE-754, round-to-nearest-even |
| f.sqrt f.neg f.abs | 54 55 56 | unary; neg/abs flip/clear the sign bit only |
| f.eq f.lt f.le | 57 58 59 | → 1/0; false if either operand is NaN |
| f.from_i | 5A | signed i64 → f64 (round to nearest even) |
| f.to_i | 5B | f64 → i64, truncate toward zero, saturating: NaN → 0, too large → i64::MAX, too small → i64::MIN |

NaN results of arithmetic have unspecified sign and payload (implementations may differ).

## 6. Safepoints, preemption, traps
Function entry and every `loop` instruction are **safepoints**: an implementation may suspend or interrupt the
invocation there (CPU time slicing, CPU budget). This is invisible to the program except through timing.

Traps abort the current invocation: `Unreachable`, `DivByZero`, `StackOverflow` (implementation-defined stack
budget; at least 1000 frames of a small function), `MemOutOfBounds` (bulk ops only), `Interrupted` (CPU budget),
`Host(code)` (raised by a host function). Instance memory is left as it was at the trap.

## 7. Validation rules (summary)
Magic/version; `12 ≤ min_mem_log2 ≤ 36`; data segments fit in `2^min_mem_log2`; import params ≤ 5, results ≤ 1;
func results ≤ 1; export names unique and refer to defined functions; every instruction well-formed; local indices
in range; call targets in range; branch depths `<` number of open labels (excluding the function body);
block nesting balanced, body ends with the function's `end`; operand stack never underflows and obeys §5's
discipline; nesting depth ≤ 1024; operand stack depth ≤ 1024; locals ≤ 65536; code ≤ 1 MiB per function.
