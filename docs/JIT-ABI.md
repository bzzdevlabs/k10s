# JIT ⇄ runtime ABI (x86-64 and AArch64, v1)

Contract between `k10-jit` (TIR → machine code) and the node runtimes (`k10-node` and `k10-metal`).
The JIT knows nothing about scheduling, stacks or I/O; it calls hooks. The runtime knows nothing about codegen.
`k10-jit` generates code for the architecture it is built for: AArch64 (A64) on aarch64, x86-64 everywhere else.

## Registers inside JIT code
| x86-64 | AArch64 | holds |
|---|---|---|
| `r15` | `x27` | memory base of the current instance |
| `r14` | `x26` | `*mut VmCtx` |
| `r13` | `x28` | memory mask `S - 1` |

All three are callee-saved in the platform ABI (SysV, AAPCS64), so `extern "C"` host functions preserve them. Every
memory access is `[r15 + ((addr + off) & r13)]` (`[x27, ((addr + off) & x28)]`) with the add done in 64-bit
wrapping arithmetic before masking. On AArch64, JIT code never writes `x29` (the frame-pointer chain stays the
caller's), `x18` (the platform register) or `v8`–`v15` (callee-saved `d8`–`d15`); `x16`/`x17` are its scratch
registers.

## VmCtx (`#[repr(C)]`, defined in `k10-jit::abi`, offsets are part of the ABI)
```rust
#[repr(C)]
pub struct VmCtx {
    pub mem_base: *mut u8,                 // +0
    pub mem_mask: u64,                     // +8   S - 1
    pub stack_limit: AtomicUsize,          // +16  safepoint check: trap/interrupt if sp (after frame) < this
    pub real_stack_limit: usize,           // +24  real limit; distinguishes overflow from an interrupt request
    pub host_fns: *const HostFn,           // +32  indexed by import index
    pub interrupt: InterruptFn,            // +40  called when an interrupt is requested
    pub trap_request: u32,                 // +48  host fns set a trap code (Trap::code) to abort after returning
    pub _pad: u32,                         // +52
    pub user: *mut c_void,                 // +56  owned by the runtime (current invocation, instance, …)
    pub jit_private: [u64; 8],             // +64  owned by the JIT (saved sp, AArch64 helper addresses)
}
pub type HostFn = unsafe extern "C" fn(ctx: *mut VmCtx, a0: i64, a1: i64, a2: i64, a3: i64, a4: i64) -> i64;
pub type InterruptFn = unsafe extern "C" fn(ctx: *mut VmCtx) -> u32;   // 0 = continue, else a trap code
```
`jit_private[0]` is the stack pointer of the innermost `invoke` frame; on AArch64 `jit_private[1..=2]` hold the
addresses of the bulk-memory helpers, stored by every `invoke`.

## API
```rust
pub fn compile(m: &k10_ir::Module) -> Result<CompiledModule, CompileError>;   // validates internally
impl CompiledModule {                       // Send + Sync; code mapped read+execute; shared by all instances
    pub fn export(&self, name: &str) -> Option<u32>;
    pub unsafe fn invoke(&self, ctx: *mut VmCtx, func: u32, args: &[i64]) -> Result<Option<i64>, Trap>;
    pub fn code_bytes(&self) -> usize;
    pub fn mapped_bytes(&self) -> usize;    // whole pages of the kernel's page size
}
```
`invoke` runs on the **current** native stack. It saves callee-saved registers on its own frame and that frame's
stack pointer in `ctx.jit_private[0]` (restoring the previous value on exit, so nested `invoke`s on the same `VmCtx`
from a host function work), so a trap anywhere in JIT code returns from `invoke` with `Err(trap)`. Only JIT frames
lie between `invoke` and a trap point: host functions and the interrupt hook *return* (with `trap_request` / a
non-zero code) and the JIT raises the trap afterwards. The runtime may switch stacks (coroutines) inside a host
function or inside the interrupt hook; JIT code is agnostic to that.

## Frames
x86-64: `call` pushes the return address; the function then subtracts its frame (`rsp ≡ 0 mod 16` at every call;
argument `i` of a TIR call at `[rsp + 8i]`, i.e. `[rsp + frame + 8 + 8i]` in the callee).

AArch64: the function pushes `x30` (`str x30, [sp, #-16]!`), checks, then allocates its frame `F` (a multiple of
16: `sp` is always 16-byte aligned); argument `i` is at `[sp + F + 16 + 8i]`. The epilogue is
`add sp, sp, #F; ldr x30, [sp], #16; ret`; nothing else writes `sp`, `x30` (besides `bl`/`blr`) or the
return-address slot.

## Safepoints (function entry and every `loop`)
x86-64:
```
lea rax, [rsp - frame_size]          ; (entry only; loops compare rsp directly)
cmp rax, [r14 + 16]                  ; stack_limit
jb  slow
slow:  if rax < [r14 + 24] → trap StackOverflow
       else call [r14 + 40](r14) → non-zero ⇒ trap with that code, zero ⇒ continue
```
AArch64:
```
str x30, [sp, #-16]!                 ; (entry) the return address, inside the caller's checked frame
sub x16, sp, #F                      ; (entry; F ≥ 4096: mov x16, #F; subs x16, sp, x16; b.lo trap_overflow)
ldr x17, [x26, #16]                  ; stack_limit
cmp x16, x17                         ; (loop: cmp sp, x17)
b.lo slow
back: mov sp, x16                    ; (entry)
slow:  [mov x16, sp]                 ; (loop)
       bl safepoint ; b back         ; thunk: x16 < [x26 + 24] → StackOverflow, else blr [x26 + 40](x26)
```
The AArch64 safepoint thunk saves `x0`–`x16`, `x30` and `d24`–`d31` (the FP/SIMD registers of pinned locals)
around the interrupt hook; the operand stack is empty at safepoints, so nothing else is live.
- **Preemption**: any thread may store `usize::MAX` into `stack_limit` (Relaxed; an aligned 64-bit load is
  single-copy atomic on both architectures). The next safepoint takes the slow path; the runtime's interrupt hook
  must restore `stack_limit` to `real_stack_limit` before returning 0.
- The runtime sets `real_stack_limit = stack_bottom + HOST_RESERVE` so that host functions and the interrupt hook
  always have at least `HOST_RESERVE` (≥ 32 KiB) of stack below the deepest JIT frame.

## Calls
- TIR→TIR calls use a JIT-private convention (every register caller-saved; pinned locals written back before and
  reloaded after).
- Host calls, x86-64: `rdi = ctx`, args in `rsi, rdx, rcx, r8, r9` (unused ones are don't-care), 16-byte aligned
  stack, `call [r14 + 32][import]`, result in `rax`; then `if ctx.trap_request != 0 → trap(trap_request)` (the JIT
  clears it). AArch64: `x0 = ctx`, args in `x1`–`x5`, `ldr x16, [x26, #32]; ldr x16, [x16, #8*import]; blr x16`,
  result in `x0`, then `ldr w17, [x26, #48]; cbnz w17, host_trap`.
- Helpers (`mem.copy`, `mem.fill`) get the memory base and mask as their first two arguments; x86-64 calls them
  through the blob's helper table, AArch64 through `ldr x16, [x26, #72 + 8h]`.
- FP/SIMD state: no FP/SIMD register holds a value across a host call, a TIR call or a safepoint (x86-64: none at
  all; AArch64: values live in `v0`–`v7`/`v16`–`v31`, pinned locals in `v24`–`v31`, written back around calls and
  saved by the safepoint thunk), so neither host functions nor coroutine switches need to preserve them.

## Traps
Unreachable, DivByZero (explicit zero checks; `div_s(MIN,-1)` handled without `#DE`; AArch64 division does not fault
and returns MIN), StackOverflow (safepoint), MemOutOfBounds (bulk ops, via internal helpers), Interrupted / Host(n)
(codes from hooks). No signal handlers are needed: SFI masking makes every load/store valid, and every other fault is
checked explicitly.

## Memory and code
The runtime provides, per instance, `S + TAIL` bytes of readable/writable memory at `mem_base` (in practice a slot
of `S + 4096` bytes). Code is W^X: emitted into RW memory, then remapped read+execute, page-aligned with the
kernel's page size (`sysconf`: 4 KiB, 16 KiB on some AArch64 kernels such as the Pi 5's). On AArch64 the loader
also makes the code visible to instruction fetch, once per module: `dc cvau` per data-cache line, `dsb ish`,
`ic ivau` per instruction-cache line, `dsb ish`, `isb` (line sizes from `CTR_EL0`, each loop skipped when
`CTR_EL0.IDC`/`DIC` say so; `k10_jit::cache::sync_icache`), then a context synchronization of every CPU that may
run it: `CompiledModule` calls `membarrier(MEMBARRIER_CMD_PRIVATE_EXPEDITED_SYNC_CORE)` (registered on first use;
the node's seccomp filter admits exactly these two commands). A runtime loading blobs itself (`CodeBlob::write_to`)
does the same. Cortex-A53 erratum 835769 (Raspberry Pi 3, many boards): a `nop` separates a load or store from a
directly following 64-bit `mul`/`msub`, as `-mfix-cortex-a53-835769` does; the JIT emits no `adrp` (erratum
843419).

## Speculation hardening
Generated code must stay inside its slot and its code even on *mispredicted* paths (Spectre; threat model in
SECURITY.md). Every guest address is masked with the mask register by data flow, so memory accesses need nothing
more. What remains are the branches whose misprediction would index or jump with a guest value.

### x86-64
Exact sequences (`r` = the register holding the index, any allocatable register: `rax rcx rdx rbx rbp rsi rdi r8 r9
r10 r12`; `r11` is the codegen's scratch register; Intel syntax):

**`br_table`, jump table** (more than `CompileOptions::br_tree_max` target runs, default 8). The table has
`n + 1` 4-byte entries, entry `n` = the default; the index is clamped by `cmovae` — data flow, which the CPU does not
predict — so not even a mispredicted path loads outside the table:
```
mov    r11d, n                 ; 41 BB imm32
cmp    r, r11                  ; unsigned 64-bit
cmovae r, r11                  ; r = min(r, n): out-of-range and "negative" indices select the default entry
lea    r11, [rip + table]      ; 4C 8D 1D rel32
movsxd r, dword [r11 + r*4]    ; entry = target - table
; then, by CompileOptions::indirect (IndirectBranch):
;   Jump (default):  add r, r11 ; jmp r
;   Lfence:          add r11, r ; lfence ; jmp r11
;   Retpoline:       add r11, r ; jmp retpoline_thunk
<0xCC padding to a multiple of 4>
table: dd target_0 - table, …, dd target_{n-1} - table, dd default - table
```
**`br_table`, compare tree** (at most `br_tree_max` runs; a run = maximal range of consecutive indices with the
same target, the last run being `[k, 2^64)` → default). A balanced binary search on the run boundaries with
unsigned compares and direct jumps only — no indirect branch, no table load:
```
cmp r, imm32 ; jae right   (or, when the left half is one run: cmp r, imm32 ; jb target_left)
…            ; leaves: jmp target
```
**Indirect calls** (host functions, the bulk-memory helpers, the interrupt hook, the entry trampoline's call of the
function), by `IndirectBranch` mode — e.g. a host call (`imp` = the import index, a compile-time constant):
```
Jump:       mov rax, [r14 + 32] ; call [rax + 8*imp]
Lfence:     mov rax, [r14 + 32] ; mov r11, [rax + 8*imp] ; lfence ; call r11
Retpoline:  mov rax, [r14 + 32] ; mov r11, [rax + 8*imp] ; call retpoline_thunk
```
(helpers: `call [rip + slot]` / `mov r11, [rip + slot]; …`; interrupt hook: `call [r14 + 40]` / `mov r11,
[r14 + 40]; …`; entry trampoline: `call rsi` / `mov r11, rsi; …`).

**Retpoline thunk** (once per module, in the prelude, 16-byte aligned, only in `Retpoline` mode; target in `r11`,
entered by `call` or `jmp`). Its `ret` is predicted by the return stack buffer — the speculation trap — never by
the branch target buffer:
```
retpoline_thunk:
    call 1f            ; E8 07 00 00 00
2:  pause              ; F3 90
    lfence             ; 0F AE E8
    jmp 2b             ; EB F9
1:  mov [rsp], r11     ; 4C 89 1C 24
    ret                ; C3
```

### AArch64
The same three modes, with Arm's speculation barriers: `csdb` (Consumption of Speculative Data Barrier, an Armv8.0
hint every core honours) after the clamp of a jump-table index, and as the `Lfence` analogue `sb` (FEAT_SB, used
when `CpuFeatures::sb` is set: detected on Linux, off in no_std builds) or else `dsb nsh; isb` (the barrier Linux
uses where SB is absent). `r` is any allocatable register (`x0`–`x15`, `x19`–`x25`); the target is always in
`x16`.

**`br_table`, jump table** — the index is clamped by `csel`, and `csdb` keeps later instructions from using a
predicted value of that selection (Arm's Spectre v1 pattern, as Linux's `array_index_nospec`):
```
mov  x16, #n                   ; movz/movk
cmp  r, x16                    ; unsigned 64-bit
csel r, r, x16, lo             ; r = min(r, n)
csdb
adr  x16, table
ldrsw r, [x16, r, lsl #2]      ; entry = target - table
add  x16, x16, r
; Jump (default): br x16 ; Lfence: <barrier> ; br x16 ; Retpoline: b retpoline_thunk
table: .word target_0 - table, …, .word default - table
```
**Compare tree**: `cmp r, #k` (or `x16 = k; cmp r, x16`), `b.hs right` / `b.lo target_left`, leaves `b target`.

**Indirect calls** (host functions, helpers, the interrupt hook, the entry trampoline's call):
```
Jump:       ldr x16, [x26, #32] ; ldr x16, [x16, #8*imp] ; blr x16
Lfence:     ldr x16, [x26, #32] ; ldr x16, [x16, #8*imp] ; <barrier> ; blr x16
Retpoline:  ldr x16, [x26, #32] ; ldr x16, [x16, #8*imp] ; bl retpoline_thunk
```
**Retpoline thunk** (16-byte aligned, Retpoline mode only; target in `x16`, entered by `bl` or `b`). `ret x16`
is predicted from the return stack, which `bl 1f` filled with the address of the trap; the caller's `x30` is kept
in `x17`:
```
retpoline_thunk:
    mov x17, x30
    bl  1f
2:  <barrier>          ; sb, or dsb nsh; isb
    b   2b
1:  mov x30, x17
    ret x16
```

### Both architectures
`Jump` mode adds no instruction to a call. The other checks:
- No clamp needed: host-function and helper indices are compile-time constants, `call`/`bl` targets are direct,
  frame offsets are constants, division-by-zero checks guard no memory access; bulk-memory helpers use
  `k10_nospec::span`.
- Stack checks are not clamped: a mispredicted one runs a few frames below `stack_limit`, but every frame slot is
  written before it is read, so only the frame's own stores are read back.
- Residual risk there: speculative store bypass (SECURITY.md).

## Verification
Independent machine-code SFI verifiers check what the JIT emits against this document: on x86-64 over objdump's
decoding (tests/xdis/sfi.rs, tests/xdis_sfi.rs and the red-team suite), on AArch64 over a decoder written from the
Arm ARM encoding tables (tests/common/a64dec.rs, cross-checked against objdump on every encoder form, on random and
mutated words and on JIT output) run on every module the k10-jit suites compile (tests/common/a64sfi.rs,
tests/sfi_a64.rs). The AArch64 rules: guest accesses `[x27, Xm]` with `Xm` from `and Xm, Xn, x28` or a constant
below `2^min(min_mem_log2, 31)` (single registers, no writeback, no scaled index); frame accesses inside the frame
the prologue checked; `x26`–`x29`, `x18`, `sp` and `x30` written only as above and `v8`–`v15` never; vmctx read only
at its documented fields; calls to function entries, the safepoint thunk from its stubs, host functions and helpers
in the mode's form; branches to the function's instructions or the prelude's trap stubs; jump tables exactly as
above; no multiply-accumulate right after a load or store.
