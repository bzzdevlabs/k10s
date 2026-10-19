//! 64-bit GDT with a TSS. The only reason for a TSS on a kernel with no user mode is the Interrupt Stack Table:
//! a double fault (e.g. after a stack overflow) runs on its own stack so it can still print a diagnostic.

use core::arch::asm;
use core::mem::size_of;

pub const KERNEL_CS: u16 = 0x08;
pub const KERNEL_DS: u16 = 0x10;
const TSS_SEL: u16 = 0x18;
/// IST index (1-based) used by the double-fault, NMI and machine-check handlers.
pub const IST_FAULT: u8 = 1;

#[repr(C, packed)]
struct Tss {
    _r0: u32,
    rsp: [u64; 3],
    _r1: u64,
    ist: [u64; 7],
    _r2: u64,
    _r3: u16,
    iomap_base: u16,
}

static mut TSS: Tss = Tss { _r0: 0, rsp: [0; 3], _r1: 0, ist: [0; 7], _r2: 0, _r3: 0, iomap_base: size_of::<Tss>() as u16 };

#[repr(C, align(16))]
struct FaultStack([u8; 16384]);
static mut FAULT_STACK: FaultStack = FaultStack([0; 16384]);

static mut GDT: [u64; 5] = [
    0,
    0x00AF_9A00_0000_FFFF, // 0x08 code64
    0x00CF_9200_0000_FFFF, // 0x10 data
    0,                     // 0x18 TSS (low)
    0,                     // 0x20 TSS (high)
];

#[repr(C, packed)]
struct Pointer {
    limit: u16,
    base: u64,
}

pub fn init() {
    unsafe {
        let stack_top = (&raw mut FAULT_STACK as u64) + size_of::<FaultStack>() as u64;
        (*(&raw mut TSS)).ist[IST_FAULT as usize - 1] = stack_top;

        // 64-bit available TSS descriptor (type 0x9, present).
        let base = &raw const TSS as u64;
        let limit = (size_of::<Tss>() - 1) as u64;
        let low = (limit & 0xFFFF)
            | ((base & 0xFF_FFFF) << 16)
            | (0x89u64 << 40)
            | (((limit >> 16) & 0xF) << 48)
            | (((base >> 24) & 0xFF) << 56);
        let gdt = &raw mut GDT;
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
