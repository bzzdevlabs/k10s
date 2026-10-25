//! igb/igc against a register model: the I225/I226 specifics (DEV_RST, the moved interrupt registers, EEE off,
//! 2.5 Gb/s), the igb path QEMU's 82576 model also runs, the SW_FW_SYNC semaphore around every PHY access, and
//! advanced descriptors whose write-back overwrites the buffer address.

mod common;

use common::*;
use k10_drivers::igc::*;
use k10_drivers::ids::Family;
use k10_drivers::{IrqMode, Nic};

const MAC: [u8; 6] = [0xA8, 0xA1, 0x59, 0x01, 0x02, 0x03];

#[derive(Default)]
struct IgcModel {
    regs: RegFile,
    igc: bool,
    phy: Phy,
    /// The firmware holds the PHY semaphore (SW_FW_SYNC bit 17) until this time.
    fw_phy_until: u64,
    mdio_without_semaphore: u32,
    reset_bits: Vec<u32>,
    tx_frames: Vec<Vec<u8>>,
    now: u64,
}

impl IgcModel {
    fn new(igc: bool) -> IgcModel {
        let mut m = IgcModel { igc, ..Default::default() };
        m.regs.set(STATUS, STATUS_GIO_MASTER_EN);
        m.regs.set(EECD, EECD_AUTO_RD);
        m.regs.set(RAL0, u32::from_le_bytes([MAC[0], MAC[1], MAC[2], MAC[3]]));
        m.regs.set(RAH0, u16::from_le_bytes([MAC[4], MAC[5]]) as u32 | 1 << 31);
        m.regs.set(EEER, EEER_TX_LPI_EN | EEER_RX_LPI_EN | EEER_LPI_FC);
        m.regs.set(IPCNFG, IPCNFG_EEE_100M_AN | IPCNFG_EEE_1G_AN | IPCNFG_EEE_2_5G_AN);
        m.phy.set(0, 0, 1 << 11 | 1 << 12);
        m.phy.set(0, 2, 0x67C9);
        m.phy.set(0, 3, 0xDC00);
        m
    }
    fn ir(&self) -> IrqRegs {
        irq_regs(if self.igc { Family::I225 } else { Family::I82576 })
    }
    fn link(&mut self) {
        let up = self.regs.get(CTRL) & CTRL_SLU != 0 && self.phy.get(0, 0) & (1 << 11) == 0;
        let s = self.regs.get(STATUS) & !(STATUS_LU | STATUS_FD | STATUS_SPEED_2500 | 3 << 6);
        let speed = if self.igc { STATUS_SPEED_2500 | 2 << 6 } else { 2 << 6 };
        self.regs.set(STATUS, if up { s | STATUS_LU | STATUS_FD | speed } else { s });
    }
    fn transmit(&mut self) {
        let base = self.regs.get(TDBAL0) as u64 | (self.regs.get(TDBAH0) as u64) << 32;
        let n = self.regs.get(TDLEN0) / 16;
        let (mut h, t) = (self.regs.get(TDH0), self.regs.get(TDT0));
        while h != t {
            let d = base + h as u64 * 16;
            unsafe {
                let addr = rd64(d);
                let ctl = rd32(d + 8);
                assert_eq!(ctl & (0xF << 20 | 1 << 29), 0x3 << 20 | 1 << 29, "advanced data descriptor");
                assert_eq!(rd32(d + 12) >> 14, ctl & 0xFFFF, "PAYLEN");
                self.tx_frames.push(std::slice::from_raw_parts(addr as *const u8, (ctl & 0xFFFF) as usize).to_vec());
                // Write-back: DD in the status dword.
                wr32(d + 12, 1);
            }
            h = (h + 1) % n;
        }
        self.regs.set(TDH0, h);
    }
    fn deliver(&mut self, f: &[u8]) -> bool {
        if self.regs.get(RCTL) & RCTL_EN == 0 || self.regs.get(RXDCTL0) & QUEUE_ENABLE == 0 {
            return false;
        }
        let base = self.regs.get(RDBAL0) as u64 | (self.regs.get(RDBAH0) as u64) << 32;
        let n = self.regs.get(RDLEN0) / 16;
        let h = self.regs.get(RDH0);
        if h == self.regs.get(RDT0) {
            return false;
        }
        let d = base + h as u64 * 16;
        unsafe {
            let addr = rd64(d);
            assert_ne!(addr, 0xDEAD_BEEF_DEAD_BEEF, "descriptor reused without its buffer address");
            std::ptr::copy_nonoverlapping(f.as_ptr(), addr as *mut u8, f.len());
            // Write-back format: RSS fields over the address, status and length.
            wr32(d, 0xDEAD_BEEF);
            wr32(d + 4, 0xDEAD_BEEF);
            wr32(d + 8, 0x3);
            wr32(d + 12, f.len() as u32);
        }
        self.regs.set(RDH0, (h + 1) % n);
        self.regs.set(EICR, 1);
        true
    }
}

impl Model for IgcModel {
    fn read(&mut self, off: u32, bytes: u8) -> u32 {
        match off {
            CTRL => self.regs.get(CTRL) & !(CTRL_RST | CTRL_DEV_RST),
            SWSM => {
                let v = self.regs.get(SWSM);
                self.regs.set(SWSM, v | SWSM_SMBI);
                v
            }
            SW_FW_SYNC => {
                let fw = if self.now < self.fw_phy_until { SWFW_PHY0 << 16 } else { 0 };
                self.regs.get(SW_FW_SYNC) | fw
            }
            _ => self.regs.read(off, bytes),
        }
    }
    fn write(&mut self, off: u32, bytes: u8, v: u32) {
        match off {
            CTRL => {
                if v & (CTRL_RST | CTRL_DEV_RST) != 0 {
                    self.reset_bits.push(v & (CTRL_RST | CTRL_DEV_RST));
                    for r in [CTRL, RCTL, TCTL, RXDCTL0, TXDCTL0, RDH0, RDT0, TDH0, TDT0, GPIE, CTRL_EXT] {
                        self.regs.set(r, 0);
                    }
                    self.regs.set(STATUS, STATUS_GIO_MASTER_EN);
                    return;
                }
                if v & CTRL_GIO_MASTER_DISABLE != 0 {
                    self.regs.set(STATUS, self.regs.get(STATUS) & !STATUS_GIO_MASTER_EN);
                }
                self.regs.set(CTRL, v);
                self.link();
            }
            MDIC => {
                if self.regs.get(SW_FW_SYNC) & SWFW_PHY0 == 0 {
                    self.mdio_without_semaphore += 1;
                }
                let (data, reg, addr, op) = (v as u16, ((v >> 16) & 0x1F) as u8, (v >> 21) & 0x1F, (v >> 26) & 3);
                if addr != 1 {
                    self.regs.set(MDIC, v | MDIC_READY | MDIC_ERROR);
                    return;
                }
                let out = if op == 1 {
                    self.phy.set(0, reg, data);
                    data
                } else {
                    self.phy.get(0, reg)
                };
                self.regs.set(MDIC, (v & !0xFFFF) | out as u32 | MDIC_READY);
                self.link();
            }
            TDT0 => {
                self.regs.set(TDT0, v);
                if self.regs.get(TCTL) & TCTL_EN != 0 && self.regs.get(TXDCTL0) & QUEUE_ENABLE != 0 {
                    self.transmit();
                }
            }
            o if o == self.ir().icr => {}
            _ => self.regs.write(off, bytes, v),
        }
    }
    fn advance(&mut self, us: u64) {
        self.now += us;
    }
}

fn traffic(hw: &MockHw<IgcModel>, d: &mut Igc<MockHw<IgcModel>>, frames: usize) {
    for i in 0..frames {
        let f = frame(64 + i % 1400, i as u8);
        assert!(hw.dev.borrow_mut().deliver(&f), "frame {i}");
        let (rx, tx) = d.rings();
        let (id, len) = rx.pop().unwrap();
        assert_eq!(rx.frame(id, len), &f[..]);
        // Echo it back.
        assert!(tx.ready());
        let copy = rx.frame(id, len).to_vec();
        tx.send(len, &mut |b| b.copy_from_slice(&copy));
        rx.recycle(id);
        d.flush();
        assert_eq!(hw.dev.borrow().tx_frames.last(), Some(&f));
    }
}

#[test]
fn i225_brings_up_with_igc_specifics() {
    let hw = MockHw::new(IgcModel::new(true));
    let mut d = Igc::new(hw.clone(), 0x15F3, 16, IrqMode::MsiX).unwrap();
    assert_eq!(d.name(), "igc (I225/I226)");
    assert_eq!(d.mac(), MAC);
    let m = hw.dev.borrow();
    assert_eq!(m.reset_bits, [CTRL_DEV_RST]);
    assert_eq!(m.mdio_without_semaphore, 0);
    assert_eq!(m.regs.get(SW_FW_SYNC) & SWFW_PHY0, 0, "PHY semaphore released");
    assert_eq!(m.regs.get(SWSM) & (SWSM_SMBI | SWSM_SWESMBI), 0, "hardware semaphore released");
    assert_eq!(m.regs.get(EEER) & (EEER_TX_LPI_EN | EEER_RX_LPI_EN | EEER_LPI_FC), 0, "EEE off");
    assert_eq!(m.regs.get(IPCNFG) & (IPCNFG_EEE_100M_AN | IPCNFG_EEE_1G_AN | IPCNFG_EEE_2_5G_AN), 0);
    assert_eq!(m.regs.get(GPIE) & GPIE_MSIX_MODE, GPIE_MSIX_MODE);
    assert_eq!(m.regs.get(IVAR0), IVAR_VALID, "receive queue 0 on vector 0");
    assert_eq!(m.regs.get(IVAR_MISC), IVAR_VALID << 8, "link on vector 0");
    assert_eq!(m.regs.get(EIAC), 1);
    assert_eq!(m.regs.get(IMS_IGC), ICR_LSC);
    assert_eq!(m.regs.get(IMS_IGB), 0, "nothing written at igb's IMS");
    assert_eq!(m.regs.get(SRRCTL0), 2 | SRRCTL_DESCTYPE_ADV_ONEBUF | SRRCTL_DROP_EN);
    assert_eq!(m.regs.get(RDT0), 15);
    assert_ne!(m.regs.get(CTRL_EXT) & CTRL_EXT_DRV_LOAD, 0);
    drop(m);
    let l = d.link();
    assert!(l.up && l.mbps == 2500 && l.full_duplex, "{l:?}");
    // Three times around the ring: every reused descriptor gets its address back.
    traffic(&hw, &mut d, 48);
    d.rx_interrupts(true);
    assert_eq!(hw.dev.borrow().regs.get(EIMS), 1);
    d.rx_interrupts(false);
    assert_eq!(hw.dev.borrow().regs.get(EIMC), 1);
}

#[test]
fn igb_82576_keeps_e1000_interrupt_registers_and_eee() {
    let hw = MockHw::new(IgcModel::new(false));
    let mut d = Igc::new(hw.clone(), 0x10C9, 16, IrqMode::MsiX).unwrap();
    assert_eq!(d.name(), "igb (82576)");
    let m = hw.dev.borrow();
    assert_eq!(m.reset_bits, [CTRL_RST]);
    assert_eq!(m.regs.get(IMS_IGB), ICR_LSC);
    assert_eq!(m.regs.get(IMS_IGC), 0);
    assert_ne!(m.regs.get(EEER), 0, "the 82576 has no EEE registers: untouched");
    drop(m);
    assert_eq!(d.link().mbps, 1000);
    traffic(&hw, &mut d, 20);
}

#[test]
fn waits_for_the_firmware_to_release_the_phy() {
    let mut m = IgcModel::new(true);
    m.fw_phy_until = 30_000;
    let hw = MockHw::new(m);
    assert!(Igc::new(hw.clone(), 0x125C, 16, IrqMode::MsiX).is_ok());
    assert!(hw.dev.borrow().now >= 30_000);
    assert_eq!(hw.dev.borrow().mdio_without_semaphore, 0);
}

#[test]
fn without_msix_it_is_polled() {
    let hw = MockHw::new(IgcModel::new(true));
    let mut d = Igc::new(hw.clone(), 0x15F2, 16, IrqMode::Msi).unwrap();
    assert_eq!(hw.dev.borrow().regs.get(GPIE), 0);
    d.rx_interrupts(true);
    assert_eq!(hw.dev.borrow().regs.get(EIMS), 0);
}
