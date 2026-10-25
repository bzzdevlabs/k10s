//! Realtek r8169-class Ethernet: the RTL8111/8168 family (and RTL8411) and the RTL8125 2.5 GbE, through their
//! memory BAR. One register layout with per-generation differences:
//!
//! * The chip version (XID) is read from TxConfig. PHY registers are reached through PHYAR on older 8168s and
//!   through the PHY OCP window (GPHY_OCP, standard registers at 0xA400) from the 8168G on and on the 8125.
//! * The 8125 moved the interrupt registers (32 bits at 0x38/0x3C) and the transmit poll register (0x90), keeps a
//!   backup of the factory address (MAC0_BKP) and can use a newer transmit descriptor format, which is turned off
//!   (MAC OCP register 0xEB58 bit 0) so both generations use the classic 16-byte descriptors.
//! * The PHY PLL is powered up (PMCH) where the chip has that control, the PHY powered up with auto-negotiation
//!   restarted.
//!
//! Not done (the real chips need checking, see metal/README.md "Hardware bring-up"): the per-revision PCIe PHY
//! (ephy) and PHY parameter tables and the PHY firmware patches (`rtl_nic/*.fw`) Linux applies; chips work
//! without them on many boards since the firmware (or the option ROM) initialized the PHY, but link quality and
//! some revisions may need them. No checksum or segmentation offload; frames shorter than 60 bytes are padded in
//! software.

use crate::ids::Family;
use crate::ring::{Format, Rx, Tx, BUF};
use crate::{mac_from, mac_valid, mii, random_mac, wait, Hw, IrqMode, Link, Nic, Regs};

pub const IDR0: u32 = 0x00;
pub const IDR4: u32 = 0x04;
pub const MAR0: u32 = 0x08;
pub const TX_DESC_LO: u32 = 0x20;
pub const TX_DESC_HI: u32 = 0x24;
pub const CHIP_CMD: u32 = 0x37;
pub const TX_POLL_8168: u32 = 0x38;
pub const INTR_MASK_8168: u32 = 0x3C;
pub const INTR_STATUS_8168: u32 = 0x3E;
pub const INTR_MASK_8125: u32 = 0x38;
pub const INTR_STATUS_8125: u32 = 0x3C;
pub const TX_CONFIG: u32 = 0x40;
pub const RX_CONFIG: u32 = 0x44;
pub const CFG9346: u32 = 0x50;
pub const PHYAR: u32 = 0x60;
pub const PHY_STATUS: u32 = 0x6C;
pub const PMCH: u32 = 0x6F;
pub const TX_POLL_8125: u32 = 0x90;
pub const MAC_OCP: u32 = 0xB0;
pub const GPHY_OCP: u32 = 0xB8;
pub const RX_MAX_SIZE: u32 = 0xDA;
pub const CPLUS_CMD: u32 = 0xE0;
pub const INTR_MITIGATE: u32 = 0xE2;
pub const RX_DESC_LO: u32 = 0xE4;
pub const RX_DESC_HI: u32 = 0xE8;
pub const MAX_TX_PACKET_SIZE: u32 = 0xEC;
pub const MAC0_BKP: u32 = 0x19E0;

pub const CMD_RESET: u8 = 1 << 4;
pub const CMD_RX_ENABLE: u8 = 1 << 3;
pub const CMD_TX_ENABLE: u8 = 1 << 2;
pub const CFG9346_UNLOCK: u8 = 0xC0;
pub const CFG9346_LOCK: u8 = 0x00;
pub const TX_POLL_NPQ: u8 = 1 << 6;
/// Unlimited DMA bursts.
pub const DMA_BURST: u32 = 7 << 8;
pub const TX_IFG_96: u32 = 3 << 24;
pub const TXCFG_AUTO_FIFO: u32 = 1 << 7;
pub const RX_ACCEPT_MY_PHYS: u32 = 1 << 1;
pub const RX_ACCEPT_MULTICAST: u32 = 1 << 2;
pub const RX_ACCEPT_BROADCAST: u32 = 1 << 3;
pub const RX_EARLY_OFF: u32 = 1 << 11;
pub const RX_MULTI_EN: u32 = 1 << 14;
pub const RX128_INT_EN: u32 = 1 << 15;
pub const RX_FETCH_DFLT_8125: u32 = 8 << 27;
pub const CPLUS_RX_CHKSUM: u16 = 1 << 5;
pub const CPLUS_RX_VLAN: u16 = 1 << 6;
pub const OCP_FLAG: u32 = 1 << 31;
pub const OCP_STD_PHY_BASE: u32 = 0xA400;
/// MAC OCP register whose bit 0 selects the 8125's newer transmit descriptor format.
pub const MAC_OCP_TX_DESC_FORMAT: u32 = 0xEB58;
pub const INT_RX_OK: u32 = 1 << 0;
pub const INT_RX_ERR: u32 = 1 << 1;
pub const INT_RX_OVERFLOW: u32 = 1 << 4;
pub const INT_LINK_CHG: u32 = 1 << 5;
pub const INT_RX_FIFO_OVER: u32 = 1 << 6;
pub const RX_INTS: u32 = INT_RX_OK | INT_RX_ERR | INT_RX_OVERFLOW | INT_RX_FIFO_OVER;
pub const PHY_FULL_DUP: u16 = 1 << 0;
pub const PHY_LINK: u16 = 1 << 1;
pub const PHY_10: u16 = 1 << 2;
pub const PHY_100: u16 = 1 << 3;
pub const PHY_1000: u16 = 1 << 4;
pub const PHY_2500_8125: u16 = 1 << 10;

/// A chip generation, from its XID (TxConfig bits 20..30 masked as Linux does).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Chip {
    pub xid: u16,
    pub r8125: bool,
}

impl Chip {
    pub fn from_txconfig(txcfg: u32, r8125: bool) -> Chip {
        Chip { xid: ((txcfg >> 20) & 0xFCF) as u16, r8125 }
    }
    /// 8168G and later (and the 8125): PHY through OCP, receive early-interrupt and multi-fetch settings.
    pub fn ocp_phy(self) -> bool {
        self.r8125 || (0x4C0..0x600).contains(&self.xid)
    }
    /// The PMCH bits that power the PHY PLL up: 8168D/DP/E/F use bit 7, 8168G and later bits 6-7.
    pub fn pll_bits(self) -> u8 {
        if self.ocp_phy() {
            0xC0
        } else if (0x280..0x300).contains(&self.xid) || (0x480..0x4C0).contains(&self.xid) {
            0x80
        } else {
            0
        }
    }
    /// 8168E-VL and later: automatic transmit FIFO.
    pub fn auto_fifo(self) -> bool {
        self.r8125 || self.xid >= 0x480 || matches!(self.xid, 0x2C8 | 0x2C9)
    }
    fn intr_mask(self) -> u32 {
        if self.r8125 { INTR_MASK_8125 } else { INTR_MASK_8168 }
    }
    fn intr_status(self) -> u32 {
        if self.r8125 { INTR_STATUS_8125 } else { INTR_STATUS_8168 }
    }
}

pub struct Rtl8169<H: Hw> {
    hw: H,
    chip: Chip,
    name: &'static str,
    mac: [u8; 6],
    rx: Rx<H::Mem>,
    tx: Tx<H::Mem>,
    irq: IrqMode,
    doorbells: u64,
}

/// Writes the interrupt mask (16 bits on the 8168, 32 on the 8125).
pub fn set_mask(r: &impl Regs, chip: Chip, mask: u32) {
    if chip.r8125 {
        r.write32(chip.intr_mask(), mask);
    } else {
        r.write16(chip.intr_mask(), mask as u16);
    }
}

/// Unmasks (true) or masks the receive interrupts, keeping link changes: callable without the driver object.
pub fn set_rx_irq(r: &impl Regs, chip: Chip, on: bool) {
    set_mask(r, chip, INT_LINK_CHG | if on { RX_INTS } else { 0 });
}

/// Acknowledges every pending event (write one to clear).
pub fn ack(r: &impl Regs, chip: Chip) -> u32 {
    if chip.r8125 {
        let s = r.read32(chip.intr_status());
        r.write32(chip.intr_status(), s);
        s
    } else {
        let s = r.read16(chip.intr_status());
        r.write16(chip.intr_status(), s);
        s as u32
    }
}

struct Phy<'a, H: Hw> {
    hw: &'a H,
    chip: Chip,
}

impl<H: Hw> Phy<'_, H> {
    fn write(&self, reg: u8, v: u16) -> bool {
        if self.chip.ocp_phy() {
            self.hw.write32(GPHY_OCP, OCP_FLAG | (OCP_STD_PHY_BASE + reg as u32 * 2) << 15 | v as u32);
            wait(self.hw, 10_000, 25, || self.hw.read32(GPHY_OCP) & OCP_FLAG == 0)
        } else {
            self.hw.write32(PHYAR, OCP_FLAG | (reg as u32 & 0x1F) << 16 | v as u32);
            let ok = wait(self.hw, 10_000, 25, || self.hw.read32(PHYAR) & OCP_FLAG == 0);
            // The chip needs a pause between PHY accesses.
            self.hw.delay_us(20);
            ok
        }
    }
    fn read(&self, reg: u8) -> Option<u16> {
        let (r, cmd) = if self.chip.ocp_phy() {
            (GPHY_OCP, (OCP_STD_PHY_BASE + reg as u32 * 2) << 15)
        } else {
            (PHYAR, (reg as u32 & 0x1F) << 16)
        };
        self.hw.write32(r, cmd);
        let mut v = 0;
        let ok = wait(self.hw, 10_000, 25, || {
            v = self.hw.read32(r);
            v & OCP_FLAG != 0
        });
        if !self.chip.ocp_phy() {
            self.hw.delay_us(20);
        }
        ok.then_some(v as u16)
    }
}

pub fn mac_ocp_read(r: &impl Regs, reg: u32) -> u16 {
    r.write32(MAC_OCP, reg << 15);
    r.read32(MAC_OCP) as u16
}

pub fn mac_ocp_write(r: &impl Regs, reg: u32, v: u16) {
    r.write32(MAC_OCP, OCP_FLAG | reg << 15 | v as u32);
}

impl<H: Hw> Rtl8169<H> {
    /// Brings up the controller with `nbufs` receive and transmit buffers; `irq`: how the kernel routed its
    /// interrupt (one MSI or MSI-X vector for every event).
    pub fn new(hw: H, device: u16, nbufs: u16, irq: IrqMode) -> Result<Rtl8169<H>, &'static str> {
        let id = crate::ids::nic(crate::ids::REALTEK, device).ok_or("not an r8169-class device")?;
        let r8125 = id.family == Family::Rtl8125;
        let txcfg = hw.read32(TX_CONFIG);
        if txcfg == !0 {
            return Err("registers read as all ones (device absent or powered down)");
        }
        let chip = Chip::from_txconfig(txcfg, r8125);
        let name = if r8125 { "r8169 (RTL8125)" } else { "r8169 (RTL8111/8168)" };
        set_mask(&hw, chip, 0);
        ack(&hw, chip);
        hw.write8(CHIP_CMD, CMD_RESET);
        if !wait(&hw, 100_000, 100, || hw.read8(CHIP_CMD) & CMD_RESET == 0) {
            return Err("reset did not complete");
        }
        hw.bus_master();

        let mut mac = [0u8; 6];
        if r8125 {
            mac = mac_from(hw.read32(MAC0_BKP), hw.read32(MAC0_BKP + 4));
        }
        if !mac_valid(&mac) {
            mac = mac_from(hw.read32(IDR0), hw.read32(IDR4));
        }
        let stored = mac_valid(&mac);
        if !stored {
            mac = random_mac(&hw);
            hw.log(format_args!("r8169: no address in the EEPROM: using a random one"));
        }

        hw.write8(CFG9346, CFG9346_UNLOCK);
        if !stored || mac != mac_from(hw.read32(IDR0), hw.read32(IDR4)) {
            // The address registers take 32-bit writes, upper half first.
            hw.write32(IDR4, u16::from_le_bytes([mac[4], mac[5]]) as u32);
            hw.write32(IDR0, u32::from_le_bytes([mac[0], mac[1], mac[2], mac[3]]));
        }
        if r8125 {
            let f = mac_ocp_read(&hw, MAC_OCP_TX_DESC_FORMAT);
            mac_ocp_write(&hw, MAC_OCP_TX_DESC_FORMAT, f & !1);
        }
        let pll = chip.pll_bits();
        if pll != 0 {
            hw.write8(PMCH, hw.read8(PMCH) | pll);
        }
        let cp = hw.read16(CPLUS_CMD);
        hw.write16(CPLUS_CMD, cp & !(CPLUS_RX_CHKSUM | CPLUS_RX_VLAN));
        hw.write16(RX_MAX_SIZE, BUF as u16);
        if !r8125 {
            // 8064 bytes in units of 128.
            hw.write8(MAX_TX_PACKET_SIZE, 0x3F);
        }
        hw.write16(INTR_MITIGATE, 0);

        let rx = Rx::new(&hw, Format::Realtek, nbufs).ok_or("no memory for the receive ring")?;
        let tx = Tx::new(&hw, Format::Realtek, nbufs).ok_or("no memory for the transmit ring")?;
        // High halves first: the low write latches the address on some revisions.
        hw.write32(TX_DESC_HI, (tx.ring_paddr() >> 32) as u32);
        hw.write32(TX_DESC_LO, tx.ring_paddr() as u32);
        hw.write32(RX_DESC_HI, (rx.ring_paddr() >> 32) as u32);
        hw.write32(RX_DESC_LO, rx.ring_paddr() as u32);
        hw.write8(CHIP_CMD, CMD_TX_ENABLE | CMD_RX_ENABLE);
        hw.write32(TX_CONFIG, DMA_BURST | TX_IFG_96 | if chip.auto_fifo() { TXCFG_AUTO_FIFO } else { 0 });
        let fetch = if r8125 {
            RX_FETCH_DFLT_8125
        } else if chip.ocp_phy() {
            RX128_INT_EN | RX_MULTI_EN | RX_EARLY_OFF
        } else {
            RX128_INT_EN
        };
        hw.write32(RX_CONFIG, fetch | DMA_BURST | RX_ACCEPT_BROADCAST | RX_ACCEPT_MY_PHYS);
        hw.write32(MAR0, 0);
        hw.write32(MAR0 + 4, 0);
        hw.write8(CFG9346, CFG9346_LOCK);

        let phy = Phy { hw: &hw, chip };
        let bmcr = phy.read(mii::BMCR).ok_or("PHY control register unreadable")?;
        phy.write(mii::BMCR, mii::autoneg(bmcr));
        let phy_id = phy.read(mii::PHYID1).zip(phy.read(mii::PHYID2)).map(|(h, l)| (h as u32) << 16 | l as u32);
        if irq != IrqMode::Polled {
            set_mask(&hw, chip, INT_LINK_CHG);
        }
        hw.log(format_args!(
            "r8169: {} {:04x}:{:04x}, XID {:03x}, PHY id {}",
            id.model,
            crate::ids::REALTEK,
            device,
            chip.xid,
            phy_id.map_or(alloc::string::String::from("unknown"), |p| alloc::format!("{p:08x}"))
        ));
        Ok(Rtl8169 { hw, chip, name, mac, rx, tx, irq, doorbells: 0 })
    }

    pub fn chip(&self) -> Chip {
        self.chip
    }
}

impl<H: Hw> Nic for Rtl8169<H> {
    type Mem = H::Mem;
    fn name(&self) -> &'static str {
        self.name
    }
    fn mac(&self) -> [u8; 6] {
        self.mac
    }
    fn rings(&mut self) -> (&mut Rx<H::Mem>, &mut Tx<H::Mem>) {
        (&mut self.rx, &mut self.tx)
    }
    fn rings_ref(&self) -> (&Rx<H::Mem>, &Tx<H::Mem>) {
        (&self.rx, &self.tx)
    }
    fn flush(&mut self) {
        if self.tx.take_dirty() {
            core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
            if self.chip.r8125 {
                self.hw.write16(TX_POLL_8125, 1);
            } else {
                self.hw.write8(TX_POLL_8168, TX_POLL_NPQ);
            }
            self.doorbells += 1;
        }
    }
    fn rx_interrupts(&mut self, on: bool) {
        if self.irq != IrqMode::Polled {
            set_rx_irq(&self.hw, self.chip, on);
        }
    }
    fn ack(&mut self) {
        ack(&self.hw, self.chip);
    }
    fn link(&mut self) -> Link {
        let s = if self.chip.r8125 { self.hw.read16(PHY_STATUS) } else { self.hw.read8(PHY_STATUS) as u16 };
        let up = s & PHY_LINK != 0;
        let mbps = if s & PHY_2500_8125 != 0 && self.chip.r8125 {
            2500
        } else if s & PHY_1000 != 0 {
            1000
        } else if s & PHY_100 != 0 {
            100
        } else if s & PHY_10 != 0 {
            10
        } else {
            0
        };
        Link { up, mbps: if up { mbps } else { 0 }, full_duplex: up && s & PHY_FULL_DUP != 0 }
    }
    fn doorbells(&self) -> u64 {
        self.doorbells
    }
}

impl<H: Hw> Drop for Rtl8169<H> {
    fn drop(&mut self) {
        set_mask(&self.hw, self.chip, 0);
        self.hw.write8(CHIP_CMD, CMD_RESET);
        self.hw.delay_us(10_000);
    }
}
