#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]

//! Shared boot handoff ABI and physical memory descriptors for Feox.

use core::slice;

/// Base `x86_64` page size in bytes.
pub const PAGE_SIZE: u64 = 4096;

/// Physical address wrapper.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Hash)]
#[repr(transparent)]
pub struct PhysicalAddress(u64);

impl PhysicalAddress {
    /// Creates a physical address from a raw integer.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Returns the raw integer value.
    #[must_use]
    pub const fn as_u64(self) -> u64 {
        self.0
    }

    /// Rounds the address up to the next page boundary.
    #[must_use]
    pub const fn align_up(self) -> Self {
        Self((self.0 + (PAGE_SIZE - 1)) & !(PAGE_SIZE - 1))
    }

    /// Rounds the address down to the previous page boundary.
    #[must_use]
    pub const fn align_down(self) -> Self {
        Self(self.0 & !(PAGE_SIZE - 1))
    }
}

/// Semantic type for memory regions supplied by firmware or a bootloader.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(u32)]
pub enum MemoryRegionKind {
    /// General-purpose RAM that the kernel may eventually allocate from.
    Usable = 1,
    /// The loaded kernel image.
    Kernel = 2,
    /// Reserved memory that must not be handed out.
    #[default]
    Reserved = 3,
    /// Memory-mapped I/O range.
    Mmio = 4,
    /// Bootloader-owned memory that may become reclaimable later.
    BootloaderReclaimable = 5,
}

impl MemoryRegionKind {
    /// Returns whether the region can be handed out by the early allocator.
    #[must_use]
    pub const fn is_usable(self) -> bool {
        matches!(self, Self::Usable)
    }
}

/// Half-open physical memory region `[start, end)`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(C)]
pub struct MemoryRegion {
    /// Inclusive start address.
    pub start: PhysicalAddress,
    /// Exclusive end address.
    pub end: PhysicalAddress,
    /// Semantic type for the region.
    pub kind: MemoryRegionKind,
}

impl MemoryRegion {
    /// Creates a region from explicit bounds and kind.
    #[must_use]
    pub const fn new(start: PhysicalAddress, end: PhysicalAddress, kind: MemoryRegionKind) -> Self {
        Self { start, end, kind }
    }

    /// Creates an empty reserved region.
    #[must_use]
    pub const fn empty() -> Self {
        Self::new(
            PhysicalAddress::new(0),
            PhysicalAddress::new(0),
            MemoryRegionKind::Reserved,
        )
    }

    /// Returns the region size in bytes.
    #[must_use]
    pub const fn size_bytes(self) -> u64 {
        self.end.as_u64().saturating_sub(self.start.as_u64())
    }

    /// Returns whether the region has no size.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.start.as_u64() >= self.end.as_u64()
    }
}

/// Feox boot handoff magic, encoded as the ASCII bytes `FEOXBOOT`.
pub const BOOT_INFO_MAGIC: u64 = u64::from_le_bytes(*b"FEOXBOOT");

/// Current boot handoff ABI version.
///
/// v3 (2026-05-22): adds [`BootInfo::ap_trampoline_phys`], the
/// physical address of a single 4 KiB page below 1 MiB reserved by
/// the loader for use as the AP boot trampoline target (SIPI vector
/// must address sub-1-MiB physical memory). Loaders that cannot
/// allocate such a page set this to 0.
///
/// v2 (2026-05-22): adds [`BootInfo::rsdp_phys`].
pub const BOOT_INFO_VERSION: u32 = 3;

/// Raw boot handoff structure passed to the kernel entrypoint in `rdi`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct BootInfo {
    /// ABI magic used to reject unknown handoff formats.
    pub magic: u64,
    /// ABI version for forward-compatibility checks.
    pub version: u32,
    /// Reserved for future flags.
    pub flags: u32,
    /// Pointer to the memory map region array.
    pub memory_map_ptr: *const MemoryRegion,
    /// Number of entries in the memory map.
    pub memory_map_len: usize,
    /// Physical address of the ACPI 2.0 RSDP, or 0 if the loader
    /// could not locate one (e.g. legacy BIOS / no ACPI table).
    pub rsdp_phys: u64,
    /// Physical address of a 4 KiB page below 1 MiB the loader has
    /// reserved for the AP boot trampoline. The kernel writes the
    /// 16-bit trampoline + an AP-status word here and points the
    /// LAPIC SIPI vector at `ap_trampoline_phys >> 12`. Zero if the
    /// loader could not allocate such a page.
    pub ap_trampoline_phys: u64,
}

impl BootInfo {
    /// Creates an empty boot handoff placeholder.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            magic: BOOT_INFO_MAGIC,
            version: BOOT_INFO_VERSION,
            flags: 0,
            memory_map_ptr: core::ptr::null(),
            memory_map_len: 0,
            rsdp_phys: 0,
            ap_trampoline_phys: 0,
        }
    }

    /// Creates a boot info structure from a memory map slice, the
    /// ACPI RSDP physical address (zero if not provided), and the AP
    /// trampoline frame physical address (zero if not allocated).
    #[must_use]
    pub const fn new(
        memory_map: &[MemoryRegion],
        rsdp_phys: u64,
        ap_trampoline_phys: u64,
    ) -> Self {
        Self {
            magic: BOOT_INFO_MAGIC,
            version: BOOT_INFO_VERSION,
            flags: 0,
            memory_map_ptr: memory_map.as_ptr(),
            memory_map_len: memory_map.len(),
            rsdp_phys,
            ap_trampoline_phys,
        }
    }
}

/// Validated boot handoff view.
#[derive(Clone, Copy, Debug)]
pub struct BootHandoff<'a> {
    memory_map: &'a [MemoryRegion],
    rsdp_phys: u64,
    ap_trampoline_phys: u64,
}

impl<'a> BootHandoff<'a> {
    /// Validates and decodes a raw boot info pointer.
    ///
    /// # Safety
    ///
    /// `ptr` must be either null or point to a valid [`BootInfo`] structure
    /// mapped into the current address space for the duration of the returned
    /// handoff view.
    #[must_use]
    pub unsafe fn from_ptr(ptr: *const BootInfo) -> Option<Self> {
        // SAFETY: the caller guarantees that `ptr` is either null or points to
        // a valid `BootInfo` in the active address space.
        let info = unsafe { ptr.as_ref() }?;
        if info.magic != BOOT_INFO_MAGIC || info.version != BOOT_INFO_VERSION {
            return None;
        }

        let memory_map = if info.memory_map_len == 0 {
            &[]
        } else {
            if info.memory_map_ptr.is_null() {
                return None;
            }

            // SAFETY: the validated boot info points at a memory-map array
            // whose length is carried in the same handoff structure.
            unsafe { slice::from_raw_parts(info.memory_map_ptr, info.memory_map_len) }
        };

        Some(Self {
            memory_map,
            rsdp_phys: info.rsdp_phys,
            ap_trampoline_phys: info.ap_trampoline_phys,
        })
    }

    /// Returns the bootloader-supplied physical memory map.
    #[must_use]
    pub const fn memory_map(self) -> &'a [MemoryRegion] {
        self.memory_map
    }

    /// Returns the bootloader-supplied ACPI RSDP physical address, or
    /// `None` if the loader could not locate one.
    #[must_use]
    pub const fn rsdp_phys(self) -> Option<u64> {
        if self.rsdp_phys == 0 {
            None
        } else {
            Some(self.rsdp_phys)
        }
    }

    /// Returns the bootloader-supplied AP trampoline frame physical
    /// address, or `None` if the loader could not allocate one.
    #[must_use]
    pub const fn ap_trampoline_phys(self) -> Option<u64> {
        if self.ap_trampoline_phys == 0 {
            None
        } else {
            Some(self.ap_trampoline_phys)
        }
    }

    /// Returns the total number of bytes marked usable in the memory map.
    #[must_use]
    pub fn usable_bytes(self) -> u64 {
        self.memory_map
            .iter()
            .filter(|region| region.kind.is_usable())
            .map(|region| region.size_bytes())
            .sum()
    }

    /// Returns the highest exclusive physical address described by the map.
    #[must_use]
    pub fn highest_physical_address(self) -> Option<PhysicalAddress> {
        self.memory_map.iter().map(|region| region.end).max()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BOOT_INFO_MAGIC, BOOT_INFO_VERSION, BootHandoff, BootInfo, MemoryRegion, MemoryRegionKind,
        PAGE_SIZE, PhysicalAddress,
    };

    #[test]
    fn physical_address_alignment_tracks_page_size() {
        let address = PhysicalAddress::new(PAGE_SIZE + 7);
        assert_eq!(address.align_down().as_u64(), PAGE_SIZE);
        assert_eq!(address.align_up().as_u64(), PAGE_SIZE * 2);
    }

    #[test]
    fn boot_handoff_rejects_bad_magic() {
        let info = BootInfo {
            magic: 0,
            version: BOOT_INFO_VERSION,
            flags: 0,
            memory_map_ptr: core::ptr::null(),
            memory_map_len: 0,
            rsdp_phys: 0,
            ap_trampoline_phys: 0,
        };

        // SAFETY: `info` lives for the duration of this test and points to a
        // valid local `BootInfo`.
        let handoff = unsafe { BootHandoff::from_ptr(&raw const info) };
        assert!(handoff.is_none());
    }

    #[test]
    fn boot_handoff_exposes_memory_map_and_summary() {
        let regions = [
            MemoryRegion::new(
                PhysicalAddress::new(0x1000),
                PhysicalAddress::new(0x3000),
                MemoryRegionKind::Usable,
            ),
            MemoryRegion::new(
                PhysicalAddress::new(0x4000),
                PhysicalAddress::new(0x5000),
                MemoryRegionKind::Reserved,
            ),
        ];
        let info = BootInfo {
            magic: BOOT_INFO_MAGIC,
            version: BOOT_INFO_VERSION,
            flags: 0,
            memory_map_ptr: regions.as_ptr(),
            memory_map_len: regions.len(),
            rsdp_phys: 0xDEAD_BEEF_F000,
            ap_trampoline_phys: 0x8000,
        };

        // SAFETY: `info` lives for the duration of this test and points to a
        // valid local `BootInfo`.
        let handoff = unsafe { BootHandoff::from_ptr(&raw const info) }.expect("valid handoff");
        assert_eq!(handoff.memory_map(), &regions);
        assert_eq!(handoff.usable_bytes(), 0x2000);
        assert_eq!(
            handoff
                .highest_physical_address()
                .map(PhysicalAddress::as_u64),
            Some(0x5000)
        );
        assert_eq!(handoff.rsdp_phys(), Some(0xDEAD_BEEF_F000));
        assert_eq!(handoff.ap_trampoline_phys(), Some(0x8000));
    }

    #[test]
    fn boot_handoff_optional_fields_zero_reports_none() {
        let info = BootInfo::new(&[], 0, 0);
        let handoff = unsafe { BootHandoff::from_ptr(&raw const info) }.expect("valid handoff");
        assert_eq!(handoff.rsdp_phys(), None);
        assert_eq!(handoff.ap_trampoline_phys(), None);
    }
}
