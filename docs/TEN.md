# Ten — the k10s workload language (v0)

Ten is a small systems language: C-level control, typed pointers and structs, no GC, no runtime, no hidden costs.
It compiles to TIR (docs/TIR.md). Safety *for the platform* comes from TIR's sandbox (a bug can only corrupt the
instance's own memory), so Ten stays close to the machine while k10s stays safe.

This document describes exactly what the compiler (`crates/k10-lang`) accepts. Examples: `examples/*.ten`.

## Lexical
- Comments: `// …` and `/* … */` (block comments nest).
- Identifiers `[A-Za-z_][A-Za-z0-9_]*`. Keywords: `fn export import const global data buffer struct include let if
  else while loop for in break continue return as null true false sizeof offsetof countof`.
- Integers (64-bit patterns): `42`, `0x2A`, `0b101`, `0o17`, `1_000`. Literals up to `2^64 - 1` are accepted and
  wrap to i64 (`0xFFFF_FFFF_FFFF_FFFF == -1`). Characters `'a'`, `'\n'`, `'\x41'` are byte values (i64);
  escapes: `\n \t \r \0 \\ \' \" \xNN`. `true`/`false` are 1/0.
- Floats (f64): `1.5`, `2e9`, `3.0e-4` (a digit is required after `.`: `1.` is not a float, so `0..n` lexes as a range).
- Strings `"…"` with the same escapes; any UTF-8 is kept as bytes. No NUL terminator is added.

## Types
- **Value types** (locals, parameters, results, globals, expression values): `i64`, `f64`, pointers `*T`, and
  `str` (parameters and locals only).
- **Storage types** (pointees, struct fields, array elements): `u8 i8 u16 i16 u32 i32 i64 f64`, pointers, structs,
  arrays `[T; N]`. Loading a small integer widens it to i64 (zero- or sign-extended per its type); storing
  truncates. A place of array type decays to a pointer to its first element.
- `*T` is an i64 address at runtime. `p + n` / `n + p` / `p - n` advance by `n * sizeof(T)`; `p - q` (same `T`)
  is the element difference; pointer comparisons are unsigned. Any `*T` converts implicitly to `*u8`
  (the "bytes" pointer); `null` converts to any pointer type. Every other conversion is explicit with `as`.
- **No implicit numeric conversions**: mixing i64 and f64 is an error (`x as f64`, `y as i64`); an integer literal
  where an f64 is expected is an error with a hint (`2.0`).
- Casts: `i64 ↔ f64` (f64 → i64 truncates toward zero, saturating, NaN → 0), `i64 ↔ *T`, `*T → *U`, and
  truncating casts to small integer types that yield an i64: `x as u8` (`& 0xFF`), `x as i8` (sign-extend low
  byte), likewise `u16 i16 u32 i32`. Floats can be cast to small integer types (through i64).
- `str` is a (pointer, length) pair occupying two values: `s.ptr: *u8`, `s.len: i64`. A `str` is produced by a
  string literal or a byte `data` item where a `str` is expected, by another `str` variable, or by
  `str(ptr, len)`. It can be passed to `str` parameters (it becomes two TIR parameters), stored in `str` locals
  and reassigned; it cannot be returned, stored in memory or used in arithmetic.
- Conditions (`if`, `while`, `&&`, `||`, `!`, `select`) accept i64 or pointers (non-zero = true), never f64.
- Struct values do not exist: a struct is only reachable through a pointer or a place (`p.field`, `p[i].field`,
  `(*p).field`). `let q = p[i];` on a struct pointer is an error (use `p + i`); whole-struct assignment is an
  error (use `memcpy`).

## Items (top level, order-independent)
```
import fn output_write(src: *u8, n: i64) -> i64;    // host function (docs/HOSTAPI.md), at most 5 values
fn add(a: i64, b: i64) -> i64 { return a + b; }
export fn handle() -> i64 { ... }                    // exported entry point (handle/init: no params, () or i64)
const LIMIT = 100;                                   // compile-time i64 or f64; `const X: f64 = 1.0;`
global hits: i64 = 0;                                // one value-type cell in memory (constant initializer)
global ratio = 0.5;                                  // type inferred from the initializer; `global p: *u8 = null;`
data HELLO = "hello\n";                              // static bytes; HELLO: *u8; sizeof(HELLO) == 6
data K: [u32; 4] = [1, 2, 3];                        // static array (missing elements are 0); K: *u32
data T: [i16] = [-2, 300];                           // length inferred from the list
buffer scratch: [u8; 4096];                          // static zeroed storage; scratch: *u8
buffer one: Point;                                   // a single element: one: *Point
struct Point { x: f64, y: f64, tag: u8, name: [u8; 8], next: *Point }   // C layout (natural alignment)
include "lib/util.ten";                              // path relative to the including file; each file once
```
- Constant expressions (const, global initializers, data elements, array lengths): literals, other consts,
  `sizeof`/`offsetof`/`countof`, arithmetic, comparisons, `&&`/`||`, casts. Division by zero is an error.
- `data` and `buffer` memory is mutable. Data elements must fit their type (`[u8]` rejects 256).
- Names live in two scopes: the prelude's and the user's (all user files). User code sees its own items first,
  then the prelude's; prelude code only sees the prelude. So a user `fn log(...)` or `fn min(...)` shadows the
  prelude's for user code without breaking prelude functions that use theirs. Duplicates within one scope are
  errors. Built-in type names cannot be redefined. User functions may shadow builtin functions (`sqrt`, …).

## Statements
```
let x = 1;  let y: f64 = 2.0;  let p: *Point = alloc(sizeof(Point)) as *Point;
let z: i64;                     // zero-initialized
let s: str = "abc";
x = x + 1;  x += 2;             // also -= *= /= %= &= |= ^= <<= >>= >>>=
p.x = 1.5;  p[3].y = 2.0;  *q = 7;  buf[i] = 'a';  p.name[2] = 0;  g = 5;   // g a global
if c { } else if d { } else { }
while c { }    loop { }    for i in a..b { }    break;  continue;
return;  return x;
f(x);           // expression statement: must be a call (`a + 1;` and `a == 1;` are errors)
{ ... }         // block (new scope)
```
- Locals are block-scoped; shadowing is allowed (`let x = x + 1;` reads the outer `x`).
- `for i in a..b` evaluates `a` then `b` once, runs with `i = a, a+1, …, b-1` (nothing if `a >= b`); `i` is a
  fresh immutable i64 (assigning it is an error).
- Compound assignment to a memory place evaluates the address once, loads the current value, then evaluates the
  right-hand side: `a[f()] += g()` calls `f` before `g`.
- A function returning a value must `return` on every path; the checker understands `return`, `unreachable()`,
  `loop` without `break`, `while` with a constant non-zero condition without `break`, and `if`/`else` where both
  branches diverge. Statements after a diverging statement are checked but not compiled.

## Expressions
Precedence, low → high (all binary operators are left-associative):

| level | operators |
|---|---|
| 1 | `\|\|` |
| 2 | `&&` |
| 3 | `== != < <= > >=` (non-associative: `a < b < c` is an error) |
| 4 | `\|` |
| 5 | `^` |
| 6 | `&` |
| 7 | `<< >> >>>` |
| 8 | `+ -` |
| 9 | `* / %` |
| 10 | `as` (postfix: `-x as f64` is `(-x) as f64`, `a + b as f64` is `a + (b as f64)`) |
| 11 | unary `- ! ~ *` |
| 12 | postfix: call `f(…)`, field `.f` (auto-dereferences `*Struct`), index `[i]` |

Bitwise operators bind tighter than comparisons (unlike C): `x & 1 == 0` is `(x & 1) == 0`.

- Integer arithmetic wraps (two's complement). `/` and `%` are signed and trap on a zero divisor
  (`MIN / -1 == MIN`, `MIN % -1 == 0`). Shift counts are taken modulo 64; `>>` is arithmetic, `>>>` logical.
- Comparisons and `!`, `&&`, `||` yield 0/1. `&&`/`||` short-circuit. Float comparisons with NaN are false
  (`!=` is true).
- f64 supports `+ - * /`, unary `-`, comparisons (IEEE-754, round to nearest even).
- **Evaluation order** is left to right everywhere: operands, call arguments, `select` arguments, address before
  value in stores. Expressions cannot assign variables; only calls have side effects.
- Indexing `p[i]` requires a pointer and an i64 index; there are no bounds checks (TIR masks every address into the
  instance memory).

### Builtins
| builtin | meaning |
|---|---|
| `sizeof(T \| data \| buffer \| "lit" \| variable)` | size in bytes (constant): types, a data/buffer item's total size, a string literal's length, 8 for a value (16 for `str`) |
| `offsetof(Struct, field)`, `countof(data \| buffer)` | field offset; number of elements (constants) |
| `memcpy(dst, src, n)`, `memset(dst, byte, n)` | TIR `mem.copy` (memmove) / `mem.fill`; trap if a range leaves the memory |
| `memsize()` | instance memory size `S` |
| `sqrt(f)`, `fabs(f)` | f64 |
| `clz(x)`, `ctz(x)`, `popcnt(x)`, `rotl(x, n)`, `rotr(x, n)` | bit operations (`clz(0) == ctz(0) == 64`) |
| `ult ule ugt uge (a, b)` | unsigned comparisons (i64 or pointers) |
| `udiv(a, b)`, `urem(a, b)` | unsigned division (trap on zero) |
| `select(c, a, b)` | `c != 0 ? a : b` without a branch; evaluates all three, in order |
| `str(p, n)` | builds a `str` |
| `to_bits(f)`, `from_bits(i)` | reinterpret f64 ↔ i64 bits (free) |
| `unreachable()` | trap `Unreachable` |
| `__heap_base` | first heap address (constant, 16-aligned) |

A string literal used where a pointer is expected has type `*u8` and points to an interned static copy (identical
literals share storage).

## Memory layout produced by the compiler
`[0, 16)` reserved (null) · globals (8 bytes each) · data · string literals · buffers (8-aligned) ·
`__heap_base` (16-aligned) … `memsize()`. Only items reachable from the exports are laid out. Initial bytes are
emitted as TIR data segments (zeros trimmed; runs closer than 16 bytes merged).
`min_mem_log2` = smallest power of two ≥ `__heap_base + 4096` (at least 12): the static area plus 4 KiB of heap.
Deployments may choose larger memories; the heap grows up to `memsize()`.

## Compilation model
- Entry points are `export fn`s; everything not reachable from them (functions, host imports, statics, the unused
  part of the prelude) is dropped. Each host import appears once, only if called.
- Calls to *forwarders* (functions whose body is a single call passing their parameters in order, plus constants)
  are redirected to the inner function: `out_str("hi")` compiles to a direct `output_write` host call.
- Code generation targets a single-pass JIT: constants are folded (never an operation that would trap), constant
  offsets of addresses (struct fields, constant indices, `+ c`) become load/store offset immediates, indices are
  scaled with shifts for power-of-two sizes, `x * 2^k` / `udiv`/`urem` by `2^k` become shifts/masks, conditions
  compile to `br_if` on inverted comparisons and short-circuit branch chains (`&&`/`||` between cheap, pure 0/1
  operands become branch-free `&`/`|`), `for`/`while` loops are rotated (one
  backward `br_if` per iteration), locals and temporaries reuse TIR local slots, `local.tee` replaces
  `local.set`/`local.get` pairs, and unreachable code is not emitted.
- TIR requires an empty operand stack at control instructions; `&&`/`||` in value position after other operands
  spill those operands to temporaries (constants and locals are simply re-read).
- Every emitted module passes `k10_ir::validate`.

## Limits
Expressions and blocks nest at most 200 levels (each operator of a chain like `a + b + c` counts as a level);
at most 255 parameter values per function, 5 per host import; TIR limits (65536 locals, 1024 operand stack /
nesting, 1 MiB of code per function) are reported as errors. The compiler stops after 100 errors.

## Diagnostics and API
```rust
k10_lang::compile(src: &str, path: Option<&Path>) -> Result<k10_ir::Module, Vec<Diagnostic>>
k10_lang::compile_file(path: &Path) -> Result<Module, Vec<Diagnostic>>        // resolves `include`
k10_lang::compile_with_sources / compile_file_with_sources(.., &mut Sources)  // also collect source texts
Diagnostic { file, line, col, len, message, help }   // 1-based line/col (characters)
diagnostic.render(&sources) / render_diagnostics(&diags, &sources)            // rustc-style with a caret
```
```
error: cannot find `totl` in this scope
 --> demo.ten:3:12
  |
3 |     return totl + 1;
  |            ^^^^
  = help: did you mean `total`?
```
Errors cover syntax (with recovery: several errors per run), unknown names and types (with "did you mean"),
type mismatches (with conversion hints), arity, missing returns, invalid assignment targets, `break` outside
loops, duplicate definitions, recursive structs and constants, data initializer ranges, entry-point signatures.
With the `testhost` cargo feature, `k10_lang::testhost::TestHost` runs compiled services on the `k10_ir`
interpreter with an in-memory implementation of the whole host API.

## Standard prelude (`std.ten`, always available; unused parts are dropped)
Source: `crates/k10-lang/src/std.ten`.
- **Host API**: every function of docs/HOSTAPI.md, declared with its HOSTAPI name and parameters (`input_len`,
  `input_read`, `req_path`, `req_method`, `output_write`, `output_status`, `call`, `call_resp_len`,
  `call_resp_read`, `kv_get`, `kv_put`, `kv_del`, `kv_add`, `kv_cas`, `log`, `now_ns`, `time_ms`, `random`,
  `config`).
  Constants `GET POST PUT DELETE OTHER_METHOD`, `CALL_UNKNOWN CALL_UNAVAILABLE CALL_TIMEOUT CALL_TRAPPED`,
  `I64_MIN I64_MAX`.
- **Allocation** (power-of-two size classes with an 8-byte header and per-class free lists over a bump pointer;
  8-byte aligned): `alloc(n) -> *u8` (traps when memory is exhausted), `try_alloc(n)` (null instead), `zalloc(n)`,
  `free(p)`, `heap_reset()` (frees everything: call it at the start of `handle` for a per-request arena),
  `heap_used()`.
- **Output**: `out(p, n)`, `out_str(s: str)` (`out_str("hi\n")`, `out_str(DATA)`, `out_str(str(p, n))`),
  `out_byte(c)`, `out_int(v)`, `out_hex(v)`, `out_hex_bytes(p, n)`, `out_f64(x, prec)`, `out_json_str(p, n)`
  (quoted and escaped), `out_body()` (stream the request body to the response), `status(code)`.
- **Input**: `read_body() -> *u8` (heap, NUL-terminated), `read_path() -> *u8` (path suffix *including* the
  `?query`, NUL-terminated), `last_len()` (length of the last buffer returned by `read_body`, `read_path`,
  `kv_read`, `call_read`, `read_config`), `query_int(name: str, default)` (`?n=123`, Go `Atoi` rules, otherwise
  `default`), `query_str(name, dst, max) -> len | -1`, `query_find(path, n, name)`, `read_config()`.
- **Parsing and formatting**: `parse_int(p, n)` (lenient), `is_int(p, n)` (strict, overflow-checked),
  `itoa(v, dst)`, `utoa(v, dst)`, `fmt_hex(v, dst)`, `hex_bytes(p, n, dst)`, `fmt_f64(x, prec, dst)` (each returns
  the length written).
- **Bytes**: `mem_eq(a, b, n)`, `find_byte(p, n, b)`, `strlen(p)`, `str_eq(p, n, s: str)`,
  `starts_with(p, n, s: str)`, `copy_str(dst, s: str)`, `is_digit(c)`, `is_space(c)`, `fnv1a(p, n)` (64-bit).
- **KV and calls**: `kv_read(key: str) -> *u8 | null`, `kv_write(key, p, n)`, `kv_get_int(key, default)`,
  `kv_put_int(key, v)` (decimal text), `kv_delete(key)`, `kv_add_int(key, delta) -> new value` (atomic, on an 8-byte
  integer value; `delta = 0` reads; traps `Host(4)` on a value that is not 8 bytes, e.g. one written by
  `kv_put_int`), `kv_cas_bytes(key, exp, exp_len, new, new_len) -> 1 | 0` (atomic compare-and-swap; `exp_len = -1`:
  the key must be absent, `new_len = -1`: delete; copies `exp` and `new` into one heap buffer unless `new` directly
  follows the expected bytes), `call_svc(name: str, req, n) -> status | error`, `call_read() -> *u8`,
  `out_call_resp()`, `log_str(s: str)`. A `kv_read` then `kv_write` is not atomic: instances of a service run
  concurrently, so read-modify-writes use `kv_add_int` or a `kv_cas_bytes` retry loop (examples/todo.ten).
- **Misc**: `min`, `max`, `abs`, `fmin`, `fmax`, `align_up(x, a)`.

## Example
```
export fn handle() {
    heap_reset();
    let body = read_body();
    out_str("you said: ");
    out(body, last_len());
    if query_int("loud", 0) {
        out_str("!");
    }
}
```
