#![allow(clippy::similar_names)]

//! Early `x86`-first paging ownership and query helpers.

use crate::memory::{
    BootMemoryMap, EarlyKernelReservations, FrameAllocator, PAGE_SIZE, PhysicalAddress,
    PhysicalFrame, ReservationKind, VirtualAddress, active_page_table_root,
};

/// Number of entries in one `x86_64` page table.
pub const PAGE_TABLE_ENTRY_COUNT: usize = 512;

/// Raw `x86_64` page-table entry.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PageTableEntry(u64);

impl PageTableEntry {
    const FLAG_PRESENT: u64 = 1 << 0;
    const FLAG_WRITABLE: u64 = 1 << 1;
    const FLAG_USER: u64 = 1 << 2;
    const FLAG_HUGE_PAGE: u64 = 1 << 7;
    const ADDRESS_MASK: u64 = 0x000f_ffff_ffff_f000;
    /// Execute-disable flag (bit 63). Requires `EFER.NXE = 1`.
    ///
    /// Set this on any page that should not be executable: stacks, data pages,
    /// and capability/MMIO windows. Never set it on code pages.
    pub const FLAG_NO_EXECUTE: u64 = 1 << 63;

    /// Creates a raw entry from an encoded `u64`.
    #[must_use]
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// Returns the encoded entry value.
    #[must_use]
    pub const fn raw(self) -> u64 {
        self.0
    }

    /// Returns whether the entry is present.
    #[must_use]
    pub const fn is_present(self) -> bool {
        (self.0 & Self::FLAG_PRESENT) != 0
    }

    /// Returns whether the entry marks a huge page.
    #[must_use]
    pub const fn is_huge_page(self) -> bool {
        (self.0 & Self::FLAG_HUGE_PAGE) != 0
    }

    /// Returns whether the entry is writable.
    #[must_use]
    pub const fn is_writable(self) -> bool {
        (self.0 & Self::FLAG_WRITABLE) != 0
    }

    /// Returns whether the entry is user accessible.
    #[must_use]
    pub const fn is_user(self) -> bool {
        (self.0 & Self::FLAG_USER) != 0
    }

    /// Returns the physical frame referenced by the entry.
    #[must_use]
    pub const fn frame(self) -> PhysicalFrame {
        PhysicalFrame::containing(PhysicalAddress::new(self.0 & Self::ADDRESS_MASK))
    }

    /// Builds a present entry for the supplied frame and raw flags.
    #[must_use]
    pub const fn present(frame: PhysicalFrame, flags: u64) -> Self {
        Self((frame.start_address().as_u64() & Self::ADDRESS_MASK) | Self::FLAG_PRESENT | flags)
    }
}

/// Result of translating a virtual address through a page-table root.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Translation {
    /// Resolved physical address.
    pub physical_address: PhysicalAddress,
    /// Leaf page-table entry used for the translation.
    pub entry: PageTableEntry,
}

/// Errors produced while creating a 4 KiB mapping.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Map4kError {
    /// A required page-table frame could not be read or written.
    MissingTableFrame(PhysicalFrame),
    /// Huge-page entries are not supported in the current bootstrap layer.
    UnsupportedHugePage {
        /// Depth where the huge-page entry was observed.
        level: PageWalkLevel,
        /// The entry that triggered the rejection.
        entry: PageTableEntry,
    },
    /// The target virtual address already had a 4 KiB mapping.
    AlreadyMapped(PageTableEntry),
    /// No additional page-table frame was available.
    OutOfTableFrames,
}

/// Errors produced while removing a 4 KiB mapping.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Unmap4kError {
    /// A required page-table frame could not be read or written.
    MissingTableFrame(PhysicalFrame),
    /// Huge-page entries are not supported in the current bootstrap layer.
    UnsupportedHugePage {
        /// Depth where the huge-page entry was observed.
        level: PageWalkLevel,
        /// The entry that triggered the rejection.
        entry: PageTableEntry,
    },
}

/// Errors produced while walking the page-table hierarchy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PageWalkError {
    /// The backing table frame could not be read from the supplied source.
    MissingTableFrame(PhysicalFrame),
    /// A huge-page entry was encountered before a 4 KiB leaf table.
    UnsupportedHugePage {
        /// Depth where the huge-page entry was observed.
        level: PageWalkLevel,
        /// The entry that triggered the rejection.
        entry: PageTableEntry,
    },
}

/// `x86_64` walk depth labels used for diagnostics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PageWalkLevel {
    /// PML4 level.
    Pml4,
    /// PDPT level.
    Pdpt,
    /// PD level.
    Pd,
    /// PT level.
    Pt,
}

impl PageWalkLevel {
    fn next(self) -> Option<Self> {
        match self {
            Self::Pml4 => Some(Self::Pdpt),
            Self::Pdpt => Some(Self::Pd),
            Self::Pd => Some(Self::Pt),
            Self::Pt => None,
        }
    }
}

/// Read-only source for page-table frames.
pub trait PageTableFrameSource {
    /// Returns the 512-entry table backing the supplied frame, if available.
    fn table(&self, frame: PhysicalFrame) -> Option<&[u64; PAGE_TABLE_ENTRY_COUNT]>;
}

/// Mutable source for page-table frames.
pub trait PageTableFrameMutSource: PageTableFrameSource {
    /// Returns the mutable 512-entry table backing the supplied frame, if available.
    fn table_mut(&mut self, frame: PhysicalFrame) -> Option<&mut [u64; PAGE_TABLE_ENTRY_COUNT]>;
}

/// Allocator used to obtain new paging-structure frames.
pub trait PageTableFrameAllocator {
    /// Allocates one 4 KiB frame for page-table use.
    fn allocate_table_frame(&mut self) -> Option<PhysicalFrame>;

    /// Records a directed edge from `parent` to the newly allocated `child`
    /// in the allocator's frame-tree sidecar, if one exists.
    ///
    /// Called by [`ensure_child_table`] immediately after every successful
    /// allocation. The default implementation is a no-op; allocators that
    /// carry a [`PageTableEdges`] sidecar override this to record the edge.
    fn record_edge(&mut self, _parent: PhysicalFrame, _child: PhysicalFrame) {}
}

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

    /// Translates a virtual address through this root using the supplied
    /// read-only table source.
    ///
    /// # Errors
    ///
    /// Returns [`PageWalkError`] when a required table frame is unavailable or
    /// when a huge-page entry is encountered before a 4 KiB leaf table.
    pub fn translate_with(
        self,
        source: &impl PageTableFrameSource,
        virtual_address: VirtualAddress,
    ) -> Result<Option<Translation>, PageWalkError> {
        let indices = virtual_address.page_table_indices();
        let Some(p4) = read_entry(source, self.frame, indices.p4, PageWalkLevel::Pml4)? else {
            return Ok(None);
        };
        let Some(p3) = read_entry(source, p4.frame(), indices.p3, PageWalkLevel::Pdpt)? else {
            return Ok(None);
        };
        let Some(p2) = read_entry(source, p3.frame(), indices.p2, PageWalkLevel::Pd)? else {
            return Ok(None);
        };
        let Some(p1) = read_entry(source, p2.frame(), indices.p1, PageWalkLevel::Pt)? else {
            return Ok(None);
        };

        Ok(Some(Translation {
            physical_address: PhysicalAddress::new(
                p1.frame().start_address().as_u64() + (virtual_address.as_u64() & (PAGE_SIZE - 1)),
            ),
            entry: p1,
        }))
    }

    /// Installs a 4 KiB mapping, allocating missing intermediate tables.
    ///
    /// # Errors
    ///
    /// Returns [`Map4kError`] when a required table is missing, a huge-page
    /// entry blocks the walk, the virtual address is already mapped, or the
    /// paging allocator runs out of table frames.
    pub fn map_4k_with(
        self,
        source: &mut impl PageTableFrameMutSource,
        allocator: &mut impl PageTableFrameAllocator,
        virtual_address: VirtualAddress,
        physical_frame: PhysicalFrame,
        flags: u64,
    ) -> Result<(), Map4kError> {
        let indices = virtual_address.page_table_indices();
        let pdpt = ensure_child_table(
            source,
            allocator,
            self.frame,
            indices.p4,
            PageWalkLevel::Pml4,
        )?;
        let pd = ensure_child_table(source, allocator, pdpt, indices.p3, PageWalkLevel::Pdpt)?;
        let pt = ensure_child_table(source, allocator, pd, indices.p2, PageWalkLevel::Pd)?;
        let table = source
            .table_mut(pt)
            .ok_or(Map4kError::MissingTableFrame(pt))?;
        let existing = PageTableEntry::from_raw(table[indices.p1 as usize]);
        if existing.is_present() {
            return Err(Map4kError::AlreadyMapped(existing));
        }
        table[indices.p1 as usize] = PageTableEntry::present(physical_frame, flags).raw();
        // Flush the TLB entry for this address. The CR3 write performed during
        // the bootstrap root switch implicitly flushes everything at that point,
        // but any call after the root is live must flush explicitly or a stale
        // translation could be used.
        #[cfg(target_os = "none")]
        crate::arch::invalidate_page(virtual_address.as_u64());
        Ok(())
    }

    /// Ensures all intermediate page tables needed for a 4 KiB-mapped range exist.
    ///
    /// This allocates any missing PML4/PDPT/PD/PT structures for every 2 MiB
    /// chunk touched by the supplied range, but does not install any leaf PTEs.
    ///
    /// # Errors
    ///
    /// Returns [`Map4kError`] when a required table is missing, a huge-page
    /// entry blocks the walk, or the paging allocator runs out of table frames.
    pub fn prepare_4k_pages_with(
        self,
        source: &mut impl PageTableFrameMutSource,
        allocator: &mut impl PageTableFrameAllocator,
        start: VirtualAddress,
        length_bytes: u64,
    ) -> Result<(), Map4kError> {
        if length_bytes == 0 {
            return Ok(());
        }

        let end = start
            .as_u64()
            .checked_add(length_bytes)
            .ok_or(Map4kError::OutOfTableFrames)?;
        let mut current = start.as_u64();
        while current < end {
            let virtual_address = VirtualAddress::new(current);
            let indices = virtual_address.page_table_indices();
            let pdpt =
                ensure_child_table(source, allocator, self.frame, indices.p4, PageWalkLevel::Pml4)?;
            let pd =
                ensure_child_table(source, allocator, pdpt, indices.p3, PageWalkLevel::Pdpt)?;
            let _pt = ensure_child_table(source, allocator, pd, indices.p2, PageWalkLevel::Pd)?;
            current = current.saturating_add(1 << 21);
        }

        Ok(())
    }

    /// Removes a 4 KiB mapping and returns the previously installed leaf entry.
    ///
    /// # Errors
    ///
    /// Returns [`Unmap4kError`] when a required table is missing or a huge-page
    /// entry blocks the walk.
    pub fn unmap_4k_with(
        self,
        source: &mut impl PageTableFrameMutSource,
        virtual_address: VirtualAddress,
    ) -> Result<Option<PageTableEntry>, Unmap4kError> {
        let indices = virtual_address.page_table_indices();
        let Some(p4) = read_entry(source, self.frame, indices.p4, PageWalkLevel::Pml4)
            .map_err(to_unmap_error)?
        else {
            return Ok(None);
        };
        let Some(p3) = read_entry(source, p4.frame(), indices.p3, PageWalkLevel::Pdpt)
            .map_err(to_unmap_error)?
        else {
            return Ok(None);
        };
        let Some(p2) = read_entry(source, p3.frame(), indices.p2, PageWalkLevel::Pd)
            .map_err(to_unmap_error)?
        else {
            return Ok(None);
        };
        let Some(_) = read_entry(source, p2.frame(), indices.p1, PageWalkLevel::Pt)
            .map_err(to_unmap_error)?
        else {
            return Ok(None);
        };

        let table = source
            .table_mut(p2.frame())
            .ok_or(Unmap4kError::MissingTableFrame(p2.frame()))?;
        let entry = PageTableEntry::from_raw(table[indices.p1 as usize]);
        if !entry.is_present() {
            return Ok(None);
        }
        table[indices.p1 as usize] = 0;
        // Flush the TLB entry so no subsequent access can reach the now-unmapped
        // physical frame through a cached translation.
        #[cfg(target_os = "none")]
        crate::arch::invalidate_page(virtual_address.as_u64());
        Ok(Some(entry))
    }
}

/// Fixed-capacity sidecar recording directed edges in the bootstrap
/// page-table tree.
///
/// Each entry `(parent_phys, child_phys)` means the page table at `parent`
/// holds an entry pointing to `child`. A capacity of 32 pairs is sufficient
/// for all intermediate frames reachable in a 4-level walk over a modest
/// kernel image (at most ~3 levels × ~10 mappings).
///
/// Produced by [`BootstrapPagingAllocator`] and consumed when the transition
/// root is replaced by the permanent kernel root, at which point the tree can
/// be enumerated to determine which frames to reclaim.
#[derive(Clone, Copy, Debug)]
pub struct PageTableEdges {
    edges: [(u64, u64); Self::CAPACITY],
    count: usize,
}

impl PageTableEdges {
    /// Maximum number of parent→child edges this sidecar can record.
    pub const CAPACITY: usize = 32;

    /// Creates an empty edge sidecar.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            edges: [(0, 0); Self::CAPACITY],
            count: 0,
        }
    }

    /// Returns the number of recorded edges.
    #[must_use]
    pub const fn count(&self) -> usize {
        self.count
    }

    /// Records a `parent → child` page-table edge.
    ///
    /// Returns `false` without recording if capacity is exhausted.
    pub fn record(&mut self, parent: PhysicalFrame, child: PhysicalFrame) -> bool {
        if self.count >= Self::CAPACITY {
            return false;
        }
        self.edges[self.count] = (
            parent.start_address().as_u64(),
            child.start_address().as_u64(),
        );
        self.count += 1;
        true
    }

    /// Returns all frames reachable from `root` via recorded edges (BFS).
    ///
    /// The returned array contains up to `CAPACITY` frames in discovery
    /// order; trailing entries are `None`. `root` itself is not included.
    #[must_use]
    pub fn frames_reachable_from(
        &self,
        root: PhysicalFrame,
    ) -> [Option<PhysicalFrame>; Self::CAPACITY] {
        let mut result = [None; Self::CAPACITY];
        let mut result_count = 0;

        // BFS frontier stored in a fixed-size array.
        let mut frontier = [0u64; Self::CAPACITY];
        let mut frontier_head = 0;
        let mut frontier_len = 1;
        frontier[0] = root.start_address().as_u64();

        while frontier_head < frontier_len {
            let current = frontier[frontier_head];
            frontier_head += 1;

            let mut i = 0;
            while i < self.count {
                let (parent, child) = self.edges[i];
                if parent == current {
                    // Skip if already in the frontier (dedup).
                    let mut found = false;
                    let mut j = 0;
                    while j < frontier_len {
                        if frontier[j] == child {
                            found = true;
                            break;
                        }
                        j += 1;
                    }
                    if !found && frontier_len < Self::CAPACITY {
                        frontier[frontier_len] = child;
                        frontier_len += 1;
                        if result_count < Self::CAPACITY {
                            result[result_count] = Some(PhysicalFrame::containing(
                                PhysicalAddress::new(child),
                            ));
                            result_count += 1;
                        }
                    }
                }
                i += 1;
            }
        }
        result
    }
}

impl Default for PageTableEdges {
    fn default() -> Self {
        Self::new()
    }
}

/// Bootstrap allocator for new paging-structure frames.
///
/// This is intentionally tiny: it only hands out 4 KiB frames and records
/// them as `BootstrapPageTables` in the early reservation set.
pub struct BootstrapPagingAllocator<'map, 'reservations> {
    map: BootMemoryMap<'map>,
    reservations: &'reservations mut EarlyKernelReservations,
    /// Sidecar recording every parent→child edge produced during allocation.
    edges: PageTableEdges,
}

impl<'map, 'reservations> BootstrapPagingAllocator<'map, 'reservations> {
    /// Creates a paging allocator over the supplied boot memory map and early
    /// reservation set.
    #[must_use]
    pub const fn new(
        map: BootMemoryMap<'map>,
        reservations: &'reservations mut EarlyKernelReservations,
    ) -> Self {
        Self {
            map,
            reservations,
            edges: PageTableEdges::new(),
        }
    }

    /// Allocates one 4 KiB frame for bootstrap paging structures and records
    /// it as kernel-owned paging memory.
    pub fn allocate_table_frame(&mut self) -> Option<PhysicalFrame> {
        let mut allocator =
            FrameAllocator::with_reservations(self.map, self.reservations.as_view());
        let frame = allocator.allocate()?;
        self.reservations
            .reserve_frame(ReservationKind::BootstrapPageTables, frame);
        Some(frame)
    }

    /// Returns the frame-tree edge sidecar accumulated during allocation.
    #[must_use]
    pub fn edges(&self) -> &PageTableEdges {
        &self.edges
    }
}

impl PageTableFrameAllocator for BootstrapPagingAllocator<'_, '_> {
    fn allocate_table_frame(&mut self) -> Option<PhysicalFrame> {
        BootstrapPagingAllocator::allocate_table_frame(self)
    }

    fn record_edge(&mut self, parent: PhysicalFrame, child: PhysicalFrame) {
        self.edges.record(parent, child);
    }
}

/// Bootstrap page-table source that treats low physical memory as identity
/// mapped in the current x86 bring-up path.
///
/// This is intentionally narrow and only exists to let the live bootstrap path
/// exercise page-table mechanisms before a broader virtual-memory model exists.
#[derive(Clone, Copy, Debug, Default)]
pub struct BootstrapIdentityMappedPageTables;

impl PageTableFrameSource for BootstrapIdentityMappedPageTables {
    fn table(&self, frame: PhysicalFrame) -> Option<&[u64; PAGE_TABLE_ENTRY_COUNT]> {
        identity_mapped_table(frame)
    }
}

impl PageTableFrameMutSource for BootstrapIdentityMappedPageTables {
    fn table_mut(&mut self, frame: PhysicalFrame) -> Option<&mut [u64; PAGE_TABLE_ENTRY_COUNT]> {
        identity_mapped_table_mut(frame)
    }
}

fn identity_mapped_table(frame: PhysicalFrame) -> Option<&'static [u64; PAGE_TABLE_ENTRY_COUNT]> {
    #[cfg(target_os = "none")]
    {
        let address = frame.start_address().as_u64();
        if address == 0 {
            return None;
        }

        let table = unsafe {
            // SAFETY: During the current x86 bootstrap lane, the loader hands
            // off with low physical memory identity mapped. Page-table frames
            // allocated and inspected here are below 4 GiB and are only used
            // during early bring-up before a richer VM model exists.
            &*(core::ptr::with_exposed_provenance::<[u64; PAGE_TABLE_ENTRY_COUNT]>(
                address as usize,
            ))
        };
        Some(table)
    }
    #[cfg(not(target_os = "none"))]
    {
        let _ = frame;
        None
    }
}

fn identity_mapped_table_mut(
    frame: PhysicalFrame,
) -> Option<&'static mut [u64; PAGE_TABLE_ENTRY_COUNT]> {
    #[cfg(target_os = "none")]
    {
        let address = frame.start_address().as_u64();
        if address == 0 {
            return None;
        }

        // C-04 aliasing invariant: Rust's aliasing rules forbid two
        // simultaneously live `&mut` references to the same memory. This
        // function returns a `'static mut` reference, so the caller must
        // ensure it does not call this function again with the same frame
        // while the previous reference is still live. In practice this is
        // guaranteed by `BootstrapPagingAllocator`, which never reuses frames,
        // meaning each distinct physical frame can only appear once in any
        // traversal. If the allocator contract is ever relaxed, a debug-mode
        // visited-set should be added to the call sites.
        debug_assert!(
            address & 0xFFF == 0,
            "page-table frame {address:#x} is not 4 KiB aligned"
        );

        let table = unsafe {
            // SAFETY: See `identity_mapped_table`. The mutable access stays
            // confined to bootstrap-owned page-table frames in the single-core
            // early bring-up path. Non-aliasing is upheld by the allocator
            // never reusing frames (C-04 invariant above).
            &mut *(core::ptr::with_exposed_provenance_mut::<[u64; PAGE_TABLE_ENTRY_COUNT]>(
                address as usize,
            ))
        };
        Some(table)
    }
    #[cfg(not(target_os = "none"))]
    {
        let _ = frame;
        None
    }
}

fn read_entry(
    source: &impl PageTableFrameSource,
    table_frame: PhysicalFrame,
    index: u16,
    level: PageWalkLevel,
) -> Result<Option<PageTableEntry>, PageWalkError> {
    let table = source
        .table(table_frame)
        .ok_or(PageWalkError::MissingTableFrame(table_frame))?;
    let entry = PageTableEntry::from_raw(table[index as usize]);
    if !entry.is_present() {
        return Ok(None);
    }
    if level.next().is_some() && entry.is_huge_page() {
        return Err(PageWalkError::UnsupportedHugePage { level, entry });
    }
    Ok(Some(entry))
}

fn ensure_child_table(
    source: &mut impl PageTableFrameMutSource,
    allocator: &mut impl PageTableFrameAllocator,
    table_frame: PhysicalFrame,
    index: u16,
    level: PageWalkLevel,
) -> Result<PhysicalFrame, Map4kError> {
    let existing = {
        let table = source
            .table_mut(table_frame)
            .ok_or(Map4kError::MissingTableFrame(table_frame))?;
        PageTableEntry::from_raw(table[index as usize])
    };

    if existing.is_present() {
        if existing.is_huge_page() {
            return Err(Map4kError::UnsupportedHugePage {
                level,
                entry: existing,
            });
        }
        return Ok(existing.frame());
    }

    let child = allocator
        .allocate_table_frame()
        .ok_or(Map4kError::OutOfTableFrames)?;
    allocator.record_edge(table_frame, child);

    // C-05: the `table_mut(child)` and subsequent `table_mut(table_frame)`
    // calls are sequential, not simultaneous — each borrow is dropped before
    // the next begins. Aliasing is only possible if `child == table_frame`,
    // which cannot happen because the allocator returns a freshly allocated
    // frame that has not been installed in any page-table tree yet.
    let child_table = source
        .table_mut(child)
        .ok_or(Map4kError::MissingTableFrame(child))?;
    child_table.fill(0);

    // P-02: intermediate entries are installed with PRESENT | WRITABLE. This
    // is correct for the bootstrap identity-mapped and transition roots where
    // all page-table levels must be kernel-writable. When per-process address
    // spaces are introduced this policy must be revisited: intermediate entries
    // covering user-space subtrees should not carry WRITABLE at the kernel
    // privilege level. See docs/PAGE_TABLE_PLAN.md.
    let table = source
        .table_mut(table_frame)
        .ok_or(Map4kError::MissingTableFrame(table_frame))?;
    table[index as usize] = PageTableEntry::present(child, 1 << 1).raw();
    Ok(child)
}

fn to_unmap_error(error: PageWalkError) -> Unmap4kError {
    match error {
        PageWalkError::MissingTableFrame(frame) => Unmap4kError::MissingTableFrame(frame),
        PageWalkError::UnsupportedHugePage { level, entry } => {
            Unmap4kError::UnsupportedHugePage { level, entry }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BootstrapPagingAllocator, Map4kError, PAGE_TABLE_ENTRY_COUNT, PageTableEdges,
        PageTableEntry, PageTableFrameAllocator, PageTableFrameMutSource, PageTableFrameSource,
        PageTableRoot, PageWalkError, PageWalkLevel, ensure_child_table, read_entry,
    };
    use crate::memory::{
        BootMemoryMap, EarlyKernelReservations, KernelImage, MemoryRegion, MemoryRegionKind,
        PAGE_SIZE, PhysicalAddress, PhysicalFrame, ReservationKind, VirtualAddress,
    };
    #[derive(Clone, Copy)]
    struct TableSlot {
        frame: PhysicalFrame,
        table: [u64; PAGE_TABLE_ENTRY_COUNT],
    }

    impl Default for TableSlot {
        fn default() -> Self {
            Self {
                frame: PhysicalFrame::containing(PhysicalAddress::new(0)),
                table: [0; PAGE_TABLE_ENTRY_COUNT],
            }
        }
    }

    #[derive(Default)]
    struct FakePageTables {
        slots: [TableSlot; 4],
        len: usize,
    }

    impl FakePageTables {
        fn insert(&mut self, frame: PhysicalFrame, table: &[u64; PAGE_TABLE_ENTRY_COUNT]) {
            self.slots[self.len] = TableSlot {
                frame,
                table: *table,
            };
            self.len += 1;
        }
    }

    impl PageTableFrameSource for FakePageTables {
        fn table(&self, frame: PhysicalFrame) -> Option<&[u64; PAGE_TABLE_ENTRY_COUNT]> {
            self.slots[..self.len]
                .iter()
                .find(|slot| slot.frame == frame)
                .map(|slot| &slot.table)
        }
    }

    impl PageTableFrameMutSource for FakePageTables {
        fn table_mut(
            &mut self,
            frame: PhysicalFrame,
        ) -> Option<&mut [u64; PAGE_TABLE_ENTRY_COUNT]> {
            self.slots[..self.len]
                .iter_mut()
                .find(|slot| slot.frame == frame)
                .map(|slot| &mut slot.table)
        }
    }

    struct FakePagingAllocator {
        frames: [PhysicalFrame; 4],
        len: usize,
        next: usize,
    }

    impl FakePagingAllocator {
        fn new(frames: [PhysicalFrame; 4], len: usize) -> Self {
            Self {
                frames,
                len,
                next: 0,
            }
        }
    }

    impl PageTableFrameAllocator for FakePagingAllocator {
        fn allocate_table_frame(&mut self) -> Option<PhysicalFrame> {
            if self.next >= self.len {
                return None;
            }
            let frame = self.frames[self.next];
            self.next += 1;
            Some(frame)
        }
    }

    #[test]
    fn bootstrap_paging_allocator_reserves_allocated_frames() {
        let usable_start = 0x0040_0000;
        let regions = [MemoryRegion {
            start: PhysicalAddress::new(usable_start),
            end: PhysicalAddress::new(usable_start + (PAGE_SIZE * 5)),
            kind: MemoryRegionKind::Usable,
        }];
        let kernel_image = KernelImage {
            start: VirtualAddress::new(0x0010_0000),
            end: VirtualAddress::new(0x0012_0000),
        };
        let active_root = PhysicalFrame::containing(PhysicalAddress::new(0x0030_0000));
        let mut reservations = EarlyKernelReservations::for_bootstrap(kernel_image, active_root);
        let mut allocator =
            BootstrapPagingAllocator::new(BootMemoryMap::new(&regions), &mut reservations);

        let first = allocator.allocate_table_frame();
        let second = allocator.allocate_table_frame();

        assert_eq!(
            first.map(|frame| frame.start_address().as_u64()),
            Some(usable_start)
        );
        assert_eq!(
            second.map(|frame| frame.start_address().as_u64()),
            Some(usable_start + PAGE_SIZE)
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

    #[test]
    fn translate_with_resolves_a_4k_mapping() {
        let root = PhysicalFrame::containing(PhysicalAddress::new(0x0010_0000));
        let pdpt = PhysicalFrame::containing(PhysicalAddress::new(0x0011_0000));
        let pd = PhysicalFrame::containing(PhysicalAddress::new(0x0012_0000));
        let pt = PhysicalFrame::containing(PhysicalAddress::new(0x0013_0000));
        let leaf = PhysicalFrame::containing(PhysicalAddress::new(0x0020_0000));
        let virtual_address = VirtualAddress::new(0xFFFF_8000_1234_5678);
        let indices = virtual_address.page_table_indices();
        let mut source = FakePageTables::default();

        let mut root_table = [0_u64; PAGE_TABLE_ENTRY_COUNT];
        root_table[indices.p4 as usize] = PageTableEntry::present(pdpt, 0).raw();
        source.insert(root, &root_table);

        let mut pdpt_table = [0_u64; PAGE_TABLE_ENTRY_COUNT];
        pdpt_table[indices.p3 as usize] = PageTableEntry::present(pd, 0).raw();
        source.insert(pdpt, &pdpt_table);

        let mut pd_table = [0_u64; PAGE_TABLE_ENTRY_COUNT];
        pd_table[indices.p2 as usize] = PageTableEntry::present(pt, 0).raw();
        source.insert(pd, &pd_table);

        let mut pt_table = [0_u64; PAGE_TABLE_ENTRY_COUNT];
        pt_table[indices.p1 as usize] = PageTableEntry::present(leaf, 1 << 1).raw();
        source.insert(pt, &pt_table);

        let translation = PageTableRoot::new(root)
            .translate_with(&source, virtual_address)
            .expect("walk should succeed")
            .expect("mapping should exist");

        assert_eq!(
            translation.physical_address,
            PhysicalAddress::new(0x0020_0678)
        );
        assert!(translation.entry.is_present());
        assert!(translation.entry.is_writable());
    }

    #[test]
    fn translate_with_returns_none_for_non_present_entry() {
        let root = PhysicalFrame::containing(PhysicalAddress::new(0x0010_0000));
        let mut source = FakePageTables::default();
        source.insert(root, &[0; PAGE_TABLE_ENTRY_COUNT]);

        let result = PageTableRoot::new(root)
            .translate_with(&source, VirtualAddress::new(0x2000))
            .expect("root table should be readable");

        assert_eq!(result, None);
    }

    #[test]
    fn read_entry_rejects_huge_pages_before_leaf_level() {
        let frame = PhysicalFrame::containing(PhysicalAddress::new(0x0010_0000));
        let mut table = [0_u64; PAGE_TABLE_ENTRY_COUNT];
        table[0] = PageTableEntry::present(frame, 1 << 7).raw();
        let mut source = FakePageTables::default();
        source.insert(frame, &table);

        let error =
            read_entry(&source, frame, 0, PageWalkLevel::Pd).expect_err("huge pages are deferred");

        assert_eq!(
            error,
            PageWalkError::UnsupportedHugePage {
                level: PageWalkLevel::Pd,
                entry: PageTableEntry::present(frame, 1 << 7),
            }
        );
    }

    #[test]
    fn ensure_child_table_allocates_missing_table() {
        let root = PhysicalFrame::containing(PhysicalAddress::new(0x0010_0000));
        let child = PhysicalFrame::containing(PhysicalAddress::new(0x0011_0000));
        let mut source = FakePageTables::default();
        source.insert(root, &[0; PAGE_TABLE_ENTRY_COUNT]);
        source.insert(child, &[u64::MAX; PAGE_TABLE_ENTRY_COUNT]);
        let mut allocator = FakePagingAllocator::new([child, root, root, root], 1);

        let allocated =
            ensure_child_table(&mut source, &mut allocator, root, 3, PageWalkLevel::Pml4)
                .expect("child table should be allocated");

        assert_eq!(allocated, child);
        let root_table = source.table(root).expect("root table should still exist");
        let child_table = source.table(child).expect("child table should exist");
        assert!(PageTableEntry::from_raw(root_table[3]).is_present());
        assert!(child_table.iter().all(|entry| *entry == 0));
    }

    #[test]
    fn map_4k_with_installs_leaf_mapping_and_intermediate_tables() {
        let root = PhysicalFrame::containing(PhysicalAddress::new(0x0010_0000));
        let pdpt = PhysicalFrame::containing(PhysicalAddress::new(0x0011_0000));
        let pd = PhysicalFrame::containing(PhysicalAddress::new(0x0012_0000));
        let pt = PhysicalFrame::containing(PhysicalAddress::new(0x0013_0000));
        let leaf = PhysicalFrame::containing(PhysicalAddress::new(0x0020_0000));
        let virtual_address = VirtualAddress::new(0xFFFF_8000_1234_5000);
        let indices = virtual_address.page_table_indices();
        let mut source = FakePageTables::default();
        source.insert(root, &[0; PAGE_TABLE_ENTRY_COUNT]);
        source.insert(pdpt, &[0; PAGE_TABLE_ENTRY_COUNT]);
        source.insert(pd, &[0; PAGE_TABLE_ENTRY_COUNT]);
        source.insert(pt, &[0; PAGE_TABLE_ENTRY_COUNT]);
        let mut allocator = FakePagingAllocator::new([pdpt, pd, pt, root], 3);

        PageTableRoot::new(root)
            .map_4k_with(&mut source, &mut allocator, virtual_address, leaf, 1 << 1)
            .expect("mapping should succeed");

        let root_table = source.table(root).unwrap();
        let pdpt_table = source.table(pdpt).unwrap();
        let pd_table = source.table(pd).unwrap();
        let pt_table = source.table(pt).unwrap();
        assert!(PageTableEntry::from_raw(root_table[indices.p4 as usize]).is_present());
        assert!(PageTableEntry::from_raw(pdpt_table[indices.p3 as usize]).is_present());
        assert!(PageTableEntry::from_raw(pd_table[indices.p2 as usize]).is_present());
        let leaf_entry = PageTableEntry::from_raw(pt_table[indices.p1 as usize]);
        assert!(leaf_entry.is_present());
        assert!(leaf_entry.is_writable());
        assert_eq!(leaf_entry.frame(), leaf);
    }

    #[test]
    fn map_4k_with_rejects_remapping_an_existing_leaf() {
        let root = PhysicalFrame::containing(PhysicalAddress::new(0x0010_0000));
        let pdpt = PhysicalFrame::containing(PhysicalAddress::new(0x0011_0000));
        let pd = PhysicalFrame::containing(PhysicalAddress::new(0x0012_0000));
        let pt = PhysicalFrame::containing(PhysicalAddress::new(0x0013_0000));
        let existing_leaf = PhysicalFrame::containing(PhysicalAddress::new(0x0020_0000));
        let replacement_leaf = PhysicalFrame::containing(PhysicalAddress::new(0x0021_0000));
        let virtual_address = VirtualAddress::new(0xFFFF_8000_1234_5000);
        let indices = virtual_address.page_table_indices();
        let mut source = FakePageTables::default();

        let mut root_table = [0; PAGE_TABLE_ENTRY_COUNT];
        root_table[indices.p4 as usize] = PageTableEntry::present(pdpt, 0).raw();
        source.insert(root, &root_table);

        let mut pdpt_table = [0; PAGE_TABLE_ENTRY_COUNT];
        pdpt_table[indices.p3 as usize] = PageTableEntry::present(pd, 0).raw();
        source.insert(pdpt, &pdpt_table);

        let mut pd_table = [0; PAGE_TABLE_ENTRY_COUNT];
        pd_table[indices.p2 as usize] = PageTableEntry::present(pt, 0).raw();
        source.insert(pd, &pd_table);

        let mut pt_table = [0; PAGE_TABLE_ENTRY_COUNT];
        pt_table[indices.p1 as usize] = PageTableEntry::present(existing_leaf, 0).raw();
        source.insert(pt, &pt_table);

        let mut allocator = FakePagingAllocator::new([root, root, root, root], 0);
        let error = PageTableRoot::new(root)
            .map_4k_with(
                &mut source,
                &mut allocator,
                virtual_address,
                replacement_leaf,
                1 << 1,
            )
            .expect_err("remapping should be rejected");

        assert_eq!(
            error,
            Map4kError::AlreadyMapped(PageTableEntry::present(existing_leaf, 0))
        );
    }

    #[test]
    fn unmap_4k_with_removes_existing_leaf_mapping() {
        let root = PhysicalFrame::containing(PhysicalAddress::new(0x0010_0000));
        let pdpt = PhysicalFrame::containing(PhysicalAddress::new(0x0011_0000));
        let pd = PhysicalFrame::containing(PhysicalAddress::new(0x0012_0000));
        let pt = PhysicalFrame::containing(PhysicalAddress::new(0x0013_0000));
        let leaf = PhysicalFrame::containing(PhysicalAddress::new(0x0020_0000));
        let virtual_address = VirtualAddress::new(0xFFFF_8000_1234_5000);
        let indices = virtual_address.page_table_indices();
        let mut source = FakePageTables::default();

        let mut root_table = [0; PAGE_TABLE_ENTRY_COUNT];
        root_table[indices.p4 as usize] = PageTableEntry::present(pdpt, 0).raw();
        source.insert(root, &root_table);

        let mut pdpt_table = [0; PAGE_TABLE_ENTRY_COUNT];
        pdpt_table[indices.p3 as usize] = PageTableEntry::present(pd, 0).raw();
        source.insert(pdpt, &pdpt_table);

        let mut pd_table = [0; PAGE_TABLE_ENTRY_COUNT];
        pd_table[indices.p2 as usize] = PageTableEntry::present(pt, 0).raw();
        source.insert(pd, &pd_table);

        let mut pt_table = [0; PAGE_TABLE_ENTRY_COUNT];
        pt_table[indices.p1 as usize] = PageTableEntry::present(leaf, 1 << 1).raw();
        source.insert(pt, &pt_table);

        let removed = PageTableRoot::new(root)
            .unmap_4k_with(&mut source, virtual_address)
            .expect("unmap should succeed")
            .expect("mapping should exist");

        assert_eq!(removed, PageTableEntry::present(leaf, 1 << 1));
        let pt_table = source.table(pt).unwrap();
        assert_eq!(
            PageTableEntry::from_raw(pt_table[indices.p1 as usize]).raw(),
            0
        );
    }

    #[test]
    fn unmap_4k_with_returns_none_when_leaf_is_missing() {
        let root = PhysicalFrame::containing(PhysicalAddress::new(0x0010_0000));
        let pdpt = PhysicalFrame::containing(PhysicalAddress::new(0x0011_0000));
        let pd = PhysicalFrame::containing(PhysicalAddress::new(0x0012_0000));
        let pt = PhysicalFrame::containing(PhysicalAddress::new(0x0013_0000));
        let virtual_address = VirtualAddress::new(0xFFFF_8000_1234_5000);
        let indices = virtual_address.page_table_indices();
        let mut source = FakePageTables::default();

        let mut root_table = [0; PAGE_TABLE_ENTRY_COUNT];
        root_table[indices.p4 as usize] = PageTableEntry::present(pdpt, 0).raw();
        source.insert(root, &root_table);

        let mut pdpt_table = [0; PAGE_TABLE_ENTRY_COUNT];
        pdpt_table[indices.p3 as usize] = PageTableEntry::present(pd, 0).raw();
        source.insert(pdpt, &pdpt_table);

        let mut pd_table = [0; PAGE_TABLE_ENTRY_COUNT];
        pd_table[indices.p2 as usize] = PageTableEntry::present(pt, 0).raw();
        source.insert(pd, &pd_table);

        source.insert(pt, &[0; PAGE_TABLE_ENTRY_COUNT]);

        let removed = PageTableRoot::new(root)
            .unmap_4k_with(&mut source, virtual_address)
            .expect("unmap should succeed");

        assert_eq!(removed, None);
    }

    #[test]
    fn map_translate_unmap_lifecycle_stays_coherent() {
        let root = PhysicalFrame::containing(PhysicalAddress::new(0x0010_0000));
        let pdpt = PhysicalFrame::containing(PhysicalAddress::new(0x0011_0000));
        let pd = PhysicalFrame::containing(PhysicalAddress::new(0x0012_0000));
        let pt = PhysicalFrame::containing(PhysicalAddress::new(0x0013_0000));
        let leaf = PhysicalFrame::containing(PhysicalAddress::new(0x0020_0000));
        let virtual_address = VirtualAddress::new(0xFFFF_8000_1234_5678);
        let mut source = FakePageTables::default();
        source.insert(root, &[0; PAGE_TABLE_ENTRY_COUNT]);
        source.insert(pdpt, &[0; PAGE_TABLE_ENTRY_COUNT]);
        source.insert(pd, &[0; PAGE_TABLE_ENTRY_COUNT]);
        source.insert(pt, &[0; PAGE_TABLE_ENTRY_COUNT]);
        let mut allocator = FakePagingAllocator::new([pdpt, pd, pt, root], 3);
        let root_wrapper = PageTableRoot::new(root);

        root_wrapper
            .map_4k_with(&mut source, &mut allocator, virtual_address, leaf, 1 << 1)
            .expect("mapping should succeed");

        let translation = root_wrapper
            .translate_with(&source, virtual_address)
            .expect("translate should succeed")
            .expect("mapping should exist");
        assert_eq!(
            translation.physical_address,
            PhysicalAddress::new(0x0020_0678)
        );

        let removed = root_wrapper
            .unmap_4k_with(&mut source, virtual_address)
            .expect("unmap should succeed")
            .expect("mapping should exist");
        assert_eq!(removed, PageTableEntry::present(leaf, 1 << 1));

        let translation_after = root_wrapper
            .translate_with(&source, virtual_address)
            .expect("translate should succeed after unmap");
        assert_eq!(translation_after, None);
    }

    #[test]
    fn page_table_edges_records_and_reports_count() {
        let a = PhysicalFrame::containing(PhysicalAddress::new(0x0010_0000));
        let b = PhysicalFrame::containing(PhysicalAddress::new(0x0011_0000));
        let c = PhysicalFrame::containing(PhysicalAddress::new(0x0012_0000));
        let mut edges = PageTableEdges::new();

        assert_eq!(edges.count(), 0);
        assert!(edges.record(a, b));
        assert!(edges.record(a, c));
        assert_eq!(edges.count(), 2);
    }

    #[test]
    fn frames_reachable_from_traverses_tree() {
        let root = PhysicalFrame::containing(PhysicalAddress::new(0x0010_0000));
        let child1 = PhysicalFrame::containing(PhysicalAddress::new(0x0011_0000));
        let child2 = PhysicalFrame::containing(PhysicalAddress::new(0x0012_0000));
        let grandchild = PhysicalFrame::containing(PhysicalAddress::new(0x0013_0000));
        let unrelated = PhysicalFrame::containing(PhysicalAddress::new(0x0014_0000));
        let mut edges = PageTableEdges::new();

        edges.record(root, child1);
        edges.record(root, child2);
        edges.record(child1, grandchild);
        // unrelated is not connected to root
        edges.record(unrelated, child2);

        let reachable = edges.frames_reachable_from(root);
        let reachable_addrs: [u64; PageTableEdges::CAPACITY] =
            core::array::from_fn(|i| reachable[i].map_or(0, |f| f.start_address().as_u64()));

        assert!(reachable_addrs.contains(&child1.start_address().as_u64()));
        assert!(reachable_addrs.contains(&child2.start_address().as_u64()));
        assert!(reachable_addrs.contains(&grandchild.start_address().as_u64()));
        assert!(!reachable_addrs.contains(&root.start_address().as_u64()));
        assert!(!reachable_addrs.contains(&unrelated.start_address().as_u64()));
    }

    #[test]
    fn bootstrap_paging_allocator_records_edges_on_map() {
        let usable_start = 0x0040_0000u64;
        let regions = [MemoryRegion {
            start: PhysicalAddress::new(usable_start),
            end: PhysicalAddress::new(usable_start + (PAGE_SIZE * 8)),
            kind: MemoryRegionKind::Usable,
        }];
        let kernel_image = KernelImage {
            start: VirtualAddress::new(0x0010_0000),
            end: VirtualAddress::new(0x0012_0000),
        };
        let active_root = PhysicalFrame::containing(PhysicalAddress::new(0x0030_0000));
        let mut reservations = EarlyKernelReservations::for_bootstrap(kernel_image, active_root);

        // Pre-populate fake page tables with slots for the root and three
        // intermediate frames that the allocator will hand out.
        let root_frame = PhysicalFrame::containing(PhysicalAddress::new(usable_start));
        let pdpt_frame = PhysicalFrame::containing(PhysicalAddress::new(usable_start + PAGE_SIZE));
        let pd_frame = PhysicalFrame::containing(PhysicalAddress::new(usable_start + PAGE_SIZE * 2));
        let pt_frame = PhysicalFrame::containing(PhysicalAddress::new(usable_start + PAGE_SIZE * 3));
        let leaf_frame = PhysicalFrame::containing(PhysicalAddress::new(usable_start + PAGE_SIZE * 4));

        let mut source = FakePageTables::default();
        source.insert(root_frame, &[0; PAGE_TABLE_ENTRY_COUNT]);
        source.insert(pdpt_frame, &[0; PAGE_TABLE_ENTRY_COUNT]);
        source.insert(pd_frame, &[0; PAGE_TABLE_ENTRY_COUNT]);
        source.insert(pt_frame, &[0; PAGE_TABLE_ENTRY_COUNT]);

        // Reserve the root frame so the allocator hands out pdpt, pd, pt in order.
        reservations.reserve_frame(ReservationKind::BootstrapPageTables, root_frame);

        let mut allocator =
            BootstrapPagingAllocator::new(BootMemoryMap::new(&regions), &mut reservations);

        PageTableRoot::new(root_frame)
            .map_4k_with(
                &mut source,
                &mut allocator,
                VirtualAddress::new(0xFFFF_8000_1234_5000),
                leaf_frame,
                1 << 1,
            )
            .expect("map should succeed");

        // Three intermediate tables were allocated (PDPT, PD, PT).
        assert_eq!(allocator.edges().count(), 3);

        // All intermediate frames are reachable from the root.
        let reachable = allocator.edges().frames_reachable_from(root_frame);
        let reachable_addrs: [u64; PageTableEdges::CAPACITY] =
            core::array::from_fn(|i| reachable[i].map_or(0, |f| f.start_address().as_u64()));
        assert!(reachable_addrs.contains(&pdpt_frame.start_address().as_u64()));
        assert!(reachable_addrs.contains(&pd_frame.start_address().as_u64()));
        assert!(reachable_addrs.contains(&pt_frame.start_address().as_u64()));
    }
}
