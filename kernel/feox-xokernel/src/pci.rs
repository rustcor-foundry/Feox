//! Minimal PCI enumeration for bootstrap device discovery.
//!
//! Walks the legacy 256-bus / 32-device / 8-function space using the
//! architecture-provided 32-bit configuration-space reader. Sufficient
//! for finding a single controller on the bus (e.g. NVMe) and reading
//! its BAR; deeper PCIe enumeration (capabilities, MSI/MSI-X, hotplug)
//! is intentionally not handled here.

/// One PCI function discovered by [`scan_for_class`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PciDevice {
    /// PCI bus number.
    pub bus: u8,
    /// PCI device number (0..32).
    pub device: u8,
    /// PCI function number (0..8).
    pub function: u8,
    /// PCI vendor ID.
    pub vendor_id: u16,
    /// PCI device ID.
    pub device_id: u16,
    /// 24-bit class code: `(class << 16) | (subclass << 8) | prog_if`.
    pub class_code: u32,
}

/// Walks the PCI configuration space and returns the first function whose
/// 24-bit class code matches `target`, or `None` if no such function is
/// present.
///
/// `target` is encoded as `(class << 16) | (subclass << 8) | prog_if`.
/// NVMe controllers report `0x010802`.
#[must_use]
#[cfg(target_os = "none")]
pub fn scan_for_class(target: u32) -> Option<PciDevice> {
    use crate::arch::x86_64::pci;

    let mut bus: u16 = 0;
    while bus < 256 {
        let mut device: u8 = 0;
        while device < 32 {
            let mut function: u8 = 0;
            while function < 8 {
                let vid_did = pci::config_read32(bus as u8, device, function, 0x00);
                let vendor = (vid_did & 0xFFFF) as u16;
                if vendor != 0xFFFF {
                    let class_word = pci::config_read32(bus as u8, device, function, 0x08);
                    let class_code = (class_word >> 8) & 0x00FF_FFFF;
                    if class_code == target {
                        return Some(PciDevice {
                            bus: bus as u8,
                            device,
                            function,
                            vendor_id: vendor,
                            device_id: (vid_did >> 16) as u16,
                            class_code,
                        });
                    }
                }
                function += 1;
            }
            device += 1;
        }
        bus += 1;
    }
    None
}

#[cfg(not(target_os = "none"))]
#[must_use]
pub fn scan_for_class(_target: u32) -> Option<PciDevice> {
    None
}

/// Reads a 64-bit BAR pair starting at `bar_index` (0..5). The low 4 bits
/// of the BAR are stripped, so the returned value is the page-aligned base
/// physical address of the memory region. Callers that need to know
/// whether the BAR is 32-bit, 64-bit, prefetchable, or I/O space must
/// inspect those bits separately.
#[must_use]
#[cfg(target_os = "none")]
pub fn bar64(device: &PciDevice, bar_index: u8) -> u64 {
    use crate::arch::x86_64::pci;

    debug_assert!(bar_index <= 4, "BAR index out of range for 64-bit pair");
    let offset = 0x10 + bar_index * 4;
    let low = pci::config_read32(device.bus, device.device, device.function, offset);
    let high = pci::config_read32(device.bus, device.device, device.function, offset + 4);
    (u64::from(high) << 32) | (u64::from(low) & !0xF_u64)
}

#[cfg(not(target_os = "none"))]
#[must_use]
pub fn bar64(_device: &PciDevice, _bar_index: u8) -> u64 {
    0
}

/// PCI class code constant for NVMe storage controllers
/// (class 0x01 mass storage, subclass 0x08 NVMe, prog-if 0x02).
pub const PCI_CLASS_NVME: u32 = 0x0001_0802;
