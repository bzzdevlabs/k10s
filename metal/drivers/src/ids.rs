//! The Ethernet controllers these drivers support, by PCI id (metal/bootproto/src/devices.rs lists the same ids for
//! k10boot's `tier=auto`; a test keeps the two in step).

pub const INTEL: u16 = 0x8086;
pub const REALTEK: u16 = 0x10EC;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Family {
    /// 82574L / 82583V: the e1000e MAC with its own PHY.
    I82574,
    /// I217/I218 (Lynx Point PCH LAN).
    PchLpt,
    /// I219 on the 100/200-series PCHs (Sunrise Point, Kaby Lake, Lewisburg).
    PchSpt,
    /// I219 on 300-series PCHs (Cannon Point) and later.
    PchCnp,
    /// 82576 (igb).
    I82576,
    /// I350 (igb).
    I350,
    /// I210 / I211 (igb).
    I210,
    /// I225 / I226 (igc).
    I225,
    /// RTL8111/8168 and RTL8411 (r8169).
    Rtl8168,
    /// RTL8125 2.5 GbE (r8169).
    Rtl8125,
}

#[derive(Clone, Copy, Debug)]
pub struct NicId {
    pub vendor: u16,
    pub device: u16,
    pub family: Family,
    pub model: &'static str,
}

const fn i(device: u16, family: Family, model: &'static str) -> NicId {
    NicId { vendor: INTEL, device, family, model }
}
const fn r(device: u16, family: Family, model: &'static str) -> NicId {
    NicId { vendor: REALTEK, device, family, model }
}

use Family::*;

pub const NICS: &[NicId] = &[
    i(0x10D3, I82574, "82574L"),
    i(0x150C, I82574, "82583V"),
    i(0x153A, PchLpt, "I217-LM"),
    i(0x153B, PchLpt, "I217-V"),
    i(0x155A, PchLpt, "I218-LM"),
    i(0x1559, PchLpt, "I218-V"),
    i(0x15A0, PchLpt, "I218-LM"),
    i(0x15A1, PchLpt, "I218-V"),
    i(0x15A2, PchLpt, "I218-LM"),
    i(0x15A3, PchLpt, "I218-V"),
    i(0x156F, PchSpt, "I219-LM"),
    i(0x1570, PchSpt, "I219-V"),
    i(0x15B7, PchSpt, "I219-LM"),
    i(0x15B8, PchSpt, "I219-V"),
    i(0x15B9, PchSpt, "I219-LM"),
    i(0x15D7, PchSpt, "I219-LM"),
    i(0x15D8, PchSpt, "I219-V"),
    i(0x15E3, PchSpt, "I219-LM"),
    i(0x15D6, PchSpt, "I219-V"),
    i(0x15BD, PchCnp, "I219-LM"),
    i(0x15BE, PchCnp, "I219-V"),
    i(0x15BB, PchCnp, "I219-LM"),
    i(0x15BC, PchCnp, "I219-V"),
    i(0x15DF, PchCnp, "I219-LM"),
    i(0x15E0, PchCnp, "I219-V"),
    i(0x15E1, PchCnp, "I219-LM"),
    i(0x15E2, PchCnp, "I219-V"),
    i(0x0D4E, PchCnp, "I219-LM"),
    i(0x0D4F, PchCnp, "I219-V"),
    i(0x0D4C, PchCnp, "I219-LM"),
    i(0x0D4D, PchCnp, "I219-V"),
    i(0x0D53, PchCnp, "I219-LM"),
    i(0x0D55, PchCnp, "I219-V"),
    i(0x15FB, PchCnp, "I219-LM"),
    i(0x15FC, PchCnp, "I219-V"),
    i(0x15F9, PchCnp, "I219-LM"),
    i(0x15FA, PchCnp, "I219-V"),
    i(0x15F4, PchCnp, "I219-LM"),
    i(0x15F5, PchCnp, "I219-V"),
    i(0x1A1E, PchCnp, "I219-LM"),
    i(0x1A1F, PchCnp, "I219-V"),
    i(0x1A1C, PchCnp, "I219-LM"),
    i(0x1A1D, PchCnp, "I219-V"),
    i(0x550A, PchCnp, "I219-LM"),
    i(0x550B, PchCnp, "I219-V"),
    i(0x550C, PchCnp, "I219-LM"),
    i(0x550D, PchCnp, "I219-V"),
    i(0x550E, PchCnp, "I219-LM"),
    i(0x550F, PchCnp, "I219-V"),
    i(0x5510, PchCnp, "I219-LM"),
    i(0x5511, PchCnp, "I219-V"),
    i(0x0DC7, PchCnp, "I219-LM"),
    i(0x0DC8, PchCnp, "I219-V"),
    i(0x0DC5, PchCnp, "I219-LM"),
    i(0x0DC6, PchCnp, "I219-V"),
    i(0x10C9, I82576, "82576"),
    i(0x1521, I350, "I350"),
    i(0x1533, I210, "I210"),
    i(0x157B, I210, "I210"),
    i(0x1539, I210, "I211"),
    i(0x15F2, I225, "I225-LM"),
    i(0x15F3, I225, "I225-V"),
    i(0x15F8, I225, "I225-I"),
    i(0x15F7, I225, "I220-V"),
    i(0x0D9F, I225, "I225-IT"),
    i(0x3100, I225, "I225-K"),
    i(0x3101, I225, "I225-K"),
    i(0x125B, I225, "I226-LM"),
    i(0x125C, I225, "I226-V"),
    i(0x125D, I225, "I226-IT"),
    i(0x125E, I225, "I221-V"),
    i(0x3102, I225, "I226-K"),
    r(0x8168, Rtl8168, "RTL8111/8168"),
    r(0x8161, Rtl8168, "RTL8111/8168"),
    r(0x8125, Rtl8125, "RTL8125"),
    r(0x3000, Rtl8125, "RTL8125BG"),
];

pub fn nic(vendor: u16, device: u16) -> Option<&'static NicId> {
    NICS.iter().find(|n| n.vendor == vendor && n.device == device)
}

impl Family {
    /// The Linux driver of the family (the name k10boot's `tier=auto` reports).
    pub fn driver(self) -> &'static str {
        match self {
            I82574 | PchLpt | PchSpt | PchCnp => "e1000e",
            I82576 | I350 | I210 => "igb",
            I225 => "igc",
            Rtl8168 | Rtl8125 => "r8169",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique() {
        for (a, x) in NICS.iter().enumerate() {
            assert!(NICS[a + 1..].iter().all(|y| (y.vendor, y.device) != (x.vendor, x.device)), "{:04x}:{:04x} twice", x.vendor, x.device);
        }
    }

    #[test]
    fn k10boot_table_lists_every_nic() {
        for n in NICS {
            let d = k10_bootproto::devices::find(n.vendor, n.device)
                .unwrap_or_else(|| panic!("{:04x}:{:04x} ({}) missing from metal/bootproto/src/devices.rs", n.vendor, n.device, n.model));
            assert_eq!(d.class, k10_bootproto::devices::Class::Network);
            assert_eq!(d.driver, n.family.driver());
        }
    }
}
