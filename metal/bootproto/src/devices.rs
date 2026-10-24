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
    // virtio-blk transitional and modern-only.
    Device { vendor: 0x1af4, device: 0x1001, class: Class::Storage, driver: "virtio-blk" },
    Device { vendor: 0x1af4, device: 0x1042, class: Class::Storage, driver: "virtio-blk (modern)" },
];

/// The table's entry for a PCI vendor and device id.
pub fn find(vendor: u16, device: u16) -> Option<&'static Device> {
    SUPPORTED.iter().find(|d| d.vendor == vendor && d.device == device)
}
