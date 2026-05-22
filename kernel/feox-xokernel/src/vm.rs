//! Bootstrap virtual-memory helpers built on capability verification.

use crate::capability;
use crate::memory::{self, PAGE_SIZE, PhysicalAddress, PhysicalFrame, VirtualAddress};
use crate::paging::{
    DirectMapPageTables, Map4kError, PageTableEntry, PageTableFrameAllocator,
    PageTableFrameMutSource, PageTableRoot, Unmap4kError,
};
use crate::runtime_context::{self, BootstrapVmMapping};
use feox_asi::{
    CapError, CapHandle, CapPermissions, CapType, MapFlags, MemError, MemMapArgs,
    MemVtoPArgs, MemVtoPBatchArgs, MappedRegion, PhysicalAddress as AsiPhysicalAddress,
};

/// Errors produced while mapping a bootstrap capability into a page table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BootstrapMapError {
    /// Capability verification failed.
    Capability(CapError),
    /// The handle does not reference physical memory.
    UnsupportedCapabilityType(CapType),
    /// The requested page lies outside the capability-backed resource.
    PageOutOfRange,
    /// The paging layer rejected the mapping.
    Paging(Map4kError),
}

impl From<CapError> for BootstrapMapError {
    fn from(value: CapError) -> Self {
        Self::Capability(value)
    }
}

impl From<Map4kError> for BootstrapMapError {
    fn from(value: Map4kError) -> Self {
        Self::Paging(value)
    }
}

/// Errors produced by the bootstrap `mem_map` / `mem_unmap` path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BootstrapVmError {
    /// The supplied capability was invalid, stale, or lacked permission.
    Capability(CapError),
    /// The supplied handle referred to an unsupported capability type.
    UnsupportedCapabilityType(CapType),
    /// Mapping flags were not supported by the current bootstrap VM lane.
    InvalidFlags,
    /// Offset or length were not page aligned.
    AlignmentViolation,
    /// Offset plus length exceeded the capability-backed resource bounds.
    OffsetOutOfRange,
    /// No free virtual region remained in the bootstrap VM window.
    OutOfVirtualSpace,
    /// No free retained mapping slot remained.
    OutOfMappingSlots,
    /// The paging layer rejected the mapping change.
    Paging(Map4kError),
    /// The requested mapping was not currently active.
    MappingNotFound,
    /// The supplied virtual address was not mapped by the supplied capability.
    AddressNotMapped,
}

impl From<CapError> for BootstrapVmError {
    fn from(value: CapError) -> Self {
        Self::Capability(value)
    }
}

impl From<Map4kError> for BootstrapVmError {
    fn from(value: Map4kError) -> Self {
        Self::Paging(value)
    }
}

impl From<BootstrapMapError> for BootstrapVmError {
    fn from(value: BootstrapMapError) -> Self {
        match value {
            BootstrapMapError::Capability(error) => Self::Capability(error),
            BootstrapMapError::UnsupportedCapabilityType(kind) => {
                Self::UnsupportedCapabilityType(kind)
            }
            BootstrapMapError::PageOutOfRange => Self::OffsetOutOfRange,
            BootstrapMapError::Paging(error) => Self::Paging(error),
        }
    }
}

impl BootstrapVmError {
    /// Converts the bootstrap VM error into the shared ASI memory error space.
    #[must_use]
    pub const fn as_mem_error(self) -> MemError {
        match self {
            Self::Capability(_)
            | Self::UnsupportedCapabilityType(_) => MemError::InvalidCapability,
            Self::InvalidFlags => MemError::InvalidFlags,
            Self::AlignmentViolation => MemError::AlignmentViolation,
            Self::OffsetOutOfRange => MemError::OffsetOutOfRange,
            Self::OutOfVirtualSpace | Self::OutOfMappingSlots => MemError::OutOfVirtualSpace,
            Self::Paging(Map4kError::OutOfTableFrames) => MemError::OutOfPhysicalMemory,
            Self::MappingNotFound | Self::AddressNotMapped => MemError::AddressNotMapped,
            Self::Paging(_) => MemError::AddressNotMapped,
        }
    }
}

fn unmap_paging_error(error: Unmap4kError) -> BootstrapVmError {
    match error {
        Unmap4kError::MissingTableFrame(_) | Unmap4kError::UnsupportedHugePage { .. } => {
            BootstrapVmError::MappingNotFound
        }
    }
}

/// Maps one 4 KiB page from a verified physical-memory capability.
///
/// This is the first capability-backed VM operation in the bootstrap lane. It
/// verifies the handle, resolves the physical page from the registered
/// resource, and installs a single 4 KiB mapping through the paging helpers.
pub fn map_bootstrap_physical_capability_4k(
    root: PageTableRoot,
    source: &mut impl PageTableFrameMutSource,
    allocator: &mut impl PageTableFrameAllocator,
    handle: CapHandle,
    virtual_address: VirtualAddress,
    page_index: usize,
    writable: bool,
    user_accessible: bool,
) -> Result<(), BootstrapMapError> {
    let required = if writable {
        CapPermissions::READ | CapPermissions::WRITE
    } else {
        CapPermissions::READ
    };
    let capability = capability::verify_bootstrap_handle(handle, required)?;
    if capability.cap_type != CapType::PhysicalMemory {
        return Err(BootstrapMapError::UnsupportedCapabilityType(
            capability.cap_type,
        ));
    }

    let resource = capability::resource(capability.resource_id)
        .ok_or(BootstrapMapError::Capability(CapError::ResourceNotFound))?;
    let offset = (page_index as u64).saturating_mul(PAGE_SIZE);
    if offset.saturating_add(PAGE_SIZE) > resource.size_bytes {
        return Err(BootstrapMapError::PageOutOfRange);
    }

    let physical_frame = PhysicalFrame::containing(PhysicalAddress::new(
        resource.base.0.saturating_add(offset),
    ));
    let mut flags = PageTableEntry::FLAG_NO_EXECUTE;
    if writable {
        flags |= 1 << 1;
    }
    if user_accessible {
        flags |= 1 << 2;
    }

    root.map_4k_with(source, allocator, virtual_address, physical_frame, flags)?;
    Ok(())
}

/// Validates whether the current bootstrap lane can honor the supplied map flags.
fn validate_bootstrap_map_flags(flags: MapFlags) -> Result<(), BootstrapVmError> {
    if !flags.contains(MapFlags::READ) {
        return Err(BootstrapVmError::InvalidFlags);
    }

    let supported = MapFlags::READ | MapFlags::WRITE;
    if (flags.0 & !supported.0) != 0 {
        return Err(BootstrapVmError::InvalidFlags);
    }

    Ok(())
}

/// Maps a capability-backed region into the retained bootstrap VM window.
pub fn mem_map_bootstrap(
    args: MemMapArgs,
) -> Result<MappedRegion, BootstrapVmError> {
    validate_bootstrap_map_flags(args.flags)?;
    if args.length_bytes == 0
        || args.offset_bytes % PAGE_SIZE != 0
        || args.length_bytes % PAGE_SIZE != 0
    {
        return Err(BootstrapVmError::AlignmentViolation);
    }

    let capability = capability::verify_bootstrap_handle(args.handle, CapPermissions::READ)?;
    if capability.cap_type != CapType::PhysicalMemory {
        return Err(BootstrapVmError::UnsupportedCapabilityType(
            capability.cap_type,
        ));
    }
    let resource = capability::resource(capability.resource_id)
        .ok_or(BootstrapVmError::Capability(CapError::ResourceNotFound))?;
    if args.offset_bytes.saturating_add(args.length_bytes) > resource.size_bytes {
        return Err(BootstrapVmError::OffsetOutOfRange);
    }

    let layout = memory::BootstrapRuntimeLayout::new();
    let region = runtime_context::allocate_vm_region(
        layout.vm_window_base().as_u64(),
        layout.vm_window_size(),
        args.length_bytes,
        args.flags,
    )
    .ok_or(BootstrapVmError::OutOfVirtualSpace)?;

    let root = PageTableRoot::active();
    let mut live_page_tables = DirectMapPageTables;
    struct NoopAllocator;
    impl PageTableFrameAllocator for NoopAllocator {
        fn allocate_table_frame(&mut self) -> Option<PhysicalFrame> {
            None
        }
    }
    let mut allocator = NoopAllocator;
    let writable = args.flags.contains(MapFlags::WRITE);
    let page_count = args.length_bytes / PAGE_SIZE;
    let offset_pages = args.offset_bytes / PAGE_SIZE;

    let mut page = 0u64;
    while page < page_count {
        let virtual_address =
            VirtualAddress::new(region.base + (page * PAGE_SIZE));
        map_bootstrap_physical_capability_4k(
            root,
            &mut live_page_tables,
            &mut allocator,
            args.handle,
            virtual_address,
            (offset_pages + page) as usize,
            writable,
            false,
        )?;
        page += 1;
    }

    runtime_context::record_vm_mapping(BootstrapVmMapping {
        region,
        handle: args.handle,
        offset_bytes: args.offset_bytes,
    })
    .map_err(|_| BootstrapVmError::OutOfMappingSlots)?;
    crate::runtime_context::push_event("bootstrap-mem-map");
    Ok(region)
}

/// Removes a retained bootstrap VM mapping.
pub fn mem_unmap_bootstrap(region: MappedRegion) -> Result<(), BootstrapVmError> {
    if region.length_bytes == 0 || region.length_bytes % PAGE_SIZE != 0 {
        return Err(BootstrapVmError::AlignmentViolation);
    }

    let Some(active) = runtime_context::remove_vm_mapping(region) else {
        return Err(BootstrapVmError::MappingNotFound);
    };

    let root = PageTableRoot::active();
    let mut live_page_tables = DirectMapPageTables;
    let page_count = active.region.length_bytes / PAGE_SIZE;
    let mut page = 0u64;
    while page < page_count {
        let virtual_address = VirtualAddress::new(active.region.base + (page * PAGE_SIZE));
        let _ = root
            .unmap_4k_with(&mut live_page_tables, virtual_address)
            .map_err(unmap_paging_error)?;
        page += 1;
    }
    crate::runtime_context::push_event("bootstrap-mem-unmap");
    Ok(())
}

/// Resolves one bootstrap VM virtual address back to its physical address.
pub fn mem_vtop_bootstrap(args: MemVtoPArgs) -> Result<AsiPhysicalAddress, BootstrapVmError> {
    let capability = capability::verify_bootstrap_handle(args.handle, CapPermissions::READ)?;
    if capability.cap_type != CapType::PhysicalMemory {
        return Err(BootstrapVmError::UnsupportedCapabilityType(
            capability.cap_type,
        ));
    }

    let mapping = runtime_context::find_vm_mapping_for_address(args.handle, args.virtual_address)
        .ok_or(BootstrapVmError::AddressNotMapped)?;
    let offset_in_mapping = args.virtual_address.saturating_sub(mapping.region.base);
    let resource = capability::resource(capability.resource_id)
        .ok_or(BootstrapVmError::Capability(CapError::ResourceNotFound))?;
    let resource_offset = mapping.offset_bytes.saturating_add(offset_in_mapping);
    if resource_offset >= resource.size_bytes {
        return Err(BootstrapVmError::OffsetOutOfRange);
    }

    #[cfg(target_os = "none")]
    {
        let root = PageTableRoot::active();
        let source = DirectMapPageTables;
        let translation = root
            .translate_with(&source, VirtualAddress::new(args.virtual_address))
            .map_err(|_| BootstrapVmError::AddressNotMapped)?
            .ok_or(BootstrapVmError::AddressNotMapped)?;
        return Ok(AsiPhysicalAddress(translation.physical_address.as_u64()));
    }

    #[cfg(not(target_os = "none"))]
    {
        Ok(AsiPhysicalAddress(
            resource.base.0.saturating_add(resource_offset),
        ))
    }
}

/// Resolves many bootstrap VM virtual addresses back to physical addresses.
pub fn mem_vtop_batch_bootstrap(args: MemVtoPBatchArgs) -> Result<usize, BootstrapVmError> {
    if args.count == 0 {
        return Ok(0);
    }
    if args.virtual_addresses.is_null() || args.physical_addresses.is_null() {
        return Err(BootstrapVmError::AddressNotMapped);
    }

    let virtual_addresses = unsafe {
        // SAFETY: the syscall layer validates the wrapper shape; this bootstrap
        // lane trusts the caller to supply `count` readable addresses.
        core::slice::from_raw_parts(args.virtual_addresses, args.count)
    };
    let physical_addresses = unsafe {
        // SAFETY: the caller supplies an output buffer with `count` writable slots.
        core::slice::from_raw_parts_mut(args.physical_addresses, args.count)
    };

    for (index, virtual_address) in virtual_addresses.iter().copied().enumerate() {
        physical_addresses[index] = mem_vtop_bootstrap(MemVtoPArgs {
            handle: args.handle,
            virtual_address,
            out_physical_address: core::ptr::null_mut(),
        })?;
    }

    crate::runtime_context::push_event("bootstrap-mem-vtop-batch");
    Ok(args.count)
}

#[cfg(test)]
mod tests {
    use super::{
        BootstrapMapError, BootstrapVmError, map_bootstrap_physical_capability_4k,
        mem_map_bootstrap, mem_unmap_bootstrap, mem_vtop_batch_bootstrap, mem_vtop_bootstrap,
    };
    use crate::capability::{
        acquire_test_lock, init_bootstrap_process, mint_bootstrap_root_capability,
        register_bootstrap_memory_resource_with_kind, request_bootstrap_capability,
    };
    use crate::memory::{self, PAGE_SIZE, PhysicalAddress, PhysicalFrame, VirtualAddress};
    use crate::paging::{
        PAGE_TABLE_ENTRY_COUNT, PageTableFrameAllocator, PageTableFrameMutSource,
        PageTableFrameSource, PageTableRoot,
    };
    use crate::runtime_context::{BootstrapVmMapping, claim_bootstrap_context, reset_for_tests};
    use feox_asi::{
        CapError, CapHandle, CapPermissions, CapRequest, CoreId, MapFlags, MemMapArgs,
        MemVtoPArgs, MemVtoPBatchArgs, MappedRegion, PageFlags,
        PhysicalAddress as AsiPhysicalAddress, ProcessId,
    };

    struct FakePageTables {
        frames: [(u64, [u64; PAGE_TABLE_ENTRY_COUNT]); 8],
        len: usize,
    }

    impl Default for FakePageTables {
        fn default() -> Self {
            Self {
                frames: [(0, [0; PAGE_TABLE_ENTRY_COUNT]); 8],
                len: 0,
            }
        }
    }

    impl FakePageTables {
        fn insert(&mut self, frame: PhysicalFrame, table: &[u64; PAGE_TABLE_ENTRY_COUNT]) {
            self.frames[self.len] = (frame.start_address().as_u64(), *table);
            self.len += 1;
        }
    }

    impl PageTableFrameSource for FakePageTables {
        fn table(&self, frame: PhysicalFrame) -> Option<&[u64; PAGE_TABLE_ENTRY_COUNT]> {
            self.frames[..self.len]
                .iter()
                .find(|(addr, _)| *addr == frame.start_address().as_u64())
                .map(|(_, table)| table)
        }
    }

    impl PageTableFrameMutSource for FakePageTables {
        fn table_mut(&mut self, frame: PhysicalFrame) -> Option<&mut [u64; PAGE_TABLE_ENTRY_COUNT]> {
            self.frames[..self.len]
                .iter_mut()
                .find(|(addr, _)| *addr == frame.start_address().as_u64())
                .map(|(_, table)| table)
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
    fn capability_backed_map_installs_physical_page() {
        let _guard = acquire_test_lock();
        init_bootstrap_process(ProcessId(0));
        register_bootstrap_memory_resource_with_kind(
            AsiPhysicalAddress(0x8000),
            PAGE_SIZE * 4,
            true,
        )
        .expect("resource");
        let handle = request_bootstrap_capability(&CapRequest::PhysicalPages {
            num_pages: 1,
            flags: PageFlags::CONTIGUOUS,
        })
        .expect("requested handle");

        let root = PhysicalFrame::containing(PhysicalAddress::new(0x1000_0000));
        let pdpt = PhysicalFrame::containing(PhysicalAddress::new(0x1001_0000));
        let pd = PhysicalFrame::containing(PhysicalAddress::new(0x1002_0000));
        let pt = PhysicalFrame::containing(PhysicalAddress::new(0x1003_0000));
        let mut source = FakePageTables::default();
        source.insert(root, &[0; PAGE_TABLE_ENTRY_COUNT]);
        source.insert(pdpt, &[0; PAGE_TABLE_ENTRY_COUNT]);
        source.insert(pd, &[0; PAGE_TABLE_ENTRY_COUNT]);
        source.insert(pt, &[0; PAGE_TABLE_ENTRY_COUNT]);
        let mut allocator = FakePagingAllocator::new([pdpt, pd, pt, root], 3);
        let root_wrapper = PageTableRoot::new(root);
        let virtual_address = VirtualAddress::new(0xFFFF_8000_5555_5000);

        map_bootstrap_physical_capability_4k(
            root_wrapper,
            &mut source,
            &mut allocator,
            handle,
            virtual_address,
            0,
            true,
            true,
        )
        .expect("mapping should succeed");

        let translation = root_wrapper
            .translate_with(&source, VirtualAddress::new(virtual_address.as_u64() + 0x678))
            .expect("translate should succeed")
            .expect("mapping should exist");
        assert_eq!(translation.physical_address, PhysicalAddress::new(0x8678));
        assert!(translation.entry.is_present());
        assert!(translation.entry.is_writable());
        assert!(translation.entry.is_user());
    }

    #[test]
    fn mapping_requires_write_permission_for_writable_pages() {
        let _guard = acquire_test_lock();
        init_bootstrap_process(ProcessId(0));
        let resource = register_bootstrap_memory_resource_with_kind(
            AsiPhysicalAddress(0xA000),
            PAGE_SIZE,
            false,
        )
        .expect("resource");
        let handle =
            mint_bootstrap_root_capability(resource, CapPermissions::READ).expect("read-only cap");

        let root = PhysicalFrame::containing(PhysicalAddress::new(0x2000_0000));
        let pdpt = PhysicalFrame::containing(PhysicalAddress::new(0x2001_0000));
        let pd = PhysicalFrame::containing(PhysicalAddress::new(0x2002_0000));
        let pt = PhysicalFrame::containing(PhysicalAddress::new(0x2003_0000));
        let mut source = FakePageTables::default();
        source.insert(root, &[0; PAGE_TABLE_ENTRY_COUNT]);
        source.insert(pdpt, &[0; PAGE_TABLE_ENTRY_COUNT]);
        source.insert(pd, &[0; PAGE_TABLE_ENTRY_COUNT]);
        source.insert(pt, &[0; PAGE_TABLE_ENTRY_COUNT]);
        let mut allocator = FakePagingAllocator::new([pdpt, pd, pt, root], 3);

        let error = map_bootstrap_physical_capability_4k(
            PageTableRoot::new(root),
            &mut source,
            &mut allocator,
            handle,
            VirtualAddress::new(0xFFFF_8000_6666_6000),
            0,
            true,
            false,
        )
        .expect_err("writable mapping should be rejected");

        assert_eq!(
            error,
            BootstrapMapError::Capability(CapError::PermissionDenied)
        );
    }

    #[test]
    fn bootstrap_mem_map_rejects_unsupported_flags() {
        let _guard = acquire_test_lock();
        claim_bootstrap_context(CoreId(0));
        let error = mem_map_bootstrap(MemMapArgs {
            handle: CapHandle::default(),
            offset_bytes: 0,
            length_bytes: PAGE_SIZE,
            flags: MapFlags::READ | MapFlags::UNCACHEABLE,
            out_region: core::ptr::null_mut(),
        })
        .expect_err("unsupported flags should be rejected");

        assert_eq!(error, BootstrapVmError::InvalidFlags);
    }

    #[test]
    fn bootstrap_mem_unmap_rejects_unknown_region() {
        let _guard = acquire_test_lock();
        claim_bootstrap_context(CoreId(0));
        let error = mem_unmap_bootstrap(MappedRegion {
            base: memory::BOOTSTRAP_VM_WINDOW_BASE,
            length_bytes: PAGE_SIZE,
            flags: MapFlags::READ,
        })
        .expect_err("unknown mapping should be rejected");

        assert_eq!(error, BootstrapVmError::MappingNotFound);
    }

    #[test]
    fn bootstrap_mem_vtop_resolves_recorded_mapping() {
        let _guard = acquire_test_lock();
        reset_for_tests();
        claim_bootstrap_context(CoreId(0));
        init_bootstrap_process(ProcessId(0));
        let resource_id = register_bootstrap_memory_resource_with_kind(
            AsiPhysicalAddress(0x4000),
            PAGE_SIZE * 4,
            true,
        )
        .expect("resource");
        let handle =
            mint_bootstrap_root_capability(resource_id, CapPermissions::READ).expect("handle");
        crate::runtime_context::record_vm_mapping(BootstrapVmMapping {
            region: MappedRegion {
                base: memory::BOOTSTRAP_VM_WINDOW_BASE,
                length_bytes: PAGE_SIZE * 2,
                flags: MapFlags::READ,
            },
            handle,
            offset_bytes: PAGE_SIZE,
        })
        .expect("mapping");

        let physical = mem_vtop_bootstrap(MemVtoPArgs {
            handle,
            virtual_address: memory::BOOTSTRAP_VM_WINDOW_BASE + 0x1800,
            out_physical_address: core::ptr::null_mut(),
        })
        .expect("translation");

        assert_eq!(physical, AsiPhysicalAddress(0x4000 + PAGE_SIZE + 0x1800));
    }

    #[test]
    fn bootstrap_mem_vtop_batch_resolves_multiple_addresses() {
        let _guard = acquire_test_lock();
        reset_for_tests();
        claim_bootstrap_context(CoreId(0));
        init_bootstrap_process(ProcessId(0));
        let resource_id = register_bootstrap_memory_resource_with_kind(
            AsiPhysicalAddress(0x8000),
            PAGE_SIZE * 4,
            true,
        )
        .expect("resource");
        let handle =
            mint_bootstrap_root_capability(resource_id, CapPermissions::READ).expect("handle");
        crate::runtime_context::record_vm_mapping(BootstrapVmMapping {
            region: MappedRegion {
                base: memory::BOOTSTRAP_VM_WINDOW_BASE,
                length_bytes: PAGE_SIZE * 2,
                flags: MapFlags::READ,
            },
            handle,
            offset_bytes: 0,
        })
        .expect("mapping");
        let inputs = [
            memory::BOOTSTRAP_VM_WINDOW_BASE + 0x100,
            memory::BOOTSTRAP_VM_WINDOW_BASE + PAGE_SIZE + 0x200,
        ];
        let mut outputs = [AsiPhysicalAddress(0); 2];

        let written = mem_vtop_batch_bootstrap(MemVtoPBatchArgs {
            handle,
            virtual_addresses: inputs.as_ptr(),
            physical_addresses: outputs.as_mut_ptr(),
            count: inputs.len(),
        })
        .expect("batch");

        assert_eq!(written, 2);
        assert_eq!(outputs[0], AsiPhysicalAddress(0x8100));
        assert_eq!(outputs[1], AsiPhysicalAddress(0x9200));
    }
}
