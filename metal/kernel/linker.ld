/* k10-metal: loaded at physical 1 MiB by QEMU's ELF loader (PVH boot), k10boot or GRUB (program headers' p_paddr),
   linked at KERNEL_BASE + 1 MiB in the top 2 GiB (mem/addr.rs): the first section's AT() sets every section's load
   address to its link address less KERNEL_BASE. The 32-bit entry code runs at physical addresses (boot.rs). */
ENTRY(_pvh_start)

KERNEL_BASE = 0xFFFFFFFF80000000;

SECTIONS
{
    . = KERNEL_BASE + 1M;
    __kernel_start = .;

    /* The PVH entry note must be in a PT_NOTE segment, 4-byte aligned (QEMU aligns the name to p_align). */
    .note.Xen : AT(ADDR(.note.Xen) - KERNEL_BASE) ALIGN(4) { KEEP(*(.note.Xen)) }

    /* GRUB looks for the Multiboot2 header in the first 32 KiB of the file (boot.rs). */
    .multiboot2 : ALIGN(8) { KEEP(*(.multiboot2)) }

    .text : ALIGN(4K)
    {
        __text_start = .;
        KEEP(*(.text.boot32))
        *(.text .text.*)
    }

    .rodata : ALIGN(4K) { __rodata_start = .; *(.rodata .rodata.*) }

    .data : ALIGN(4K) { __data_start = .; *(.data .data.*) }

    .bss (NOLOAD) : ALIGN(4K)
    {
        __bss_start = .;
        *(.bss.boot)
        *(.bss .bss.*)
        *(COMMON)
        . = ALIGN(8);
        __bss_end = .;
    }

    /* The Multiboot2 shim's start_info, which _pvh_start must not zero (boot.rs). */
    .mb2 (NOLOAD) : ALIGN(8) { *(.mb2) }

    . = ALIGN(4K);
    __kernel_end = .;

    /DISCARD/ : { *(.eh_frame .eh_frame_hdr .comment .note.GNU-stack) }
}
