//! The Intel TCO watchdog timer of the ICH/PCH chipsets. Its registers sit at TCOBASE in I/O space: reload
//! (TCO_RLD), status (TCO1_STS: first timeout; TCO2_STS: second timeout, i.e. a watchdog reset happened),
//! control (TCO1_CNT: TMR_HLT stops the timer) and the timer value (TCO_TMR, 0.6 s ticks). The timer counts down;
//! the first expiry sets TIMEOUT and reloads, the second resets the machine — unless the chipset's NO_REBOOT bit,
//! which firmware often sets, is on. Where TCOBASE and NO_REBOOT live depends on the generation:
//!
//! | generation | found through | TCOBASE | NO_REBOOT |
//! |---|---|---|---|
//! | 2: ICH6–ICH10, 5- to 9-series PCH, QEMU q35 | LPC bridge 00:1f.0 | ACPI base (0x40) + 0x60 | GCS bit 5, chipset config space (RCBA, 0xF0) + 0x3410 |
//! | 4: 100/200-series PCH (Sunrise Point, Kaby Lake, Lewisburg) | SMBus 00:1f.4 | TCOBASE (0x50) | SMBCTRL bit 1 through the P2SB sideband BAR (port 0xC6, 0x0C) |
//! | 6: 300-series PCH (Cannon Point) and later | SMBus 00:1f.4 | TCOBASE (0x50) | TCO1_CNT bit 0 |

use super::{Bdf, Platform, Watchdog};

pub const TCO_RLD: u16 = 0x00;
pub const TCO1_STS: u16 = 0x04;
pub const TCO2_STS: u16 = 0x06;
pub const TCO1_CNT: u16 = 0x08;
pub const TCO_TMR: u16 = 0x12;
pub const TCO1_STS_TIMEOUT: u16 = 1 << 3;
pub const TCO2_STS_SECOND_TO: u16 = 1 << 1;
pub const TCO2_STS_BOOT: u16 = 1 << 2;
pub const TCO1_CNT_TMR_HLT: u16 = 1 << 11;
pub const TCO1_CNT_NO_REBOOT_V6: u16 = 1 << 0;
pub const LPC_ACPI_BASE: u16 = 0x40;
pub const LPC_ACPI_CNTL: u16 = 0x44;
pub const LPC_ACPI_EN: u32 = 1 << 7;
pub const LPC_RCBA: u16 = 0xF0;
pub const GCS_OFFSET: u64 = 0x3410;
pub const GCS_NO_REBOOT: u32 = 1 << 5;
pub const SMBUS_TCOBASE: u16 = 0x50;
pub const SMBUS_TCOCTL: u16 = 0x54;
pub const TCOCTL_BASE_EN: u32 = 1 << 8;
pub const P2SB: Bdf = Bdf::new(0, 0x1F, 1);
pub const P2SB_CTRL: u16 = 0xE0;
pub const P2SB_HIDE: u32 = 1 << 8;
pub const SBREG_SMBCTRL: u64 = 0xC6_000C;
pub const SMBCTRL_NO_REBOOT: u32 = 1 << 1;
/// Timer ticks in milliseconds.
pub const TICK_MS: u32 = 600;

/// Which TCO generation a PCI function (an LPC bridge or an SMBus controller) carries, by Intel device id.
pub fn generation(device: u16, class: u8, subclass: u8) -> Option<u8> {
    let lpc = class == 0x06 && subclass == 0x01;
    let smbus = class == 0x0C && subclass == 0x05;
    let v2 = matches!(device,
        0x2640..=0x2642 | 0x27B8 | 0x27B9 | 0x27BC | 0x27BD | 0x2810..=0x2815 | 0x2912 | 0x2914 | 0x2916..=0x2919
        | 0x3A14 | 0x3A16 | 0x3A18 | 0x3A1A | 0x3B00..=0x3B1F | 0x1C40..=0x1C5F | 0x1D40 | 0x1D41 | 0x1E40..=0x1E5F
        | 0x8C40..=0x8C5F | 0x9C40..=0x9C47 | 0x8CC0..=0x8CDF | 0x9CC1..=0x9CCF | 0x8D40..=0x8D5F);
    let v4 = matches!(device, 0xA123 | 0x9D23 | 0xA2A3 | 0xA1A3 | 0xA223);
    let v6 = matches!(device,
        0xA323 | 0x9DA3 | 0x06A3 | 0x02A3 | 0xA3A3 | 0x34A3 | 0xA0A3 | 0x43A3 | 0x4DA3 | 0x4B23 | 0x51A3 | 0x7AA3
        | 0x54A3 | 0x7A23 | 0x7E22 | 0x7F23);
    match () {
        _ if lpc && v2 => Some(2),
        _ if smbus && v4 => Some(4),
        _ if smbus && v6 => Some(6),
        _ => None,
    }
}

enum NoReboot {
    /// A dword of memory-mapped chipset registers and its bit.
    Mem(u64, u32),
    /// TCO1_CNT bit 0.
    Cnt,
}

pub struct Tco {
    base: u16,
    version: u8,
    no_reboot: NoReboot,
    fired: bool,
}

impl Tco {
    /// The TCO timer of the LPC bridge (generation 2) or SMBus controller (4, 6) `f`.
    pub fn probe(p: &dyn Platform, f: Bdf, version: u8) -> Result<Tco, &'static str> {
        let (base, no_reboot) = match version {
            2 => {
                if p.cfg_read32(f, LPC_ACPI_CNTL) & LPC_ACPI_EN == 0 {
                    return Err("ACPI I/O decoding disabled in the LPC bridge");
                }
                let acpi = p.cfg_read32(f, LPC_ACPI_BASE) & 0xFF80;
                let rcba = p.cfg_read32(f, LPC_RCBA);
                if acpi == 0 || rcba & 1 == 0 {
                    return Err("no ACPI base or chipset configuration base");
                }
                let gcs = (rcba & 0xFFFF_C000) as u64 + GCS_OFFSET;
                if !p.map(gcs, 4) {
                    return Err("chipset configuration registers not mappable");
                }
                (acpi as u16 + 0x60, NoReboot::Mem(gcs, GCS_NO_REBOOT))
            }
            4 | 6 => {
                if p.cfg_read32(f, SMBUS_TCOCTL) & TCOCTL_BASE_EN == 0 {
                    return Err("TCO I/O range disabled in the SMBus controller");
                }
                let base = (p.cfg_read32(f, SMBUS_TCOBASE) & 0xFFE0) as u16;
                if base == 0 {
                    return Err("no TCO base");
                }
                if version == 6 {
                    (base, NoReboot::Cnt)
                } else {
                    let bar = p2sb_bar(p).ok_or("P2SB sideband BAR unavailable")?;
                    let smbctrl = bar + SBREG_SMBCTRL;
                    if !p.map(smbctrl, 4) {
                        return Err("P2SB sideband registers not mappable");
                    }
                    (base, NoReboot::Mem(smbctrl, SMBCTRL_NO_REBOOT))
                }
            }
            _ => return Err("unknown TCO generation"),
        };
        let mut t = Tco { base, version, no_reboot, fired: false };
        if p.inw(base + TCO1_CNT) == 0xFFFF {
            return Err("TCO registers read as all ones");
        }
        t.fired = p.inw(base + TCO2_STS) & TCO2_STS_SECOND_TO != 0;
        p.outw(base + TCO1_STS, TCO1_STS_TIMEOUT);
        p.outw(base + TCO2_STS, TCO2_STS_SECOND_TO);
        p.outw(base + TCO2_STS, TCO2_STS_BOOT);
        t.halt(p);
        Ok(t)
    }

    fn halt(&self, p: &dyn Platform) {
        let c = p.inw(self.base + TCO1_CNT);
        p.outw(self.base + TCO1_CNT, c | TCO1_CNT_TMR_HLT);
        self.set_no_reboot(p, true);
    }

    fn set_no_reboot(&self, p: &dyn Platform, on: bool) -> bool {
        match self.no_reboot {
            NoReboot::Mem(a, bit) => {
                let v = p.mmio_read32(a);
                let want = if on { v | bit } else { v & !bit };
                p.mmio_write32(a, want);
                p.mmio_read32(a) & bit == want & bit
            }
            NoReboot::Cnt => {
                let r = self.base + TCO1_CNT;
                let v = p.inw(r);
                let want = if on { v | TCO1_CNT_NO_REBOOT_V6 } else { v & !TCO1_CNT_NO_REBOOT_V6 };
                p.outw(r, want);
                p.inw(r) & TCO1_CNT_NO_REBOOT_V6 == want & TCO1_CNT_NO_REBOOT_V6
            }
        }
    }
}

/// The P2SB's sideband BAR, unhiding the device for the read if the firmware hid it (and hiding it again).
fn p2sb_bar(p: &dyn Platform) -> Option<u64> {
    let ctrl = p.cfg_read32(P2SB, P2SB_CTRL);
    if ctrl == !0 {
        return None;
    }
    if ctrl & P2SB_HIDE != 0 {
        p.cfg_write32(P2SB, P2SB_CTRL, ctrl & !P2SB_HIDE);
    }
    let lo = p.cfg_read32(P2SB, 0x10);
    let hi = if lo & 0b110 == 0b100 { p.cfg_read32(P2SB, 0x14) } else { 0 };
    if ctrl & P2SB_HIDE != 0 {
        p.cfg_write32(P2SB, P2SB_CTRL, ctrl);
    }
    let bar = (hi as u64) << 32 | (lo & !0xF) as u64;
    (lo != !0 && bar != 0).then_some(bar)
}

impl Watchdog for Tco {
    fn name(&self) -> &'static str {
        match self.version {
            2 => "Intel TCO (ICH/PCH, LPC)",
            4 => "Intel TCO (100/200-series PCH)",
            _ => "Intel TCO (300-series PCH or later)",
        }
    }
    fn arm(&mut self, p: &dyn Platform, seconds: u32) -> Result<u32, &'static str> {
        // Two countdowns before the reset: each gets half the timeout.
        let ticks = (seconds.saturating_mul(1000) / TICK_MS / 2).clamp(4, 0x3FF);
        let r = self.base + TCO_TMR;
        p.outw(r, (p.inw(r) & 0xFC00) | ticks as u16);
        if !self.set_no_reboot(p, false) {
            return Err("NO_REBOOT is locked on by the firmware");
        }
        p.outw(self.base + TCO_RLD, 1);
        p.outw(self.base + TCO1_STS, TCO1_STS_TIMEOUT);
        let c = self.base + TCO1_CNT;
        p.outw(c, p.inw(c) & !TCO1_CNT_TMR_HLT);
        if p.inw(c) & TCO1_CNT_TMR_HLT != 0 {
            self.set_no_reboot(p, true);
            return Err("the timer stays halted (locked by the firmware)");
        }
        Ok(ticks * 2 * TICK_MS / 1000)
    }
    fn pet(&mut self, p: &dyn Platform) {
        p.outw(self.base + TCO_RLD, 1);
        p.outw(self.base + TCO1_STS, TCO1_STS_TIMEOUT);
    }
    fn disarm(&mut self, p: &dyn Platform) {
        self.halt(p);
    }
    fn fired_before(&self) -> bool {
        self.fired
    }
}
