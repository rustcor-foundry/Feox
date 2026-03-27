//! Early memory primitives and bootstrap-time memory planning.

use crate::arch;
pub use feox_boot::{MemoryRegion, MemoryRegionKind, PhysicalAddress};

/// Base x86_64 page size in bytes.
pub const PAGE_SIZE: u64 = feox_boot::PAGE_SIZE;

/// Legacy low-memory region reserved during x86 bootstrap.
pub const X86_LEGACY_LOW_MEMORY_BYTES: u64 = 1024 * 1024;

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

/// Explicit physical reservation range used to keep early kernel-owned memory
/// out of the allocatable frame pool.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ReservedRegion {
    start: PhysicalAddress,
    end: PhysicalAddress,
}

impl ReservedRegion {
    /// Creates a reservation from explicit bounds.
    #[must_use]
    pub const fn new(start: PhysicalAddress, end: PhysicalAddress) -> Self {
        Self { start, end }
    }

    /// Creates a reservation covering one physical frame.
    #[must_use]
    pub const fn for_frame(frame: PhysicalFrame) -> Self {
        Self {
            start: frame.start_address(),
            end: PhysicalAddress::new(frame.start_address().as_u64() + PAGE_SIZE),
        }
    }

    /// Returns whether the reservation contains the supplied address.
    #[must_use]
    pub const fn contains(self, address: PhysicalAddress) -> bool {
        address.as_u64() >= self.start.as_u64() && address.as_u64() < self.end.as_u64()
    }

    /// Returns the start address of the reservation.
    #[must_use]
    pub const fn start(self) -> PhysicalAddress {
        self.start
    }

    /// Returns the first address after the reservation.
    #[must_use]
    pub const fn end(self) -> PhysicalAddress {
        self.end
    }
}

/// Semantic kind for an early kernel-owned reservation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ReservationKind {
    /// Unused slot in the fixed-size bootstrap reservation set.
    #[default]
    Unused,
    /// Memory occupied by the loaded kernel image.
    KernelImage,
    /// The active top-level page-table root observed at bootstrap.
    ActivePageTableRoot,
    /// x86 legacy low physical memory kept out of the allocatable pool.
    LegacyLowMemory,
    /// Bootstrap-era page-table frames allocated after handoff.
    BootstrapPageTables,
    /// Bootstrap-era per-core state or stacks.
    BootstrapPerCoreState,
}

impl ReservationKind {
    /// Returns a short diagnostic label for the reservation category.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unused => "unused",
            Self::KernelImage => "kernel-image",
            Self::ActivePageTableRoot => "active-page-table-root",
            Self::LegacyLowMemory => "legacy-low-memory",
            Self::BootstrapPageTables => "bootstrap-page-tables",
            Self::BootstrapPerCoreState => "bootstrap-per-core-state",
        }
    }
}

/// Fixed-size early reservation set used during bootstrap before any dynamic
/// kernel-owned allocation structures exist.
#[derive(Clone, Copy, Debug, Default)]
pub struct EarlyKernelReservations {
    regions: [ReservedRegion; Self::MAX_REGIONS],
    kinds: [ReservationKind; Self::MAX_REGIONS],
    len: usize,
}

impl EarlyKernelReservations {
    /// Maximum number of early reservation entries tracked during bootstrap.
    pub const MAX_REGIONS: usize = 6;

    /// Builds the initial reservation set for the currently loaded kernel image
    /// and active top-level page table root.
    #[must_use]
    pub fn for_bootstrap(kernel_image: KernelImage, active_root: PhysicalFrame) -> Self {
        let mut reservations = Self::default();
        reservations.reserve_region(
            ReservationKind::LegacyLowMemory,
            ReservedRegion::new(
                PhysicalAddress::new(0),
                PhysicalAddress::new(X86_LEGACY_LOW_MEMORY_BYTES),
            ),
        );
        reservations.reserve_region(
            ReservationKind::KernelImage,
            ReservedRegion::new(
                PhysicalAddress::new(kernel_image.start.as_u64()),
                PhysicalAddress::new(kernel_image.end.as_u64()),
            ),
        );
        reservations.reserve_frame(
            ReservationKind::ActivePageTableRoot,
            active_root,
        );
        reservations
    }

    /// Returns the reservation count.
    #[must_use]
    pub const fn len(self) -> usize {
        self.len
    }

    /// Returns the reservations as a borrowed view.
    #[must_use]
    pub fn as_view(&self) -> BootReservations<'_> {
        BootReservations::new(&self.regions[..self.len])
    }

    /// Returns the number of reservations of the supplied kind.
    #[must_use]
    pub fn count_by_kind(&self, kind: ReservationKind) -> usize {
        self.kinds[..self.len]
            .iter()
            .copied()
            .filter(|candidate| *candidate == kind)
            .count()
    }

    /// Returns the reserved region for the supplied kind, if present.
    #[must_use]
    pub fn region_for_kind(&self, kind: ReservationKind) -> Option<ReservedRegion> {
        self.kinds[..self.len]
            .iter()
            .copied()
            .position(|candidate| candidate == kind)
            .map(|index| self.regions[index])
    }

    /// Reserves a physical frame under the supplied semantic category.
    pub fn reserve_frame(&mut self, kind: ReservationKind, frame: PhysicalFrame) {
        self.reserve_region(kind, ReservedRegion::for_frame(frame));
    }

    /// Reserves an explicit physical range under the supplied semantic category.
    pub fn reserve_region(&mut self, kind: ReservationKind, region: ReservedRegion) {
        if region.start.as_u64() >= region.end.as_u64() {
            return;
        }

        if self
            .regions
            .iter()
            .take(self.len)
            .copied()
            .any(|existing| existing == region)
        {
            return;
        }

        assert!(self.len < Self::MAX_REGIONS, "too many early kernel reservations");
        self.regions[self.len] = region;
        self.kinds[self.len] = kind;
        self.len += 1;
    }

    /// Returns the semantic category labels for all tracked reservations.
    #[must_use]
    pub fn kind_labels(&self) -> [&'static str; Self::MAX_REGIONS] {
        let mut labels = [ReservationKind::Unused.as_str(); Self::MAX_REGIONS];
        let mut index = 0;
        while index < self.len {
            labels[index] = self.kinds[index].as_str();
            index += 1;
        }
        labels
    }
}

/// Borrowed list of early kernel-owned physical reservations.
#[derive(Clone, Copy, Debug, Default)]
pub struct BootReservations<'a> {
    regions: &'a [ReservedRegion],
}

impl<'a> BootReservations<'a> {
    /// Creates a reservation view from a borrowed slice.
    #[must_use]
    pub const fn new(regions: &'a [ReservedRegion]) -> Self {
        Self { regions }
    }

    /// Returns whether the supplied address is reserved.
    #[must_use]
    pub fn contains(self, address: PhysicalAddress) -> bool {
        self.regions.iter().copied().any(|region| region.contains(address))
    }

    /// Returns the first address after the reservation containing the supplied address.
    #[must_use]
    pub fn next_unreserved_address(self, address: PhysicalAddress) -> PhysicalAddress {
        self.regions
            .iter()
            .copied()
            .find(|region| region.contains(address))
            .map_or(address, |region| region.end())
    }

    /// Returns the number of explicit reservations.
    #[must_use]
    pub const fn len(self) -> usize {
        self.regions.len()
    }
}

/// Linear physical frame allocator over the usable boot memory regions.
#[derive(Clone, Copy, Debug)]
pub struct FrameAllocator<'a> {
    regions: &'a [MemoryRegion],
    reservations: BootReservations<'a>,
    region_index: usize,
    next: PhysicalAddress,
}

impl<'a> FrameAllocator<'a> {
    /// Creates an allocator over the supplied boot memory map.
    #[must_use]
    pub fn new(map: BootMemoryMap<'a>) -> Self {
        Self::with_reservations(map, BootReservations::default())
    }

    /// Creates an allocator over the supplied boot memory map while excluding
    /// explicitly reserved physical ranges.
    #[must_use]
    pub fn with_reservations(map: BootMemoryMap<'a>, reservations: BootReservations<'a>) -> Self {
        let mut allocator = Self {
            regions: map.regions(),
            reservations,
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
            if self.reservations.contains(candidate) {
                self.next = self.reservations.next_unreserved_address(candidate).align_up();
                continue;
            }
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
        BootMemoryMap, BootReservations, EarlyKernelReservations, FrameAllocator, KernelImage,
        MemoryRegion, MemoryRegionKind, PAGE_SIZE, PhysicalAddress, PhysicalFrame,
        ReservationKind, ReservedRegion, VirtualAddress, X86_LEGACY_LOW_MEMORY_BYTES,
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

    #[test]
    fn frame_allocator_skips_explicit_reserved_ranges() {
        let regions = [MemoryRegion {
            start: PhysicalAddress::new(PAGE_SIZE),
            end: PhysicalAddress::new(PAGE_SIZE * 5),
            kind: MemoryRegionKind::Usable,
        }];
        let reservations = [ReservedRegion::new(
            PhysicalAddress::new(PAGE_SIZE * 2),
            PhysicalAddress::new(PAGE_SIZE * 3),
        )];
        let mut allocator = FrameAllocator::with_reservations(
            BootMemoryMap::new(&regions),
            BootReservations::new(&reservations),
        );

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
            Some(PAGE_SIZE * 3)
        );
        assert_eq!(
            allocator
                .allocate()
                .map(|frame| frame.start_address().as_u64()),
            Some(PAGE_SIZE * 4)
        );
        assert_eq!(allocator.allocate(), None);
    }

    #[test]
    fn early_kernel_reservations_include_kernel_image_and_active_root() {
        let kernel_image = KernelImage {
            start: VirtualAddress::new(0x0010_0000),
            end: VirtualAddress::new(0x0012_0000),
        };
        let active_root = PhysicalFrame::containing(PhysicalAddress::new(0x0030_0000));
        let reservations = EarlyKernelReservations::for_bootstrap(kernel_image, active_root);
        let view = reservations.as_view();

        assert_eq!(reservations.len(), 3);
        assert_eq!(reservations.kind_labels()[0], "legacy-low-memory");
        assert_eq!(reservations.kind_labels()[1], "kernel-image");
        assert_eq!(reservations.kind_labels()[2], "active-page-table-root");
        assert_eq!(reservations.count_by_kind(ReservationKind::LegacyLowMemory), 1);
        assert_eq!(reservations.count_by_kind(ReservationKind::KernelImage), 1);
        assert_eq!(
            reservations.count_by_kind(ReservationKind::ActivePageTableRoot),
            1
        );
        assert!(view.contains(PhysicalAddress::new(0x0000_1000)));
        assert_eq!(
            reservations
                .region_for_kind(ReservationKind::LegacyLowMemory)
                .map(|region| region.end().as_u64()),
            Some(X86_LEGACY_LOW_MEMORY_BYTES)
        );
        assert!(view.contains(PhysicalAddress::new(0x0010_1000)));
        assert!(view.contains(PhysicalAddress::new(0x0030_0000)));
        assert!(!view.contains(PhysicalAddress::new(0x0040_0000)));
        assert_eq!(
            reservations
                .region_for_kind(ReservationKind::ActivePageTableRoot)
                .map(ReservedRegion::start),
            Some(PhysicalAddress::new(0x0030_0000))
        );
    }

    #[test]
    fn early_kernel_reservations_can_track_future_bootstrap_categories() {
        let mut reservations = EarlyKernelReservations::default();
        reservations.reserve_frame(
            ReservationKind::BootstrapPageTables,
            PhysicalFrame::containing(PhysicalAddress::new(0x0040_0000)),
        );
        reservations.reserve_region(
            ReservationKind::BootstrapPerCoreState,
            ReservedRegion::new(
                PhysicalAddress::new(0x0050_0000),
                PhysicalAddress::new(0x0050_4000),
            ),
        );

        assert_eq!(reservations.len(), 2);
        assert_eq!(
            reservations.count_by_kind(ReservationKind::BootstrapPageTables),
            1
        );
        assert_eq!(
            reservations.count_by_kind(ReservationKind::BootstrapPerCoreState),
            1
        );
        assert_eq!(reservations.kind_labels()[0], "bootstrap-page-tables");
        assert_eq!(reservations.kind_labels()[1], "bootstrap-per-core-state");
    }
}
