//! r8169-class against a register model: an older RTL8168 (PHY through PHYAR), an RTL8168H (PHY through the OCP
//! window) and an RTL8125 (32-bit interrupt registers, MAC0_BKP, transmit poll at 0x90, the new descriptor format
//! turned off); OWN-bit rings with the end-of-ring bit, the CRC counted in received lengths, short frames padded.

mod common;

use std::collections::HashMap;

use common::*;
use k10_drivers::rtl8169::*;
use k10_drivers::{IrqMode, Nic};

const MAC: [u8; 6] = [0x00, 0xE0, 0x4C, 0x68, 0x00, 0x01];

#[derive(Default)]
struct RtlModel {
    regs: RegFile,
    r8125: bool,
    phy: Phy,
    mac_ocp: HashMap<u32, u16>,
    locked: bool,
    writes_while_locked: u32,
    phyar_used: bool,
    ocp_used: bool,
    rx_i: u64,
    tx_i: u64,
    tx_frames: Vec<Vec<u8>>,
}

impl RtlModel {
    fn new(xid: u32, r8125: bool, mac_in_bkp: bool) -> RtlModel {
        let mut m = RtlModel { r8125, locked: true, ..Default::default() };
        m.regs.set(TX_CONFIG, xid << 20);
        let (lo, hi) = (u32::from_le_bytes([MAC[0], MAC[1], MAC[2], MAC[3]]), u16::from_le_bytes([MAC[4], MAC[5]]) as u32);
        if mac_in_bkp {
            m.regs.set(MAC0_BKP, lo);
            m.regs.set(MAC0_BKP + 4, hi);
        } else {
            m.regs.set(IDR0, lo);
            m.regs.set(IDR4, hi);
        }
        m.phy.set(0, 0, 1 << 11 | 1 << 12);
        m.phy.set(0, 2, 0x001C);
        m.phy.set(0, 3, 0xC800);
        m.mac_ocp.insert(MAC_OCP_TX_DESC_FORMAT, 0x0001);
        m
    }
    fn update_link(&mut self) {
        let up = self.phy.get(0, 0) & (1 << 11) == 0 && self.phy.get(0, 0) & (1 << 9) != 0;
        let s = if up { PHY_LINK | PHY_FULL_DUP | if self.r8125 { PHY_2500_8125 } else { PHY_1000 } } else { 0 };
        self.regs.write(PHY_STATUS, 2, s as u32);
    }
    fn ring(&self, lo: u32) -> u64 {
        self.regs.get(lo) as u64 | (self.regs.get(lo + 4) as u64) << 32
    }
    fn transmit(&mut self) {
        let base = self.ring(TX_DESC_LO);
        loop {
            let d = base + self.tx_i * 16;
            let o = unsafe { rd32(d) };
            if o & (1 << 31) == 0 {
                break;
            }
            assert_eq!(o & (3 << 28), 3 << 28, "first and last segment");
            let len = (o & 0x3FFF) as usize;
            let addr = unsafe { rd64(d + 8) };
            self.tx_frames.push(unsafe { std::slice::from_raw_parts(addr as *const u8, len) }.to_vec());
            unsafe { wr32(d, o & !(1 << 31)) };
            self.tx_i = if o & (1 << 30) != 0 { 0 } else { self.tx_i + 1 };
        }
    }
    fn deliver(&mut self, f: &[u8]) -> bool {
        if self.regs.read(CHIP_CMD, 1) & CMD_RX_ENABLE as u32 == 0 {
            return false;
        }
        let d = self.ring(RX_DESC_LO) + self.rx_i * 16;
        let o = unsafe { rd32(d) };
        if o & (1 << 31) == 0 {
            return false;
        }
        assert!(f.len() + 4 <= (o & 0x3FFF) as usize);
        unsafe {
            std::ptr::copy_nonoverlapping(f.as_ptr(), rd64(d + 8) as *mut u8, f.len());
            wr32(d, (o & (1 << 30)) | 3 << 28 | (f.len() + 4) as u32);
        }
        self.rx_i = if o & (1 << 30) != 0 { 0 } else { self.rx_i + 1 };
        let st = if self.r8125 { INTR_STATUS_8125 } else { INTR_STATUS_8168 };
        self.regs.set(st & !3, self.regs.get(st & !3) | (INT_RX_OK << ((st & 3) * 8)));
        true
    }
}

impl Model for RtlModel {
    fn read(&mut self, off: u32, bytes: u8) -> u32 {
        match off {
            CHIP_CMD => self.regs.read(CHIP_CMD, 1) & !(CMD_RESET as u32),
            _ => self.regs.read(off, bytes),
        }
    }
    fn write(&mut self, off: u32, bytes: u8, v: u32) {
        match off {
            CHIP_CMD if v & CMD_RESET as u32 != 0 => {
                self.rx_i = 0;
                self.tx_i = 0;
                self.regs.write(CHIP_CMD, 1, 0);
            }
            CFG9346 => {
                self.locked = v as u8 != CFG9346_UNLOCK;
                self.regs.write(off, bytes, v);
            }
            IDR0 | IDR4 | TX_CONFIG | RX_CONFIG | RX_DESC_LO | RX_DESC_HI | TX_DESC_LO | TX_DESC_HI if self.locked && (off == IDR0 || off == IDR4) => {
                self.writes_while_locked += 1;
            }
            PHYAR => {
                self.phyar_used = true;
                let reg = ((v >> 16) & 0x1F) as u8;
                if v & OCP_FLAG != 0 {
                    self.phy.set(0, reg, v as u16);
                    self.regs.set(PHYAR, v & !OCP_FLAG);
                } else {
                    self.regs.set(PHYAR, OCP_FLAG | (v & 0x1F_0000) | self.phy.get(0, reg) as u32);
                }
                self.update_link();
            }
            GPHY_OCP => {
                self.ocp_used = true;
                let addr = (v >> 15) & 0xFFFF;
                assert!(addr >= OCP_STD_PHY_BASE && addr < OCP_STD_PHY_BASE + 32, "standard PHY window");
                let reg = ((addr - OCP_STD_PHY_BASE) / 2) as u8;
                if v & OCP_FLAG != 0 {
                    self.phy.set(0, reg, v as u16);
                    self.regs.set(GPHY_OCP, v & !OCP_FLAG);
                } else {
                    self.regs.set(GPHY_OCP, OCP_FLAG | self.phy.get(0, reg) as u32);
                }
                self.update_link();
            }
            MAC_OCP => {
                let addr = (v >> 15) & 0xFFFF;
                if v & OCP_FLAG != 0 {
                    self.mac_ocp.insert(addr, v as u16);
                } else {
                    self.regs.set(MAC_OCP, *self.mac_ocp.get(&addr).unwrap_or(&0) as u32);
                }
            }
            TX_POLL_8168 if !self.r8125 && bytes == 1 => {
                assert_eq!(v as u8, TX_POLL_NPQ);
                self.transmit();
            }
            TX_POLL_8125 if self.r8125 => {
                assert_eq!((bytes, v), (2, 1));
                self.transmit();
            }
            // Interrupt status: write one to clear.
            o if o == if self.r8125 { INTR_STATUS_8125 } else { INTR_STATUS_8168 } => {
                let cur = self.regs.read(o, bytes);
                self.regs.write(o, bytes, cur & !v);
            }
            _ => self.regs.write(off, bytes, v),
        }
    }
}

fn run(xid: u32, r8125: bool, device: u16, mac_in_bkp: bool) -> (MockHw<RtlModel>, Rtl8169<MockHw<RtlModel>>) {
    let hw = MockHw::new(RtlModel::new(xid, r8125, mac_in_bkp));
    let d = Rtl8169::new(hw.clone(), device, 16, IrqMode::MsiX).unwrap();
    (hw, d)
}

fn traffic(hw: &MockHw<RtlModel>, d: &mut Rtl8169<MockHw<RtlModel>>) {
    // Twice around the ring, with out-of-order returns in between.
    for round in 0..2u8 {
        let frames: Vec<Vec<u8>> = (0..16).map(|i| frame(64 + i * 50, round * 16 + i as u8)).collect();
        for f in &frames {
            assert!(hw.dev.borrow_mut().deliver(f));
        }
        assert!(!hw.dev.borrow_mut().deliver(&frame(64, 0)), "ring full");
        let (rx, _) = d.rings();
        let got: Vec<(u16, usize)> = std::iter::from_fn(|| rx.pop()).collect();
        assert_eq!(got.len(), 16);
        for (&(id, len), f) in got.iter().zip(&frames) {
            assert_eq!(rx.frame(id, len), &f[..], "the CRC is not part of the frame");
        }
        for &(id, _) in got.iter().rev() {
            rx.recycle(id);
        }
    }
    let (_, tx) = d.rings();
    assert!(tx.ready());
    tx.send(42, &mut |b| b.copy_from_slice(&frame(42, 7)));
    d.flush();
    let m = hw.dev.borrow();
    let sent = m.tx_frames.last().unwrap();
    assert_eq!(sent.len(), 60, "short frame padded");
    assert_eq!(&sent[..42], &frame(42, 7)[..]);
    assert!(sent[42..].iter().all(|&b| b == 0));
}

#[test]
fn rtl8168_older_revision_uses_phyar() {
    let (hw, mut d) = run(0x380, false, 0x8168, false);
    assert_eq!(d.name(), "r8169 (RTL8111/8168)");
    assert_eq!(d.mac(), MAC);
    assert!(!d.chip().ocp_phy());
    {
        let m = hw.dev.borrow();
        assert!(m.phyar_used && !m.ocp_used);
        assert!(m.locked, "configuration registers locked again");
        assert_eq!(m.writes_while_locked, 0);
        assert_eq!(m.regs.read(INTR_MASK_8168, 2), INT_LINK_CHG);
        let accept = RX_ACCEPT_BROADCAST | RX_ACCEPT_MULTICAST | RX_ACCEPT_MY_PHYS;
        assert_eq!(m.regs.get(RX_CONFIG) & accept, accept);
        assert_eq!((m.regs.get(MAR0), m.regs.get(MAR0 + 4)), (u32::MAX, u32::MAX));
        assert_eq!(m.regs.read(RX_MAX_SIZE, 2), 2048);
    }
    assert!(d.link().up);
    traffic(&hw, &mut d);
}

#[test]
fn rtl8168h_uses_the_ocp_phy_window() {
    let (hw, mut d) = run(0x541, false, 0x8168, false);
    assert!(d.chip().ocp_phy());
    {
        let m = hw.dev.borrow();
        assert!(m.ocp_used && !m.phyar_used);
        assert_eq!(m.regs.read(PMCH, 1) & 0xC0, 0xC0, "PHY PLL powered up");
        assert_ne!(m.regs.get(TX_CONFIG) & TXCFG_AUTO_FIFO, 0);
    }
    let l = d.link();
    assert!(l.up && l.mbps == 1000 && l.full_duplex);
    traffic(&hw, &mut d);
    d.rx_interrupts(true);
    assert_eq!(hw.dev.borrow().regs.read(INTR_MASK_8168, 2), INT_LINK_CHG | RX_INTS);
    hw.dev.borrow_mut().regs.write(INTR_STATUS_8168, 2, (INT_RX_OK | INT_LINK_CHG) as u32);
    d.ack();
    assert_eq!(hw.dev.borrow().regs.read(INTR_STATUS_8168, 2), 0);
}

#[test]
fn rtl8125_specifics() {
    let (hw, mut d) = run(0x641, true, 0x8125, true);
    assert_eq!(d.name(), "r8169 (RTL8125)");
    assert_eq!(d.mac(), MAC, "address from MAC0_BKP");
    {
        let m = hw.dev.borrow();
        assert_eq!(m.mac_ocp[&MAC_OCP_TX_DESC_FORMAT] & 1, 0, "new descriptor format off");
        assert_eq!(m.regs.get(INTR_MASK_8125), INT_LINK_CHG);
        assert_eq!(m.regs.get(RX_CONFIG) & RX_FETCH_DFLT_8125, RX_FETCH_DFLT_8125);
        assert_eq!(&m.regs.get(IDR0).to_le_bytes(), &MAC[..4], "address programmed while unlocked");
        assert_eq!(m.writes_while_locked, 0);
    }
    assert_eq!(d.link().mbps, 2500);
    traffic(&hw, &mut d);
    d.rx_interrupts(false);
    assert_eq!(hw.dev.borrow().regs.get(INTR_MASK_8125), INT_LINK_CHG);
}

#[test]
fn chip_generations() {
    assert!(!Chip::from_txconfig(0x2C8 << 20, false).ocp_phy());
    assert!(Chip::from_txconfig(0x2C8 << 20, false).auto_fifo());
    assert_eq!(Chip::from_txconfig(0x281 << 20, false).pll_bits(), 0x80);
    assert_eq!(Chip::from_txconfig(0x3C0 << 20, false).pll_bits(), 0);
    assert!(Chip::from_txconfig(0x4C0 << 20, false).ocp_phy());
    // Bits 4-5 of the XID field (revision-specific) are masked out.
    assert_eq!(Chip::from_txconfig(0x5F8 << 20, false).xid, 0x5C8);
}
