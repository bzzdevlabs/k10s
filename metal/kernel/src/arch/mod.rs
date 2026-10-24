//! Thin wrappers over x86-64 instructions (port I/O, MSRs, CPUID, TSC, control registers, interrupts).

pub mod apic;
pub mod gdt;
pub mod idt;
pub mod ioapic;
pub mod paging;

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

/// Fills `buf` from RDRAND; false if it failed (callers refuse the operation, never fall back).
#[must_use]
pub fn rdrand_bytes(buf: &mut [u8]) -> bool {
    for chunk in buf.chunks_mut(8) {
        let Some(v) = rdrand() else { return false };
        chunk.copy_from_slice(&v.to_le_bytes()[..chunk.len()]);
    }
    true
}

pub fn has_rdrand() -> bool {
    cpuid(1, 0).2 & (1 << 30) != 0
}

/// Resets the CPU (and so the machine) by a triple fault: an empty IDT, then an exception.
pub fn triple_fault() -> ! {
    #[repr(C, packed)]
    struct Ptr {
        limit: u16,
        base: u64,
    }
    let p = Ptr { limit: 0, base: 0 };
    unsafe { asm!("lidt [{}]", "int3", in(reg) &p, options(nostack)) };
    halt_forever()
}
