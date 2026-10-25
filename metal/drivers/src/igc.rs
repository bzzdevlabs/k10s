//! Intel I225/I226 (igc) and the igb family it derives from (82576, I350, I210/I211): advanced one-buffer
//! descriptors, per-queue ring registers at 0xC000 (receive) and 0xE000 (transmit), extended interrupt
//! registers (EICR/EIMS) with MSI-X. The differences that matter here: the interrupt cause registers moved (ICR at
//! 0x1500 on igc, 0x00C0 on igb), igc resets through CTRL.DEV_RST, the 2.5 Gb/s speed bit, and Energy-Efficient
//! Ethernet, which is turned off (I225 link drops with EEE are a known erratum; the 82576 has none).
//!
//! Bring-up: requests drained (GIO master disable), interrupts masked, reset, NVM auto-read waited for, the
//! driver-loaded bit set, the PHY (behind the software/firmware semaphore SW_FW_SYNC) powered up with
//! auto-negotiation restarted, the address read from receive address 0, the queue 0 rings enabled (polling the
//! enable bits), receive interrupts on MSI-X entry 0 together with link changes (the "other" cause). QEMU's igb
//! model (82576) runs the shared path end to end; the igc specifics are checked against a register model only.

use crate::ids::Family;
use crate::ring::{Format, Rx, Tx, BUF};
use crate::{mac_from, mac_valid, mii, random_mac, wait, Hw, IrqMode, Link, Nic, Regs};

pub const CTRL: u32 = 0x0000;
pub const STATUS: u32 = 0x0008;
pub const EECD: u32 = 0x0010;
pub const CTRL_EXT: u32 = 0x0018;
pub const MDIC: u32 = 0x0020;
pub const RCTL: u32 = 0x0100;
pub const TCTL: u32 = 0x0400;
pub const EEER: u32 = 0x0E30;
pub const IPCNFG: u32 = 0x0E38;
pub const GPIE: u32 = 0x1514;
pub const EICS: u32 = 0x1520;
pub const EIMS: u32 = 0x1524;
pub const EIMC: u32 = 0x1528;
pub const EIAC: u32 = 0x152C;
pub const EIAM: u32 = 0x1530;
pub const EICR: u32 = 0x1580;
pub const EITR0: u32 = 0x1680;
pub const IVAR0: u32 = 0x1700;
pub const IVAR_MISC: u32 = 0x1740;
pub const MTA: u32 = 0x5200;
pub const RAL0: u32 = 0x5400;
pub const RAH0: u32 = 0x5404;
pub const SWSM: u32 = 0x5B50;
pub const SW_FW_SYNC: u32 = 0x5B5C;
pub const RDBAL0: u32 = 0xC000;
pub const RDBAH0: u32 = 0xC004;
pub const RDLEN0: u32 = 0xC008;
pub const SRRCTL0: u32 = 0xC00C;
pub const RDH0: u32 = 0xC010;
pub const RDT0: u32 = 0xC018;
pub const RXDCTL0: u32 = 0xC028;
pub const TDBAL0: u32 = 0xE000;
pub const TDBAH0: u32 = 0xE004;
pub const TDLEN0: u32 = 0xE008;
pub const TDH0: u32 = 0xE010;
pub const TDT0: u32 = 0xE018;
pub const TXDCTL0: u32 = 0xE028;

/// The legacy interrupt registers: igb keeps e1000's, igc moved them.
pub const ICR_IGB: u32 = 0x00C0;
pub const IMS_IGB: u32 = 0x00D0;
pub const IMC_IGB: u32 = 0x00D8;
pub const ICR_IGC: u32 = 0x1500;
pub const IMS_IGC: u32 = 0x1508;
pub const IMC_IGC: u32 = 0x150C;

pub const CTRL_GIO_MASTER_DISABLE: u32 = 1 << 2;
pub const CTRL_SLU: u32 = 1 << 6;
pub const CTRL_FRCSPD: u32 = 1 << 11;
pub const CTRL_FRCDPX: u32 = 1 << 12;
pub const CTRL_RST: u32 = 1 << 26;
pub const CTRL_DEV_RST: u32 = 1 << 29;
pub const STATUS_FD: u32 = 1 << 0;
pub const STATUS_LU: u32 = 1 << 1;
pub const STATUS_GIO_MASTER_EN: u32 = 1 << 19;
pub const STATUS_SPEED_2500: u32 = 1 << 22;
pub const EECD_AUTO_RD: u32 = 1 << 9;
pub const CTRL_EXT_DRV_LOAD: u32 = 1 << 28;
pub const MDIC_OP_WRITE: u32 = 1 << 26;
pub const MDIC_OP_READ: u32 = 2 << 26;
pub const MDIC_READY: u32 = 1 << 28;
pub const MDIC_ERROR: u32 = 1 << 30;
pub const EEER_TX_LPI_EN: u32 = 1 << 16;
pub const EEER_RX_LPI_EN: u32 = 1 << 17;
pub const EEER_LPI_FC: u32 = 1 << 18;
pub const IPCNFG_EEE_100M_AN: u32 = 1 << 2;
pub const IPCNFG_EEE_1G_AN: u32 = 1 << 3;
pub const IPCNFG_EEE_2_5G_AN: u32 = 1 << 4;
pub const GPIE_NSICR: u32 = 1 << 0;
pub const GPIE_MSIX_MODE: u32 = 1 << 4;
pub const GPIE_PBA: u32 = 1 << 31;
pub const IVAR_VALID: u32 = 0x80;
pub const ICR_LSC: u32 = 1 << 2;
pub const ICR_RXDW: u32 = 1 << 7;
pub const SWSM_SMBI: u32 = 1 << 0;
pub const SWSM_SWESMBI: u32 = 1 << 1;
pub const SWFW_PHY0: u32 = 1 << 1;
pub const RCTL_EN: u32 = 1 << 1;
pub const RCTL_BAM: u32 = 1 << 15;
pub const RCTL_SECRC: u32 = 1 << 26;
pub const TCTL_EN: u32 = 1 << 1;
pub const TCTL_PSP: u32 = 1 << 3;
pub const TCTL_CT: u32 = 0x0F << 4;
pub const TCTL_RTLC: u32 = 1 << 24;
pub const SRRCTL_DESCTYPE_ADV_ONEBUF: u32 = 1 << 25;
pub const SRRCTL_DROP_EN: u32 = 1 << 31;
pub const QUEUE_ENABLE: u32 = 1 << 25;
/// Prefetch 8, host 8, write-back 4 descriptors.
pub const RXDCTL_THRESHOLDS: u32 = 8 | 8 << 8 | 4 << 16;
/// Prefetch 8, host 1, write-back 1: a completion is written back for every frame.
pub const TXDCTL_THRESHOLDS: u32 = 8 | 1 << 8 | 1 << 16;
const PHY_ADDR: u8 = 1;

/// The interrupt registers of a family member.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IrqRegs {
    pub icr: u32,
    pub ims: u32,
    pub imc: u32,
}

pub fn irq_regs(family: Family) -> IrqRegs {
    if family == Family::I225 {
        IrqRegs { icr: ICR_IGC, ims: IMS_IGC, imc: IMC_IGC }
    } else {
        IrqRegs { icr: ICR_IGB, ims: IMS_IGB, imc: IMC_IGB }
    }
}

pub struct Igc<H: Hw> {
    hw: H,
    family: Family,
    name: &'static str,
    mac: [u8; 6],
    rx: Rx<H::Mem>,
    tx: Tx<H::Mem>,
    irq: IrqMode,
    doorbells: u64,
}

struct Chip<'a, H: Hw> {
    hw: &'a H,
    ir: IrqRegs,
}

impl<H: Hw> Chip<'_, H> {
    fn r(&self, off: u32) -> u32 {
        self.hw.read32(off)
    }
    fn w(&self, off: u32, v: u32) {
        self.hw.write32(off, v)
    }
    fn set(&self, off: u32, bits: u32) {
        self.w(off, self.r(off) | bits)
    }
    fn clear(&self, off: u32, bits: u32) {
        self.w(off, self.r(off) & !bits)
    }
    fn flush(&self) {
        self.r(STATUS);
    }

    /// The hardware semaphore guarding SW_FW_SYNC: reading SWSM claims SMBI; then SWESMBI must stick.
    fn hw_semaphore(&self) -> bool {
        if !wait(self.hw, 100_000, 50, || self.r(SWSM) & SWSM_SMBI == 0) {
            return false;
        }
        self.set(SWSM, SWSM_SWESMBI);
        if self.r(SWSM) & SWSM_SWESMBI == 0 {
            self.clear(SWSM, SWSM_SMBI | SWSM_SWESMBI);
            return false;
        }
        true
    }
    fn hw_semaphore_release(&self) {
        self.clear(SWSM, SWSM_SMBI | SWSM_SWESMBI);
    }

    /// Claims `mask` in SW_FW_SYNC (free when neither software nor firmware holds it).
    fn acquire(&self, mask: u32) -> bool {
        for _ in 0..200 {
            if !self.hw_semaphore() {
                break;
            }
            let s = self.r(SW_FW_SYNC);
            if s & (mask | mask << 16) == 0 {
                self.w(SW_FW_SYNC, s | mask);
                self.hw_semaphore_release();
                return true;
            }
            self.hw_semaphore_release();
            self.hw.delay_us(5000);
        }
        self.hw.log(format_args!("igb/igc: the PHY semaphore stays held by the firmware"));
        false
    }
    fn release(&self, mask: u32) {
        if self.hw_semaphore() {
            self.clear(SW_FW_SYNC, mask);
            self.hw_semaphore_release();
        }
    }

    fn mdic(&self, reg: u8, write: Option<u16>) -> Option<u16> {
        let op = write.map_or(MDIC_OP_READ, |v| MDIC_OP_WRITE | v as u32);
        self.w(MDIC, op | (reg as u32 & 0x1F) << 16 | (PHY_ADDR as u32) << 21);
        let mut v = 0;
        let ok = wait(self.hw, 10_000, 5, || {
            v = self.r(MDIC);
            v & MDIC_READY != 0
        });
        (ok && v & MDIC_ERROR == 0).then_some(v as u16)
    }

    fn quiesce(&self) {
        self.set(CTRL, CTRL_GIO_MASTER_DISABLE);
        if !wait(self.hw, 80_000, 100, || self.r(STATUS) & STATUS_GIO_MASTER_EN == 0) {
            self.hw.log(format_args!("igb/igc: bus master requests still pending before the reset"));
        }
        self.w(self.ir.imc, !0);
        self.w(EIMC, !0);
        self.w(RCTL, 0);
        self.w(TCTL, TCTL_PSP);
        self.flush();
        self.hw.delay_us(10_000);
    }
}

impl<H: Hw> Igc<H> {
    /// Brings up the controller with `nbufs` receive and transmit buffers; `irq` says whether the kernel routed MSI-X
    /// entry 0 (MSI is not used by this family here: every member has MSI-X).
    pub fn new(hw: H, device: u16, nbufs: u16, irq: IrqMode) -> Result<Igc<H>, &'static str> {
        let id = crate::ids::nic(crate::ids::INTEL, device).ok_or("not an igb/igc device")?;
        let family = id.family;
        let name = match family {
            Family::I225 => "igc (I225/I226)",
            Family::I210 => "igb (I210/I211)",
            Family::I350 => "igb (I350)",
            Family::I82576 => "igb (82576)",
            _ => return Err("not an igb/igc device"),
        };
        let irq = if irq == IrqMode::MsiX { irq } else { IrqMode::Polled };
        let tag = if family == Family::I225 { "igc" } else { "igb" };
        let c = Chip { hw: &hw, ir: irq_regs(family) };
        if c.r(STATUS) == !0 {
            return Err("registers read as all ones (device absent or powered down)");
        }
        c.quiesce();
        c.set(CTRL, if family == Family::I225 { CTRL_DEV_RST } else { CTRL_RST });
        hw.delay_us(5000);
        if !wait(&hw, 100_000, 1000, || c.r(CTRL) & (CTRL_RST | CTRL_DEV_RST) == 0) {
            hw.log(format_args!("{tag}: reset did not complete"));
        }
        if !wait(&hw, 10_000, 1000, || c.r(EECD) & EECD_AUTO_RD != 0) {
            hw.log(format_args!("{tag}: NVM auto-read not reported done"));
        }
        c.w(c.ir.imc, !0);
        c.w(EIMC, !0);
        c.r(c.ir.icr);
        hw.bus_master();
        c.set(CTRL_EXT, CTRL_EXT_DRV_LOAD);

        if family != Family::I82576 {
            c.clear(EEER, EEER_TX_LPI_EN | EEER_RX_LPI_EN | EEER_LPI_FC);
            c.clear(IPCNFG, IPCNFG_EEE_100M_AN | IPCNFG_EEE_1G_AN | IPCNFG_EEE_2_5G_AN);
        }
        if !c.acquire(SWFW_PHY0) {
            return Err("PHY semaphore unavailable");
        }
        let phy = c.mdic(mii::PHYID1, None).zip(c.mdic(mii::PHYID2, None)).map(|(h, l)| (h as u32) << 16 | l as u32);
        let bmcr = c.mdic(mii::BMCR, None);
        if let Some(b) = bmcr {
            c.mdic(mii::BMCR, Some(mii::autoneg(b)));
        }
        c.release(SWFW_PHY0);
        if bmcr.is_none() {
            return Err("PHY control register unreadable");
        }
        c.w(CTRL, (c.r(CTRL) | CTRL_SLU) & !(CTRL_FRCSPD | CTRL_FRCDPX));

        let mut mac = mac_from(c.r(RAL0), c.r(RAH0));
        if !mac_valid(&mac) {
            mac = random_mac(&hw);
            hw.log(format_args!("{tag}: no address in the NVM: using a random one"));
        }
        c.w(RAL0, u32::from_le_bytes([mac[0], mac[1], mac[2], mac[3]]));
        c.w(RAH0, u16::from_le_bytes([mac[4], mac[5]]) as u32 | 1 << 31);
        for i in 0..128 {
            c.w(MTA + 4 * i, 0);
        }

        let rx = Rx::new(&hw, Format::IntelAdvanced, nbufs).ok_or("no memory for the receive ring")?;
        let tx = Tx::new(&hw, Format::IntelAdvanced, nbufs).ok_or("no memory for the transmit ring")?;
        c.w(RXDCTL0, 0);
        c.w(RDBAL0, rx.ring_paddr() as u32);
        c.w(RDBAH0, (rx.ring_paddr() >> 32) as u32);
        c.w(RDLEN0, rx.ring_bytes());
        c.w(RDH0, 0);
        c.w(RDT0, 0);
        c.w(SRRCTL0, (BUF as u32 / 1024) | SRRCTL_DESCTYPE_ADV_ONEBUF | SRRCTL_DROP_EN);
        c.w(RXDCTL0, RXDCTL_THRESHOLDS | QUEUE_ENABLE);
        if !wait(&hw, 10_000, 100, || c.r(RXDCTL0) & QUEUE_ENABLE != 0) {
            return Err("receive queue 0 did not enable");
        }
        c.w(TXDCTL0, 0);
        c.w(TDBAL0, tx.ring_paddr() as u32);
        c.w(TDBAH0, (tx.ring_paddr() >> 32) as u32);
        c.w(TDLEN0, tx.ring_bytes());
        c.w(TDH0, 0);
        c.w(TDT0, 0);
        c.w(TXDCTL0, TXDCTL_THRESHOLDS | QUEUE_ENABLE);
        if !wait(&hw, 10_000, 100, || c.r(TXDCTL0) & QUEUE_ENABLE != 0) {
            return Err("transmit queue 0 did not enable");
        }
        c.w(TCTL, TCTL_EN | TCTL_PSP | TCTL_CT | TCTL_RTLC);
        c.w(RCTL, RCTL_EN | RCTL_BAM | RCTL_SECRC);
        c.w(RDT0, rx.tail() as u32);

        if irq == IrqMode::MsiX {
            // Vector 0: receive queue 0 and the "other" causes (link). Its EICR bit clears itself when the message
            // is sent; the kernel masks and unmasks it through EIMS/EIMC.
            c.w(GPIE, GPIE_NSICR | GPIE_MSIX_MODE | GPIE_PBA);
            c.w(IVAR0, IVAR_VALID);
            c.w(IVAR_MISC, IVAR_VALID << 8);
            c.w(EIAC, 1);
            c.w(EIAM, 0);
            c.w(EITR0, 0);
            c.w(c.ir.ims, ICR_LSC);
        }
        hw.log(format_args!(
            "{tag}: {} {:04x}:{:04x}, PHY id {}",
            id.model,
            crate::ids::INTEL,
            device,
            phy.map_or(alloc::string::String::from("unknown"), |p| alloc::format!("{p:08x}"))
        ));
        Ok(Igc { hw, family, name, mac, rx, tx, irq, doorbells: 0 })
    }

    pub fn family(&self) -> Family {
        self.family
    }
}

/// Unmasks (true) or masks MSI-X vector 0 (receive and link): callable without the driver object.
pub fn set_rx_irq(r: &impl Regs, on: bool) {
    r.write32(if on { EIMS } else { EIMC }, 1);
}

/// Clears the "other" causes (link changes), which are not auto-cleared.
pub fn ack(r: &impl Regs, family: Family) -> u32 {
    r.read32(irq_regs(family).icr)
}

impl<H: Hw> Nic for Igc<H> {
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
            self.hw.write32(TDT0, self.tx.tail() as u32);
            self.doorbells += 1;
        }
        if let Some(t) = self.rx.take_tail() {
            core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
            self.hw.write32(RDT0, t as u32);
            self.doorbells += 1;
        }
    }
    fn rx_interrupts(&mut self, on: bool) {
        if self.irq == IrqMode::MsiX {
            set_rx_irq(&self.hw, on);
        }
    }
    fn ack(&mut self) {
        ack(&self.hw, self.family);
    }
    fn link(&mut self) -> Link {
        let s = self.hw.read32(STATUS);
        let up = s & STATUS_LU != 0;
        let mbps = if self.family == Family::I225 && s & STATUS_SPEED_2500 != 0 {
            2500
        } else {
            match (s >> 6) & 3 {
                0 => 10,
                1 => 100,
                _ => 1000,
            }
        };
        Link { up, mbps: if up { mbps } else { 0 }, full_duplex: up && s & STATUS_FD != 0 }
    }
    fn doorbells(&self) -> u64 {
        self.doorbells
    }
}

impl<H: Hw> Drop for Igc<H> {
    fn drop(&mut self) {
        let ir = irq_regs(self.family);
        self.hw.write32(ir.imc, !0);
        self.hw.write32(EIMC, !0);
        self.hw.write32(RCTL, 0);
        self.hw.write32(TCTL, TCTL_PSP);
        self.hw.write32(RXDCTL0, 0);
        self.hw.write32(TXDCTL0, 0);
        let rst = if self.family == Family::I225 { CTRL_DEV_RST } else { CTRL_RST };
        self.hw.write32(CTRL, self.hw.read32(CTRL) | rst);
        self.hw.delay_us(5000);
    }
}
