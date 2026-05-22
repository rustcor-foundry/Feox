//! Legacy x86 PCI configuration space access via the 0xCF8/0xCFC port pair.
//!
//! Each access writes the target address to `0xCF8` then reads or writes
//! `0xCFC` as a 32-bit value. PCI enumeration code in the kernel uses
//! [`config_read32`] to walk the bus/device/function space.

use super::cpu;

const CONFIG_ADDRESS_PORT: u16 = 0xCF8;
const CONFIG_DATA_PORT: u16 = 0xCFC;

/// Reads a 32-bit word from PCI configuration space.
///
/// `offset` is the byte offset within the device's 256-byte configuration
/// header. Bits 0–1 of `offset` are ignored because legacy PCI config
/// accesses are naturally 32-bit aligned.
#[must_use]
pub fn config_read32(bus: u8, device: u8, function: u8, offset: u8) -> u32 {
    debug_assert!(device < 32, "PCI device {device} out of range");
    debug_assert!(function < 8, "PCI function {function} out of range");
    let address: u32 = 0x8000_0000
        | (u32::from(bus) << 16)
        | (u32::from(device) << 11)
        | (u32::from(function) << 8)
        | u32::from(offset & 0xFC);
    unsafe {
        // SAFETY: writing to 0xCF8 selects the next PCI config target; the
        // subsequent read from 0xCFC observes only that target's word.
        cpu::outl(CONFIG_ADDRESS_PORT, address);
        cpu::inl(CONFIG_DATA_PORT)
    }
}
