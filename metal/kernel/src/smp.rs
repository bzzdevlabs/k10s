//! Application processors (SMP).
//!
//! The CPUs come from the ACPI MADT (or an MP table): every enabled local APIC / x2APIC entry, up to [`MAX_CPUS`]
//! and `k10.smp=N`. Disabled entries and CPUs the firmware did not list are never woken (a broadcast INIT would).
//! The boot CPU (BSP, CPU 0) copies a small real-mode trampoline to physical page 0x8000 through the direct map, makes
//! that alias read-only (the page stays: an AP that wakes up after the bring-up finds [`AP_CLOSED`] and halts), and
//! sends each AP an INIT, then (after 1 ms) a STARTUP, and a second STARTUP to those that are not up 200 µs later
//! (SDM 8.4.4.1), all APs in parallel. Each application processor starts in 16-bit real mode at 0x8000, loads a
//! known CR0 (caches on), switches to 32-bit protected mode, takes a CPU index with an atomic increment (paging is
//! still off: physical addresses), enables PAE + long mode (+ NX when the BSP has it) on the trampoline's page tables
//! ([`AP_CR3`]: the kernel's upper half plus the trampoline's page, identity-mapped read+execute: the only identity
//! mapping after boot, never loaded by a running CPU), jumps to the kernel image, loads the kernel's PML4 (one shared
//! address space), picks its stack from [`AP_STACKS`] and calls [`ap_main`].
//! There it gets its per-CPU state (GS base), its own GDT + TSS (IST stack), the shared IDT, the PAT and its local
//! APIC (in the BSP's mode, xAPIC or x2APIC), and enters the worker loop (runtime::worker_main).

use alloc::vec::Vec;
use core::arch::global_asm;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::arch::paging::{self, P};
use crate::arch::{self, apic, gdt, idt};
use crate::mem::frames::{self, Tag, FRAME};
use crate::mem::{PhysAddr, VirtAddr};
use crate::percpu::{self, MAX_CPUS};
use crate::{kprintln, time};

/// Physical address of the trampoline (STARTUP vector 0x08).
const TRAMPOLINE: PhysAddr = PhysAddr::new(0x8000);
const AP_STACK_SIZE: usize = 64 * 1024;

/// Next CPU index handed to an AP (the BSP is 0).
#[no_mangle]
static AP_NEXT_INDEX: AtomicU32 = AtomicU32::new(1);
/// EFER bits the APs set: LME, and NXE when the boot CPU enabled NX (the page tables then carry NX bits).
#[no_mangle]
static AP_EFER: AtomicU32 = AtomicU32::new(1 << 8);
/// Set once the bring-up is over: an AP that only wakes up now halts before touching anything.
#[no_mangle]
static AP_CLOSED: AtomicU32 = AtomicU32::new(0);
/// Physical address of the trampoline's PML4 (below 4 GiB: the AP loads it in 32-bit mode).
#[no_mangle]
static AP_CR3: AtomicU32 = AtomicU32::new(0);
/// Physical address of the kernel's PML4.
#[no_mangle]
static AP_KERNEL_CR3: AtomicU64 = AtomicU64::new(0);
/// Stack tops by CPU index (0 = none: the AP halts).
#[no_mangle]
static AP_STACKS: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
static FAULT_STACKS: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];

global_asm!(
    r#"
.set KVMA, {kvma}
.pushsection .rodata.ap_trampoline, "a"
.balign 16
.global ap_tramp_start
ap_tramp_start:
.code16
    cli
    cld
    xorw %ax, %ax
    movw %ax, %ds
    lgdtl (0x8000 + ap_gdt_ptr - ap_tramp_start)
    /* CR0 = PE | ET: a known value (INIT keeps CD and NW, which a CPU the firmware never ran may have set). */
    movl $0x11, %eax
    movl %eax, %cr0
    ljmpl $0x08, $(0x8000 + ap_pm32 - ap_tramp_start)
.code32
ap_pm32:
    movw $0x10, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %ss
    cmpl $0, AP_CLOSED - KVMA
    jne 3f
    /* CPU index (paging is off: plain physical access to the kernel's AP_NEXT_INDEX). */
    movl $1, %eax
    lock xaddl %eax, AP_NEXT_INDEX - KVMA
    movl %eax, %edi
    /* CR4: PAE, OSFXSR, OSXMMEXCPT (as boot.rs) */
    movl %cr4, %eax
    orl $((1 << 5) | (1 << 9) | (1 << 10)), %eax
    movl %eax, %cr4
    movl AP_CR3 - KVMA, %eax
    movl %eax, %cr3
    /* EFER: LME, and NXE if the boot CPU has NX (AP_EFER) */
    movl $0xC0000080, %ecx
    rdmsr
    orl AP_EFER - KVMA, %eax
    wrmsr
    /* CR0: PG, WP, NE, ET, MP, PE (caches on, EM and TS clear) */
    movl $0x80010033, %eax
    movl %eax, %cr0
    ljmpl $0x18, $(0x8000 + ap_lm64 - ap_tramp_start)
3:  cli
    hlt
    jmp 3b
.code64
ap_lm64:
    movw $0x10, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %ss
    movabsq $ap_entry64, %rax
    jmpq *%rax
.balign 8
ap_gdt:
    /* Accessed bits preset: once paging is on the page is read-only, and the CPU must not need to write it. */
    .quad 0
    .quad 0x00CF9B000000FFFF
    .quad 0x00CF93000000FFFF
    .quad 0x00AF9B000000FFFF
ap_gdt_ptr:
    .word 31
    .long 0x8000 + ap_gdt - ap_tramp_start
.global ap_tramp_end
ap_tramp_end:
.popsection

/* In the kernel image: the kernel's PML4, the stack, ap_main. */
.pushsection .text.ap_entry64, "ax"
.code64
ap_entry64:
    movq AP_KERNEL_CR3(%rip), %rax
    movq %rax, %cr3
    movl %edi, %edi
    cmpq ${max_cpus}, %rdi
    jae 2f
    leaq AP_STACKS(%rip), %rax
    movq (%rax,%rdi,8), %rsp
    testq %rsp, %rsp
    jz 2f
    xorl %ebp, %ebp
    callq ap_main
2:  cli
    hlt
    jmp 2b
.popsection
"#,
    max_cpus = const MAX_CPUS,
    kvma = const crate::mem::addr::KERNEL_BASE,
    options(att_syntax)
);

extern "C" {
    static ap_tramp_start: u8;
    static ap_tramp_end: u8;
    static mut ap_pml4: [u64; 512];
}

fn delay_us(us: u64) {
    let end = arch::rdtsc() + time::ns_to_tsc(us * 1000);
    while arch::rdtsc() < end {
        core::hint::spin_loop();
    }
}

fn is_up(apic_id: u32, n: usize) -> bool {
    (1..n).any(|i| percpu::cpu(i).online.load(Ordering::Acquire) && percpu::cpu(i).apic_id.load(Ordering::Relaxed) == apic_id)
}

/// Starts the application processors (at most `limit` CPUs in total). Returns the number of CPUs online.
pub fn start_aps(limit: usize) -> usize {
    let me = apic::id();
    let listed: Vec<u32> = crate::acpi::get().map_or(Vec::new(), |t| t.cpus.iter().map(|c| c.apic_id).filter(|&id| id != me).collect());
    let want = (listed.len() + 1).min(limit).min(MAX_CPUS);
    if want <= 1 {
        if crate::verbose() {
            kprintln!("smp: 1 CPU ({} listed by the firmware)", listed.len() + 1);
        }
        return 1;
    }
    if !trampoline() {
        kprintln!("smp: no memory for the trampoline's page tables: 1 CPU");
        return 1;
    }
    // Stacks and fault (IST) stacks for the APs we start.
    let mut n = 1;
    while n < want {
        let Some(s) = frames::alloc(AP_STACK_SIZE / FRAME as usize, 1, Tag::Stack) else { break };
        let Some(f) = frames::alloc(gdt::FAULT_STACK_SIZE / FRAME as usize, 1, Tag::Stack) else {
            frames::free(s, AP_STACK_SIZE / FRAME as usize, Tag::Stack);
            break;
        };
        AP_STACKS[n].store((s.virt() + AP_STACK_SIZE as u64).get(), Ordering::Relaxed);
        FAULT_STACKS[n].store((f.virt() + gdt::FAULT_STACK_SIZE as u64).get(), Ordering::Relaxed);
        n += 1;
    }
    let targets = &listed[..n - 1];
    AP_EFER.store(1 << 8 | if paging::nx_enabled() { 1 << 11 } else { 0 }, Ordering::Relaxed);
    let t0 = time::now_ns();
    let vector = (TRAMPOLINE.get() >> 12) as u8;
    for &id in targets {
        apic::send_init(id);
    }
    delay_us(1000);
    for &id in targets {
        apic::send_startup(id, vector);
    }
    delay_us(200);
    for &id in targets {
        if !is_up(id, n) {
            apic::send_startup(id, vector);
        }
    }
    let deadline = t0 + 2_000_000_000;
    while time::now_ns() < deadline && percpu::online() < n {
        core::hint::spin_loop();
    }
    // The trampoline and its page tables stay: an AP waking up later must find the code that halts it.
    AP_CLOSED.store(1, Ordering::SeqCst);
    let up = percpu::online();
    if crate::verbose() || up < n {
        kprintln!("smp: {} CPU(s) online of {} started, {} listed (in {} us)", up, n, listed.len() + 1, (time::now_ns() - t0) / 1000);
        for &id in targets {
            if !is_up(id, n) {
                kprintln!("smp: CPU with APIC id {id} did not start");
            }
        }
    }
    up
}

/// Writes the trampoline and builds its page tables (kept for an AP that passed [`AP_CLOSED`] just before it closed).
fn trampoline() -> bool {
    let (start, end) = (&raw const ap_tramp_start, &raw const ap_tramp_end);
    // SAFETY: page 0x8000 is conventional memory below 1 MiB that nothing else uses after boot (the frame
    // allocator starts at 64 KiB, the PVH boot information has been copied, and UEFI loaders reserve the page).
    unsafe { core::ptr::copy_nonoverlapping(start, TRAMPOLINE.ptr::<u8>(), end as usize - start as usize) };
    if !paging::seal_readonly(TRAMPOLINE) {
        return false;
    }
    let root = VirtAddr::of(&raw const ap_pml4).phys();
    let va = TRAMPOLINE.get();
    let mut t = root;
    for level in [4, 3, 2] {
        // SAFETY: tables built here, reached through the direct map.
        unsafe {
            let e = t.ptr::<u64>().add(paging::index(va, level));
            if *e & P == 0 {
                let Some(next) = paging::alloc_table() else { return false };
                *e = next.get() | P | paging::W;
            }
            t = PhysAddr::new(*e & paging::ADDR);
        }
    }
    // SAFETY: as above; read+execute (no W, no NX).
    unsafe { *t.ptr::<u64>().add(paging::index(va, 1)) = va | P };
    paging::copy_kernel_half(root);
    AP_CR3.store(root.get() as u32, Ordering::Relaxed);
    AP_KERNEL_CR3.store(paging::kernel_root().get(), Ordering::Relaxed);
    true
}

/// 64-bit entry of an application processor (from the trampoline), on its own stack.
#[no_mangle]
extern "C" fn ap_main(index: u64) -> ! {
    let i = index as usize;
    percpu::init_cpu(i);
    gdt::init_cpu(i, FAULT_STACKS[i].load(Ordering::Relaxed));
    idt::load();
    paging::init_pat();
    apic::init_local();
    let me = percpu::this();
    me.apic_id.store(apic::id(), Ordering::Relaxed);
    paging::sync_tlb();
    me.online.store(true, Ordering::Release);
    arch::interrupts_enable();
    crate::runtime::worker_main()
}
