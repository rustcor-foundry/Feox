//! AP boot v1: place a tiny real-mode trampoline at the
//! loader-allocated sub-1-MiB frame, send INIT-SIPI-SIPI to one AP
//! through the LAPIC, and observe the AP running by polling a magic
//! word the trampoline writes before halting.
//!
//! Scope: validates the LAPIC IPI plumbing + trampoline placement +
//! SIPI vector decoding. The trampoline does NOT transition into
//! protected/long mode and does NOT enter Rust on the AP. Reaching
//! a Rust `ap_entry` is the next focus.

use crate::lapic::{self, LapicError};
use crate::memory::{PAGE_SIZE, PhysicalAddress};
use crate::mmio::{BootstrapMmioError, MmioRegion, mmio_map_bootstrap, mmio_unmap_bootstrap};

/// Byte offset within the trampoline frame at which the AP writes
/// [`AP_ALIVE_MAGIC`] before halting. Kept distinct from the code
/// (which lives at offset 0) so the BSP can clear it before SIPI and
/// poll it after.
const MAGIC_OFFSET: usize = 0xFF0;

/// Magic word the trampoline writes when it executes. Picking a
/// recognizable byte pattern keeps debugging easy.
const AP_ALIVE_MAGIC: u16 = 0xCAFE;

/// Hand-assembled 16-bit real-mode trampoline.
///
/// ```text
///     [bits 16]
///     [org 0]
///     cli                          ; FA
///     mov ax, cs                   ; 8C C8
///     mov ds, ax                   ; 8E D8
///     mov word [0xFF0], 0xCAFE     ; C7 06 F0 0F FE CA
///     hlt                          ; F4
/// hang:
///     jmp hang                     ; EB FE
/// ```
///
/// CS is set by SIPI to `vector << 8`, so `mov ds, cs` lets the
/// trampoline reach [`MAGIC_OFFSET`] via the segment base it was
/// loaded under without depending on a fixed absolute phys.
const TRAMPOLINE_CODE: [u8; 14] = [
    0xFA, // cli
    0x8C, 0xC8, // mov ax, cs
    0x8E, 0xD8, // mov ds, ax
    0xC7, 0x06, 0xF0, 0x0F, 0xFE, 0xCA, // mov word [0xFF0], 0xCAFE
    0xF4, // hlt
    0xEB, 0xFE, // jmp $
];

/// Errors returned by the AP boot driver.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SmpError {
    /// The loader did not allocate a sub-1-MiB trampoline frame.
    NoTrampolineFrame,
    /// The trampoline frame is not 4 KiB aligned, or is at or above 1
    /// MiB, so the SIPI vector cannot reach it.
    InvalidTrampolinePhys,
    /// MMIO mapping for the trampoline frame failed.
    TrampolineMapFailed,
    /// LAPIC layer rejected an IPI.
    Lapic(LapicError),
    /// The trampoline never wrote [`AP_ALIVE_MAGIC`] within the
    /// observation window.
    ApNotAlive,
}

impl From<BootstrapMmioError> for SmpError {
    fn from(_: BootstrapMmioError) -> Self {
        Self::TrampolineMapFailed
    }
}

impl From<LapicError> for SmpError {
    fn from(err: LapicError) -> Self {
        Self::Lapic(err)
    }
}

/// Bring up a single AP by sending INIT-SIPI-SIPI and watching for
/// the trampoline's magic word.
///
/// Returns `Ok(())` once the AP's write becomes visible. Diagnostic
/// trampoline placement details and timings live in
/// `docs/SMP_BRINGUP.md` once that doc is written.
pub fn bring_up_first_ap(
    trampoline_phys: u64,
    target_apic_id: u8,
) -> Result<(), SmpError> {
    if trampoline_phys == 0 {
        return Err(SmpError::NoTrampolineFrame);
    }
    if trampoline_phys % PAGE_SIZE != 0 || trampoline_phys >= 0x10_0000 {
        return Err(SmpError::InvalidTrampolinePhys);
    }

    let region = TrampolineMap::new(trampoline_phys)?;

    // Write the trampoline code + clear the magic word.
    unsafe {
        // SAFETY: `region` is a freshly mapped 4 KiB writable kernel
        // alias; nothing else points at it.
        let dst = region.as_mut_ptr();
        core::ptr::copy_nonoverlapping(TRAMPOLINE_CODE.as_ptr(), dst, TRAMPOLINE_CODE.len());
        core::ptr::write_volatile(dst.add(MAGIC_OFFSET).cast::<u16>(), 0);
    }

    // INIT IPI, then per Intel's recommendation wait ~10 ms before
    // the first SIPI and ~200 µs between SIPIs. We don't have a
    // calibrated timer yet so spin counts are conservative for QEMU.
    lapic::send_init(target_apic_id)?;
    lapic::spin_delay(10_000_000);

    let vector = (trampoline_phys >> 12) as u8;
    lapic::send_startup(target_apic_id, vector)?;
    lapic::spin_delay(200_000);
    lapic::send_startup(target_apic_id, vector)?;

    // Poll for the AP to write the magic word.
    let alive = poll_for_magic(&region, 5_000_000);

    if alive {
        Ok(())
    } else {
        Err(SmpError::ApNotAlive)
    }
}

fn poll_for_magic(region: &TrampolineMap, poll_iterations: u64) -> bool {
    for _ in 0..poll_iterations {
        let magic = unsafe {
            // SAFETY: see `TrampolineMap::as_mut_ptr`.
            core::ptr::read_volatile(region.as_ptr().add(MAGIC_OFFSET).cast::<u16>())
        };
        if magic == AP_ALIVE_MAGIC {
            return true;
        }
        core::hint::spin_loop();
    }
    false
}

/// RAII wrapper that aliases the AP trampoline frame as a writable
/// cacheable kernel mapping and unmaps it on drop.
struct TrampolineMap {
    region: MmioRegion,
}

impl TrampolineMap {
    fn new(phys: u64) -> Result<Self, SmpError> {
        let region = mmio_map_bootstrap(
            PhysicalAddress::new(phys),
            PAGE_SIZE,
            true,
            false, // cacheable: this is RAM the loader set aside
        )?;
        Ok(Self { region })
    }

    fn as_ptr(&self) -> *const u8 {
        self.region.virtual_base as *const u8
    }

    fn as_mut_ptr(&self) -> *mut u8 {
        self.region.virtual_base as *mut u8
    }
}

impl Drop for TrampolineMap {
    fn drop(&mut self) {
        let _ = mmio_unmap_bootstrap(self.region);
    }
}

#[cfg(test)]
mod tests {
    use super::{AP_ALIVE_MAGIC, MAGIC_OFFSET, TRAMPOLINE_CODE};

    #[test]
    fn trampoline_byte_layout_is_frozen() {
        // Sanity-check the assembled bytes against the comment in
        // the source. Catches accidental edits to the array literal
        // before they reach a kernel boot.
        assert_eq!(TRAMPOLINE_CODE.len(), 14);
        assert_eq!(TRAMPOLINE_CODE[0], 0xFA); // cli
        assert_eq!(TRAMPOLINE_CODE[11], 0xF4); // hlt
        assert_eq!(TRAMPOLINE_CODE[12..14], [0xEB, 0xFE]); // jmp $
    }

    #[test]
    fn magic_offset_outside_code() {
        assert!(MAGIC_OFFSET > TRAMPOLINE_CODE.len());
        assert!(MAGIC_OFFSET + 2 <= 4096);
    }

    #[test]
    fn ap_alive_magic_is_recognizable() {
        // 0xCAFE was chosen as a distinctive byte pattern; a value
        // of 0 would alias the cleared-state and break detection.
        assert_ne!(AP_ALIVE_MAGIC, 0);
    }
}
