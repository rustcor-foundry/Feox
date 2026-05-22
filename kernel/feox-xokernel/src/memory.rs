//! Early memory primitives and bootstrap-time memory planning.

use crate::arch;
pub use feox_boot::{MemoryRegion, MemoryRegionKind, PhysicalAddress};

/// Base `x86_64` page size in bytes.
pub const PAGE_SIZE: u64 = feox_boot::PAGE_SIZE;

/// Legacy low-memory region reserved during x86 bootstrap.
pub const X86_LEGACY_LOW_MEMORY_BYTES: u64 = 1024 * 1024;
/// Higher-half base for the retained bootstrap kernel window.
pub const BOOTSTRAP_KERNEL_WINDOW_BASE: u64 = 0xFFFF_9000_0000_0000;
/// Higher-half base for the retained bootstrap stack window.
pub const BOOTSTRAP_STACK_WINDOW_BASE: u64 = 0xFFFF_9000_0200_0000;
/// Higher-half base for the retained bootstrap runtime-data window.
pub const BOOTSTRAP_DATA_WINDOW_BASE: u64 = 0xFFFF_9000_0300_0000;
/// Higher-half base for the bootstrap capability-backed VM window.
pub const BOOTSTRAP_VM_WINDOW_BASE: u64 = 0xFFFF_9000_0400_0000;
/// Size of the bootstrap VM window in bytes.
pub const BOOTSTRAP_VM_WINDOW_SIZE: u64 = 64 * 1024 * 1024;
/// Higher-half base for the bootstrap page-table access window.
pub const BOOTSTRAP_PAGE_TABLE_ACCESS_WINDOW_BASE: u64 = 0xFFFF_9000_0800_0000;
/// Size of the bootstrap page-table access window in bytes.
///
/// The access window mechanism is retired (`docs/PAGE_TABLE_ACCESS_PLAN.md`);
/// the live VM lane walks page tables through the permanent direct map at
/// [`DIRECT_MAP_BASE`]. This 20 KiB virtual range is preserved as a reserved
/// slot in `0xFFFF_9000` so any future tactical mechanism that wants the
/// same address-space footprint can claim it without churn.
pub const BOOTSTRAP_PAGE_TABLE_ACCESS_WINDOW_SIZE: u64 = 20 * 1024;

// ---------------------------------------------------------------------------
// Permanent kernel virtual layout — policy markers
//
// These constants record the layout commitments captured in
// `docs/VIRTUAL_ADDRESS_LAYOUT.md`. They are not yet backed by live mappings;
// implementation passes will install mappings inside these regions and the
// doc's implementation-status table will be updated as each region goes live.
// Any code that needs to reason about the permanent kernel layout (e.g. to
// reject mappings outside it) should reference these constants instead of
// re-deriving the addresses.
// ---------------------------------------------------------------------------

/// Base of the permanent direct map of physical memory.
///
/// Translation: `direct_map_va = DIRECT_MAP_BASE + phys_addr`.
pub const DIRECT_MAP_BASE: u64 = 0xFFFF_C000_0000_0000;

/// Maximum physical RAM coverage of the direct map, in bytes.
pub const DIRECT_MAP_SIZE: u64 = 32 * 1024 * 1024 * 1024 * 1024;

/// Base of the per-core kernel data region.
///
/// Each logical core owns one stride-sized slot starting here:
/// `core_base(core_id) = PER_CORE_BASE + core_id as u64 * PER_CORE_STRIDE`.
pub const PER_CORE_BASE: u64 = 0xFFFF_E000_0000_0000;

/// Stride between adjacent per-core slots, in bytes (1 TiB).
pub const PER_CORE_STRIDE: u64 = 1 << 40;

/// Maximum number of cores the locked layout reserves space for.
pub const PER_CORE_MAX_CORES: u64 = 32;

/// Base of the kernel-owned MMIO mapping region.
pub const MMIO_BASE: u64 = 0xFFFF_F000_0000_0000;

/// Size of the kernel-owned MMIO region, in bytes (8 TiB).
pub const MMIO_SIZE: u64 = 8 * 1024 * 1024 * 1024 * 1024;

/// Size of the MMIO sub-window whose page-table intermediates are prebuilt
/// during transition root construction. Sized to comfortably hold a handful
/// of typical device BARs (NVMe, USB, GPU control, modest framebuffers)
/// without bloating the boot-time page-table footprint. MMIO mappings beyond
/// this prebuild require expanding it first.
pub const MMIO_PREBUILT_SIZE: u64 = 64 * 1024 * 1024;

/// Base of the kernel vmalloc / capability-table dynamic region.
pub const KERNEL_VMALLOC_BASE: u64 = 0xFFFF_F800_0000_0000;

/// Size of the kernel vmalloc region, in bytes (8 TiB).
pub const KERNEL_VMALLOC_SIZE: u64 = 8 * 1024 * 1024 * 1024 * 1024;

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

    /// Returns the four `x86_64` page-table indices for this address.
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

/// `x86_64` page-table indices derived from a canonical virtual address.
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
#[derive(Clone, Copy, Debug)]
pub struct EarlyKernelReservations {
    regions: [ReservedRegion; Self::MAX_REGIONS],
    kinds: [ReservationKind; Self::MAX_REGIONS],
    len: usize,
}

impl Default for EarlyKernelReservations {
    fn default() -> Self {
        Self {
            regions: [ReservedRegion::new(PhysicalAddress::new(0), PhysicalAddress::new(0));
                Self::MAX_REGIONS],
            kinds: [ReservationKind::Unused; Self::MAX_REGIONS],
            len: 0,
        }
    }
}

impl EarlyKernelReservations {
    /// Maximum number of early reservation entries tracked during bootstrap.
    ///
    /// Grew from 96 → 256 when the kernel started running the async
    /// runtime in the boot probe: bigger bootstrap stack (8 pages) plus
    /// the extra page-table frames allocated for the direct map's 4 KiB
    /// head/tail mappings and the MMIO prebuild push the total over the
    /// old cap. 256 leaves comfortable headroom.
    pub const MAX_REGIONS: usize = 256;

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
        reservations.reserve_frame(ReservationKind::ActivePageTableRoot, active_root);
        reservations
    }

    /// Returns the reservation count.
    #[must_use]
    pub const fn len(self) -> usize {
        self.len
    }

    /// Returns whether no reservations are currently tracked.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.len == 0
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
    ///
    /// # Panics
    ///
    /// Panics if the fixed-size bootstrap reservation set is already full.
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

        assert!(
            self.len < Self::MAX_REGIONS,
            "too many early kernel reservations"
        );
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
        self.regions
            .iter()
            .copied()
            .any(|region| region.contains(address))
    }

    /// Returns the first address after the reservation containing the supplied address.
    #[must_use]
    pub fn next_unreserved_address(self, address: PhysicalAddress) -> PhysicalAddress {
        self.regions
            .iter()
            .copied()
            .find(|region| region.contains(address))
            .map_or(address, ReservedRegion::end)
    }

    /// Returns the number of explicit reservations.
    #[must_use]
    pub const fn len(self) -> usize {
        self.regions.len()
    }

    /// Returns whether there are no explicit reservations.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.regions.is_empty()
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
                self.next = self
                    .reservations
                    .next_unreserved_address(candidate)
                    .align_up();
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

    /// Returns whether the supplied address lies within this kernel image.
    #[must_use]
    pub const fn contains(self, address: VirtualAddress) -> bool {
        address.as_u64() >= self.start.as_u64() && address.as_u64() < self.end.as_u64()
    }
}

/// Named higher-half layout for the retained bootstrap runtime slice.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(clippy::struct_field_names)]
pub struct BootstrapRuntimeLayout {
    kernel_window_base: VirtualAddress,
    stack_window_base: VirtualAddress,
    data_window_base: VirtualAddress,
    vm_window_base: VirtualAddress,
    page_table_access_window_base: VirtualAddress,
}

impl Default for BootstrapRuntimeLayout {
    fn default() -> Self {
        Self::new()
    }
}

impl BootstrapRuntimeLayout {
    /// Creates the default retained bootstrap layout.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            kernel_window_base: VirtualAddress::new(BOOTSTRAP_KERNEL_WINDOW_BASE),
            stack_window_base: VirtualAddress::new(BOOTSTRAP_STACK_WINDOW_BASE),
            data_window_base: VirtualAddress::new(BOOTSTRAP_DATA_WINDOW_BASE),
            vm_window_base: VirtualAddress::new(BOOTSTRAP_VM_WINDOW_BASE),
            page_table_access_window_base: VirtualAddress::new(
                BOOTSTRAP_PAGE_TABLE_ACCESS_WINDOW_BASE,
            ),
        }
    }

    /// Returns the retained higher-half kernel image window base.
    #[must_use]
    pub const fn kernel_window_base(self) -> VirtualAddress {
        self.kernel_window_base
    }

    /// Returns the first address after the retained higher-half kernel window.
    #[must_use]
    pub const fn kernel_window_end(self, kernel_image: KernelImage) -> VirtualAddress {
        VirtualAddress::new(self.kernel_window_base.as_u64() + kernel_image.size_bytes())
    }

    /// Returns the retained higher-half stack window base.
    #[must_use]
    pub const fn stack_window_base(self) -> VirtualAddress {
        self.stack_window_base
    }

    /// Returns the retained higher-half runtime-data window base.
    #[must_use]
    pub const fn data_window_base(self) -> VirtualAddress {
        self.data_window_base
    }

    /// Returns the bootstrap capability-backed VM window base.
    #[must_use]
    pub const fn vm_window_base(self) -> VirtualAddress {
        self.vm_window_base
    }

    /// Returns the bootstrap capability-backed VM window size in bytes.
    #[must_use]
    pub const fn vm_window_size(self) -> u64 {
        BOOTSTRAP_VM_WINDOW_SIZE
    }

    /// Returns the first address after the bootstrap VM window.
    #[must_use]
    pub const fn vm_window_end(self) -> VirtualAddress {
        VirtualAddress::new(self.vm_window_base.as_u64() + BOOTSTRAP_VM_WINDOW_SIZE)
    }

    /// Returns the bootstrap page-table access window base.
    #[must_use]
    pub const fn page_table_access_window_base(self) -> VirtualAddress {
        self.page_table_access_window_base
    }

    /// Returns the bootstrap page-table access window size in bytes.
    #[must_use]
    pub const fn page_table_access_window_size(self) -> u64 {
        BOOTSTRAP_PAGE_TABLE_ACCESS_WINDOW_SIZE
    }

    /// Returns the first address after the bootstrap page-table access window.
    #[must_use]
    pub const fn page_table_access_window_end(self) -> VirtualAddress {
        VirtualAddress::new(
            self.page_table_access_window_base.as_u64() + BOOTSTRAP_PAGE_TABLE_ACCESS_WINDOW_SIZE,
        )
    }

    /// Returns the kernel-image-relative alias for a kernel address.
    #[must_use]
    pub const fn alias_for_kernel_address(
        self,
        kernel_image: KernelImage,
        address: VirtualAddress,
    ) -> Option<VirtualAddress> {
        if !kernel_image.contains(address) {
            return None;
        }

        Some(VirtualAddress::new(
            self.kernel_window_base.as_u64() + (address.as_u64() - kernel_image.start.as_u64()),
        ))
    }

    /// Returns the offset that maps low kernel-image addresses into the
    /// retained higher-half kernel window.
    #[must_use]
    pub const fn handler_delta(self, kernel_image: KernelImage) -> u64 {
        self.kernel_window_base
            .as_u64()
            .saturating_sub(kernel_image.start.as_u64())
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
        BOOTSTRAP_DATA_WINDOW_BASE, BOOTSTRAP_KERNEL_WINDOW_BASE,
        BOOTSTRAP_PAGE_TABLE_ACCESS_WINDOW_BASE, BOOTSTRAP_PAGE_TABLE_ACCESS_WINDOW_SIZE,
        BOOTSTRAP_STACK_WINDOW_BASE, BOOTSTRAP_VM_WINDOW_BASE, BootMemoryMap, BootReservations,
        BootstrapRuntimeLayout, EarlyKernelReservations, FrameAllocator, KernelImage, MemoryRegion,
        MemoryRegionKind, PAGE_SIZE, PhysicalAddress, PhysicalFrame, ReservationKind,
        ReservedRegion, VirtualAddress,
        X86_LEGACY_LOW_MEMORY_BYTES,
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
        assert_eq!(
            reservations.count_by_kind(ReservationKind::LegacyLowMemory),
            1
        );
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

    #[test]
    fn bootstrap_runtime_layout_derives_named_windows() {
        let layout = BootstrapRuntimeLayout::new();
        let kernel_image = KernelImage {
            start: VirtualAddress::new(0x0010_0000),
            end: VirtualAddress::new(0x0011_8000),
        };
        let entry = VirtualAddress::new(0x0010_0350);

        assert_eq!(
            layout.kernel_window_base().as_u64(),
            BOOTSTRAP_KERNEL_WINDOW_BASE
        );
        assert_eq!(
            layout.kernel_window_end(kernel_image).as_u64(),
            BOOTSTRAP_KERNEL_WINDOW_BASE + 0x18_000
        );
        assert_eq!(
            layout.stack_window_base().as_u64(),
            BOOTSTRAP_STACK_WINDOW_BASE
        );
        assert_eq!(
            layout.data_window_base().as_u64(),
            BOOTSTRAP_DATA_WINDOW_BASE
        );
        assert_eq!(
            layout.vm_window_base().as_u64(),
            BOOTSTRAP_VM_WINDOW_BASE
        );
        assert_eq!(
            layout.page_table_access_window_base().as_u64(),
            BOOTSTRAP_PAGE_TABLE_ACCESS_WINDOW_BASE
        );
        assert_eq!(
            layout.page_table_access_window_end().as_u64(),
            BOOTSTRAP_PAGE_TABLE_ACCESS_WINDOW_BASE + BOOTSTRAP_PAGE_TABLE_ACCESS_WINDOW_SIZE
        );
        assert_eq!(
            layout
                .alias_for_kernel_address(kernel_image, entry)
                .map(VirtualAddress::as_u64),
            Some(BOOTSTRAP_KERNEL_WINDOW_BASE + 0x350)
        );
        assert_eq!(
            layout.handler_delta(kernel_image),
            BOOTSTRAP_KERNEL_WINDOW_BASE - 0x0010_0000
        );
    }
}
