//! Thin wrappers over x86-64 instructions (port I/O, MSRs, CPUID, TSC, control registers, interrupts).

pub mod gdt;
pub mod idt;
pub mod apic;

use core::arch::asm;

#[inline(always)]
pub fn outb(port: u16, v: u8) {
    unsafe { asm!("out dx, al", in("dx") port, in("al") v, options(nomem, nostack, preserves_flags)) }
}
#[inline(always)]
pub fn inb(port: u16) -> u8 {
    let v: u8;
    unsafe { asm!("in al, dx", out("al") v, in("dx") port, options(nomem, nostack, preserves_flags)) }
    v
}
#[inline(always)]
pub fn outw(port: u16, v: u16) {
    unsafe { asm!("out dx, ax", in("dx") port, in("ax") v, options(nomem, nostack, preserves_flags)) }
}
#[inline(always)]
pub fn inw(port: u16) -> u16 {
    let v: u16;
    unsafe { asm!("in ax, dx", out("ax") v, in("dx") port, options(nomem, nostack, preserves_flags)) }
    v
}
#[inline(always)]
pub fn outl(port: u16, v: u32) {
    unsafe { asm!("out dx, eax", in("dx") port, in("eax") v, options(nomem, nostack, preserves_flags)) }
}
#[inline(always)]
pub fn inl(port: u16) -> u32 {
    let v: u32;
    unsafe { asm!("in eax, dx", out("eax") v, in("dx") port, options(nomem, nostack, preserves_flags)) }
    v
}

#[inline(always)]
pub fn rdmsr(msr: u32) -> u64 {
    let (lo, hi): (u32, u32);
    unsafe { asm!("rdmsr", in("ecx") msr, out("eax") lo, out("edx") hi, options(nomem, nostack, preserves_flags)) }
    (hi as u64) << 32 | lo as u64
}
#[inline(always)]
pub fn wrmsr(msr: u32, v: u64) {
    unsafe {
        asm!("wrmsr", in("ecx") msr, in("eax") v as u32, in("edx") (v >> 32) as u32, options(nostack, preserves_flags))
    }
}

/// `cpuid(leaf, subleaf)` → (eax, ebx, ecx, edx).
#[inline]
pub fn cpuid(leaf: u32, sub: u32) -> (u32, u32, u32, u32) {
    let r = core::arch::x86_64::__cpuid_count(leaf, sub);
    (r.eax, r.ebx, r.ecx, r.edx)
}

#[inline(always)]
pub fn rdtsc() -> u64 {
    unsafe { core::arch::x86_64::_rdtsc() }
}

#[inline(always)]
pub fn read_cr2() -> u64 {
    let v: u64;
    unsafe { asm!("mov {}, cr2", out(reg) v, options(nomem, nostack, preserves_flags)) }
    v
}

#[inline(always)]
pub fn interrupts_enable() {
    unsafe { asm!("sti", options(nomem, nostack)) }
}
#[inline(always)]
pub fn interrupts_disable() {
    unsafe { asm!("cli", options(nomem, nostack)) }
}
#[inline(always)]
pub fn interrupts_enabled() -> bool {
    let f: u64;
    unsafe { asm!("pushfq; pop {}", out(reg) f, options(nomem, preserves_flags)) }
    f & (1 << 9) != 0
}

/// Atomically enables interrupts and halts until the next one (`sti` delays interrupt delivery by one
/// instruction, so a wake-up interrupt cannot slip in between the caller's last check and `hlt`).
#[inline(always)]
pub fn enable_and_halt() {
    unsafe { asm!("sti; hlt", options(nomem, nostack)) }
}

#[inline(always)]
pub fn halt_forever() -> ! {
    loop {
        unsafe { asm!("cli; hlt", options(nomem, nostack)) }
    }
}

#[inline(always)]
pub fn pause() {
    core::hint::spin_loop()
}

/// RDRAND, if the CPU has it (retries a few times as the SDM recommends).
pub fn rdrand() -> Option<u64> {
    if !has_rdrand() {
        return None;
    }
    for _ in 0..10 {
        let v: u64;
        let ok: u8;
        unsafe { asm!("rdrand {}", "setc {}", out(reg) v, out(reg_byte) ok, options(nomem, nostack)) }
        if ok != 0 {
            return Some(v);
        }
    }
    None
}

pub fn has_rdrand() -> bool {
    cpuid(1, 0).2 & (1 << 30) != 0
}

/// QEMU `isa-debug-exit` device (`-device isa-debug-exit,iobase=0xf4,iosize=0x04`): QEMU exits with status
/// `(code << 1) | 1`. Without the device the write is ignored.
pub fn qemu_exit(code: u8) {
    outl(0xf4, code as u32);
}

/// Exit codes written to `isa-debug-exit` (QEMU status = code * 2 + 1).
pub const EXIT_SUCCESS: u8 = 0x10; // → 33
pub const EXIT_FAILURE: u8 = 0x11; // → 35
