//! Boot path: PVH entry (32-bit protected mode) → long mode → `kmain`.
//!
//! Why PVH and not Multiboot v1: QEMU's Multiboot loader refuses ELF64 images ("Cannot load x86-64 image, give a
//! 32bit one"), so Multiboot would need either a 32-bit ELF wrapper or the a.out-kludge header plus a flat binary.
//! PVH (the Xen/x86 "direct boot" ABI) is accepted by `qemu-system-x86_64 -kernel` for a plain ELF64 file that
//! carries an `XEN_ELFNOTE_PHYS32_ENTRY` note; the same image also boots on Firecracker and Cloud Hypervisor. QEMU
//! enters at that address through its `pvh.bin` option ROM with:
//!   * 32-bit protected mode, paging off, flat 4 GiB CS/DS, interrupts off, A20 on;
//!   * `ebx` = physical address of `struct hvm_start_info` (version 1: command line, E820 memory map, and
//!     `-initrd` as module 0) — see `pvh.rs`;
//!   * no usable stack, GDT or IDT.
//!
//! The trampoline below zeroes .bss, builds identity-mapped page tables for the first 4 GiB with 2 MiB pages
//! (PML4 → 1 PDPT → 4 PDs), enables PAE + long mode + paging (and SSE, for the future JIT), loads a minimal GDT and
//! far-returns into 64-bit code, which sets up the boot stack and calls `kmain`. Everything is linked at 1 MiB
//! (see linker.ld) and runs at its physical address.

use core::arch::global_asm;

global_asm!(
    r#"
/* ---------------------------------------------------------------------------------------------------------- */
/* ELF note read by QEMU (hw/i386/x86.c: load_elfboot): type 18 = XEN_ELFNOTE_PHYS32_ENTRY. For an ELF64 image */
/* QEMU reads the descriptor as a 64-bit value, so it is a .quad (like Linux's vmlinux).                         */
.pushsection .note.Xen, "a", @note
    .balign 4
    .long 4                 /* namesz ("Xen\0") */
    .long 8                 /* descsz */
    .long 18                /* XEN_ELFNOTE_PHYS32_ENTRY */
    .asciz "Xen"
    .balign 4
    .quad _pvh_start
    .balign 4
.popsection

/* ---------------------------------------------------------------------------------------------------------- */
.pushsection .bss.boot, "aw", @nobits
    .balign 4096
    .global boot_pml4
boot_pml4:  .skip 4096
boot_pdpt:  .skip 4096
    .global boot_pd
boot_pd:    .skip 4096 * 4          /* 4 page directories = 4 GiB of 2 MiB pages */
    .balign 16
    .global boot_stack_bottom
boot_stack_bottom:
    .skip 65536
    .global boot_stack_top
boot_stack_top:
    .balign 8
    .global pvh_start_info_ptr
pvh_start_info_ptr: .quad 0
.popsection

.pushsection .rodata.boot, "a"
    .balign 16
boot_gdt:
    .quad 0                         /* null */
    .quad 0x00AF9A000000FFFF        /* 0x08: 64-bit code, DPL 0 (L=1) */
    .quad 0x00CF92000000FFFF        /* 0x10: data, DPL 0 */
boot_gdt_end:
    .balign 8
boot_gdt_ptr:
    .word boot_gdt_end - boot_gdt - 1
    .long boot_gdt
.popsection

/* ---------------------------------------------------------------------------------------------------------- */
.pushsection .text.boot32, "ax"
.code32
    .global _pvh_start
_pvh_start:
    cli
    cld
    /* Zero .bss (QEMU zero-fills it too, but do not rely on the loader). ebx (start info) is preserved. */
    mov $__bss_start, %edi
    mov $__bss_end, %ecx
    sub %edi, %ecx
    xor %eax, %eax
    rep stosb
    mov %ebx, pvh_start_info_ptr
    mov $boot_stack_top, %esp

    /* PML4[0] → PDPT; PDPT[0..4] → PD0..PD3 (present | writable). */
    mov $boot_pdpt, %eax
    or $0x3, %eax
    mov %eax, boot_pml4
    mov $boot_pd, %eax
    or $0x3, %eax
    mov $boot_pdpt, %edi
    mov $4, %ecx
1:  mov %eax, (%edi)
    add $4096, %eax
    add $8, %edi
    loop 1b

    /* PD entries: entry i maps [i * 2 MiB, (i+1) * 2 MiB) with present | writable | PS (2 MiB page). */
    mov $boot_pd, %edi
    mov $0x83, %eax
    mov $2048, %ecx
2:  mov %eax, (%edi)
    add $0x200000, %eax
    add $8, %edi
    loop 2b

    /* The top 512 MiB (0xE000_0000..4 GiB: PCI MMIO hole, I/O APIC, local APIC) are mapped uncached
       (PCD | PWT). Guest RAM never reaches there with QEMU's i440fx machine and -m <= 3.5 GiB. */
    mov $boot_pd + 1792 * 8, %edi
    mov $256, %ecx
3:  orl $0x18, (%edi)
    add $8, %edi
    loop 3b

    /* CR4: PAE (5), OSFXSR (9), OSXMMEXCPT (10). SSE is enabled for the future JIT; the kernel itself is
       compiled soft-float, so interrupt handlers never touch vector registers. */
    mov %cr4, %eax
    or $((1 << 5) | (1 << 9) | (1 << 10)), %eax
    mov %eax, %cr4
    mov $boot_pml4, %eax
    mov %eax, %cr3
    /* EFER.LME */
    mov $0xC0000080, %ecx
    rdmsr
    or $(1 << 8), %eax
    wrmsr
    /* CR0: PG (31), WP (16), NE (5), MP (1), PE (0); clear EM (2) and TS (3). */
    mov %cr0, %eax
    and $~((1 << 2) | (1 << 3)), %eax
    or $((1 << 31) | (1 << 16) | (1 << 5) | (1 << 1) | 1), %eax
    mov %eax, %cr0

    lgdt boot_gdt_ptr
    /* Far return into the 64-bit code segment (selector 0x08). */
    push $0x08
    push $long_mode_entry
    lret

.code64
long_mode_entry:
    mov $0x10, %ax
    mov %ax, %ds
    mov %ax, %es
    mov %ax, %ss
    xor %eax, %eax
    mov %ax, %fs
    mov %ax, %gs
    mov $boot_stack_top, %rsp
    xor %ebp, %ebp
    call kmain
4:  cli
    hlt
    jmp 4b
.popsection
"#,
    options(att_syntax)
);

extern "C" {
    static boot_stack_bottom: u8;
    static boot_stack_top: u8;
    static pvh_start_info_ptr: u64;
    static __kernel_start: u8;
    static __kernel_end: u8;
}

/// Physical address of the PVH `hvm_start_info` handed over in `ebx`.
pub fn start_info_paddr() -> u64 {
    // SAFETY: written once by the trampoline before any Rust code runs.
    unsafe { core::ptr::read_volatile(&raw const pvh_start_info_ptr) }
}

/// `[start, end)` of the loaded kernel image (code, data, bss, boot page tables and boot stack).
pub fn kernel_image() -> (u64, u64) {
    (&raw const __kernel_start as u64, &raw const __kernel_end as u64)
}

/// `[bottom, top)` of the boot stack (the scheduler's stack).
pub fn boot_stack() -> (u64, u64) {
    (&raw const boot_stack_bottom as u64, &raw const boot_stack_top as u64)
}
