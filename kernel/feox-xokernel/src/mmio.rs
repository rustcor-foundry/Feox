//! Bootstrap-scoped MMIO mapping helpers.
//!
//! These install kernel-only mappings inside the permanent MMIO zone at
//! [`crate::memory::MMIO_BASE`]. Device subsystems use these to alias
//! their BARs into kernel virtual space with the correct caching
//! attributes. There is no syscall ABI yet — this is an internal kernel
//! API. The first device drivers (NVMe, etc.) call it directly.

use crate::memory::{
    self, MMIO_BASE, MMIO_PREBUILT_SIZE, PAGE_SIZE, PhysicalAddress, PhysicalFrame, VirtualAddress,
};
use crate::paging::{Map4kError, PageTableEntry, PageTableFrameAllocator, PageTableRoot};
use crate::runtime_context::{self, BootstrapMmioMapping};

/// Errors returned by [`mmio_map_bootstrap`] and [`mmio_unmap_bootstrap`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BootstrapMmioError {
    /// `length_bytes` was zero or not a multiple of [`PAGE_SIZE`].
    AlignmentViolation,
    /// `physical_base` was not page-aligned.
    UnalignedPhysical,
    /// No virtual range of the requested size was available inside the
    /// prebuilt MMIO sub-window.
    OutOfVirtualSpace,
    /// The retained MMIO mapping table is full.
    OutOfMappingSlots,
    /// A paging-layer error occurred while installing or removing the
    /// leaf mapping.
    Paging(Map4kError),
    /// `mmio_unmap_bootstrap` was called with a region that did not
    /// correspond to an active mapping.
    MappingNotFound,
}

impl From<Map4kError> for BootstrapMmioError {
    fn from(value: Map4kError) -> Self {
        Self::Paging(value)
    }
}

/// A live MMIO mapping handed back to the caller.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MmioRegion {
    /// Virtual base inside the MMIO zone.
    pub virtual_base: u64,
    /// Physical base aliased by the mapping.
    pub physical_base: u64,
    /// Mapping length in bytes (page-aligned).
    pub length_bytes: u64,
    /// True if the mapping was installed with cache-disable.
    pub uncached: bool,
    /// True if the mapping was installed writable.
    pub writable: bool,
}

/// Maps a kernel-only MMIO region.
///
/// The mapping is installed inside the prebuilt MMIO sub-window starting at
/// [`MMIO_BASE`]. Pages are non-executable. Pass `uncached = true` for
/// device MMIO (the typical case) so accesses bypass the data cache;
/// `uncached = false` is appropriate only for prefetchable, coherent
/// device ranges.
///
/// # Errors
///
/// Returns [`BootstrapMmioError`] when alignment, capacity, or paging
/// constraints are violated.
pub fn mmio_map_bootstrap(
    physical_base: PhysicalAddress,
    length_bytes: u64,
    writable: bool,
    uncached: bool,
) -> Result<MmioRegion, BootstrapMmioError> {
    if length_bytes == 0 || length_bytes % PAGE_SIZE != 0 {
        return Err(BootstrapMmioError::AlignmentViolation);
    }
    let phys = physical_base.as_u64();
    if phys % PAGE_SIZE != 0 {
        return Err(BootstrapMmioError::UnalignedPhysical);
    }

    let virtual_base =
        runtime_context::allocate_mmio_range(MMIO_BASE, MMIO_PREBUILT_SIZE, length_bytes)
            .ok_or(BootstrapMmioError::OutOfVirtualSpace)?;

    #[cfg(target_os = "none")]
    {
        use crate::paging::DirectMapPageTables;

        let root = PageTableRoot::active();
        let mut live = DirectMapPageTables;
        struct NoopAllocator;
        impl PageTableFrameAllocator for NoopAllocator {
            fn allocate_table_frame(&mut self) -> Option<PhysicalFrame> {
                None
            }
        }
        let mut allocator = NoopAllocator;

        let mut flags = PageTableEntry::FLAG_NO_EXECUTE;
        if writable {
            flags |= 1_u64 << 1;
        }
        if uncached {
            flags |= PageTableEntry::FLAG_CACHE_DISABLE | PageTableEntry::FLAG_WRITE_THROUGH;
        }

        let mut offset = 0_u64;
        while offset < length_bytes {
            let va = VirtualAddress::new(virtual_base + offset);
            let pa = PhysicalFrame::containing(PhysicalAddress::new(phys + offset));
            root.map_4k_with(&mut live, &mut allocator, va, pa, flags)?;
            offset += PAGE_SIZE;
        }
    }
    #[cfg(not(target_os = "none"))]
    {
        let _ = (writable, uncached);
    }

    let mapping = BootstrapMmioMapping {
        virtual_base,
        physical_base: phys,
        length_bytes,
        uncached,
        writable,
    };
    runtime_context::record_mmio_mapping(mapping)
        .map_err(|_| BootstrapMmioError::OutOfMappingSlots)?;
    runtime_context::push_event("bootstrap-mmio-map");

    Ok(MmioRegion {
        virtual_base,
        physical_base: phys,
        length_bytes,
        uncached,
        writable,
    })
}

/// Removes an active MMIO mapping returned by [`mmio_map_bootstrap`].
///
/// # Errors
///
/// Returns [`BootstrapMmioError::MappingNotFound`] when the region does
/// not match an active mapping, or a paging error if the live unmap
/// fails. The bump cursor is *not* rewound — the virtual range stays
/// reserved until the runtime gains a real freelist.
pub fn mmio_unmap_bootstrap(region: MmioRegion) -> Result<(), BootstrapMmioError> {
    let _mapping = runtime_context::remove_mmio_mapping(region.virtual_base)
        .ok_or(BootstrapMmioError::MappingNotFound)?;

    #[cfg(target_os = "none")]
    {
        use crate::paging::DirectMapPageTables;

        let root = PageTableRoot::active();
        let mut live = DirectMapPageTables;
        let mut offset = 0_u64;
        while offset < region.length_bytes {
            let va = VirtualAddress::new(region.virtual_base + offset);
            let _ = root
                .unmap_4k_with(&mut live, va)
                .map_err(|_| BootstrapMmioError::MappingNotFound)?;
            offset += PAGE_SIZE;
        }
    }
    let _ = memory::PAGE_SIZE; // keep memory import alive in host build
    runtime_context::push_event("bootstrap-mmio-unmap");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{BootstrapMmioError, mmio_map_bootstrap, mmio_unmap_bootstrap};
    use crate::capability::acquire_test_lock;
    use crate::memory::{PAGE_SIZE, PhysicalAddress};
    use crate::runtime_context::{claim_bootstrap_context, mmio_mappings, reset_for_tests};
    use feox_asi::CoreId;

    #[test]
    fn mmio_rejects_zero_length() {
        let _guard = acquire_test_lock();
        reset_for_tests();
        claim_bootstrap_context(CoreId(0));
        assert_eq!(
            mmio_map_bootstrap(PhysicalAddress::new(0xFEE0_0000), 0, false, true),
            Err(BootstrapMmioError::AlignmentViolation)
        );
    }

    #[test]
    fn mmio_rejects_unaligned_length() {
        let _guard = acquire_test_lock();
        reset_for_tests();
        claim_bootstrap_context(CoreId(0));
        assert_eq!(
            mmio_map_bootstrap(PhysicalAddress::new(0xFEE0_0000), PAGE_SIZE - 1, false, true),
            Err(BootstrapMmioError::AlignmentViolation)
        );
    }

    #[test]
    fn mmio_rejects_unaligned_physical() {
        let _guard = acquire_test_lock();
        reset_for_tests();
        claim_bootstrap_context(CoreId(0));
        assert_eq!(
            mmio_map_bootstrap(PhysicalAddress::new(0xFEE0_0001), PAGE_SIZE, false, true),
            Err(BootstrapMmioError::UnalignedPhysical)
        );
    }

    #[test]
    fn mmio_records_and_releases_a_mapping() {
        let _guard = acquire_test_lock();
        reset_for_tests();
        claim_bootstrap_context(CoreId(0));
        let region = mmio_map_bootstrap(PhysicalAddress::new(0xFEE0_0000), PAGE_SIZE, true, true)
            .expect("mmio map should succeed");
        assert_eq!(region.physical_base, 0xFEE0_0000);
        assert_eq!(region.length_bytes, PAGE_SIZE);
        assert_eq!(mmio_mappings().iter().flatten().count(), 1);
        mmio_unmap_bootstrap(region).expect("mmio unmap should succeed");
        assert_eq!(mmio_mappings().iter().flatten().count(), 0);
    }
}
