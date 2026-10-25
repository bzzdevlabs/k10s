//! The PCI devices the native kernel drives: the one table that k10boot's `tier=auto` matches the machine against
//! (metal/README.md, "Tiers"). A driver that lands in the kernel adds its devices here, nowhere else.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Class {
    Network,
    Storage,
}

#[derive(Clone, Copy, Debug)]
pub struct Device {
    pub vendor: u16,
    pub device: u16,
    pub class: Class,
    pub driver: &'static str,
}

pub const SUPPORTED: &[Device] = &[
    // virtio-net transitional (QEMU's virtio-net-pci on a root bus; driven as virtio 1.x when it offers the modern
    // capabilities, else legacy) and modern-only (behind a PCIe port, Hetzner, Vultr, Cloud Hypervisor).
    Device { vendor: 0x1af4, device: 0x1000, class: Class::Network, driver: "virtio-net" },
    Device { vendor: 0x1af4, device: 0x1041, class: Class::Network, driver: "virtio-net (modern)" },
    // Intel e1000e: 82574L/82583V, I217/I218 (Lynx Point), I219 (100-series PCH and later).
    Device { vendor: 0x8086, device: 0x10d3, class: Class::Network, driver: "e1000e" },
    Device { vendor: 0x8086, device: 0x150c, class: Class::Network, driver: "e1000e" },
    Device { vendor: 0x8086, device: 0x153a, class: Class::Network, driver: "e1000e" },
    Device { vendor: 0x8086, device: 0x153b, class: Class::Network, driver: "e1000e" },
    Device { vendor: 0x8086, device: 0x155a, class: Class::Network, driver: "e1000e" },
    Device { vendor: 0x8086, device: 0x1559, class: Class::Network, driver: "e1000e" },
    Device { vendor: 0x8086, device: 0x15a0, class: Class::Network, driver: "e1000e" },
    Device { vendor: 0x8086, device: 0x15a1, class: Class::Network, driver: "e1000e" },
    Device { vendor: 0x8086, device: 0x15a2, class: Class::Network, driver: "e1000e" },
    Device { vendor: 0x8086, device: 0x15a3, class: Class::Network, driver: "e1000e" },
    Device { vendor: 0x8086, device: 0x156f, class: Class::Network, driver: "e1000e" },
    Device { vendor: 0x8086, device: 0x1570, class: Class::Network, driver: "e1000e" },
    Device { vendor: 0x8086, device: 0x15b7, class: Class::Network, driver: "e1000e" },
    Device { vendor: 0x8086, device: 0x15b8, class: Class::Network, driver: "e1000e" },
    Device { vendor: 0x8086, device: 0x15b9, class: Class::Network, driver: "e1000e" },
    Device { vendor: 0x8086, device: 0x15d7, class: Class::Network, driver: "e1000e" },
    Device { vendor: 0x8086, device: 0x15d8, class: Class::Network, driver: "e1000e" },
    Device { vendor: 0x8086, device: 0x15e3, class: Class::Network, driver: "e1000e" },
    Device { vendor: 0x8086, device: 0x15d6, class: Class::Network, driver: "e1000e" },
    Device { vendor: 0x8086, device: 0x15bd, class: Class::Network, driver: "e1000e" },
    Device { vendor: 0x8086, device: 0x15be, class: Class::Network, driver: "e1000e" },
    Device { vendor: 0x8086, device: 0x15bb, class: Class::Network, driver: "e1000e" },
    Device { vendor: 0x8086, device: 0x15bc, class: Class::Network, driver: "e1000e" },
    Device { vendor: 0x8086, device: 0x15df, class: Class::Network, driver: "e1000e" },
    Device { vendor: 0x8086, device: 0x15e0, class: Class::Network, driver: "e1000e" },
    Device { vendor: 0x8086, device: 0x15e1, class: Class::Network, driver: "e1000e" },
    Device { vendor: 0x8086, device: 0x15e2, class: Class::Network, driver: "e1000e" },
    Device { vendor: 0x8086, device: 0x0d4e, class: Class::Network, driver: "e1000e" },
    Device { vendor: 0x8086, device: 0x0d4f, class: Class::Network, driver: "e1000e" },
    Device { vendor: 0x8086, device: 0x0d4c, class: Class::Network, driver: "e1000e" },
    Device { vendor: 0x8086, device: 0x0d4d, class: Class::Network, driver: "e1000e" },
    Device { vendor: 0x8086, device: 0x0d53, class: Class::Network, driver: "e1000e" },
    Device { vendor: 0x8086, device: 0x0d55, class: Class::Network, driver: "e1000e" },
    Device { vendor: 0x8086, device: 0x15fb, class: Class::Network, driver: "e1000e" },
    Device { vendor: 0x8086, device: 0x15fc, class: Class::Network, driver: "e1000e" },
    Device { vendor: 0x8086, device: 0x15f9, class: Class::Network, driver: "e1000e" },
    Device { vendor: 0x8086, device: 0x15fa, class: Class::Network, driver: "e1000e" },
    Device { vendor: 0x8086, device: 0x15f4, class: Class::Network, driver: "e1000e" },
    Device { vendor: 0x8086, device: 0x15f5, class: Class::Network, driver: "e1000e" },
    Device { vendor: 0x8086, device: 0x1a1e, class: Class::Network, driver: "e1000e" },
    Device { vendor: 0x8086, device: 0x1a1f, class: Class::Network, driver: "e1000e" },
    Device { vendor: 0x8086, device: 0x1a1c, class: Class::Network, driver: "e1000e" },
    Device { vendor: 0x8086, device: 0x1a1d, class: Class::Network, driver: "e1000e" },
    Device { vendor: 0x8086, device: 0x550a, class: Class::Network, driver: "e1000e" },
    Device { vendor: 0x8086, device: 0x550b, class: Class::Network, driver: "e1000e" },
    Device { vendor: 0x8086, device: 0x550c, class: Class::Network, driver: "e1000e" },
    Device { vendor: 0x8086, device: 0x550d, class: Class::Network, driver: "e1000e" },
    Device { vendor: 0x8086, device: 0x550e, class: Class::Network, driver: "e1000e" },
    Device { vendor: 0x8086, device: 0x550f, class: Class::Network, driver: "e1000e" },
    Device { vendor: 0x8086, device: 0x5510, class: Class::Network, driver: "e1000e" },
    Device { vendor: 0x8086, device: 0x5511, class: Class::Network, driver: "e1000e" },
    Device { vendor: 0x8086, device: 0x0dc7, class: Class::Network, driver: "e1000e" },
    Device { vendor: 0x8086, device: 0x0dc8, class: Class::Network, driver: "e1000e" },
    Device { vendor: 0x8086, device: 0x0dc5, class: Class::Network, driver: "e1000e" },
    Device { vendor: 0x8086, device: 0x0dc6, class: Class::Network, driver: "e1000e" },
    // Intel igb: 82576 (QEMU's igb), I350, I210/I211.
    Device { vendor: 0x8086, device: 0x10c9, class: Class::Network, driver: "igb" },
    Device { vendor: 0x8086, device: 0x1521, class: Class::Network, driver: "igb" },
    Device { vendor: 0x8086, device: 0x1533, class: Class::Network, driver: "igb" },
    Device { vendor: 0x8086, device: 0x157b, class: Class::Network, driver: "igb" },
    Device { vendor: 0x8086, device: 0x1539, class: Class::Network, driver: "igb" },
    // Intel igc: I225/I226 (and I220-V/I221-V).
    Device { vendor: 0x8086, device: 0x15f2, class: Class::Network, driver: "igc" },
    Device { vendor: 0x8086, device: 0x15f3, class: Class::Network, driver: "igc" },
    Device { vendor: 0x8086, device: 0x15f8, class: Class::Network, driver: "igc" },
    Device { vendor: 0x8086, device: 0x15f7, class: Class::Network, driver: "igc" },
    Device { vendor: 0x8086, device: 0x0d9f, class: Class::Network, driver: "igc" },
    Device { vendor: 0x8086, device: 0x3100, class: Class::Network, driver: "igc" },
    Device { vendor: 0x8086, device: 0x3101, class: Class::Network, driver: "igc" },
    Device { vendor: 0x8086, device: 0x125b, class: Class::Network, driver: "igc" },
    Device { vendor: 0x8086, device: 0x125c, class: Class::Network, driver: "igc" },
    Device { vendor: 0x8086, device: 0x125d, class: Class::Network, driver: "igc" },
    Device { vendor: 0x8086, device: 0x125e, class: Class::Network, driver: "igc" },
    Device { vendor: 0x8086, device: 0x3102, class: Class::Network, driver: "igc" },
    // Realtek r8169-class: RTL8111/8168/8411 and RTL8125.
    Device { vendor: 0x10ec, device: 0x8168, class: Class::Network, driver: "r8169" },
    Device { vendor: 0x10ec, device: 0x8161, class: Class::Network, driver: "r8169" },
    Device { vendor: 0x10ec, device: 0x8125, class: Class::Network, driver: "r8169" },
    Device { vendor: 0x10ec, device: 0x3000, class: Class::Network, driver: "r8169" },
    // virtio-blk transitional and modern-only.
    Device { vendor: 0x1af4, device: 0x1001, class: Class::Storage, driver: "virtio-blk" },
    Device { vendor: 0x1af4, device: 0x1042, class: Class::Storage, driver: "virtio-blk (modern)" },
];

/// The table's entry for a PCI vendor and device id.
pub fn find(vendor: u16, device: u16) -> Option<&'static Device> {
    SUPPORTED.iter().find(|d| d.vendor == vendor && d.device == device)
}

/// Controllers driven by their PCI class code (class, subclass, programming interface), whatever the vendor.
#[derive(Clone, Copy, Debug)]
pub struct ByClass {
    pub class: u32,
    pub kind: Class,
    pub driver: &'static str,
}

pub const CLASSES: &[ByClass] = &[
    ByClass { class: 0x01_08_02, kind: Class::Storage, driver: "nvme" },
    ByClass { class: 0x01_06_01, kind: Class::Storage, driver: "ahci" },
];

/// The driver of a storage controller (ids, then class code `class`: class << 16 | subclass << 8 | prog-if).
pub fn storage(vendor: u16, device: u16, class: u32) -> Option<&'static str> {
    find(vendor, device).filter(|d| d.class == Class::Storage).map(|d| d.driver).or_else(|| {
        CLASSES.iter().find(|c| c.class == class && c.kind == Class::Storage).map(|c| c.driver)
    })
}
