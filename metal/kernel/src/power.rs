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
//! * The power button, as Tier 1's: the main loop leaves the cluster, then powers off. The fixed-feature button (PM1
//!   PWRBTN, unless the FADT says the button is a control method) and the GPEs whose AML method notifies a power
//!   button device (`k10_drivers::power_button`) are the only SCI sources enabled; the SCI handler takes their events
//!   without running AML (a level-triggered GPE is disabled once taken, as its method would have cleared its source).
//!   Hardware-reduced platforms (Generic Event Device) and buttons behind an embedded controller are not covered.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::acpi::{self, Gas};
use crate::sync::SetOnce;
use k10_drivers::power_button::Gpe;
use crate::arch::{self, inb, outb};
use crate::kprintln;

/// Set once a CPU has begun stopping the machine: the others halt on the stop NMI.
pub static STOPPING: AtomicBool = AtomicBool::new(false);
/// `k10.panic`: seconds before a fatal error reboots (u64::MAX = never).
static PANIC_REBOOT_S: AtomicU64 = AtomicU64::new(10);

const PM1_SCI_EN: u64 = 1;
const PM1_PWRBTN: u64 = 1 << 8;
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

/// The power button's events the SCI handler takes: the fixed-feature button, the GPEs of control-method ones.
struct Button {
    fixed: bool,
    gpes: alloc::vec::Vec<Gpe>,
}

static BUTTON: SetOnce<Button> = SetOnce::new();
/// Set by the SCI handler on a press: 1 + the GPE number, or u32::MAX for the fixed-feature button; 0 = none.
static PRESSED: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// The PM1 status and enable registers of an event block (16 bits each, the enable half after the status half).
fn pm1(evt: Gas, len: u8) -> (Gas, Gas) {
    let sts = Gas { bits: 16, access: 2, ..evt };
    (sts, Gas { addr: evt.addr + (len / 2).max(2) as u64, ..sts })
}

/// GPE `n`'s status and enable bytes and its bit, in the GPE0 block or else the GPE1 block (numbered from
/// GPE1_BASE).
fn gpe_reg(f: &acpi::Fadt, n: u16) -> Option<(Gas, Gas, u8)> {
    let (blk, len, i) = if (n as usize) < f.gpe0_len as usize / 2 * 8 {
        (f.gpe0, f.gpe0_len, n as usize)
    } else {
        let i = (n as usize).checked_sub(f.gpe1_base as usize)?;
        (f.gpe1, f.gpe1_len, i)
    };
    let half = len as usize / 2;
    if !blk.valid() || blk.space == acpi::SPACE_PCI || i >= half * 8 {
        return None;
    }
    let byte = |at: usize| Gas { addr: blk.addr + at as u64, bits: 8, access: 1, ..blk };
    Some((byte(i / 8), byte(half + i / 8), 1 << (i % 8)))
}

/// Enables the power button's events and routes the SCI to this CPU (CPU 0, once the I/O APICs are up).
pub fn init_button() {
    let Some(t) = acpi::get() else { return };
    let Some(f) = t.fadt.filter(|f| f.pm1a_evt.valid() && !t.hw_reduced()) else {
        if crate::verbose() {
            kprintln!("power button: none (no ACPI PM1 event block{})", if t.hw_reduced() { ": hardware-reduced ACPI" } else { "" });
        }
        return;
    };
    let fixed = f.flags & acpi::FADT_PWR_BUTTON == 0;
    let gpes: alloc::vec::Vec<Gpe> = t.power_gpes.iter().filter(|g| gpe_reg(&f, g.number).is_some()).copied().collect();
    if !fixed && gpes.is_empty() {
        kprintln!("power button: a control method one whose GPE the ACPI tables do not show: not handled");
        return;
    }
    acpi_mode(&f);
    // Nothing else raises the SCI: every PM1 event and GPE off, their status cleared, then the button's on.
    for (evt, len) in [(f.pm1a_evt, f.pm1_evt_len), (f.pm1b_evt, f.pm1_evt_len)] {
        if evt.valid() {
            pm1(evt, len).1.write(0);
            clear_pm1_status(evt);
        }
    }
    for (blk, len) in [(f.gpe0, f.gpe0_len), (f.gpe1, f.gpe1_len)] {
        disable_gpes(blk, len);
        if blk.valid() && blk.space != acpi::SPACE_PCI {
            for i in 0..len as u64 / 2 {
                Gas { addr: blk.addr + i, bits: 8, access: 1, ..blk }.write(0xFF);
            }
        }
    }
    // The SCI is shared, level-triggered and active low unless an interrupt override says otherwise.
    let (gsi, mut flags) = t.isa_irq(f.sci_int as u8);
    if flags & 3 == 0 {
        flags |= 3;
    }
    if (flags >> 2) & 3 == 0 {
        flags |= 3 << 2;
    }
    if !crate::arch::ioapic::route(gsi, crate::arch::idt::VEC_SCI, crate::arch::apic::id(), flags) {
        kprintln!("power button: cannot route the SCI (IRQ {}, GSI {gsi}): not handled", f.sci_int);
        return;
    }
    BUTTON.set(Button { fixed, gpes });
    let b = BUTTON.get().expect("just set");
    if fixed {
        for evt in [f.pm1a_evt, f.pm1b_evt].into_iter().filter(Gas::valid) {
            pm1(evt, f.pm1_evt_len).1.write(PM1_PWRBTN);
        }
    }
    for g in &b.gpes {
        if let Some((_, en, bit)) = gpe_reg(&f, g.number) {
            en.write(en.read() | bit as u64);
        }
    }
    if crate::verbose() {
        let mut what = alloc::string::String::from(if fixed { "fixed-feature" } else { "" });
        for g in &b.gpes {
            what.push_str(&alloc::format!("{}GPE {:#04x} ({})", if what.is_empty() { "" } else { ", " }, g.number, if g.level { "level" } else { "edge" }));
        }
        kprintln!("power button: {what}, SCI on IRQ {} (GSI {gsi})", f.sci_int);
    }
}

/// The SCI (interrupt context, CPU 0): takes the button's events and asks the main loop to power off.
pub fn on_sci() {
    let (Some(b), Some(f)) = (BUTTON.get(), acpi::get().and_then(|t| t.fadt)) else { return };
    let mut pressed = 0;
    if b.fixed {
        for evt in [f.pm1a_evt, f.pm1b_evt].into_iter().filter(Gas::valid) {
            let (sts, en) = pm1(evt, f.pm1_evt_len);
            if sts.read() & en.read() & PM1_PWRBTN != 0 {
                sts.write(PM1_PWRBTN);
                pressed = u32::MAX;
            }
        }
    }
    for g in &b.gpes {
        let Some((sts, en, bit)) = gpe_reg(&f, g.number) else { continue };
        let bit = bit as u64;
        if sts.read() & en.read() & bit != 0 {
            if g.level {
                en.write(en.read() & !bit);
            }
            sts.write(bit);
            pressed = 1 + g.number as u32;
        }
    }
    if pressed != 0 {
        PRESSED.store(pressed, Ordering::Relaxed);
        request(false);
    }
}

/// A press since the last call, described for the console.
pub fn take_button_press() -> Option<alloc::string::String> {
    match PRESSED.swap(0, Ordering::Relaxed) {
        0 => None,
        u32::MAX => Some("fixed-feature".into()),
        n => Some(alloc::format!("GPE {:#04x}", n - 1)),
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
