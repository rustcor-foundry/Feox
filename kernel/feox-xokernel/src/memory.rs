//! Early memory primitives and bootstrap-time memory planning.

use crate::arch;
pub use feox_boot::{MemoryRegion, MemoryRegionKind, PhysicalAddress};

/// Base x86_64 page size in bytes.
pub const PAGE_SIZE: u64 = feox_boot::PAGE_SIZE;

/// Virtual address wrapper.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Hash)]
#[repr(transparent)]
pub struct VirtualAddress(u64);

impl VirtualAddress {
    /// Creates a virtual address from a raw integer.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Returns the raw integer value.
    #[must_use]
    pub const fn as_u64(self) -> u64 {
        self.0
    }

    /// Returns the four x86_64 page-table indices for this address.
    #[must_use]
    pub const fn page_table_indices(self) -> PageTableIndices {
        PageTableIndices {
            p4: ((self.0 >> 39) & 0x1FF) as u16,
            p3: ((self.0 >> 30) & 0x1FF) as u16,
            p2: ((self.0 >> 21) & 0x1FF) as u16,
            p1: ((self.0 >> 12) & 0x1FF) as u16,
        }
    }
}

/// x86_64 page-table indices derived from a canonical virtual address.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PageTableIndices {
    /// PML4 index.
    pub p4: u16,
    /// PDPT index.
    pub p3: u16,
    /// Page-directory index.
    pub p2: u16,
    /// Page-table index.
    pub p1: u16,
}

/// A 4 KiB physical page frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PhysicalFrame {
    start: PhysicalAddress,
}

impl PhysicalFrame {
    /// Creates a frame from a page-aligned start address.
    #[must_use]
    pub const fn containing(address: PhysicalAddress) -> Self {
        Self {
            start: address.align_down(),
        }
    }

    /// Returns the frame start address.
    #[must_use]
    pub const fn start_address(self) -> PhysicalAddress {
        self.start
    }
}

/// Borrowed boot memory map view.
#[derive(Clone, Copy, Debug)]
pub struct BootMemoryMap<'a> {
    regions: &'a [MemoryRegion],
}

impl<'a> BootMemoryMap<'a> {
    /// Creates a boot memory map from a borrowed region slice.
    #[must_use]
    pub const fn new(regions: &'a [MemoryRegion]) -> Self {
        Self { regions }
    }

    /// Returns the underlying region slice.
    #[must_use]
    pub const fn regions(self) -> &'a [MemoryRegion] {
        self.regions
    }
}

/// Linear physical frame allocator over the usable boot memory regions.
#[derive(Clone, Copy, Debug)]
pub struct FrameAllocator<'a> {
    regions: &'a [MemoryRegion],
    region_index: usize,
    next: PhysicalAddress,
}

impl<'a> FrameAllocator<'a> {
    /// Creates an allocator over the supplied boot memory map.
    #[must_use]
    pub fn new(map: BootMemoryMap<'a>) -> Self {
        let mut allocator = Self {
            regions: map.regions(),
            region_index: 0,
            next: PhysicalAddress::new(0),
        };
        allocator.seek_next_usable_region();
        allocator
    }

    /// Allocates the next available 4 KiB frame.
    pub fn allocate(&mut self) -> Option<PhysicalFrame> {
        loop {
            let region = *self.regions.get(self.region_index)?;
            if !region.kind.is_usable() {
                self.region_index += 1;
                self.seek_next_usable_region();
                continue;
            }

            let candidate = self.next.align_up();
            if candidate.as_u64() + PAGE_SIZE > region.end.as_u64() {
                self.region_index += 1;
                self.seek_next_usable_region();
                continue;
            }

            self.next = PhysicalAddress::new(candidate.as_u64() + PAGE_SIZE);
            return Some(PhysicalFrame::containing(candidate));
        }
    }

    fn seek_next_usable_region(&mut self) {
        while let Some(region) = self.regions.get(self.region_index).copied() {
            if region.kind.is_usable() {
                self.next = region.start.align_up();
                return;
            }

            self.region_index += 1;
        }
    }
}

/// Linker-defined kernel image bounds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KernelImage {
    /// Kernel image start.
    pub start: VirtualAddress,
    /// Kernel image end.
    pub end: VirtualAddress,
}

impl KernelImage {
    /// Returns the image size in bytes.
    #[must_use]
    pub const fn size_bytes(self) -> u64 {
        self.end.as_u64().saturating_sub(self.start.as_u64())
    }
}

/// Returns the current kernel image bounds from the linker symbols.
#[must_use]
#[cfg(target_os = "none")]
pub fn kernel_image() -> KernelImage {
    unsafe extern "C" {
        static __kernel_start: u8;
        static __kernel_end: u8;
    }

    KernelImage {
        start: VirtualAddress::new((&raw const __kernel_start) as *const u8 as u64),
        end: VirtualAddress::new((&raw const __kernel_end) as *const u8 as u64),
    }
}

/// Returns a stub kernel image on host builds where linker symbols are not
/// present.
#[must_use]
#[cfg(not(target_os = "none"))]
pub fn kernel_image() -> KernelImage {
    KernelImage {
        start: VirtualAddress::new(0),
        end: VirtualAddress::new(0),
    }
}

/// Returns the active top-level page table frame from CR3.
#[must_use]
pub fn active_page_table_root() -> PhysicalFrame {
    PhysicalFrame::containing(PhysicalAddress::new(
        arch::active_page_table_root() & !(PAGE_SIZE - 1),
    ))
}

#[cfg(test)]
mod tests {
    use super::{
        BootMemoryMap, FrameAllocator, MemoryRegion, MemoryRegionKind, PAGE_SIZE, PhysicalAddress,
        VirtualAddress,
    };

    #[test]
    fn virtual_address_indices_match_x86_64_layout() {
        let address = VirtualAddress::new(0xFFFF_8000_1234_5678);
        let indices = address.page_table_indices();

        assert_eq!(indices.p4, 256);
        assert_eq!(indices.p3, 0);
        assert_eq!(indices.p2, 145);
        assert_eq!(indices.p1, 325);
    }

    #[test]
    fn frame_allocator_skips_non_usable_regions() {
        let regions = [
            MemoryRegion {
                start: PhysicalAddress::new(0),
                end: PhysicalAddress::new(PAGE_SIZE),
                kind: MemoryRegionKind::Reserved,
            },
            MemoryRegion {
                start: PhysicalAddress::new(PAGE_SIZE),
                end: PhysicalAddress::new(PAGE_SIZE * 4),
                kind: MemoryRegionKind::Usable,
            },
        ];
        let mut allocator = FrameAllocator::new(BootMemoryMap::new(&regions));

        assert_eq!(
            allocator
                .allocate()
                .map(|frame| frame.start_address().as_u64()),
            Some(PAGE_SIZE)
        );
        assert_eq!(
            allocator
                .allocate()
                .map(|frame| frame.start_address().as_u64()),
            Some(PAGE_SIZE * 2)
        );
        assert_eq!(
            allocator
                .allocate()
                .map(|frame| frame.start_address().as_u64()),
            Some(PAGE_SIZE * 3)
        );
        assert_eq!(allocator.allocate(), None);
    }
}
