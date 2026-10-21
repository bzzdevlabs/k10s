//! Application processors (SMP).
//!
//! The boot CPU (BSP, CPU 0) copies a small real-mode trampoline to physical page 0x8000, makes that page executable
//! for the duration, and sends INIT + 2×STARTUP (SDM 8.4.4.1) to all other CPUs at once. Each application processor
//! (AP) starts in 16-bit real mode at 0x8000, switches to 32-bit protected mode, takes a CPU index with an atomic
//! increment (paging is still off, so the trampoline's page may stay read-only for it), enables PAE + long mode +
//! NX, loads the BSP's page tables (one shared address space) and jumps to 64-bit code that picks its stack from
//! [`AP_STACKS`] and calls [`ap_main`]. There it gets its per-CPU state (GS base), its own GDT + TSS (IST stack),
//! the shared IDT and its local APIC (TSC-deadline timer), and enters the worker loop (runtime::worker_main).
//!
//! The number of CPUs comes from QEMU's firmware configuration interface (fw_cfg key 0x05, `-smp N`); without it
//! the BSP waits 20 ms and counts. `k10.smp=N` limits the number of CPUs used.

use core::arch::global_asm;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::arch::{self, apic, gdt, idt, paging};
use crate::mem::frames::{self, Tag, FRAME};
use crate::percpu::{self, MAX_CPUS};
use crate::{kprintln, time};

/// Physical address of the trampoline (STARTUP vector 0x08).
const TRAMPOLINE: u64 = 0x8000;
const AP_STACK_SIZE: usize = 64 * 1024;

/// Next CPU index handed to an AP (the BSP is 0).
#[no_mangle]
static AP_NEXT_INDEX: AtomicU32 = AtomicU32::new(1);
/// Stack tops by CPU index (0 = none: the AP halts).
#[no_mangle]
static AP_STACKS: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
static FAULT_STACKS: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];

global_asm!(
    r#"
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
    movl %cr0, %eax
    orl $1, %eax
    movl %eax, %cr0
    ljmpl $0x08, $(0x8000 + ap_pm32 - ap_tramp_start)
.code32
ap_pm32:
    movw $0x10, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %ss
    /* CPU index (paging is off: plain physical access to the kernel's AP_NEXT_INDEX). */
    movl $1, %eax
    lock xaddl %eax, AP_NEXT_INDEX
    movl %eax, %edi
    /* CR4: PAE, OSFXSR, OSXMMEXCPT (as boot.rs) */
    movl %cr4, %eax
    orl $((1 << 5) | (1 << 9) | (1 << 10)), %eax
    movl %eax, %cr4
    movl $boot_pml4, %eax
    movl %eax, %cr3
    /* EFER: LME | NXE (the page tables use NX bits) */
    movl $0xC0000080, %ecx
    rdmsr
    orl $((1 << 8) | (1 << 11)), %eax
    wrmsr
    /* CR0: PG, WP, NE, MP, PE; clear EM and TS */
    movl %cr0, %eax
    andl $~((1 << 2) | (1 << 3)), %eax
    orl $((1 << 31) | (1 << 16) | (1 << 5) | (1 << 1) | 1), %eax
    movl %eax, %cr0
    ljmpl $0x18, $(0x8000 + ap_lm64 - ap_tramp_start)
.code64
ap_lm64:
    movw $0x10, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %ss
    movl %edi, %edi
    cmpq $8, %rdi
    jae 2f
    movq AP_STACKS(,%rdi,8), %rsp
    testq %rsp, %rsp
    jz 2f
    xorl %ebp, %ebp
    movabsq $ap_main, %rax
    callq *%rax
2:  cli
    hlt
    jmp 2b
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
"#,
    options(att_syntax)
);

extern "C" {
    static ap_tramp_start: u8;
    static ap_tramp_end: u8;
}

/// Number of CPUs QEMU was started with (`-smp`), from fw_cfg; `None` without fw_cfg.
fn fw_cfg_cpus() -> Option<usize> {
    crate::fw_cfg::read_u16(crate::fw_cfg::NB_CPUS).map(usize::from).filter(|&n| n >= 1)
}

fn delay_us(us: u64) {
    let end = arch::rdtsc() + time::ns_to_tsc(us * 1000);
    while arch::rdtsc() < end {
        core::hint::spin_loop();
    }
}

/// Starts the application processors (at most `limit` CPUs in total). Returns the number of CPUs online.
pub fn start_aps(limit: usize) -> usize {
    let present = fw_cfg_cpus();
    let want = present.unwrap_or(MAX_CPUS).min(limit).min(MAX_CPUS);
    if want <= 1 {
        return 1;
    }
    // Stacks and fault (IST) stacks for the APs we expect.
    for i in 1..want {
        let Some(s) = frames::alloc(AP_STACK_SIZE / FRAME as usize, 1, Tag::Stack) else { break };
        let Some(f) = frames::alloc(gdt::FAULT_STACK_SIZE / FRAME as usize, 1, Tag::Stack) else { break };
        AP_STACKS[i].store(s + AP_STACK_SIZE as u64, Ordering::Relaxed);
        FAULT_STACKS[i].store(f + gdt::FAULT_STACK_SIZE as u64, Ordering::Relaxed);
    }
    // The trampoline: written while the page is writable and non-executable, then made read+execute.
    let (start, end) = (&raw const ap_tramp_start as usize, &raw const ap_tramp_end as usize);
    paging::set_low_page(TRAMPOLINE, false);
    // SAFETY: page 0x8000 is conventional memory below 1 MiB that nothing else uses after boot (the frame
    // allocator starts at 1 MiB and the PVH boot information has been copied).
    unsafe { core::ptr::copy_nonoverlapping(start as *const u8, TRAMPOLINE as *mut u8, end - start) };
    paging::set_low_page(TRAMPOLINE, true);
    let t0 = time::now_ns();
    apic::start_all_aps((TRAMPOLINE >> 12) as u8, &delay_us);
    // Wait for the APs we expect (or 20 ms when the count is unknown).
    let deadline = t0 + if present.is_some() { 500_000_000 } else { 20_000_000 };
    while time::now_ns() < deadline && percpu::online() < want {
        core::hint::spin_loop();
    }
    paging::set_low_page(TRAMPOLINE, false);
    let n = percpu::online();
    if crate::verbose() || n < want {
        kprintln!("smp: {} CPU(s) online of {} (in {} us)", n, present.map_or(0, |p| p), (time::now_ns() - t0) / 1000);
    }
    n
}

/// 64-bit entry of an application processor (from the trampoline), on its own stack.
#[no_mangle]
extern "C" fn ap_main(index: u64) -> ! {
    let i = index as usize;
    percpu::init_cpu(i);
    gdt::init_cpu(i, FAULT_STACKS[i].load(Ordering::Relaxed));
    idt::load();
    apic::init_local();
    let me = percpu::this();
    me.apic_id.store(apic::id(), Ordering::Relaxed);
    paging::sync_tlb();
    me.online.store(true, Ordering::Release);
    arch::interrupts_enable();
    crate::runtime::worker_main()
}
