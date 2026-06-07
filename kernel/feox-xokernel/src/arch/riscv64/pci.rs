//! Memory-mapped (ECAM) PCIe configuration access and device discovery.
//!
//! riscv64 has no x86-style config ports; PCIe configuration space is reached
//! through the ECAM (Enhanced Configuration Access Mechanism) MMIO window. The
//! config address for `(bus, device, function, offset)` is:
//!
//! ```text
//! ECAM_BASE + (bus << 20) | (device << 15) | (function << 12) | offset
//! ```
//!
//! Milestone 6a scope: enumerate the root bus and find the NVMe controller,
//! reading its identity from config space. Controller register (BAR) access,
//! BAR assignment, and the admin/IO queues land in 6b. The ECAM window must be
//! mapped before use (see `paging::build_kernel_address_space`).

use core::ptr::read_volatile;

/// QEMU virt PCIe ECAM base (`pci-host-ecam-generic`).
///
/// Hardcoded for the QEMU virt target; on real hardware (e.g. the Orange Pi
/// RV) this is parsed from the device tree's PCIe host-bridge node.
pub const ECAM_BASE: usize = 0x3000_0000;
/// ECAM window size on QEMU virt (256 buses x 1 MiB).
pub const ECAM_SIZE: usize = 0x1000_0000;

/// NVMe class code: mass storage (0x01) / NVM Express (0x08) / prog-if 0x02.
pub const CLASS_NVME: u32 = 0x01_0802;

/// Sentinel vendor id returned by ECAM for an absent function.
const VENDOR_NONE: u16 = 0xFFFF;

/// One PCIe function discovered on the bus.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PciDevice {
    /// Bus number.
    pub bus: u8,
    /// Device (slot) number, 0..32.
    pub device: u8,
    /// Function number, 0..8.
    pub function: u8,
    /// Vendor id.
    pub vendor_id: u16,
    /// Device id.
    pub device_id: u16,
    /// 24-bit class code: `(class << 16) | (subclass << 8) | prog_if`.
    pub class_code: u32,
}

/// Computes the ECAM MMIO address for a config-space dword.
fn ecam_addr(bus: u8, device: u8, function: u8, offset: u16) -> usize {
    ECAM_BASE
        + ((bus as usize) << 20)
        + ((device as usize) << 15)
        + ((function as usize) << 12)
        + offset as usize
}

/// Reads a 32-bit config-space register (offset must be 4-byte aligned).
fn config_read32(bus: u8, device: u8, function: u8, offset: u16) -> u32 {
    // SAFETY: the ECAM window is mapped R/W as device memory by the kernel
    // address space; every computed address lies within it, and config reads
    // are side-effect-free.
    unsafe { read_volatile(ecam_addr(bus, device, function, offset) as *const u32) }
}

/// Scans the root bus for the first function whose 24-bit class code matches
/// `target` (e.g. [`CLASS_NVME`]). QEMU virt exposes a single root bus.
#[must_use]
pub fn scan_for_class(target: u32) -> Option<PciDevice> {
    for device in 0..32u8 {
        for function in 0..8u8 {
            let id = config_read32(0, device, function, 0x00);
            let vendor = (id & 0xFFFF) as u16;
            if vendor == VENDOR_NONE {
                continue;
            }
            let class_word = config_read32(0, device, function, 0x08);
            let class_code = (class_word >> 8) & 0x00FF_FFFF;
            if class_code == target {
                return Some(PciDevice {
                    bus: 0,
                    device,
                    function,
                    vendor_id: vendor,
                    device_id: (id >> 16) as u16,
                    class_code,
                });
            }
        }
    }
    None
}

/// Reads the raw 32-bit BAR register at `index` (0..6) for a device. A value of
/// 0 (after masking the low type bits) means the BAR is unassigned — firmware
/// did not program it (expected when booting OpenSBI -> kernel with no UEFI/
/// U-Boot PCI enumeration), so the kernel must assign it before use.
#[must_use]
pub fn bar_raw(device: &PciDevice, index: u8) -> u32 {
    config_read32(device.bus, device.device, device.function, 0x10 + u16::from(index) * 4)
}
