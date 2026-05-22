//! Per-core kernel data area.
//!
//! Each logical core owns a small fixed-layout struct used by
//! kernel-internal code that needs cheap "who am I" / "what's my
//! current task" answers. The struct is reached via a GS-relative load
//! (`mov rax, gs:[0]`) on x86_64 — `IA32_GS_BASE` holds the kernel
//! virtual address of the current core's [`PerCoreData`].
//!
//! ## Current scope (single-core bring-up)
//!
//! Only core 0 is wired up. The per-core area lives at the page's
//! direct-map alias (`DIRECT_MAP_BASE + phys`); the locked virtual
//! slot at [`crate::memory::PER_CORE_BASE`] (`0xFFFF_E000_0000_0000`)
//! is still reserved but not yet explicitly mapped. The direct-map
//! alias is a stable kernel-only address, so accessor code is
//! unaffected — only the eventual "all cores observe per-CPU at the
//! same VA stride" property is deferred.

use core::sync::atomic::{AtomicU64, Ordering};

use feox_asi::{CapHandle, CapPermissions, CapRequest, PageFlags};

use crate::capability::{request_bootstrap_capability, resource, verify_bootstrap_handle};
use crate::memory::DIRECT_MAP_BASE;

/// Magic value used to validate a per-core area was initialized
/// through [`initialize_core0`] and not (e.g.) wandered into by
/// pointer corruption.
pub const PER_CORE_MAGIC: u64 = 0xFE0C_0DEF_ACEC_0FE1;

/// Per-core kernel data area, layout-frozen for GS-relative access.
///
/// `self_ptr` is at offset 0 so `mov rax, gs:[0]` materializes the
/// area's own kernel virtual address — the canonical entry point for
/// [`current`].
#[derive(Debug)]
#[repr(C)]
pub struct PerCoreData {
    /// Pointer to this struct (its own kernel virtual address). Used
    /// by [`current`] to materialize a reference via `gs:[0]`.
    pub self_ptr: *const PerCoreData,
    /// [`PER_CORE_MAGIC`] when the area has been initialized.
    pub magic: u64,
    /// Logical core identifier (matches `feox_asi::CoreId`).
    pub core_id: u32,
    /// Reserved for future fields (current task pointer, IST tops,
    /// scheduler hooks, ...).
    pub _reserved: u32,
}

// SAFETY: `PerCoreData` is owned per-core; the raw pointer field
// names the struct's own kernel virtual address and is never shared
// across cores.
unsafe impl Send for PerCoreData {}
unsafe impl Sync for PerCoreData {}

#[cfg(target_os = "none")]
const IA32_GS_BASE: u32 = 0xC000_0101;

/// Kernel-private record of the live per-core area for core 0 so the
/// boot self-test can compare what it reads through GS against what
/// initialization installed.
static CORE0_BASE: AtomicU64 = AtomicU64::new(0);

/// Initializes the per-core data area for core 0.
///
/// Allocates one 4 KiB physical page via the bootstrap capability
/// layer, writes a [`PerCoreData`] at its direct-map alias, and loads
/// the alias address into `IA32_GS_BASE` so subsequent kernel code can
/// read its own per-core block via [`current`].
///
/// Returns the capability handle backing the page so the caller can
/// release it during shutdown (release lifecycle is a follow-up; the
/// page leaks for the rest of the boot today).
pub fn initialize_core0() -> Result<CapHandle, &'static str> {
    if CORE0_BASE.load(Ordering::Acquire) != 0 {
        return Err("per_core: core 0 already initialized");
    }

    let handle = request_bootstrap_capability(&CapRequest::PhysicalPages {
        num_pages: 1,
        flags: PageFlags::CONTIGUOUS,
    })
    .map_err(|_| "per_core: PhysicalPages cap request failed")?;

    let view = verify_bootstrap_handle(handle, CapPermissions::READ | CapPermissions::WRITE)
        .map_err(|_| "per_core: cap verify failed")?;
    let res =
        resource(view.resource_id).ok_or("per_core: resource lookup failed for fresh cap")?;
    let phys = res.base.0;
    let virt = DIRECT_MAP_BASE.wrapping_add(phys);

    let area = virt as *mut PerCoreData;
    unsafe {
        // SAFETY: virt aliases the freshly allocated page (kernel-only
        // direct map); nothing else holds a pointer to it.
        core::ptr::write(
            area,
            PerCoreData {
                self_ptr: area as *const PerCoreData,
                magic: PER_CORE_MAGIC,
                core_id: 0,
                _reserved: 0,
            },
        );
    }

    #[cfg(target_os = "none")]
    unsafe {
        // SAFETY: writing IA32_GS_BASE only affects this core's GS
        // base address; it does not invalidate any held references.
        crate::arch::x86_64::cpu::wrmsr(IA32_GS_BASE, virt);
    }

    CORE0_BASE.store(virt, Ordering::Release);
    Ok(handle)
}

/// Returns a reference to the current core's per-core data area.
///
/// On bare metal, materializes the reference via a GS-relative load
/// of the self-pointer at offset 0. On host tests there is no GS
/// base, so this falls back to the recorded core-0 address.
///
/// # Panics
///
/// Panics if [`initialize_core0`] has not been called.
#[cfg(target_os = "none")]
pub fn current() -> &'static PerCoreData {
    let ptr: *const PerCoreData;
    unsafe {
        // SAFETY: GS_BASE was set by `initialize_core0`; the loaded
        // self-pointer names a live `PerCoreData` whose lifetime
        // extends for the rest of the boot.
        core::arch::asm!(
            "mov {0}, gs:[0]",
            out(reg) ptr,
            options(readonly, nostack, preserves_flags),
        );
        assert!(!ptr.is_null(), "per_core::current called before initialize_core0");
        &*ptr
    }
}

/// Host-test version of [`current`] that reads the recorded base
/// without touching GS (no GS_BASE outside of QEMU).
#[cfg(not(target_os = "none"))]
pub fn current() -> &'static PerCoreData {
    let base = CORE0_BASE.load(Ordering::Acquire);
    assert!(base != 0, "per_core::current called before initialize_core0");
    unsafe {
        // SAFETY: `CORE0_BASE` is set by `initialize_core0` to a
        // kernel-direct-map alias of a live PerCoreData; the reference
        // lives for the rest of the test.
        &*(base as *const PerCoreData)
    }
}

/// Convenience accessor returning the current core's logical id.
#[must_use]
pub fn core_id() -> u32 {
    current().core_id
}

#[cfg(test)]
mod tests {
    use super::{PerCoreData, PER_CORE_MAGIC};
    use core::mem::{offset_of, size_of};

    #[test]
    fn per_core_data_layout_is_frozen() {
        // self_ptr at offset 0 is required for `gs:[0]` to load the
        // self-pointer.
        assert_eq!(offset_of!(PerCoreData, self_ptr), 0);
        // 8 (self_ptr) + 8 (magic) + 4 (core_id) + 4 (_reserved) = 24
        assert_eq!(size_of::<PerCoreData>(), 24);
    }

    #[test]
    fn magic_is_non_zero() {
        // A zero magic would not distinguish initialized state from a
        // zeroed page.
        assert_ne!(PER_CORE_MAGIC, 0);
    }
}
