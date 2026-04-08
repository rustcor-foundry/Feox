//! Bootstrap virtual-memory helpers built on capability verification.

use crate::capability;
use crate::memory::{PAGE_SIZE, PhysicalAddress, PhysicalFrame, VirtualAddress};
use crate::paging::{Map4kError, PageTableEntry, PageTableFrameAllocator, PageTableFrameMutSource, PageTableRoot};
use feox_asi::{CapError, CapHandle, CapPermissions, CapType};

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

#[cfg(test)]
mod tests {
    use super::{BootstrapMapError, map_bootstrap_physical_capability_4k};
    use crate::capability::{
        acquire_test_lock, init_bootstrap_process, mint_bootstrap_root_capability,
        register_bootstrap_memory_resource_with_kind, request_bootstrap_capability,
    };
    use crate::memory::{PAGE_SIZE, PhysicalAddress, PhysicalFrame, VirtualAddress};
    use crate::paging::{
        PAGE_TABLE_ENTRY_COUNT, PageTableFrameAllocator, PageTableFrameMutSource,
        PageTableFrameSource, PageTableRoot,
    };
    use feox_asi::{
        CapError, CapPermissions, CapRequest, PageFlags, PhysicalAddress as AsiPhysicalAddress,
        ProcessId,
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
}
