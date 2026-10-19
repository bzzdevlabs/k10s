# JIT ⇄ runtime ABI (x86-64, v1)

Contract between `k10-jit` (TIR → machine code) and the node runtimes (`k10-node`, later `k10-metal`).
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
