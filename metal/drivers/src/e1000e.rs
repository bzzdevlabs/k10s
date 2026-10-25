//! Intel e1000e: the 82574L/82583V and the PCH LAN controllers (I217/I218 on Lynx Point, I219 on 100-series and
//! later PCHs). Same MAC registers and legacy descriptors; the PCH parts add a PHY shared with the Management
//! Engine:
//!
//! * The PHY sits behind both SMBus (used by the ME and in low-power states) and the MAC's MDIO interface. Every
//!   PHY access holds the software semaphore (EXTCNF_CTRL.SWFLAG; on the 82574 the same bit is MDIO ownership).
//! * Ultra Low Power (ULP, Lynx Point-LP and later): the PHY may be parked by the firmware or the previous OS. With
//!   a valid ME firmware the driver asks the ME to undo it (H2ME) and waits; without, it toggles LANPHYPC, takes
//!   the PHY out of forced SMBus mode (PHY and MAC side), clears the ULP configuration and resets the PHY.
//! * PHY access check: if the PHY id cannot be read, the MAC is forced into SMBus mode, then LANPHYPC is toggled
//!   (unless the ME blocks PHY resets), then SMBus mode is released again — the handover Linux's driver performs.
//! * K1 (a power state of the MAC–PHY interconnect) is turned off, in the Kumeran register and in the PHY: Linux
//!   turns it off at 1 Gb/s on these PCHs because of packet loss; it stays off here at every speed.
//! * I219 (Sunrise Point and later): if the PCI register DESC_RING_STATUS says a descriptor ring flush is required
//!   (DMA left running), one dummy frame is sent and the receive unit cycled before the reset, else the reset can
//!   hang the DMA engine. The transmit unit gets at most two outstanding read requests (TARC0; Sunrise Point and
//!   Kaby Lake errata: transmit hangs).
//!
//! Then the MAC is reset (with its PHY on PCH parts, unless the ME blocks it), the address read from receive
//! address 0 (loaded from the NVM by the hardware), the PHY powered up with auto-negotiation restarted, the rings
//! set up (legacy descriptors, 2 KiB buffers, CRC stripped, broadcast accepted, short frames padded) and the
//! receive interrupt routed to one MSI message (the 82574's MSI-X is not used; the I219 has MSI only). No checksum
//! or segmentation offload: smoltcp computes and checks checksums.

use crate::ids::Family;
use crate::ring::{Format, Rx, Tx};
use crate::{mac_from, mac_valid, mii, random_mac, wait, Hw, IrqMode, Link, Nic};

pub const CTRL: u32 = 0x0000;
pub const STATUS: u32 = 0x0008;
pub const EECD: u32 = 0x0010;
pub const CTRL_EXT: u32 = 0x0018;
pub const MDIC: u32 = 0x0020;
pub const KMRNCTRLSTA: u32 = 0x0034;
pub const FEXTNVM3: u32 = 0x003C;
pub const ICR: u32 = 0x00C0;
pub const ITR: u32 = 0x00C4;
pub const IMS: u32 = 0x00D0;
pub const IMC: u32 = 0x00D8;
pub const FEXTNVM7: u32 = 0x00E4;
pub const RCTL: u32 = 0x0100;
pub const TCTL: u32 = 0x0400;
pub const TIPG: u32 = 0x0410;
pub const EXTCNF_CTRL: u32 = 0x0F00;
pub const PHY_CTRL: u32 = 0x0F10;
pub const RDBAL: u32 = 0x2800;
pub const RDBAH: u32 = 0x2804;
pub const RDLEN: u32 = 0x2808;
pub const RDH: u32 = 0x2810;
pub const RDT: u32 = 0x2818;
pub const RDTR: u32 = 0x2820;
pub const RXDCTL: u32 = 0x2828;
pub const RADV: u32 = 0x282C;
pub const TDBAL: u32 = 0x3800;
pub const TDBAH: u32 = 0x3804;
pub const TDLEN: u32 = 0x3808;
pub const TDH: u32 = 0x3810;
pub const TDT: u32 = 0x3818;
pub const TXDCTL: u32 = 0x3828;
pub const TARC0: u32 = 0x3840;
pub const MTA: u32 = 0x5200;
pub const RAL0: u32 = 0x5400;
pub const RAH0: u32 = 0x5404;
pub const MANC: u32 = 0x5820;
pub const GCR: u32 = 0x5B00;
pub const H2ME: u32 = 0x5B50;
pub const FWSM: u32 = 0x5B54;
pub const GCR2: u32 = 0x5B64;
/// PCI configuration register: descriptor ring status (I219).
pub const PCI_DESC_RING_STATUS: u16 = 0xE4;
pub const FLUSH_DESC_REQUIRED: u32 = 1 << 8;

pub const CTRL_FD: u32 = 1 << 0;
pub const CTRL_GIO_MASTER_DISABLE: u32 = 1 << 2;
pub const CTRL_SLU: u32 = 1 << 6;
pub const CTRL_SPD_100: u32 = 1 << 8;
pub const CTRL_SPD_1000: u32 = 1 << 9;
pub const CTRL_FRCSPD: u32 = 1 << 11;
pub const CTRL_FRCDPX: u32 = 1 << 12;
pub const CTRL_LANPHYPC_OVERRIDE: u32 = 1 << 16;
pub const CTRL_LANPHYPC_VALUE: u32 = 1 << 17;
pub const CTRL_RST: u32 = 1 << 26;
pub const CTRL_PHY_RST: u32 = 1 << 31;
pub const STATUS_FD: u32 = 1 << 0;
pub const STATUS_LU: u32 = 1 << 1;
pub const STATUS_LAN_INIT_DONE: u32 = 1 << 9;
pub const STATUS_PHYRA: u32 = 1 << 10;
pub const STATUS_GIO_MASTER_EN: u32 = 1 << 19;
pub const EECD_AUTO_RD: u32 = 1 << 9;
pub const CTRL_EXT_LPCD: u32 = 1 << 2;
pub const CTRL_EXT_FORCE_SMBUS: u32 = 1 << 11;
pub const CTRL_EXT_SPD_BYPS: u32 = 1 << 15;
pub const CTRL_EXT_DRV_LOAD: u32 = 1 << 28;
pub const MDIC_OP_WRITE: u32 = 1 << 26;
pub const MDIC_OP_READ: u32 = 2 << 26;
pub const MDIC_READY: u32 = 1 << 28;
pub const MDIC_ERROR: u32 = 1 << 30;
pub const KMRN_REN: u32 = 1 << 21;
pub const KMRN_K1_CONFIG: u32 = 0x7;
pub const KMRN_K1_ENABLE: u16 = 1 << 1;
pub const FEXTNVM3_PHY_CFG_COUNTER_MASK: u32 = 3 << 26;
pub const FEXTNVM3_PHY_CFG_COUNTER_50MS: u32 = 2 << 26;
pub const FEXTNVM7_DISABLE_SMB_PERST: u32 = 1 << 5;
pub const EXTCNF_SWFLAG: u32 = 1 << 5;
pub const PHY_CTRL_D0A_LPLU: u32 = 1 << 1;
pub const PHY_CTRL_NOND0A_LPLU: u32 = 1 << 2;
pub const PHY_CTRL_NOND0A_GBE_DISABLE: u32 = 1 << 3;
pub const PHY_CTRL_GBE_DISABLE: u32 = 1 << 6;
pub const FWSM_RSPCIPHY: u32 = 1 << 6;
pub const FWSM_ULP_CFG_DONE: u32 = 1 << 10;
pub const FWSM_FW_VALID: u32 = 1 << 15;
pub const H2ME_ULP: u32 = 1 << 11;
pub const H2ME_ENFORCE_SETTINGS: u32 = 1 << 12;
pub const MANC_BLK_PHY_RST_ON_IDE: u32 = 1 << 18;
pub const ICR_LSC: u32 = 1 << 2;
pub const ICR_RXDMT0: u32 = 1 << 4;
pub const ICR_RXO: u32 = 1 << 6;
pub const ICR_RXT0: u32 = 1 << 7;
pub const RX_CAUSES: u32 = ICR_RXT0 | ICR_RXO | ICR_RXDMT0;
pub const RCTL_EN: u32 = 1 << 1;
pub const RCTL_BAM: u32 = 1 << 15;
pub const RCTL_SECRC: u32 = 1 << 26;
pub const TCTL_EN: u32 = 1 << 1;
pub const TCTL_PSP: u32 = 1 << 3;
pub const TCTL_CT: u32 = 0x0F << 4;
pub const TCTL_COLD: u32 = 0x3F << 12;
pub const TCTL_RTLC: u32 = 1 << 24;
pub const TCTL_MULR: u32 = 1 << 28;
pub const TXDCTL_COUNT_DESC: u32 = 1 << 22;
pub const TARC0_MULTIQ_REQ_MASK: u32 = 3 << 28;
pub const TARC0_MULTIQ_2_REQ: u32 = 1 << 29;
/// Copper inter-packet gap: IPGT 8, IPGR1 8, IPGR2 6.
pub const TIPG_COPPER: u32 = 8 | 8 << 10 | 6 << 20;

/// PHY registers of the PCH parts' PHY (82577 and later), as (page, register).
pub const CV_SMB_CTRL: (u16, u8) = (769, 23);
pub const CV_SMB_CTRL_FORCE_SMBUS: u16 = 1 << 0;
pub const HV_PM_CTRL: (u16, u8) = (770, 17);
pub const HV_PM_CTRL_K1_ENABLE: u16 = 1 << 14;
pub const ULP_CONFIG1: (u16, u8) = (779, 16);
pub const ULP_CONFIG1_START: u16 = 1 << 0;
/// IND, STICKY_ULP, INBAND_EXIT, WOL_HOST, RESET_TO_SMBUS, EN_ULP_LANPHYPC, DIS_CLR_STICKY_ON_PERST,
/// DISABLE_SMB_PERST.
pub const ULP_CONFIG1_CLEAR: u16 = 1 << 2 | 1 << 4 | 1 << 5 | 1 << 6 | 1 << 8 | 1 << 10 | 1 << 11 | 1 << 12;
const PHY_PAGE_SELECT: u8 = 0x1F;
/// Pages from here on are reached at PHY address 1 (port control, power management); lower ones at address 2.
const HV_PORT_PAGES: u16 = 768;

/// The PCH LAN parts without ULP (desktop Lynx Point): I217-LM/V, I218-LM2/V2.
const NO_ULP: [u16; 4] = [0x153A, 0x153B, 0x15A0, 0x15A1];

pub struct E1000e<H: Hw> {
    hw: H,
    family: Family,
    name: &'static str,
    mac: [u8; 6],
    rx: Rx<H::Mem>,
    tx: Tx<H::Mem>,
    irq: IrqMode,
    doorbells: u64,
}

impl Family {
    fn is_pch(self) -> bool {
        matches!(self, Family::PchLpt | Family::PchSpt | Family::PchCnp)
    }
}

/// Register-level helpers shared by bring-up and the steady state.
struct Chip<'a, H: Hw> {
    hw: &'a H,
    family: Family,
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

    fn me_valid(&self) -> bool {
        self.family.is_pch() && self.r(FWSM) & FWSM_FW_VALID != 0
    }

    /// Whether the firmware forbids resetting the PHY (the ME uses it, or manageability on the 82574).
    fn phy_reset_blocked(&self) -> bool {
        if self.family.is_pch() {
            self.r(FWSM) & FWSM_RSPCIPHY == 0
        } else {
            self.r(MANC) & MANC_BLK_PHY_RST_ON_IDE != 0
        }
    }

    /// The software/firmware semaphore for the PHY and NVM: wait until the firmware releases it, claim it, check
    /// that the claim held.
    fn acquire(&self) -> bool {
        if !wait(self.hw, 1_000_000, 1000, || self.r(EXTCNF_CTRL) & EXTCNF_SWFLAG == 0) {
            self.hw.log(format_args!("e1000e: the PHY semaphore stays held by the firmware"));
            return false;
        }
        self.set(EXTCNF_CTRL, EXTCNF_SWFLAG);
        if !wait(self.hw, 1_000_000, 1000, || self.r(EXTCNF_CTRL) & EXTCNF_SWFLAG != 0) {
            self.clear(EXTCNF_CTRL, EXTCNF_SWFLAG);
            self.hw.log(format_args!("e1000e: the PHY semaphore was refused"));
            return false;
        }
        true
    }
    fn release(&self) {
        self.clear(EXTCNF_CTRL, EXTCNF_SWFLAG);
    }

    fn mdic(&self, phy: u8, reg: u8, write: Option<u16>) -> Option<u16> {
        let op = match write {
            Some(v) => MDIC_OP_WRITE | v as u32,
            None => MDIC_OP_READ,
        };
        self.w(MDIC, op | (reg as u32 & 0x1F) << 16 | (phy as u32 & 0x1F) << 21);
        let mut v = 0;
        if !wait(self.hw, 10_000, 5, || {
            v = self.r(MDIC);
            v & MDIC_READY != 0
        }) || v & MDIC_ERROR != 0
        {
            return None;
        }
        Some(v as u16)
    }

    /// PHY register access (semaphore held): page 0 at the PHY's own address, the port control and power
    /// management pages (768 and above) at address 1 after selecting the page.
    fn phy(&self, (page, reg): (u16, u8), write: Option<u16>) -> Option<u16> {
        if !self.family.is_pch() {
            return self.mdic(1, reg, write);
        }
        let addr = if page >= HV_PORT_PAGES { 1 } else { 2 };
        if page != 0 && reg > 0x0F {
            self.mdic(addr, PHY_PAGE_SELECT, Some(page << 5))?;
        }
        self.mdic(addr, reg, write)
    }
    fn phy_read(&self, r: (u16, u8)) -> Option<u16> {
        self.phy(r, None)
    }
    fn phy_write(&self, r: (u16, u8), v: u16) -> Option<()> {
        self.phy(r, Some(v)).map(|_| ())
    }

    fn phy_id(&self) -> Option<u32> {
        let hi = self.phy_read((0, mii::PHYID1))?;
        let lo = self.phy_read((0, mii::PHYID2))?;
        let id = (hi as u32) << 16 | lo as u32;
        (hi != 0 && hi != 0xFFFF).then_some(id)
    }

    fn kmrn_read(&self, off: u32) -> u16 {
        self.w(KMRNCTRLSTA, (off << 16) & 0x1F_0000 | KMRN_REN);
        self.hw.delay_us(2);
        self.r(KMRNCTRLSTA) as u16
    }
    fn kmrn_write(&self, off: u32, v: u16) {
        self.w(KMRNCTRLSTA, (off << 16) & 0x1F_0000 | v as u32);
        self.hw.delay_us(2);
    }

    /// Power-cycles the PHY through the LANPHYPC pin (which also takes it out of SMBus mode).
    fn toggle_lanphypc(&self) {
        let f = self.r(FEXTNVM3);
        self.w(FEXTNVM3, (f & !FEXTNVM3_PHY_CFG_COUNTER_MASK) | FEXTNVM3_PHY_CFG_COUNTER_50MS);
        let c = self.r(CTRL);
        self.w(CTRL, (c | CTRL_LANPHYPC_OVERRIDE) & !CTRL_LANPHYPC_VALUE);
        self.flush();
        self.hw.delay_us(10);
        self.clear(CTRL, CTRL_LANPHYPC_OVERRIDE);
        self.flush();
        if !wait(self.hw, 100_000, 5000, || self.r(CTRL_EXT) & CTRL_EXT_LPCD != 0) {
            self.hw.log(format_args!("e1000e: LANPHYPC power cycle not reported done"));
        }
        self.hw.delay_us(30_000);
    }

    fn force_smbus(&self, on: bool) {
        if on {
            self.set(CTRL_EXT, CTRL_EXT_FORCE_SMBUS);
            // Lets the MAC finish retries of earlier PHY reads.
            self.hw.delay_us(50_000);
        } else {
            self.clear(CTRL_EXT, CTRL_EXT_FORCE_SMBUS);
        }
    }

    /// Takes the PHY out of Ultra Low Power (Lynx Point-LP and later). Returns false if the ME did not answer.
    fn ulp_exit(&self, device: u16) -> bool {
        if self.family == Family::PchLpt && NO_ULP.contains(&device) {
            return true;
        }
        if self.me_valid() {
            let h = self.r(H2ME);
            self.w(H2ME, (h & !H2ME_ULP) | H2ME_ENFORCE_SETTINGS);
            let ok = wait(self.hw, 2_500_000, 10_000, || self.r(FWSM) & FWSM_ULP_CFG_DONE == 0);
            self.clear(H2ME, H2ME_ENFORCE_SETTINGS);
            if !ok {
                self.hw.log(format_args!("e1000e: the ME did not take the PHY out of ULP"));
            }
            return ok;
        }
        if !self.acquire() {
            return false;
        }
        self.toggle_lanphypc();
        let smb = match self.phy_read(CV_SMB_CTRL) {
            Some(v) => Some(v),
            None => {
                // The MAC may be in PCIe mode while the PHY is on SMBus: force the MAC to SMBus to reach it.
                self.force_smbus(true);
                self.phy_read(CV_SMB_CTRL)
            }
        };
        let mut ok = false;
        if let Some(v) = smb {
            ok = self.phy_write(CV_SMB_CTRL, v & !CV_SMB_CTRL_FORCE_SMBUS).is_some();
            self.force_smbus(false);
            if let Some(pm) = self.phy_read(HV_PM_CTRL) {
                self.phy_write(HV_PM_CTRL, pm | HV_PM_CTRL_K1_ENABLE);
            }
            if let Some(u) = self.phy_read(ULP_CONFIG1) {
                let u = u & !ULP_CONFIG1_CLEAR;
                ok &= self.phy_write(ULP_CONFIG1, u).is_some() && self.phy_write(ULP_CONFIG1, u | ULP_CONFIG1_START).is_some();
            }
            self.clear(FEXTNVM7, FEXTNVM7_DISABLE_SMB_PERST);
        } else {
            self.hw.log(format_args!("e1000e: PHY unreachable while leaving ULP"));
        }
        self.release();
        if !self.phy_reset_blocked() {
            self.phy_reset();
        }
        self.hw.delay_us(50_000);
        ok
    }

    /// Makes the PCH PHY reachable over MDIO: the SMBus/MDIO handover (see the module docs). Semaphore held.
    fn phy_handover(&self) -> bool {
        if self.phy_id().is_some() {
            return self.unforce_smbus();
        }
        self.force_smbus(true);
        if self.phy_id().is_some() {
            return self.unforce_smbus();
        }
        if self.phy_reset_blocked() {
            self.hw.log(format_args!("e1000e: PHY unreachable and its reset is blocked by the ME"));
            return false;
        }
        self.toggle_lanphypc();
        if self.phy_id().is_some() {
            return self.unforce_smbus();
        }
        self.force_smbus(false);
        self.phy_id().is_some() && self.unforce_smbus()
    }

    /// With the PHY reachable and no ME firmware: neither side forced into SMBus mode.
    fn unforce_smbus(&self) -> bool {
        if !self.me_valid() {
            if let Some(v) = self.phy_read(CV_SMB_CTRL) {
                self.phy_write(CV_SMB_CTRL, v & !CV_SMB_CTRL_FORCE_SMBUS);
            }
            self.force_smbus(false);
        }
        true
    }

    /// A PHY reset pulse, then (PCH) the hardware's reload of the PHY configuration from the NVM.
    fn phy_reset(&self) {
        if !self.acquire() {
            return;
        }
        self.set(CTRL, CTRL_PHY_RST);
        self.flush();
        self.hw.delay_us(100);
        self.clear(CTRL, CTRL_PHY_RST);
        self.flush();
        self.hw.delay_us(150);
        self.release();
        self.lan_init_done();
    }

    fn lan_init_done(&self) {
        if !self.family.is_pch() {
            self.hw.delay_us(10_000);
            return;
        }
        if !wait(self.hw, 150_000, 100, || self.r(STATUS) & STATUS_LAN_INIT_DONE != 0) {
            self.hw.log(format_args!("e1000e: PHY configuration (LAN_INIT_DONE) not reported"));
        }
        self.clear(STATUS, STATUS_LAN_INIT_DONE | STATUS_PHYRA);
        self.hw.delay_us(10_000);
    }

    /// K1 off (module docs). Semaphore held.
    fn k1_off(&self) {
        let k = self.kmrn_read(KMRN_K1_CONFIG);
        self.kmrn_write(KMRN_K1_CONFIG, k & !KMRN_K1_ENABLE);
        self.hw.delay_us(20);
        // The new K1 setting takes effect through a brief forced-speed bypass.
        let ctrl = self.r(CTRL);
        let ext = self.r(CTRL_EXT);
        self.w(CTRL, (ctrl & !(CTRL_SPD_1000 | CTRL_SPD_100)) | CTRL_FRCSPD);
        self.w(CTRL_EXT, ext | CTRL_EXT_SPD_BYPS);
        self.flush();
        self.hw.delay_us(20);
        self.w(CTRL, ctrl);
        self.w(CTRL_EXT, ext);
        self.flush();
        self.hw.delay_us(20);
        if let Some(v) = self.phy_read(HV_PM_CTRL) {
            self.phy_write(HV_PM_CTRL, v & !HV_PM_CTRL_K1_ENABLE);
        }
    }

    /// Stops DMA: interrupts masked, receive and transmit off, bus mastering requests drained.
    fn quiesce(&self) {
        self.w(IMC, !0);
        self.w(RCTL, 0);
        self.w(TCTL, TCTL_PSP);
        self.flush();
        self.hw.delay_us(10_000);
        self.set(CTRL, CTRL_GIO_MASTER_DISABLE);
        if !wait(self.hw, 80_000, 100, || self.r(STATUS) & STATUS_GIO_MASTER_EN == 0) {
            self.hw.log(format_args!("e1000e: bus master requests still pending before the reset"));
        }
    }

    /// The I219's descriptor ring flush (module docs), with a throwaway ring that the caller keeps until the reset.
    fn flush_rings(&self) -> Option<Tx<H::Mem>> {
        if self.hw.config_read32(PCI_DESC_RING_STATUS & !3) & FLUSH_DESC_REQUIRED == 0 || self.r(TDLEN) == 0 {
            return None;
        }
        self.hw.log(format_args!("e1000e: flushing the descriptor rings the firmware left running"));
        let mut t = Tx::new(self.hw, Format::IntelLegacy, 8)?;
        self.w(TDBAL, t.ring_paddr() as u32);
        self.w(TDBAH, (t.ring_paddr() >> 32) as u32);
        self.w(TDLEN, t.ring_bytes());
        self.w(TDH, 0);
        self.w(TDT, 0);
        self.set(TCTL, TCTL_EN);
        t.queue_flush_frame(512);
        self.w(TDT, t.tail() as u32);
        self.flush();
        self.hw.delay_us(250);
        if self.hw.config_read32(PCI_DESC_RING_STATUS & !3) & FLUSH_DESC_REQUIRED != 0 {
            let rctl = self.r(RCTL);
            self.w(RCTL, rctl & !RCTL_EN);
            self.flush();
            self.hw.delay_us(150);
            let d = self.r(RXDCTL);
            // Prefetch threshold 31, host threshold 1, in descriptor units.
            self.w(RXDCTL, (d & 0xFFFF_C000) | 0x1F | 1 << 8 | 1 << 24);
            self.w(RCTL, rctl | RCTL_EN);
            self.flush();
            self.hw.delay_us(150);
            self.w(RCTL, rctl & !RCTL_EN);
        }
        self.w(TCTL, TCTL_PSP);
        self.flush();
        self.hw.delay_us(10_000);
        Some(t)
    }

    /// Resets the MAC (and the PHY with it on PCH parts when allowed), waits for the NVM and PHY loads.
    fn reset(&self) {
        let phy_too = self.family.is_pch() && !self.phy_reset_blocked();
        // The semaphore is held across the reset, which clears it.
        let locked = self.acquire();
        self.set(CTRL, CTRL_RST | if phy_too { CTRL_PHY_RST } else { 0 });
        self.hw.delay_us(20_000);
        if !wait(self.hw, 100_000, 1000, || self.r(CTRL) & CTRL_RST == 0) {
            self.hw.log(format_args!("e1000e: reset did not complete"));
        }
        if locked {
            self.release();
        }
        if !self.family.is_pch() && !wait(self.hw, 10_000, 1000, || self.r(EECD) & EECD_AUTO_RD != 0) {
            self.hw.log(format_args!("e1000e: NVM auto-read not reported done"));
        }
        if phy_too {
            self.lan_init_done();
        } else {
            self.hw.delay_us(10_000);
        }
        self.w(IMC, !0);
        self.r(ICR);
    }
}

impl<H: Hw> E1000e<H> {
    /// Brings up the controller with `nbufs` receive and transmit buffers. `irq`: how the kernel routed its
    /// interrupt (`MsiX` is treated as MSI: one vector for every cause).
    pub fn new(hw: H, device: u16, nbufs: u16, irq: IrqMode) -> Result<E1000e<H>, &'static str> {
        let id = crate::ids::nic(crate::ids::INTEL, device).ok_or("not an e1000e device")?;
        let family = id.family;
        let name = match family {
            Family::I82574 => "e1000e (82574)",
            Family::PchLpt => "e1000e (I217/I218)",
            _ => "e1000e (I219)",
        };
        let c = Chip { hw: &hw, family };
        if c.r(STATUS) == !0 {
            return Err("registers read as all ones (device absent or powered down)");
        }
        c.quiesce();
        let flush_ring = if matches!(family, Family::PchSpt | Family::PchCnp) { c.flush_rings() } else { None };
        if family.is_pch() {
            c.ulp_exit(device);
            if c.acquire() {
                let ok = c.phy_handover();
                c.release();
                if !ok {
                    return Err("PHY unreachable over MDIO (ULP/SMBus handover failed)");
                }
            } else {
                return Err("PHY semaphore unavailable");
            }
        }
        c.reset();
        drop(flush_ring);
        hw.bus_master();
        c.set(CTRL_EXT, CTRL_EXT_DRV_LOAD);
        if family.is_pch() {
            // Gigabit and full power in D0: the firmware may have left low-power link settings for a sleep state.
            c.clear(PHY_CTRL, PHY_CTRL_GBE_DISABLE | PHY_CTRL_D0A_LPLU | PHY_CTRL_NOND0A_LPLU | PHY_CTRL_NOND0A_GBE_DISABLE);
        }
        if !c.acquire() {
            return Err("PHY semaphore unavailable");
        }
        if family.is_pch() {
            c.k1_off();
        }
        let phy = c.phy_id();
        let bmcr = c.phy_read((0, mii::BMCR));
        if let Some(b) = bmcr {
            c.phy_write((0, mii::BMCR), mii::autoneg(b));
        }
        c.release();
        if bmcr.is_none() {
            return Err("PHY control register unreadable");
        }

        let mut mac = mac_from(c.r(RAL0), c.r(RAH0));
        if !mac_valid(&mac) {
            mac = random_mac(&hw);
            hw.log(format_args!("e1000e: no address in the NVM: using a random one"));
        }
        c.w(RAL0, u32::from_le_bytes([mac[0], mac[1], mac[2], mac[3]]));
        c.w(RAH0, u16::from_le_bytes([mac[4], mac[5]]) as u32 | 1 << 31);
        for i in 0..128 {
            c.w(MTA + 4 * i, 0);
        }

        // Errata settings.
        c.set(TXDCTL, TXDCTL_COUNT_DESC);
        match family {
            Family::I82574 => {
                // PCIe completion handling (82574 specification update; transmit timeouts with ASPM).
                c.set(GCR, 1 << 22);
                c.set(GCR2, 1);
            }
            Family::PchSpt | Family::PchCnp => {
                let t = c.r(TARC0);
                c.w(TARC0, (t & !TARC0_MULTIQ_REQ_MASK) | TARC0_MULTIQ_2_REQ);
            }
            _ => {}
        }
        c.w(CTRL, (c.r(CTRL) | CTRL_SLU) & !(CTRL_FRCSPD | CTRL_FRCDPX));

        let rx = Rx::new(&hw, Format::IntelLegacy, nbufs).ok_or("no memory for the receive ring")?;
        let tx = Tx::new(&hw, Format::IntelLegacy, nbufs).ok_or("no memory for the transmit ring")?;
        c.w(RDBAL, rx.ring_paddr() as u32);
        c.w(RDBAH, (rx.ring_paddr() >> 32) as u32);
        c.w(RDLEN, rx.ring_bytes());
        c.w(RDH, 0);
        c.w(RDT, 0);
        // Interrupt as soon as a frame is written back (no moderation timers: the kernel masks them while busy).
        c.w(RDTR, 0);
        c.w(RADV, 0);
        c.w(ITR, 0);
        c.w(TDBAL, tx.ring_paddr() as u32);
        c.w(TDBAH, (tx.ring_paddr() >> 32) as u32);
        c.w(TDLEN, tx.ring_bytes());
        c.w(TDH, 0);
        c.w(TDT, 0);
        c.w(TIPG, TIPG_COPPER);
        let mulr = if family.is_pch() { TCTL_MULR } else { 0 };
        c.w(TCTL, TCTL_EN | TCTL_PSP | TCTL_CT | TCTL_COLD | TCTL_RTLC | mulr);
        c.w(RCTL, RCTL_EN | RCTL_BAM | RCTL_SECRC);
        c.w(RDT, rx.tail() as u32);
        if irq != IrqMode::Polled {
            c.w(IMS, ICR_LSC);
        }
        let model = id.model;
        hw.log(format_args!(
            "e1000e: {model} {:04x}:{:04x}, PHY id {}, {}",
            crate::ids::INTEL,
            device,
            phy.map_or(alloc::string::String::from("unknown"), |p| alloc::format!("{p:08x}")),
            if c.me_valid() { "ME firmware present" } else { "no ME firmware" }
        ));
        Ok(E1000e { hw, family, name, mac, rx, tx, irq, doorbells: 0 })
    }

    pub fn family(&self) -> Family {
        self.family
    }
}

/// Unmasks (true) or masks the receive interrupt causes: callable without the driver object (interrupt handlers).
pub fn set_rx_irq(r: &impl crate::Regs, on: bool) {
    r.write32(if on { IMS } else { IMC }, RX_CAUSES);
}

/// Reads (and so clears) the interrupt causes: the MSI is raised again only for new causes.
pub fn ack(r: &impl crate::Regs) -> u32 {
    r.read32(ICR)
}

impl<H: Hw> Nic for E1000e<H> {
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
            self.hw.write32(TDT, self.tx.tail() as u32);
            self.doorbells += 1;
        }
        if let Some(t) = self.rx.take_tail() {
            core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
            self.hw.write32(RDT, t as u32);
            self.doorbells += 1;
        }
    }
    fn rx_interrupts(&mut self, on: bool) {
        if self.irq != IrqMode::Polled {
            set_rx_irq(&self.hw, on);
        }
    }
    fn ack(&mut self) {
        ack(&self.hw);
    }
    fn link(&mut self) -> Link {
        let s = self.hw.read32(STATUS);
        let up = s & STATUS_LU != 0;
        let mbps = match (s >> 6) & 3 {
            0 => 10,
            1 => 100,
            _ => 1000,
        };
        Link { up, mbps: if up { mbps } else { 0 }, full_duplex: up && s & STATUS_FD != 0 }
    }
    fn doorbells(&self) -> u64 {
        self.doorbells
    }
}

impl<H: Hw> Drop for E1000e<H> {
    fn drop(&mut self) {
        // Stop DMA before the rings' memory is freed.
        self.hw.write32(IMC, !0);
        self.hw.write32(RCTL, 0);
        self.hw.write32(TCTL, TCTL_PSP);
        self.hw.write32(CTRL, self.hw.read32(CTRL) | CTRL_RST);
        self.hw.delay_us(20_000);
    }
}
