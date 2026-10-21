# JIT ⇄ runtime ABI (x86-64, v1)

Contract between `k10-jit` (TIR → machine code) and the node runtimes (`k10-node` and `k10-metal`).
The JIT knows nothing about scheduling, stacks or I/O; it calls hooks. The runtime knows nothing about codegen.

## Registers inside JIT code
| reg | holds |
|---|---|
| `r15` | memory base of the current instance |
| `r14` | `*mut VmCtx` |
| `r13` | memory mask `S - 1` |

All three are callee-saved in the SysV ABI, so `extern "C"` host functions preserve them. Every memory access is
`[r15 + ((addr + off) & r13)]` with the add done in 64-bit wrapping arithmetic before masking.

## VmCtx (`#[repr(C)]`, defined in `k10-jit::abi`, offsets are part of the ABI)
```rust
#[repr(C)]
pub struct VmCtx {
    pub mem_base: *mut u8,                 // +0
    pub mem_mask: u64,                     // +8   S - 1
    pub stack_limit: AtomicUsize,          // +16  safepoint check: trap/interrupt if rsp (after frame) < this
    pub real_stack_limit: usize,           // +24  real limit; distinguishes overflow from an interrupt request
    pub host_fns: *const HostFn,           // +32  indexed by import index
    pub interrupt: InterruptFn,            // +40  called when an interrupt is requested
    pub trap_request: u32,                 // +48  host fns set a trap code (Trap::code) to abort after returning
    pub _pad: u32,                         // +52
    pub user: *mut c_void,                 // +56  owned by the runtime (current invocation, instance, …)
    pub jit_private: [u64; 8],             // +64  owned by the JIT (saved rsp etc. for trap unwinding)
}
pub type HostFn = unsafe extern "C" fn(ctx: *mut VmCtx, a0: i64, a1: i64, a2: i64, a3: i64, a4: i64) -> i64;
pub type InterruptFn = unsafe extern "C" fn(ctx: *mut VmCtx) -> u32;   // 0 = continue, else a trap code
```

## API
```rust
pub fn compile(m: &k10_ir::Module) -> Result<CompiledModule, CompileError>;   // validates internally
impl CompiledModule {                       // Send + Sync; code mapped read+execute; shared by all instances
    pub fn export(&self, name: &str) -> Option<u32>;
    pub unsafe fn invoke(&self, ctx: *mut VmCtx, func: u32, args: &[i64]) -> Result<Option<i64>, Trap>;
    pub fn code_bytes(&self) -> usize;
}
```
`invoke` runs on the **current** native stack. It saves callee-saved registers on its own frame and that frame's
`rsp` in `ctx.jit_private[0]` (restoring the previous value on exit, so nested `invoke`s on the same `VmCtx` from a
host function work), so a trap anywhere in JIT code returns from `invoke` with `Err(trap)`. Only JIT frames lie between `invoke` and a trap
point: host functions and the interrupt hook *return* (with `trap_request` / a non-zero code) and the JIT raises the
trap afterwards. The runtime may switch stacks (coroutines) inside a host function or inside the interrupt hook;
JIT code is agnostic to that.

## Safepoints (function entry and every `loop`)
```
lea rax, [rsp - frame_size]          ; (entry only; loops can compare rsp directly)
cmp rax, [r14 + 16]                  ; stack_limit
jb  slow
slow:  if rax < [r14 + 24] → trap StackOverflow
       else call [r14 + 40](r14) → non-zero ⇒ trap with that code, zero ⇒ continue
```
- **Preemption**: any thread may store `usize::MAX` into `stack_limit` (Relaxed). The next safepoint takes the slow
  path; the runtime's interrupt hook must restore `stack_limit` to `real_stack_limit` before returning 0.
- The runtime sets `real_stack_limit = stack_bottom + HOST_RESERVE` so that host functions and the interrupt hook
  always have at least `HOST_RESERVE` (≥ 32 KiB) of stack below the deepest JIT frame.

## Calls
- TIR→TIR calls use a JIT-private convention.
- Host calls: `rdi = ctx`, args in `rsi, rdx, rcx, r8, r9` (unused ones are don't-care), 16-byte aligned stack,
  `call [r14 + 32][import]`, result in `rax`; then `if ctx.trap_request != 0 → trap(trap_request)` (the JIT clears it).

## Traps
Unreachable, DivByZero (explicit zero checks; `div_s(MIN,-1)` handled without `#DE`), StackOverflow (safepoint),
MemOutOfBounds (bulk ops, via internal helpers), Interrupted / Host(n) (codes from hooks). No signal handlers are
needed: SFI masking makes every load/store valid, and every other fault is checked explicitly.

## Memory
The runtime provides, per instance, `S + TAIL` bytes of readable/writable memory at `mem_base` (in practice a slot
of `S + 4096` bytes). Code is W^X: emitted into RW memory, then remapped read+execute.

## Speculation hardening
Generated code must stay inside its slot and its code even on *mispredicted* paths (Spectre; threat model in
SECURITY.md). Every guest address is masked with `r13` by data flow, so memory accesses need nothing more. What
remains are the branches whose misprediction would index or jump with a guest value; their exact sequences
(`r` = the register holding the index, any allocatable register: `rax rcx rdx rbx rbp rsi rdi r8 r9 r10 r12`;
`r11` is the codegen's scratch register; Intel syntax):

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
`Jump` mode adds no instruction to a call. The other checks:
- No clamp needed: host-function and helper indices are compile-time constants, `call` targets are direct, frame
  offsets are constants, division-by-zero checks guard no memory access; bulk-memory helpers use `k10_nospec::span`.
- Stack checks (`cmp rax, [r14 + 16]; jb slow`) are not clamped: a mispredicted one runs a few frames below
  `stack_limit`, but every frame slot is written before it is read, so only the frame's own stores are read back.
- Residual risk there: speculative store bypass (SECURITY.md).
