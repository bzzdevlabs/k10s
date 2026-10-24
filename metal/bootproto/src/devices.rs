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
    // virtio-net with its legacy interface (transitional: QEMU's virtio-net-pci unless disable-legacy=on).
    Device { vendor: 0x1af4, device: 0x1000, class: Class::Network, driver: "virtio-net (legacy)" },
];

/// The table's entry for a PCI vendor and device id.
pub fn find(vendor: u16, device: u16) -> Option<&'static Device> {
    SUPPORTED.iter().find(|d| d.vendor == vendor && d.device == device)
}
