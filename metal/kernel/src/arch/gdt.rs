//! 64-bit GDT with a TSS, one pair per CPU. The only reason for a TSS on a kernel with no user mode is the Interrupt
//! Stack Table: a double fault (e.g. after a stack overflow) runs on its own stack so it can still print a
//! diagnostic. Each CPU needs its own TSS (the CPU marks the loaded TSS busy in the GDT), hence its own GDT.

use core::arch::asm;
use core::mem::size_of;

use crate::percpu::MAX_CPUS;

pub const KERNEL_CS: u16 = 0x08;
pub const KERNEL_DS: u16 = 0x10;
const TSS_SEL: u16 = 0x18;
/// IST index (1-based) used by the double-fault, NMI and machine-check handlers.
pub const IST_FAULT: u8 = 1;

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct Tss {
    _r0: u32,
    rsp: [u64; 3],
    _r1: u64,
    ist: [u64; 7],
    _r2: u64,
    _r3: u16,
    iomap_base: u16,
}

// Zero-initialized (in .bss): `init_cpu` fills in each CPU's entry.
const TSS0: Tss = Tss { _r0: 0, rsp: [0; 3], _r1: 0, ist: [0; 7], _r2: 0, _r3: 0, iomap_base: 0 };
static mut TSS: [Tss; MAX_CPUS] = [TSS0; MAX_CPUS];

/// The boot CPU's fault stack (application processors get theirs from the frame allocator).
#[repr(C, align(16))]
struct FaultStack([u8; 16384]);
static mut FAULT_STACK: FaultStack = FaultStack([0; 16384]);
pub const FAULT_STACK_SIZE: usize = 16384;

const GDT0: [u64; 5] = [
    0,
    0x00AF_9A00_0000_FFFF, // 0x08 code64
    0x00CF_9200_0000_FFFF, // 0x10 data
    0,                     // 0x18 TSS (low)
    0,                     // 0x20 TSS (high)
];
static mut GDT: [[u64; 5]; MAX_CPUS] = [[0; 5]; MAX_CPUS];

#[repr(C, packed)]
struct Pointer {
    limit: u16,
    base: u64,
}

/// Boot CPU.
pub fn init() {
    let top = (&raw mut FAULT_STACK as u64) + size_of::<FaultStack>() as u64;
    init_cpu(0, top);
}

/// Loads CPU `cpu`'s GDT and TSS, with `fault_stack_top` as its IST stack.
pub fn init_cpu(cpu: usize, fault_stack_top: u64) {
    unsafe {
        let tss = (&raw mut TSS).cast::<Tss>().add(cpu);
        (*tss).ist[IST_FAULT as usize - 1] = fault_stack_top;
        (*tss).iomap_base = size_of::<Tss>() as u16;

        // 64-bit available TSS descriptor (type 0x9, present).
        let base = tss as u64;
        let limit = (size_of::<Tss>() - 1) as u64;
        let low = (limit & 0xFFFF)
            | ((base & 0xFF_FFFF) << 16)
            | (0x89u64 << 40)
            | (((limit >> 16) & 0xF) << 48)
            | (((base >> 24) & 0xFF) << 56);
        let gdt = (&raw mut GDT).cast::<[u64; 5]>().add(cpu);
        *gdt = GDT0;
        (*gdt)[3] = low;
        (*gdt)[4] = base >> 32;

        let ptr = Pointer { limit: (size_of::<[u64; 5]>() - 1) as u16, base: gdt as u64 };
        asm!(
            "lgdt [{p}]",
            // Reload CS with a far return, then the data segments.
            "push {cs}",
            "lea {tmp}, [rip + 2f]",
            "push {tmp}",
            "retfq",
            "2:",
            "mov ds, {ds:x}",
            "mov es, {ds:x}",
            "mov ss, {ds:x}",
            "ltr {tss:x}",
            p = in(reg) &ptr,
            cs = in(reg) KERNEL_CS as u64,
            tmp = out(reg) _,
            ds = in(reg) KERNEL_DS as u64,
            tss = in(reg) TSS_SEL as u64,
        );
    }
}
