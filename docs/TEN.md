# Ten — the k10s workload language (v0)

Ten is a small systems language: C-level control, typed pointers and structs, no GC, no runtime, no hidden costs.
It compiles to TIR (docs/TIR.md). Safety *for the platform* comes from TIR's sandbox (a bug can only corrupt the
instance's own memory), so Ten stays close to the machine while k10s stays safe.

> This is the target design. The compiler author may refine details, but must keep this document in sync with what
> the compiler actually accepts.

## Lexical
`//` and `/* */` comments. Identifiers `[A-Za-z_][A-Za-z0-9_]*`. Integers: `42`, `0x2A`, `0b101`, `1_000`; chars
`'a'`, `'\n'`, `'\x41'` (i64). Floats: `1.5`, `2e9`, `3.0e-4` (f64). Strings `"..."` with `\n \t \r \\ \" \0 \xNN`.

## Types
- Value types (locals, params, returns, globals): `i64`, `f64`, pointers `*T`.
- Storage types (pointees, struct fields, arrays): `u8 i8 u16 i16 u32 i32 i64 f64`, structs, pointers.
  Loading a small integer widens to i64 (zero- or sign-extended per its type); storing truncates.
- `*T` is an i64 address at runtime. `p + n` advances by `n * sizeof(T)` (C semantics); `p - q` gives the element
  difference. Casts: `x as f64`, `x as i64`, `p as *u8`, `p as i64`, `n as *Point`.
- Mixing i64 and f64 without `as` is a type error. Conditions accept i64 or pointers (non-zero = true).

## Items (top level, order-independent)
```
import fn output_write(src: *u8, n: i64) -> i64;   // host function, see docs/HOSTAPI.md
fn add(a: i64, b: i64) -> i64 { return a + b; }
export fn handle() -> i64 { ... }                   // exported entry point
const LIMIT = 100;                                  // compile-time constant (i64 or f64)
global hits: i64 = 0;                               // static cell in memory
data HELLO = "hello\n";                             // static bytes; HELLO: *u8; sizeof(HELLO) == 6
buffer scratch: [u8; 4096];                         // static zeroed array; scratch: *u8
struct Point { x: f64, y: f64, tag: u8 }            // C layout (natural alignment)
include "lib/util.ten";                             // textual include, path relative to the current file
```

## Statements
```
let x = 1;  let y: f64 = 2.0;  let p: *Point = alloc(sizeof(Point)) as *Point;
x = x + 1;  x += 2;            // also -= *= /= %= &= |= ^= <<= >>=
p.x = 1.5;  p[3].y = 2.0;  *q = 7;  buf[i] = 'a';
if c { } else if d { } else { }
while c { }    loop { }    for i in 0..n { }    break;  continue;
return;  return x;
f(x);          // expression statement
```
Locals are block-scoped; shadowing is allowed.

## Expressions
Precedence, low → high: `||`, `&&`, `|`, `^`, `&`, `== !=`, `< <= > >=`, `<< >> >>>`, `+ -`, `* / %`,
unary `- ! ~ *`, postfix call / `.field` (auto-deref of `*Struct`) / `[index]`, then `as`.
`&&`/`||` short-circuit and yield 0/1. Comparisons are signed; `>>` is arithmetic, `>>>` logical.

Builtins: `sizeof(T | data | buffer | "lit")`, `offsetof(Struct, field)`, `memcpy(d, s, n)`, `memset(d, b, n)`,
`memsize()`, `sqrt(f)`, `fabs(f)`, `clz(x)`, `ctz(x)`, `popcnt(x)`, `rotl(x, n)`, `rotr(x, n)`,
`ult/ule/ugt/uge(a, b)`, `udiv/urem(a, b)`, `unreachable()`.
A string literal used as an expression has type `*u8` and points to an interned static copy.

## Memory layout produced by the compiler
`[0, 16)` reserved (null) · globals · data · buffers (8-aligned) · `__heap_base` (16-aligned) … `memsize()`.
`min_mem_log2` = smallest power of two holding the static area plus 4 KiB of heap (at least 2^12).

## Standard prelude (`std.ten`, always available; unused functions are dropped)
Allocation (`alloc`, `free`, `heap_reset` for per-request arenas), output helpers (`out`, `out_str`, `out_int`,
`out_f64`), input helpers (read whole body, path), parsing (`parse_int`), bytes (`mem_eq`, `find_byte`),
hashing (`fnv1a`), host API declarations for everything in docs/HOSTAPI.md.

## Example
```
export fn handle() -> i64 {
    let n = input_len();
    let body = alloc(n);
    input_read(body, 0, n);
    out_str("you said: ");
    out(body, n);
    return 0;
}
```
