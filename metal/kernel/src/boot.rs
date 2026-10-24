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
//!
//! Two more ways in end at the same entry with a `start_info` (metal/README.md, "Booting"):
//!   * k10boot, the UEFI loader (metal/boot), loads the ELF's segments and enters `_pvh_start` as QEMU does;
//!   * GRUB on BIOS (`multiboot2`): the Multiboot2 header below asks for the command line, modules (page-aligned),
//!     memory map and ACPI RSDP, and makes `_mb2_start` the entry (32-bit protected mode, paging off, eax = the
//!     Multiboot2 magic, ebx = its information). That shim converts the information into a `start_info` in `.mb2`,
//!     a section after .bss that `_pvh_start` does not zero: the memory map entries (their first 20 bytes are an
//!     E820 entry), the modules with their command lines as names, the RSDP copied (GRUB's copy lives in its
//!     information, which the kernel reuses as RAM). Then `_pvh_start`.

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
/* Multiboot2 header (GRUB finds it in the first 32 KiB of the file): requests (optional), entry, module alignment. */
.pushsection .multiboot2, "a"
    .balign 8
mb2_header:
    .long 0xE85250D6
    .long 0                 /* i386 */
    .long mb2_header_end - mb2_header
    .long 0x100000000 - (0xE85250D6 + (mb2_header_end - mb2_header))
    .balign 8
    .word 1, 1              /* information request, optional: */
    .long 8 + 4 * 5
    .long 1, 3, 6, 14, 15   /* command line, modules, memory map, ACPI RSDP v1, v2 */
    .balign 8
    .word 3, 0              /* entry address */
    .long 12
    .long _mb2_start
    .balign 8
    .word 6, 0              /* modules page-aligned: the kernel releases module pages one module at a time */
    .long 8
    .balign 8
    .word 0, 0              /* end */
    .long 8
mb2_header_end:
.popsection

.pushsection .mb2, "aw", @nobits
    .balign 8
mb2_si:      .skip 64       /* struct hvm_start_info */
mb2_modlist: .skip 32 * 16  /* struct hvm_modlist_entry[MAX_MODULES] */
mb2_memmap:  .skip 24 * 256 /* struct hvm_memmap_table_entry[MAX_MEMMAP] */
mb2_rsdp:    .skip 40
mb2_rsdp_v2: .skip 4
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
    .skip 131072                    /* kmain's frame holds the boot information (~16 KiB) */
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
    .global _mb2_start
_mb2_start:
    cli
    cld
    mov $boot_stack_top, %esp
    cmp $0x36d76289, %eax
    jne 9f
    mov (%ebx), %edx
    add %ebx, %edx                  /* edx: end of the information */
    lea 8(%ebx), %esi               /* esi: the first tag */
    xor %eax, %eax
    mov $mb2_si, %edi
    mov $64, %ecx
    rep stosb
    movl $0, mb2_rsdp_v2
1:  cmp %edx, %esi
    jae 7f
    mov (%esi), %eax                /* tag type */
    test %eax, %eax
    jz 7f
    cmp $1, %eax
    je 10f
    cmp $3, %eax
    je 20f
    cmp $6, %eax
    je 30f
    cmp $14, %eax
    je 40f
    cmp $15, %eax
    je 50f
6:  mov 4(%esi), %eax               /* next tag: size rounded up to 8 bytes */
    add $7, %eax
    and $~7, %eax
    jz 7f
    add %eax, %esi
    jmp 1b

10: lea 8(%esi), %eax               /* command line */
    mov %eax, mb2_si + 24
    jmp 6b

20: mov mb2_si + 12, %eax           /* module: start, end, then its command line (the name) */
    cmp $16, %eax
    jae 6b
    shl $5, %eax
    add $mb2_modlist, %eax
    mov 8(%esi), %ecx
    mov %ecx, (%eax)
    mov 12(%esi), %ecx
    sub 8(%esi), %ecx
    mov %ecx, 8(%eax)
    lea 16(%esi), %ecx
    mov %ecx, 16(%eax)
    incl mb2_si + 12
    jmp 6b

30: push %esi                       /* memory map: entry size, version, then entries */
    push %edx
    mov 8(%esi), %ebx
    mov 4(%esi), %edx
    add %esi, %edx                  /* edx: end of the tag */
    add $16, %esi
    cmp $24, %ebx
    jb 39f
31: lea (%esi, %ebx), %eax
    cmp %edx, %eax
    ja 39f
    mov mb2_si + 48, %eax
    cmp $256, %eax
    jae 39f
    imul $24, %eax, %edi
    add $mb2_memmap, %edi
    mov %esi, %ecx
    movsl                           /* base, length, type */
    movsl
    movsl
    movsl
    movsl
    movl $0, (%edi)
    mov %ecx, %esi
    add %ebx, %esi
    incl mb2_si + 48
    jmp 31b
39: pop %edx
    pop %esi
    jmp 6b

40: cmpl $0, mb2_rsdp_v2            /* ACPI RSDP v1 (20 bytes), unless v2 came first */
    jne 6b
    mov $20, %ecx
    jmp 45f
50: movl $1, mb2_rsdp_v2            /* ACPI RSDP v2 (36 bytes) */
    mov 4(%esi), %ecx
    sub $8, %ecx
    cmp $40, %ecx
    jbe 45f
    mov $40, %ecx
45: push %esi
    add $8, %esi
    mov $mb2_rsdp, %edi
    rep movsb
    pop %esi
    movl $mb2_rsdp, mb2_si + 32
    jmp 6b

7:  movl $0x336ec578, mb2_si        /* magic, version 1, module list, memory map */
    movl $1, mb2_si + 4
    movl $mb2_modlist, mb2_si + 16
    movl $mb2_memmap, mb2_si + 40
    mov $mb2_si, %ebx
    jmp _pvh_start
9:  hlt                             /* not started by a Multiboot2 loader */
    jmp 9b

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
