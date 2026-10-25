//! Watchdog drivers against a model chipset: the TCO timer in its three generations (LPC/RCBA as on q35, SMBus
//! with the P2SB sideband as on 100-series PCHs, SMBus with NO_REBOOT in TCO1_CNT from the 300-series on), the
//! i6300ESB's unlock sequence, and ACPI WDAT tables run against the TCO registers.

use std::cell::RefCell;
use std::collections::HashMap;

use k10_drivers::watchdog::itco::*;
use k10_drivers::watchdog::{esb, wdat, Bdf, Platform, Watchdog};

#[derive(Default)]
struct Chipset {
    io: RefCell<HashMap<u16, u16>>,
    cfg: RefCell<HashMap<(Bdf, u16), u32>>,
    mem: RefCell<HashMap<u64, u32>>,
    /// Ports whose bits are write-one-to-clear (status registers).
    w1c: Vec<u16>,
    /// Memory words that ignore writes (locked by firmware).
    locked: Vec<u64>,
    /// The ESB reload register's unlock state: 0, 1 (0x80 seen), 2 (0x86 seen).
    esb_unlock: RefCell<u8>,
    esb_violations: RefCell<u32>,
    esb_base: u64,
    log: RefCell<Vec<String>>,
}

impl Platform for Chipset {
    fn inb(&self, port: u16) -> u8 {
        self.inw(port) as u8
    }
    fn inw(&self, port: u16) -> u16 {
        *self.io.borrow().get(&port).unwrap_or(&0)
    }
    fn inl(&self, port: u16) -> u32 {
        self.inw(port) as u32 | (self.inw(port + 2) as u32) << 16
    }
    fn outb(&self, port: u16, v: u8) {
        self.outw(port, v as u16)
    }
    fn outw(&self, port: u16, v: u16) {
        self.log.borrow_mut().push(format!("out {port:#x} {v:#x}"));
        let mut io = self.io.borrow_mut();
        let cur = *io.get(&port).unwrap_or(&0);
        io.insert(port, if self.w1c.contains(&port) { cur & !v } else { v });
    }
    fn outl(&self, port: u16, v: u32) {
        self.outw(port, v as u16);
        self.outw(port + 2, (v >> 16) as u16);
    }
    fn cfg_read32(&self, f: Bdf, off: u16) -> u32 {
        *self.cfg.borrow().get(&(f, off)).unwrap_or(&!0)
    }
    fn cfg_write32(&self, f: Bdf, off: u16, v: u32) {
        self.cfg.borrow_mut().insert((f, off), v);
    }
    fn cfg_write16(&self, f: Bdf, off: u16, v: u16) {
        let a = off & !3;
        let shift = (off & 2) * 8;
        let cur = self.cfg_read32(f, a);
        self.cfg_write32(f, a, (cur & !(0xFFFF << shift)) | (v as u32) << shift);
    }
    fn cfg_write8(&self, f: Bdf, off: u16, v: u8) {
        let a = off & !3;
        let shift = (off & 3) * 8;
        let cur = self.cfg_read32(f, a);
        self.cfg_write32(f, a, (cur & !(0xFF << shift)) | (v as u32) << shift);
    }
    fn map(&self, _paddr: u64, _len: u64) -> bool {
        true
    }
    fn mmio_read32(&self, a: u64) -> u32 {
        *self.mem.borrow().get(&a).unwrap_or(&0)
    }
    fn mmio_write32(&self, a: u64, v: u32) {
        self.esb_check(a);
        if !self.locked.contains(&a) {
            self.mem.borrow_mut().insert(a, v);
        }
    }
    fn mmio_read16(&self, a: u64) -> u16 {
        self.mmio_read32(a) as u16
    }
    fn mmio_write16(&self, a: u64, v: u16) {
        if self.esb_base != 0 && a == self.esb_base + esb::RELOAD {
            let mut s = self.esb_unlock.borrow_mut();
            // As QEMU's model: 0x80 always starts the sequence (Linux unlocks, reads, unlocks again, writes).
            match (*s, v) {
                (_, esb::UNLOCK1) => *s = 1,
                (1, esb::UNLOCK2) => *s = 2,
                (2, _) => {
                    *s = 0;
                    drop(s);
                    // Write-one-to-clear timeout status, the reload bit pulses.
                    let cur = self.mmio_read32(a) as u16;
                    self.mem.borrow_mut().insert(a, (cur & !(v & esb::RELOAD_TIMEOUT)) as u32);
                    self.log.borrow_mut().push(format!("esb reload {v:#x}"));
                }
                _ => {
                    *s = 0;
                    *self.esb_violations.borrow_mut() += 1;
                }
            }
            return;
        }
        self.mmio_write32(a, v as u32)
    }
    fn mmio_read8(&self, a: u64) -> u8 {
        self.mmio_read32(a) as u8
    }
    fn mmio_write8(&self, a: u64, v: u8) {
        self.mmio_write32(a, v as u32)
    }
    fn delay_us(&self, _us: u32) {}
}

impl Chipset {
    fn esb_check(&self, a: u64) {
        if self.esb_base != 0 && (a == self.esb_base + esb::TIMER1 || a == self.esb_base + esb::TIMER2) {
            let mut s = self.esb_unlock.borrow_mut();
            if *s != 2 {
                *self.esb_violations.borrow_mut() += 1;
            }
            *s = 0;
        }
    }
}

const LPC: Bdf = Bdf::new(0, 0x1F, 0);
const SMBUS: Bdf = Bdf::new(0, 0x1F, 4);
const TCO: u16 = 0x660;
const GCS: u64 = 0xFED1_C000 + 0x3410;

/// QEMU q35's ICH9 as SeaBIOS/OVMF leave it: ACPI base 0x600, RCBA 0xFED1C000, NO_REBOOT set, timer halted, a
/// second-timeout status from the previous boot.
fn ich9() -> Chipset {
    let c = Chipset { w1c: vec![TCO + TCO1_STS, TCO + TCO2_STS], ..Default::default() };
    c.cfg_write32(LPC, LPC_ACPI_BASE, 0x601);
    c.cfg_write32(LPC, LPC_ACPI_CNTL, 0x80);
    c.cfg_write32(LPC, LPC_RCBA, 0xFED1_C001);
    c.mem.borrow_mut().insert(GCS, GCS_NO_REBOOT | 1);
    c.io.borrow_mut().insert(TCO + TCO1_CNT, TCO1_CNT_TMR_HLT);
    c.io.borrow_mut().insert(TCO + TCO2_STS, TCO2_STS_SECOND_TO | TCO2_STS_BOOT);
    c.io.borrow_mut().insert(TCO + TCO_TMR, 0x0004);
    c
}

#[test]
fn generations_by_device_id() {
    assert_eq!(generation(0x2918, 0x06, 0x01), Some(2), "QEMU q35 ICH9 LPC");
    assert_eq!(generation(0x8C4E, 0x06, 0x01), Some(2), "Lynx Point");
    assert_eq!(generation(0xA123, 0x0C, 0x05), Some(4), "Sunrise Point SMBus");
    assert_eq!(generation(0x7A23, 0x0C, 0x05), Some(6), "Raptor Lake SMBus");
    assert_eq!(generation(0x2918, 0x0C, 0x05), None, "class must match");
    assert_eq!(generation(0x1234, 0x06, 0x01), None);
}

#[test]
fn tco_v2_through_the_lpc_bridge() {
    let c = ich9();
    let mut t = Tco::probe(&c, LPC, 2).unwrap();
    assert!(t.fired_before(), "SECOND_TO_STS from the previous boot");
    assert_eq!(c.inw(TCO + TCO2_STS), 0, "status cleared");
    assert_ne!(c.inw(TCO + TCO1_CNT) & TCO1_CNT_TMR_HLT, 0, "stays halted until armed");
    assert_eq!(t.arm(&c, 60).unwrap(), 60);
    assert_eq!(c.inw(TCO + TCO_TMR) & 0x3FF, 50, "two countdowns of 50 × 0.6 s");
    assert_eq!(c.mmio_read32(GCS), 1, "NO_REBOOT cleared, the other GCS bits kept");
    assert_eq!(c.inw(TCO + TCO1_CNT) & TCO1_CNT_TMR_HLT, 0, "running");
    c.log.borrow_mut().clear();
    t.pet(&c);
    assert_eq!(c.log.borrow()[0], format!("out {:#x} 0x1", TCO + TCO_RLD));
    t.disarm(&c);
    assert_ne!(c.inw(TCO + TCO1_CNT) & TCO1_CNT_TMR_HLT, 0);
    assert_ne!(c.mmio_read32(GCS) & GCS_NO_REBOOT, 0);
}

#[test]
fn tco_refuses_a_locked_no_reboot() {
    let mut c = ich9();
    c.locked.push(GCS);
    let mut t = Tco::probe(&c, LPC, 2).unwrap();
    assert_eq!(t.arm(&c, 60), Err("NO_REBOOT is locked on by the firmware"));
    assert_ne!(c.inw(TCO + TCO1_CNT) & TCO1_CNT_TMR_HLT, 0, "left halted");
}

#[test]
fn tco_v2_needs_acpi_decoding() {
    let c = ich9();
    c.cfg_write32(LPC, LPC_ACPI_CNTL, 0);
    assert!(Tco::probe(&c, LPC, 2).is_err());
}

#[test]
fn tco_v6_no_reboot_in_tco1_cnt() {
    let base = 0x400;
    let c = Chipset { w1c: vec![base + TCO1_STS, base + TCO2_STS], ..Default::default() };
    c.cfg_write32(SMBUS, SMBUS_TCOBASE, 0x401);
    c.cfg_write32(SMBUS, SMBUS_TCOCTL, TCOCTL_BASE_EN);
    c.io.borrow_mut().insert(base + TCO1_CNT, TCO1_CNT_TMR_HLT | TCO1_CNT_NO_REBOOT_V6);
    let mut t = Tco::probe(&c, SMBUS, 6).unwrap();
    assert!(!t.fired_before());
    t.arm(&c, 30).unwrap();
    assert_eq!(c.inw(base + TCO1_CNT) & (TCO1_CNT_TMR_HLT | TCO1_CNT_NO_REBOOT_V6), 0);
    t.disarm(&c);
    assert_eq!(c.inw(base + TCO1_CNT) & (TCO1_CNT_TMR_HLT | TCO1_CNT_NO_REBOOT_V6), TCO1_CNT_TMR_HLT | TCO1_CNT_NO_REBOOT_V6);
}

#[test]
fn tco_v4_through_a_hidden_p2sb() {
    let base = 0x400;
    let sbreg = 0xFD00_0000u64;
    let c = Chipset { w1c: vec![base + TCO1_STS, base + TCO2_STS], ..Default::default() };
    c.cfg_write32(SMBUS, SMBUS_TCOBASE, 0x401);
    c.cfg_write32(SMBUS, SMBUS_TCOCTL, TCOCTL_BASE_EN);
    c.cfg_write32(P2SB, P2SB_CTRL, P2SB_HIDE);
    c.cfg_write32(P2SB, 0x10, sbreg as u32 | 0b100);
    c.cfg_write32(P2SB, 0x14, 0);
    c.mem.borrow_mut().insert(sbreg + SBREG_SMBCTRL, SMBCTRL_NO_REBOOT);
    c.io.borrow_mut().insert(base + TCO1_CNT, TCO1_CNT_TMR_HLT);
    let mut t = Tco::probe(&c, SMBUS, 4).unwrap();
    assert_eq!(c.cfg_read32(P2SB, P2SB_CTRL), P2SB_HIDE, "hidden again");
    t.arm(&c, 30).unwrap();
    assert_eq!(c.mmio_read32(sbreg + SBREG_SMBCTRL), 0);
    assert_eq!(c.inw(base + TCO1_CNT) & TCO1_CNT_TMR_HLT, 0);
}

#[test]
fn esb_unlocks_before_every_write() {
    let f = Bdf::new(0, 5, 0);
    let bar = 0xFEBF_0000;
    let c = Chipset { esb_base: bar, ..Default::default() };
    c.cfg_write32(f, esb::CONFIG & !3, 0);
    c.cfg_write32(f, esb::LOCK & !3, 0);
    c.mem.borrow_mut().insert(bar + esb::RELOAD, esb::RELOAD_TIMEOUT as u32);
    let mut e = esb::Esb::new(&c, f, bar).unwrap();
    assert!(e.fired_before());
    assert_eq!(c.mmio_read16(bar + esb::RELOAD) & esb::RELOAD_TIMEOUT, 0, "timeout status cleared");
    assert_eq!(c.cfg_read16(f, esb::CONFIG), esb::CONFIG_WATCHDOG);
    assert_eq!(e.arm(&c, 30).unwrap(), 30);
    assert_eq!(c.mmio_read32(bar + esb::TIMER1), 30 << 9);
    assert_eq!(c.mmio_read32(bar + esb::TIMER2), 30 << 9);
    assert_eq!(c.cfg_read8(f, esb::LOCK), esb::LOCK_ENABLE);
    e.pet(&c);
    e.disarm(&c);
    assert_eq!(c.cfg_read8(f, esb::LOCK), 0);
    assert_eq!(*c.esb_violations.borrow(), 0);
}

/// A WDAT describing q35's TCO timer, as test.sh passes it to QEMU (metal/tools/wdat.py writes the same table).
fn q35_wdat(flags: u8) -> Vec<u8> {
    let io = |addr: u16, bits: u8, off: u8| (1u8, bits, off, if bits == 16 { 2u8 } else { 3u8 }, addr as u64);
    let mem = |addr: u64, off: u8| (0u8, 32u8, off, 3u8, addr);
    #[rustfmt::skip]
    let entries: Vec<(u8, u8, (u8, u8, u8, u8, u64), u32, u32)> = vec![
        (wdat::RESET, wdat::WRITE_VALUE, io(TCO + TCO_RLD, 16, 0), 1, 0xFFFF),
        (wdat::QUERY_COUNTDOWN, wdat::READ_COUNTDOWN, io(TCO + TCO_TMR, 16, 0), 0, 0x3FF),
        (wdat::SET_COUNTDOWN, wdat::WRITE_COUNTDOWN | wdat::PRESERVE, io(TCO + TCO_TMR, 16, 0), 0, 0x3FF),
        (wdat::QUERY_RUNNING, wdat::READ_VALUE, io(TCO + TCO1_CNT, 16, 11), 0, 1),
        (wdat::SET_RUNNING, wdat::WRITE_VALUE | wdat::PRESERVE, io(TCO + TCO1_CNT, 16, 11), 0, 1),
        (wdat::QUERY_STOPPED, wdat::READ_VALUE, io(TCO + TCO1_CNT, 16, 11), 1, 1),
        (wdat::SET_STOPPED, wdat::WRITE_VALUE | wdat::PRESERVE, io(TCO + TCO1_CNT, 16, 11), 1, 1),
        (wdat::QUERY_REBOOT, wdat::READ_VALUE, mem(GCS, 5), 0, 1),
        (wdat::SET_REBOOT, wdat::WRITE_VALUE | wdat::PRESERVE, mem(GCS, 5), 0, 1),
        (wdat::QUERY_STATUS, wdat::READ_VALUE, io(TCO + TCO2_STS, 16, 1), 1, 1),
        (wdat::SET_STATUS, wdat::WRITE_VALUE, io(TCO + TCO2_STS, 16, 1), 1, 1),
    ];
    let mut t = Vec::new();
    t.extend_from_slice(b"WDAT");
    t.extend_from_slice(&[0; 32]);
    t.extend_from_slice(&32u32.to_le_bytes());
    t.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0, 0, 0]);
    t.extend_from_slice(&1200u32.to_le_bytes());
    t.extend_from_slice(&0x3FFu32.to_le_bytes());
    t.extend_from_slice(&2u32.to_le_bytes());
    t.extend_from_slice(&[flags, 0, 0, 0]);
    t.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for (action, instr, (space, bits, off, access, addr), value, mask) in entries {
        t.extend_from_slice(&[action, instr, 0, 0, space, bits, off, access]);
        t.extend_from_slice(&addr.to_le_bytes());
        t.extend_from_slice(&value.to_le_bytes());
        t.extend_from_slice(&mask.to_le_bytes());
    }
    let len = t.len() as u32;
    t[4..8].copy_from_slice(&len.to_le_bytes());
    t
}

#[test]
fn wdat_drives_the_tco_timer() {
    let c = ich9();
    let w = wdat::Wdat::parse(&q35_wdat(wdat::FLAG_ENABLED)).unwrap();
    assert_eq!((w.period_ms, w.min_count, w.max_count, w.entries.len()), (1200, 2, 0x3FF, 11));
    assert_eq!(w.memory().collect::<Vec<_>>(), [(GCS, 4), (GCS, 4)]);
    let mut w = w.init(&c).unwrap();
    assert!(w.fired_before());
    assert_eq!(c.inw(TCO + TCO2_STS) & TCO2_STS_SECOND_TO, 0, "status cleared");
    assert_eq!(w.arm(&c, 60).unwrap(), 60);
    assert_eq!(c.inw(TCO + TCO_TMR), 50, "60 s in 1.2 s units");
    assert_eq!(c.inw(TCO + TCO1_CNT) & TCO1_CNT_TMR_HLT, 0);
    assert_eq!(c.mmio_read32(GCS), 1, "SET_REBOOT cleared NO_REBOOT only");
    c.log.borrow_mut().clear();
    w.pet(&c);
    assert_eq!(c.log.borrow()[0], format!("out {:#x} 0x1", TCO + TCO_RLD));
    w.disarm(&c);
    assert_ne!(c.inw(TCO + TCO1_CNT) & TCO1_CNT_TMR_HLT, 0);
}

#[test]
fn wdat_rejections() {
    let c = ich9();
    assert!(wdat::Wdat::parse(&q35_wdat(0)).unwrap().init(&c).is_err(), "disabled by the firmware");
    let mut t = q35_wdat(wdat::FLAG_ENABLED);
    t[68 + 4] = 2; // PCI configuration space: not supported
    assert!(wdat::Wdat::parse(&t).is_err());
    assert!(wdat::Wdat::parse(&t[..60]).is_err());
}
