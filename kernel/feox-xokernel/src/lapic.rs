//! Minimal Local APIC driver.
//!
//! Maps the LAPIC MMIO region (typically `0xFEE0_0000`) through the
//! MMIO bring-up lane, exposes basic register access, and provides
//! the INIT / Startup IPI sequence used to bring secondary processors
//! online.

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::memory::{PAGE_SIZE, PhysicalAddress};
use crate::mmio::{BootstrapMmioError, mmio_map_bootstrap};

/// Local APIC register offsets (from the MMIO base).
const REG_ID: usize = 0x020;
const REG_VERSION: usize = 0x030;
const REG_ICR_LOW: usize = 0x300;
const REG_ICR_HIGH: usize = 0x310;

/// Delivery status bit in ICR low (1 = send pending, 0 = idle).
const ICR_DELIVERY_PENDING: u32 = 1 << 12;

/// Delivery mode encodings (bits 10:8 of ICR low).
const DELIVERY_MODE_INIT: u32 = 5 << 8;
const DELIVERY_MODE_STARTUP: u32 = 6 << 8;

/// Level bit (14): 1 = assert.
const LEVEL_ASSERT: u32 = 1 << 14;

/// Maximum number of times to poll ICR delivery status before giving
/// up. At a few CPU cycles per read this is plenty for QEMU; bare
/// metal may need to tune higher.
const DELIVERY_POLL_LIMIT: u32 = 1_000_000;

/// Errors returned by the LAPIC driver.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LapicError {
    /// [`initialize`] has not yet been called.
    NotInitialized,
    /// The MMIO mapping for the LAPIC base failed.
    MmioMapFailed,
    /// `delivery status` did not clear within [`DELIVERY_POLL_LIMIT`]
    /// polls after an IPI write.
    DeliveryTimeout,
}

impl From<BootstrapMmioError> for LapicError {
    fn from(_: BootstrapMmioError) -> Self {
        Self::MmioMapFailed
    }
}

/// LAPIC MMIO virtual base, set by [`initialize`]. Zero means
/// uninitialized.
static LAPIC_VIRT_BASE: AtomicUsize = AtomicUsize::new(0);

/// LAPIC MMIO physical base, recorded for debug introspection.
static LAPIC_PHYS_BASE: AtomicU64 = AtomicU64::new(0);

/// Initializes the LAPIC driver by mapping the MMIO base.
///
/// `phys_base` is the controller's physical address as reported by
/// the ACPI MADT (`local_apic_address`). One 4 KiB MMIO mapping is
/// enough to reach every register used by this driver.
pub fn initialize(phys_base: u64) -> Result<(), LapicError> {
    if LAPIC_VIRT_BASE.load(Ordering::Acquire) != 0 {
        return Ok(());
    }
    let region = mmio_map_bootstrap(
        PhysicalAddress::new(phys_base),
        PAGE_SIZE,
        true,
        true, // uncached: LAPIC is strictly device memory
    )?;
    LAPIC_VIRT_BASE.store(region.virtual_base as usize, Ordering::Release);
    LAPIC_PHYS_BASE.store(phys_base, Ordering::Release);
    Ok(())
}

/// Returns the LAPIC MMIO physical base passed to [`initialize`].
#[must_use]
pub fn physical_base() -> u64 {
    LAPIC_PHYS_BASE.load(Ordering::Acquire)
}

fn virt_base() -> Result<usize, LapicError> {
    let base = LAPIC_VIRT_BASE.load(Ordering::Acquire);
    if base == 0 {
        Err(LapicError::NotInitialized)
    } else {
        Ok(base)
    }
}

fn read_reg(offset: usize) -> Result<u32, LapicError> {
    let base = virt_base()?;
    unsafe {
        // SAFETY: `initialize` mapped a 4 KiB MMIO range, so any
        // offset inside that page is a valid read.
        Ok(core::ptr::read_volatile((base + offset) as *const u32))
    }
}

fn write_reg(offset: usize, value: u32) -> Result<(), LapicError> {
    let base = virt_base()?;
    unsafe {
        // SAFETY: as in `read_reg`.
        core::ptr::write_volatile((base + offset) as *mut u32, value);
    }
    Ok(())
}

/// Reads the LAPIC ID register. The high 8 bits encode the
/// processor's APIC ID.
pub fn read_id() -> Result<u8, LapicError> {
    Ok((read_reg(REG_ID)? >> 24) as u8)
}

/// Reads the LAPIC version register.
pub fn read_version() -> Result<u32, LapicError> {
    read_reg(REG_VERSION)
}

fn await_delivery_idle() -> Result<(), LapicError> {
    for _ in 0..DELIVERY_POLL_LIMIT {
        if (read_reg(REG_ICR_LOW)? & ICR_DELIVERY_PENDING) == 0 {
            return Ok(());
        }
        core::hint::spin_loop();
    }
    Err(LapicError::DeliveryTimeout)
}

fn send_ipi(dst_apic_id: u8, icr_low: u32) -> Result<(), LapicError> {
    // Write the destination first (high half) and then the command
    // (low half) — the write to ICR_LOW triggers the send.
    write_reg(REG_ICR_HIGH, (dst_apic_id as u32) << 24)?;
    write_reg(REG_ICR_LOW, icr_low)?;
    await_delivery_idle()
}

/// Sends an INIT IPI (assert) to `dst_apic_id`.
///
/// Drives the AP into a reset-like state, ready to receive a
/// subsequent Startup IPI. Modern hardware tolerates skipping the
/// de-assert phase, which keeps this driver minimal.
pub fn send_init(dst_apic_id: u8) -> Result<(), LapicError> {
    send_ipi(dst_apic_id, DELIVERY_MODE_INIT | LEVEL_ASSERT)
}

/// Sends a Startup IPI carrying `vector`. The AP begins executing
/// at physical address `vector << 12`.
pub fn send_startup(dst_apic_id: u8, vector: u8) -> Result<(), LapicError> {
    send_ipi(
        dst_apic_id,
        DELIVERY_MODE_STARTUP | LEVEL_ASSERT | (vector as u32),
    )
}

/// Crude spin delay. Used between INIT and SIPI / between SIPIs to
/// satisfy the Intel-recommended timing windows (10 ms after INIT,
/// 200 µs between SIPIs). Without a calibrated timer we fall back
/// on a busy loop; the count is conservative for QEMU + modern
/// hardware.
pub fn spin_delay(busy_iterations: u64) {
    let mut i = 0_u64;
    while i < busy_iterations {
        core::hint::spin_loop();
        i = i.wrapping_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::DELIVERY_POLL_LIMIT;

    #[test]
    fn delivery_poll_limit_is_non_trivial() {
        // Sanity: the poll loop has to spin long enough for QEMU
        // to settle the IPI but not so long it visibly stalls boot.
        assert!(DELIVERY_POLL_LIMIT >= 1000);
    }
}
