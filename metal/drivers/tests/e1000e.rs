//! e1000e against register models of the 82574L (also run for real under QEMU's `-device e1000e`) and of the
//! I219 with its PHY behind ULP, SMBus and the Management Engine (no QEMU model: these are the checks short of
//! the real chip). The models follow the 82574 and I219 datasheets and the behavior Linux's driver relies on.

mod common;

use std::collections::HashMap;

use common::*;
use k10_drivers::e1000e::*;
use k10_drivers::{IrqMode, Nic};

const MAC: [u8; 6] = [0x00, 0x1B, 0x21, 0xAA, 0xBB, 0xCC];

#[derive(Default)]
struct E1000eModel {
    regs: RegFile,
    pch: bool,
    /// Valid ME firmware (FWSM.FW_VALID).
    me: bool,
    /// The ME answers H2ME ULP requests.
    me_answers: bool,
    /// FWSM.RSPCIPHY clear: PHY resets blocked.
    phy_reset_blocked: bool,
    phy: Phy,
    page_sel: u16,
    /// The PHY is parked (ULP, or SMBus mode the MAC cannot reach): MDIO cycles fail.
    parked: bool,
    me_done_at: Option<u64>,
    kmrn: HashMap<u32, u16>,
    flush_required: bool,
    lanphypc_armed: bool,
    /// The firmware holds the PHY semaphore until this time.
    fw_sem_until: u64,
    mdio_without_semaphore: u32,
    tx_frames: Vec<Vec<u8>>,
    events: Vec<&'static str>,
    now: u64,
}

impl E1000eModel {
    fn new(pch: bool) -> E1000eModel {
        let mut m = E1000eModel { pch, me_answers: true, ..Default::default() };
        m.power_on();
        m
    }
    fn power_on(&mut self) {
        let r = &mut self.regs;
        r.set(STATUS, STATUS_GIO_MASTER_EN | if self.pch { STATUS_LAN_INIT_DONE } else { 0 });
        r.set(EECD, EECD_AUTO_RD);
        r.set(RAL0, u32::from_le_bytes([MAC[0], MAC[1], MAC[2], MAC[3]]));
        r.set(RAH0, u16::from_le_bytes([MAC[4], MAC[5]]) as u32 | 1 << 31);
        self.phy.set(0, 2, 0x0154);
        self.phy.set(0, 3, 0x00A0);
        self.phy.set(0, 0, mii_pdown());
        self.kmrn.insert(KMRN_K1_CONFIG, KMRN_K1_ENABLE | 0x8000);
    }
    fn fwsm(&self) -> u32 {
        let mut f = self.regs.get(FWSM);
        f = if self.me { f | FWSM_FW_VALID } else { f & !FWSM_FW_VALID };
        if self.phy_reset_blocked { f & !FWSM_RSPCIPHY } else { f | FWSM_RSPCIPHY }
    }
    fn reset(&mut self, phy_too: bool) {
        self.events.push(if phy_too { "reset+phy" } else { "reset" });
        for off in [CTRL, RCTL, TCTL, RDH, RDT, TDH, TDT, RDLEN, TDLEN, ICR, IMS, EXTCNF_CTRL, CTRL_EXT] {
            self.regs.set(off, 0);
        }
        let s = self.regs.get(STATUS);
        self.regs.set(STATUS, (s | STATUS_GIO_MASTER_EN) & !STATUS_LU);
        if phy_too {
            self.phy_reset();
        }
    }
    fn phy_reset(&mut self) {
        self.events.push("phy-reset");
        let b = self.phy.get(0, 0);
        self.phy.set(0, 0, (b & !(1 << 9)) | 1 << 12);
        self.regs.set(STATUS, self.regs.get(STATUS) | STATUS_LAN_INIT_DONE);
    }
    fn update_link(&mut self) {
        let up = self.regs.get(CTRL) & CTRL_SLU != 0 && self.phy.get(0, 0) & (1 << 11) == 0 && self.phy.get(0, 0) & (1 << 12) != 0 && !self.parked;
        let s = self.regs.get(STATUS) & !(STATUS_LU | STATUS_FD | 3 << 6);
        self.regs.set(STATUS, if up { s | STATUS_LU | STATUS_FD | 2 << 6 } else { s });
    }
    fn mdic(&mut self, v: u32) {
        if self.regs.get(EXTCNF_CTRL) & EXTCNF_SWFLAG == 0 {
            self.mdio_without_semaphore += 1;
        }
        let (data, reg, addr, op) = (v as u16, ((v >> 16) & 0x1F) as u8, (v >> 21) & 0x1F, (v >> 26) & 3);
        let ok_addr = if self.pch { addr == 1 || addr == 2 } else { addr == 1 };
        if self.parked || !ok_addr {
            self.regs.set(MDIC, v | MDIC_READY | MDIC_ERROR);
            return;
        }
        if self.pch && addr == 1 && reg == 0x1F {
            self.page_sel = data >> 5;
            self.regs.set(MDIC, v | MDIC_READY);
            return;
        }
        let page = if self.pch && addr == 1 && reg > 15 { self.page_sel } else { 0 };
        let out = if op == 1 {
            self.phy.set(page, reg, data);
            data
        } else {
            self.phy.get(page, reg)
        };
        self.regs.set(MDIC, (v & !0xFFFF) | out as u32 | MDIC_READY);
        self.update_link();
    }
    fn transmit(&mut self) {
        let base = self.regs.get(TDBAL) as u64 | (self.regs.get(TDBAH) as u64) << 32;
        let n = self.regs.get(TDLEN) / 16;
        let (mut h, t) = (self.regs.get(TDH), self.regs.get(TDT));
        while h != t {
            let d = base + h as u64 * 16;
            unsafe {
                let addr = rd64(d);
                let w = rd32(d + 8);
                let len = (w & 0xFFFF) as usize;
                let cmd = (w >> 24) as u8;
                if cmd & 1 != 0 {
                    self.tx_frames.push(std::slice::from_raw_parts(addr as *const u8, len).to_vec());
                } else {
                    self.events.push("flush-frame");
                    self.flush_required = false;
                }
                wr32(d + 12, rd32(d + 12) | 1);
            }
            h = (h + 1) % n;
        }
        self.regs.set(TDH, h);
    }
    /// Receives `f` with `errors` in the descriptor: false if the ring has no descriptor for the device.
    fn deliver_with(&mut self, f: &[u8], errors: u8) -> bool {
        if self.regs.get(RCTL) & RCTL_EN == 0 {
            return false;
        }
        let base = self.regs.get(RDBAL) as u64 | (self.regs.get(RDBAH) as u64) << 32;
        let n = self.regs.get(RDLEN) / 16;
        let h = self.regs.get(RDH);
        if h == self.regs.get(RDT) {
            return false;
        }
        let d = base + h as u64 * 16;
        unsafe {
            let addr = rd64(d);
            std::ptr::copy_nonoverlapping(f.as_ptr(), addr as *mut u8, f.len());
            wr32(d + 8, f.len() as u32);
            wr32(d + 12, 0x3 | (errors as u32) << 8);
        }
        self.regs.set(RDH, (h + 1) % n);
        self.regs.set(ICR, self.regs.get(ICR) | ICR_RXT0);
        true
    }
    fn deliver(&mut self, f: &[u8]) -> bool {
        self.deliver_with(f, 0)
    }
}

fn mii_pdown() -> u16 {
    1 << 11 | 1 << 12
}

impl Model for E1000eModel {
    fn read(&mut self, off: u32, bytes: u8) -> u32 {
        match off {
            CTRL => self.regs.get(CTRL) & !(CTRL_RST | CTRL_PHY_RST),
            FWSM => self.fwsm(),
            ICR => {
                let v = self.regs.get(ICR);
                self.regs.set(ICR, 0);
                v
            }
            _ => self.regs.read(off, bytes),
        }
    }
    fn write(&mut self, off: u32, bytes: u8, v: u32) {
        match off {
            CTRL => {
                let old = self.regs.get(CTRL);
                if v & CTRL_RST != 0 {
                    self.reset(v & CTRL_PHY_RST != 0);
                    return;
                }
                if v & CTRL_PHY_RST != 0 && old & CTRL_PHY_RST == 0 {
                    self.phy_reset();
                }
                if v & CTRL_LANPHYPC_OVERRIDE != 0 && v & CTRL_LANPHYPC_VALUE == 0 {
                    self.lanphypc_armed = true;
                }
                if v & CTRL_LANPHYPC_OVERRIDE == 0 && self.lanphypc_armed {
                    self.lanphypc_armed = false;
                    self.events.push("lanphypc");
                    self.parked = false;
                    self.regs.set(CTRL_EXT, self.regs.get(CTRL_EXT) | CTRL_EXT_LPCD);
                }
                if v & CTRL_GIO_MASTER_DISABLE != 0 {
                    self.regs.set(STATUS, self.regs.get(STATUS) & !STATUS_GIO_MASTER_EN);
                }
                self.regs.set(CTRL, v);
                self.update_link();
            }
            STATUS => {
                // LAN_INIT_DONE and PHYRA are cleared by writing zero.
                let s = self.regs.get(STATUS);
                self.regs.set(STATUS, s & (v | !(STATUS_LAN_INIT_DONE | STATUS_PHYRA)));
            }
            EXTCNF_CTRL => {
                if self.now < self.fw_sem_until && v & EXTCNF_SWFLAG != 0 {
                    return;
                }
                self.regs.set(EXTCNF_CTRL, v);
            }
            MDIC => self.mdic(v),
            KMRNCTRLSTA => {
                let o = (v >> 16) & 0x1F;
                if v & KMRN_REN != 0 {
                    self.regs.set(KMRNCTRLSTA, *self.kmrn.get(&o).unwrap_or(&0) as u32);
                } else {
                    self.kmrn.insert(o, v as u16);
                }
            }
            H2ME => {
                self.regs.set(H2ME, v);
                if self.me && self.me_answers && v & H2ME_ENFORCE_SETTINGS != 0 && v & H2ME_ULP == 0 {
                    self.me_done_at = Some(self.now + 30_000);
                }
            }
            TDT => {
                self.regs.set(TDT, v);
                if self.regs.get(TCTL) & TCTL_EN != 0 {
                    self.transmit();
                }
            }
            _ => self.regs.write(off, bytes, v),
        }
    }
    fn config_read32(&mut self, off: u16) -> u32 {
        if off == PCI_DESC_RING_STATUS & !3 && self.flush_required { FLUSH_DESC_REQUIRED } else { 0 }
    }
    fn advance(&mut self, us: u64) {
        self.now += us;
        if self.me_done_at.is_some_and(|t| self.now >= t) {
            self.me_done_at = None;
            self.events.push("me-ulp-exit");
            self.regs.set(FWSM, self.regs.get(FWSM) & !FWSM_ULP_CFG_DONE);
            self.parked = false;
        }
    }
}

fn bring_up(m: E1000eModel, device: u16) -> (MockHw<E1000eModel>, Result<E1000e<MockHw<E1000eModel>>, &'static str>) {
    let hw = MockHw::new(m);
    let d = E1000e::new(hw.clone(), device, 32, IrqMode::Msi);
    (hw, d)
}

#[test]
fn e82574_brings_up_and_moves_frames() {
    let (hw, d) = bring_up(E1000eModel::new(false), 0x10D3);
    let mut d = d.unwrap();
    assert_eq!(d.name(), "e1000e (82574)");
    assert_eq!(d.mac(), MAC);
    {
        let m = hw.dev.borrow();
        assert_eq!(m.mdio_without_semaphore, 0);
        assert_eq!(m.regs.get(EXTCNF_CTRL) & EXTCNF_SWFLAG, 0, "semaphore released");
        assert_ne!(m.regs.get(CTRL_EXT) & CTRL_EXT_DRV_LOAD, 0);
        assert_eq!(m.regs.get(RCTL), RCTL_EN | RCTL_MPE | RCTL_BAM | RCTL_SECRC);
        assert_eq!(m.regs.get(TCTL) & (TCTL_EN | TCTL_PSP), TCTL_EN | TCTL_PSP);
        assert_eq!(m.regs.get(RDT), 31, "all but one receive descriptor given to the device");
        assert_eq!(m.regs.get(RDLEN), 32 * 16);
        assert_eq!(m.regs.get(IMS), ICR_LSC);
        assert_eq!(m.phy.get(0, 0) & (1 << 11), 0, "PHY powered up");
        assert_ne!(m.regs.get(GCR2) & 1, 0, "82574 PCIe errata bit");
        assert_eq!(m.events, ["reset"]);
    }
    let l = d.link();
    assert!(l.up && l.full_duplex && l.mbps == 1000, "{l:?}");

    let frames: Vec<Vec<u8>> = (0..3).map(|i| frame(60 + i * 100, i as u8)).collect();
    for f in &frames {
        assert!(hw.dev.borrow_mut().deliver(f));
    }
    let (rx, _) = d.rings();
    assert!(rx.pending());
    for (i, f) in frames.iter().enumerate() {
        let (id, len) = rx.pop().unwrap();
        assert_eq!((id as usize, len), (i, f.len()));
        assert_eq!(rx.frame(id, len), &f[..]);
        rx.recycle(id);
    }
    assert!(rx.pop().is_none());
    d.flush();
    assert_eq!(hw.dev.borrow().regs.get(RDT), 2, "tail = the last buffer returned");

    // The ring fills up after 31 frames.
    let mut n = 0;
    while hw.dev.borrow_mut().deliver(&frame(64, 9)) {
        n += 1;
    }
    assert_eq!(n, 31);
    let (rx, _) = d.rings();
    let mut got = 0;
    while let Some((id, _)) = rx.pop() {
        rx.recycle(id);
        got += 1;
    }
    assert_eq!(got, 31);

    for i in 0..40u8 {
        {
            let (_, tx) = d.rings();
            assert!(tx.ready());
            let f = frame(42 + i as usize, i);
            tx.send(f.len(), &mut |b| b.copy_from_slice(&f));
        }
        if i % 10 == 9 {
            d.flush();
        }
    }
    let m = hw.dev.borrow();
    assert_eq!(m.tx_frames.len(), 40);
    assert!(m.tx_frames.iter().enumerate().all(|(i, f)| *f == frame(42 + i, i as u8)));
    drop(m);
    assert_eq!(d.counters().tx_packets, 40);
    assert_eq!(d.counters().rx_packets, 34);
}

#[test]
fn e82574_recycles_out_of_order_and_drops_bad_frames() {
    let (hw, d) = bring_up(E1000eModel::new(false), 0x10D3);
    let mut d = d.unwrap();
    {
        let mut m = hw.dev.borrow_mut();
        m.deliver(&frame(100, 1));
        // A CRC error: dropped, its buffer given back.
        m.deliver_with(&frame(100, 2), 0x01);
        m.deliver(&frame(100, 3));
        m.deliver(&frame(100, 4));
    }
    let (rx, _) = d.rings();
    let a = rx.pop().unwrap();
    let b = rx.pop().unwrap();
    let c = rx.pop().unwrap();
    assert_eq!((a.0, b.0, c.0), (0, 2, 3));
    assert_eq!(rx.errors, 1);
    assert_eq!(rx.frame(b.0, b.1)[20], 3);
    // Buffer 1 (the bad frame) came back already; 3 and 2 come back before 0: the tail waits for 0.
    rx.recycle(c.0);
    rx.recycle(b.0);
    assert_eq!(rx.take_tail(), None);
    rx.recycle(a.0);
    assert_eq!(rx.tail(), 3);
    d.flush();
    assert_eq!(hw.dev.borrow().regs.get(RDT), 3);
}

#[test]
fn e82574_waits_for_the_firmware_semaphore() {
    let mut m = E1000eModel::new(false);
    m.fw_sem_until = 5_000;
    let (hw, d) = bring_up(m, 0x10D3);
    assert!(d.is_ok());
    assert_eq!(hw.dev.borrow().mdio_without_semaphore, 0);
}

#[test]
fn i219_leaves_ulp_without_me_and_hands_the_phy_over() {
    let mut m = E1000eModel::new(true);
    m.parked = true;
    m.phy.set(769, 23, CV_SMB_CTRL_FORCE_SMBUS);
    m.phy.set(779, 16, ULP_CONFIG1_CLEAR);
    m.phy.set(770, 17, 0);
    m.regs.set(FEXTNVM7, FEXTNVM7_DISABLE_SMB_PERST);
    m.regs.set(PHY_CTRL, PHY_CTRL_GBE_DISABLE | PHY_CTRL_NOND0A_LPLU);
    let (hw, d) = bring_up(m, 0x15BC);
    let mut d = d.unwrap();
    assert_eq!(d.name(), "e1000e (I219)");
    let m = hw.dev.borrow();
    assert_eq!(m.mdio_without_semaphore, 0);
    assert_eq!(m.regs.get(EXTCNF_CTRL) & EXTCNF_SWFLAG, 0);
    assert_eq!(m.events.first(), Some(&"lanphypc"), "{:?}", m.events);
    assert!(m.events.contains(&"reset+phy"), "{:?}", m.events);
    assert_eq!(m.phy.get(769, 23) & CV_SMB_CTRL_FORCE_SMBUS, 0, "PHY out of forced SMBus mode");
    assert_eq!(m.phy.get(779, 16) & ULP_CONFIG1_CLEAR, 0, "ULP configuration cleared");
    assert_ne!(m.phy.get(779, 16) & ULP_CONFIG1_START, 0, "ULP change committed");
    assert_eq!(m.regs.get(CTRL_EXT) & CTRL_EXT_FORCE_SMBUS, 0, "MAC out of forced SMBus mode");
    assert_eq!(m.regs.get(FEXTNVM7) & FEXTNVM7_DISABLE_SMB_PERST, 0);
    assert_eq!(m.kmrn[&KMRN_K1_CONFIG] & KMRN_K1_ENABLE, 0, "K1 off (Kumeran)");
    assert_eq!(m.phy.get(770, 17) & HV_PM_CTRL_K1_ENABLE, 0, "K1 off (PHY)");
    assert_eq!(m.regs.get(PHY_CTRL) & (PHY_CTRL_GBE_DISABLE | PHY_CTRL_NOND0A_LPLU), 0);
    assert_eq!(m.regs.get(TARC0) & TARC0_MULTIQ_REQ_MASK, TARC0_MULTIQ_2_REQ);
    assert_ne!(m.regs.get(TCTL) & TCTL_MULR, 0);
    drop(m);
    assert!(d.link().up);
}

#[test]
fn i219_asks_the_me_to_leave_ulp() {
    let mut m = E1000eModel::new(true);
    m.me = true;
    m.parked = true;
    m.regs.set(FWSM, FWSM_ULP_CFG_DONE);
    m.regs.set(H2ME, H2ME_ULP);
    let (hw, d) = bring_up(m, 0x15FC);
    assert!(d.is_ok(), "{:?}", d.err());
    let m = hw.dev.borrow();
    assert_eq!(m.events.first(), Some(&"me-ulp-exit"), "{:?}", m.events);
    assert!(!m.events.contains(&"lanphypc"), "no LANPHYPC toggle with the ME in charge: {:?}", m.events);
    assert_eq!(m.regs.get(H2ME) & (H2ME_ULP | H2ME_ENFORCE_SETTINGS), 0);
    assert_eq!(m.mdio_without_semaphore, 0);
}

#[test]
fn i219_unreachable_phy_with_resets_blocked_fails_cleanly() {
    let mut m = E1000eModel::new(true);
    m.me = true;
    m.me_answers = false;
    m.phy_reset_blocked = true;
    m.parked = true;
    m.regs.set(FWSM, FWSM_ULP_CFG_DONE);
    let (hw, d) = bring_up(m, 0x15BE);
    assert_eq!(d.err(), Some("PHY unreachable over MDIO (ULP/SMBus handover failed)"));
    assert!(hw.logged("the ME did not take the PHY out of ULP"));
    assert!(hw.logged("its reset is blocked by the ME"));
    assert_eq!(hw.dev.borrow().regs.get(EXTCNF_CTRL) & EXTCNF_SWFLAG, 0, "semaphore released on failure");
}

#[test]
fn i219_flushes_rings_left_running_before_the_reset() {
    let mut m = E1000eModel::new(true);
    m.flush_required = true;
    m.regs.set(TDLEN, 4096);
    let (hw, d) = bring_up(m, 0x15B8);
    assert!(d.is_ok());
    let m = hw.dev.borrow();
    let f = m.events.iter().position(|e| *e == "flush-frame").expect("flush frame sent");
    let r = m.events.iter().position(|e| e.starts_with("reset")).unwrap();
    assert!(f < r, "{:?}", m.events);
    assert!(!m.flush_required);
    assert!(m.tx_frames.is_empty(), "the flush frame is not a real frame");
    drop(m);
    assert!(hw.logged("flushing the descriptor rings"));
}

#[test]
fn interrupt_helpers_touch_only_their_registers() {
    let (hw, d) = bring_up(E1000eModel::new(false), 0x10D3);
    let mut d = d.unwrap();
    d.rx_interrupts(true);
    assert_eq!(hw.dev.borrow().regs.get(IMS), RX_CAUSES);
    d.rx_interrupts(false);
    assert_eq!(hw.dev.borrow().regs.get(IMC), RX_CAUSES);
    hw.dev.borrow_mut().regs.set(ICR, ICR_RXT0);
    d.ack();
    assert_eq!(hw.dev.borrow().regs.get(ICR), 0);
}
