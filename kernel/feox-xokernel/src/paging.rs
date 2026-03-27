//! Early x86-first paging ownership helpers.

use crate::memory::{
    active_page_table_root, BootMemoryMap, EarlyKernelReservations, FrameAllocator, PhysicalFrame,
    ReservationKind,
};

/// Wrapper for a top-level page-table root frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PageTableRoot {
    frame: PhysicalFrame,
}

impl PageTableRoot {
    /// Wraps an existing page-table root frame.
    #[must_use]
    pub const fn new(frame: PhysicalFrame) -> Self {
        Self { frame }
    }

    /// Returns the currently active top-level page-table root.
    #[must_use]
    pub fn active() -> Self {
        Self::new(active_page_table_root())
    }

    /// Returns the backing physical frame for this root.
    #[must_use]
    pub const fn frame(self) -> PhysicalFrame {
        self.frame
    }
}

/// Bootstrap allocator for new paging-structure frames.
///
/// This is intentionally tiny: it only hands out 4 KiB frames and records
/// them as `BootstrapPageTables` in the early reservation set.
pub struct BootstrapPagingAllocator<'map, 'reservations> {
    map: BootMemoryMap<'map>,
    reservations: &'reservations mut EarlyKernelReservations,
}

impl<'map, 'reservations> BootstrapPagingAllocator<'map, 'reservations> {
    /// Creates a paging allocator over the supplied boot memory map and early
    /// reservation set.
    #[must_use]
    pub const fn new(
        map: BootMemoryMap<'map>,
        reservations: &'reservations mut EarlyKernelReservations,
    ) -> Self {
        Self { map, reservations }
    }

    /// Allocates one 4 KiB frame for bootstrap paging structures and records
    /// it as kernel-owned paging memory.
    pub fn allocate_table_frame(&mut self) -> Option<PhysicalFrame> {
        let mut allocator = FrameAllocator::with_reservations(self.map, self.reservations.as_view());
        let frame = allocator.allocate()?;
        self.reservations
            .reserve_frame(ReservationKind::BootstrapPageTables, frame);
        Some(frame)
    }
}

#[cfg(test)]
mod tests {
    use super::{BootstrapPagingAllocator, PageTableRoot};
    use crate::memory::{
        BootMemoryMap, EarlyKernelReservations, KernelImage, MemoryRegion, MemoryRegionKind,
        PAGE_SIZE, PhysicalAddress, PhysicalFrame, ReservationKind, VirtualAddress,
    };

    #[test]
    fn bootstrap_paging_allocator_reserves_allocated_frames() {
        let regions = [MemoryRegion {
            start: PhysicalAddress::new(PAGE_SIZE),
            end: PhysicalAddress::new(PAGE_SIZE * 6),
            kind: MemoryRegionKind::Usable,
        }];
        let kernel_image = KernelImage {
            start: VirtualAddress::new(0x0010_0000),
            end: VirtualAddress::new(0x0012_0000),
        };
        let active_root = PhysicalFrame::containing(PhysicalAddress::new(PAGE_SIZE));
        let mut reservations = EarlyKernelReservations::for_bootstrap(kernel_image, active_root);
        let mut allocator =
            BootstrapPagingAllocator::new(BootMemoryMap::new(&regions), &mut reservations);

        let first = allocator.allocate_table_frame();
        let second = allocator.allocate_table_frame();

        assert_eq!(
            first.map(|frame| frame.start_address().as_u64()),
            Some(PAGE_SIZE * 2)
        );
        assert_eq!(
            second.map(|frame| frame.start_address().as_u64()),
            Some(PAGE_SIZE * 3)
        );
        assert_eq!(
            reservations.count_by_kind(ReservationKind::BootstrapPageTables),
            2
        );
    }

    #[test]
    fn page_table_root_wraps_explicit_frame() {
        let frame = PhysicalFrame::containing(PhysicalAddress::new(0x0040_0000));
        let root = PageTableRoot::new(frame);

        assert_eq!(root.frame(), frame);
    }
}
