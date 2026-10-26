//! Interrupt Descriptor Table: CPU exceptions (fatal, with a register dump on the console), the stop NMI, and the
//! few device interrupts the kernel uses (LAPIC timer, the NIC's MSI-X or I/O APIC line). Entry stubs are generated in assembly: they push a
//! uniform frame (dummy error code where the CPU pushes none, the vector number, all general registers) and call
//! [`isr_dispatch`].

use core::arch::{asm, global_asm};
use core::mem::size_of;

use super::gdt::{IST_FAULT, KERNEL_CS};
use crate::kprintln;

pub const VEC_TIMER: u8 = 0x20;
pub const VEC_NET: u8 = 0x21;
/// Inter-processor wake-up: work was queued for this CPU (the handler only acknowledges; the point is leaving `hlt`).
pub const VEC_WAKE: u8 = 0x22;
/// Disk completions (MSI-X): the handler only acknowledges; the point is leaving `hlt` on CPU 0.
pub const VEC_DISK: u8 = 0x23;
/// The ACPI SCI (I/O APIC line): the power button (power.rs).
pub const VEC_SCI: u8 = 0x24;
pub const VEC_SPURIOUS: u8 = 0xFF;
/// NMIs that were not the stop NMI, and the last one's NMI status (port 0x61).
pub static NMIS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static NMI_REASON: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0);
/// Number of stubs 0..NSTUBS, plus the spurious vector.
const NSTUBS: usize = 48;

macro_rules! isr_stubs {
    () => {
        r#"
.macro ISR_NOERR n
isr_stub_\n:
    pushq $0
    pushq $\n
    jmp isr_common
.endm
.macro ISR_ERR n
isr_stub_\n:
    pushq $\n
    jmp isr_common
.endm

.pushsection .text.isr, "ax"
.irp n, 0,1,2,3,4,5,6,7,9,15,16,18,19,20,22,23,24,25,26,27,28,31
    ISR_NOERR \n
.endr
.irp n, 8,10,11,12,13,14,17,21,29,30
    ISR_ERR \n
.endr
.irp n, 32,33,34,35,36,37,38,39,40,41,42,43,44,45,46,47,255
    ISR_NOERR \n
.endr

isr_common:
    push %rax
    push %rbx
    push %rcx
    push %rdx
    push %rsi
    push %rdi
    push %rbp
    push %r8
    push %r9
    push %r10
    push %r11
    push %r12
    push %r13
    push %r14
    push %r15
    mov %rsp, %rdi
    cld
    call isr_dispatch
    pop %r15
    pop %r14
    pop %r13
    pop %r12
    pop %r11
    pop %r10
    pop %r9
    pop %r8
    pop %rbp
    pop %rdi
    pop %rsi
    pop %rdx
    pop %rcx
    pop %rbx
    pop %rax
    add $16, %rsp
    iretq
.popsection

.pushsection .rodata.isr, "a"
.balign 8
.global isr_stub_table
isr_stub_table:
.irp n, 0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31,32,33,34,35,36,37,38,39,40,41,42,43,44,45,46,47
    .quad isr_stub_\n
.endr
.global isr_stub_spurious
isr_stub_spurious:
    .quad isr_stub_255
.popsection
"#
    };
}

global_asm!(isr_stubs!(), options(att_syntax));

extern "C" {
    static isr_stub_table: [u64; NSTUBS];
    static isr_stub_spurious: u64;
}

/// Saved state, in the order the stubs push it (lowest address first).
#[repr(C)]
#[derive(Debug)]
pub struct Frame {
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub r11: u64,
    pub r10: u64,
    pub r9: u64,
    pub r8: u64,
    pub rbp: u64,
    pub rdi: u64,
    pub rsi: u64,
    pub rdx: u64,
    pub rcx: u64,
    pub rbx: u64,
    pub rax: u64,
    pub vector: u64,
    pub error: u64,
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Entry {
    off_lo: u16,
    sel: u16,
    ist: u8,
    attr: u8,
    off_mid: u16,
    off_hi: u32,
    _zero: u32,
}

static mut IDT: [Entry; 256] = [Entry { off_lo: 0, sel: 0, ist: 0, attr: 0, off_mid: 0, off_hi: 0, _zero: 0 }; 256];

fn gate(handler: u64, ist: u8) -> Entry {
    Entry {
        off_lo: handler as u16,
        sel: KERNEL_CS,
        ist,
        attr: 0x8E, // present, DPL 0, 64-bit interrupt gate (IF cleared on entry)
        off_mid: (handler >> 16) as u16,
        off_hi: (handler >> 32) as u32,
        _zero: 0,
    }
}

pub fn init() {
    unsafe {
        let idt = &raw mut IDT;
        for v in 0..NSTUBS {
            // Double fault, NMI and machine check get the dedicated IST stack.
            let ist = if v == 8 || v == 2 || v == 18 { IST_FAULT } else { 0 };
            (*idt)[v] = gate(isr_stub_table[v], ist);
        }
        (*idt)[VEC_SPURIOUS as usize] = gate(isr_stub_spurious, 0);
    }
    load();
}

/// Loads the (shared) IDT on this CPU.
pub fn load() {
    #[repr(C, packed)]
    struct Ptr {
        limit: u16,
        base: u64,
    }
    let p = Ptr { limit: (size_of::<[Entry; 256]>() - 1) as u16, base: &raw const IDT as u64 };
    unsafe { asm!("lidt [{}]", in(reg) &p, options(readonly, nostack, preserves_flags)) };
}

const EXCEPTIONS: [&str; 32] = [
    "divide error", "debug", "NMI", "breakpoint", "overflow", "bound range", "invalid opcode", "device not available",
    "double fault", "coprocessor segment overrun", "invalid TSS", "segment not present", "stack-segment fault",
    "general protection", "page fault", "reserved", "x87 FP error", "alignment check", "machine check",
    "SIMD FP error", "virtualization", "control protection", "reserved", "reserved", "reserved", "reserved",
    "reserved", "reserved", "hypervisor injection", "VMM communication", "security", "reserved",
];

#[no_mangle]
extern "C" fn isr_dispatch(f: &mut Frame) {
    match f.vector as u8 {
        // Another CPU is powering off, rebooting or panicking (power.rs).
        2 if crate::power::STOPPING.load(core::sync::atomic::Ordering::SeqCst) => super::halt_forever(),
        // Any other NMI (a BMC, a watchdog, a bus error: port 0x61 bits 7 and 6) is counted, reported by the main
        // loop, and survived, as Linux does.
        2 => {
            NMIS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            NMI_REASON.store(super::inb(0x61), core::sync::atomic::Ordering::Relaxed);
        }
        v if v < 32 => exception(f),
        VEC_TIMER => {
            crate::time::on_timer_irq();
            super::apic::eoi();
        }
        VEC_NET => {
            crate::net::on_irq();
            super::apic::eoi();
        }
        VEC_WAKE => {
            crate::percpu::this().wakeups.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            super::apic::eoi();
        }
        VEC_SCI => {
            crate::power::on_sci();
            super::apic::eoi();
        }
        VEC_SPURIOUS => {}
        _ => super::apic::eoi(),
    }
}

fn exception(f: &Frame) -> ! {
    // Take the console even if the faulting code held it.
    unsafe { crate::serial::SERIAL.force_unlock() };
    let v = f.vector as usize;
    kprintln!("\n!!! CPU exception {} ({}) error={:#x}", v, EXCEPTIONS[v], f.error);
    kprintln!("    rip={:#018x} cs={:#x} rflags={:#x} rsp={:#018x} ss={:#x}", f.rip, f.cs, f.rflags, f.rsp, f.ss);
    if v == 14 {
        kprintln!(
            "    cr2={:#018x} ({} {} {})",
            super::read_cr2(),
            if f.error & 1 != 0 { "protection" } else { "not-present" },
            if f.error & 2 != 0 { "write" } else { "read" },
            if f.error & 16 != 0 { "ifetch" } else { "" }
        );
    }
    kprintln!("    rax={:#018x} rbx={:#018x} rcx={:#018x} rdx={:#018x}", f.rax, f.rbx, f.rcx, f.rdx);
    kprintln!("    rsi={:#018x} rdi={:#018x} rbp={:#018x} r8 ={:#018x}", f.rsi, f.rdi, f.rbp, f.r8);
    kprintln!("    r9 ={:#018x} r10={:#018x} r11={:#018x} r12={:#018x}", f.r9, f.r10, f.r11, f.r12);
    kprintln!("    r13={:#018x} r14={:#018x} r15={:#018x}", f.r13, f.r14, f.r15);
    if let Some(s) = crate::coro::current_stack() {
        kprintln!("    running on coroutine stack {:#x}..{:#x}", s.0, s.1);
    }
    kprintln!("    on CPU {}", crate::percpu::id());
    crate::power::fatal_stop()
}
