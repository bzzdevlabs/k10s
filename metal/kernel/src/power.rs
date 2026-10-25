//! Power off and reboot through ACPI, the same on VMs and real machines (no hypervisor-specific exit device).
//!
//! * Power off: ACPI S5. Hardware-reduced platforms (QEMU microvm, Cloud Hypervisor) write SLP_TYP | SLP_EN to the
//!   FADT sleep control register; the others switch to ACPI mode if the firmware left it in legacy mode (SMI_CMD ←
//!   ACPI_ENABLE, then wait for SCI_EN), disable every GPE (no wake-up), clear the PM1 status bits and write SLP_TYP,
//!   then SLP_TYP | SLP_EN, to PM1a/PM1b control. The `\_S5` values come from the DSDT scan; `_PTS` is not run
//!   (no AML). Firecracker has no ACPI power-off: there a reset ends the VM, so power off resets.
//! * Reboot: the FADT reset register, then the 8042 keyboard controller pulse, then port 0xCF9 (PCH/PIIX reset
//!   control), then a triple fault.
//! * Fatal errors and panics stop the other CPUs (NMI), print, wait `k10.panic` seconds (default 10; 0 = halt
//!   forever) and reboot, so a boot-counting loader can fall back to another image.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::acpi::{self, Gas};
use crate::arch::{self, inb, outb};
use crate::kprintln;

/// Set once a CPU has begun stopping the machine: the others halt on the stop NMI.
pub static STOPPING: AtomicBool = AtomicBool::new(false);
/// `k10.panic`: seconds before a fatal error reboots (u64::MAX = never).
static PANIC_REBOOT_S: AtomicU64 = AtomicU64::new(10);

const PM1_SCI_EN: u64 = 1;
const PM1_SLP_EN: u64 = 1 << 13;
const SLP_TYP_SHIFT: u32 = 10;

/// A power request from the admin API: 0 none, 1 off, 2 reboot; and when it came (ns since boot).
static REQUEST: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0);
static REQUEST_AT: AtomicU64 = AtomicU64::new(0);

/// Asks the main loop to power off or reboot (after answering the request and leaving the cluster).
pub fn request(reboot: bool) {
    REQUEST_AT.store(crate::time::now_ns(), Ordering::Relaxed);
    REQUEST.store(if reboot { 2 } else { 1 }, Ordering::Release);
}

/// The pending request: (reboot, when).
pub fn requested() -> Option<(bool, u64)> {
    match REQUEST.load(Ordering::Acquire) {
        0 => None,
        r => Some((r == 2, REQUEST_AT.load(Ordering::Relaxed))),
    }
}

pub fn set_panic_reboot(seconds: Option<u64>) {
    PANIC_REBOOT_S.store(match seconds {
        Some(0) => u64::MAX,
        Some(s) => s,
        None => 10,
    }, Ordering::Relaxed);
}

/// Busy-waits (works with interrupts off and before the clock is calibrated: then a rough loop).
fn delay_ms(ms: u64) {
    let hz = crate::time::tsc_hz();
    if hz == 0 {
        for _ in 0..ms * 200_000 {
            core::hint::spin_loop();
        }
        return;
    }
    let end = arch::rdtsc() + hz / 1000 * ms;
    while arch::rdtsc() < end {
        core::hint::spin_loop();
    }
}

/// Stops every other CPU the kernel started (an NMI each; their handler halts when [`STOPPING`] is set), then takes
/// the console whoever held it. Returns false if this CPU lost the race to another one stopping the machine. What
/// runs after it takes no lock but the console's: register accesses are lock-free (acpi.rs maps them at boot).
fn stop_others() -> bool {
    if STOPPING.swap(true, Ordering::SeqCst) {
        return false;
    }
    let me = crate::percpu::id();
    let mut sent = false;
    for i in (0..crate::percpu::MAX_CPUS).filter(|&i| i != me) {
        let c = crate::percpu::cpu(i);
        if c.online.load(Ordering::Acquire) {
            arch::apic::send_nmi(c.apic_id.load(Ordering::Relaxed));
            sent = true;
        }
    }
    if sent {
        delay_ms(10);
    }
    // SAFETY: every other CPU is halted (or was never started), so whoever held the consoles never runs again.
    unsafe {
        crate::serial::SERIAL.force_unlock();
        crate::console::force_unlock();
    }
    true
}

fn firecracker() -> bool {
    acpi::get().is_some_and(|t| &t.oem == b"FIRECK")
}

/// Powers the machine off. Never returns: halts if no power-off method worked.
pub fn off() -> ! {
    arch::interrupts_disable();
    if !stop_others() {
        arch::halt_forever();
    }
    crate::watchdog::disarm();
    kprintln!("k10-metal: power off");
    if firecracker() {
        reset();
    }
    if let Some(t) = acpi::get() {
        if let (Some(f), Some((a, b))) = (t.fadt, t.s5) {
            if t.hw_reduced() {
                if f.sleep_control.valid() {
                    f.sleep_control.write(((a as u64 & 7) << 2) | (1 << 5));
                }
            } else if f.pm1a_cnt.valid() {
                acpi_mode(&f);
                disable_gpes(f.gpe0, f.gpe0_len);
                disable_gpes(f.gpe1, f.gpe1_len);
                clear_pm1_status(f.pm1a_evt);
                clear_pm1_status(f.pm1b_evt);
                let typ = |cnt: Gas, v: u8| (cnt.read() & !(7 << SLP_TYP_SHIFT | PM1_SLP_EN)) | (v as u64 & 7) << SLP_TYP_SHIFT;
                let (ta, tb) = (typ(f.pm1a_cnt, a), f.pm1b_cnt.valid().then(|| typ(f.pm1b_cnt, b)));
                f.pm1a_cnt.write(ta);
                if let Some(tb) = tb {
                    f.pm1b_cnt.write(tb);
                }
                f.pm1a_cnt.write(ta | PM1_SLP_EN);
                if let Some(tb) = tb {
                    f.pm1b_cnt.write(tb | PM1_SLP_EN);
                }
            }
            delay_ms(3000);
        }
    }
    kprintln!("k10-metal: ACPI power off unavailable or failed; halted");
    arch::halt_forever()
}

/// Switches from legacy (SMM-owned) to ACPI mode if needed, waiting up to 3 s for SCI_EN.
fn acpi_mode(f: &acpi::Fadt) {
    if f.pm1a_cnt.read() & PM1_SCI_EN != 0 || f.smi_cmd == 0 || f.acpi_enable == 0 {
        return;
    }
    outb(f.smi_cmd as u16, f.acpi_enable);
    for _ in 0..300 {
        if f.pm1a_cnt.read() & PM1_SCI_EN != 0 {
            return;
        }
        delay_ms(10);
    }
}

/// Zeroes the enable half of a GPE block (I/O or memory, byte registers).
fn disable_gpes(blk: Gas, len: u8) {
    if !blk.valid() || blk.space == acpi::SPACE_PCI {
        return;
    }
    let half = len as u64 / 2;
    for i in 0..half {
        Gas { addr: blk.addr + half + i, bits: 8, access: 1, ..blk }.write(0);
    }
}

/// Writes 1 to every PM1 status bit (write-one-to-clear), wake status included.
fn clear_pm1_status(evt: Gas) {
    if evt.valid() {
        Gas { bits: 16, access: 2, ..evt }.write(0xFFFF);
    }
}

/// Resets the machine. Never returns.
pub fn reboot() -> ! {
    arch::interrupts_disable();
    if !stop_others() {
        arch::halt_forever();
    }
    crate::watchdog::disarm();
    kprintln!("k10-metal: reboot");
    reset()
}

fn reset() -> ! {
    if let Some(f) = acpi::get().and_then(|t| t.fadt) {
        if f.reset.valid() && (f.flags & acpi::FADT_RESET_REG_SUP != 0 || f.reset.space == acpi::SPACE_IO) {
            f.reset.write(f.reset_value as u64);
            delay_ms(50);
        }
    }
    // 8042: wait for an empty input buffer, then pulse the reset line (absent controllers read 0xFF).
    if inb(0x64) != 0xFF {
        for _ in 0..1000 {
            if inb(0x64) & 2 == 0 {
                break;
            }
            delay_ms(1);
        }
        outb(0x64, 0xFE);
        delay_ms(50);
    }
    // Reset control register: system reset, then full reset (CPU and platform).
    outb(0xCF9, 0x02);
    delay_ms(1);
    outb(0xCF9, 0x06);
    delay_ms(50);
    arch::triple_fault()
}

/// The fatal-error path (panics, CPU exceptions, configuration errors): the message is already printed.
pub fn fatal_stop() -> ! {
    arch::interrupts_disable();
    if !stop_others() {
        arch::halt_forever();
    }
    let s = PANIC_REBOOT_S.load(Ordering::Relaxed);
    if s == u64::MAX {
        crate::watchdog::disarm();
        kprintln!("k10-metal: halted (k10.panic=0)");
        crate::console::udp::panic_flush();
        arch::halt_forever();
    }
    kprintln!("k10-metal: rebooting in {s} s (k10.panic=N to change, 0 to halt)");
    crate::console::udp::panic_flush();
    delay_ms(s.saturating_mul(1000));
    reset()
}
