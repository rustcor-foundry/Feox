//! Bootstrap capability system for the first ASI authority lane.

#[cfg(test)]
extern crate std;

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU8, AtomicU32, Ordering};

use crate::bootabi::MemoryRegionKind;
use feox_asi::{
    CapError, CapHandle, CapInfo, CapPermissions, CapRequest, CapType, PageFlags, PhysicalAddress,
    ProcessId,
};

/// Maximum capabilities available to the bootstrap process.
pub const MAX_CAPS_PER_PROCESS: usize = 256;
/// Maximum resources tracked in the bootstrap registry.
pub const MAX_RESOURCES: usize = 512;
/// Maximum delegation nodes tracked in the bootstrap tree.
pub const MAX_DELEGATION_NODES: usize = 512;

const SLOT_FREE: u8 = 0;
const SLOT_ACTIVE: u8 = 1;

const RESOURCE_FREE: u8 = 0;
const RESOURCE_ACTIVE: u8 = 1;

const NODE_FREE: u8 = 0;
const NODE_ACTIVE: u8 = 1;

const NO_PARENT_ID: u32 = u32::MAX;
const NO_NODE_ID: u32 = u32::MAX;

/// Opaque resource index into the bootstrap resource registry.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(transparent)]
pub struct ResourceId(pub u32);

/// Opaque node index into the bootstrap delegation tree.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(transparent)]
pub struct DelegationNodeId(pub u32);

/// Resource shape tracked by the bootstrap registry.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ResourceView {
    /// Registered resource ID.
    pub id: ResourceId,
    /// Resource type.
    pub cap_type: CapType,
    /// Base physical address when relevant.
    pub base: PhysicalAddress,
    /// Resource length in bytes.
    pub size_bytes: u64,
    /// Root owning process, if any.
    pub owner: Option<ProcessId>,
    /// Number of active capabilities referencing this resource.
    pub active_cap_count: u32,
    /// Whether this resource may satisfy page-allocation requests.
    pub allocatable: bool,
    /// Bytes already consumed from this resource.
    pub allocated_bytes: u64,
}

/// One live capability view returned by verification.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CapabilityView {
    /// Shared handle.
    pub handle: CapHandle,
    /// Resource type.
    pub cap_type: CapType,
    /// Granted permission bits.
    pub permissions: CapPermissions,
    /// Registered resource identifier.
    pub resource_id: ResourceId,
    /// Delegation node attached to this capability.
    pub delegation_node: DelegationNodeId,
}

#[repr(C, align(64))]
struct CapSlot {
    generation: AtomicU32,
    state: AtomicU8,
    cap_type: CapType,
    permissions: CapPermissions,
    resource_id: ResourceId,
    delegation_node: DelegationNodeId,
    parent_id: u32,
    parent_generation: u32,
    child_count: u32,
    _pad: [u8; 28],
}

impl CapSlot {
    const fn free() -> Self {
        Self {
            generation: AtomicU32::new(0),
            state: AtomicU8::new(SLOT_FREE),
            cap_type: CapType::PhysicalMemory,
            permissions: CapPermissions::empty(),
            resource_id: ResourceId(0),
            delegation_node: DelegationNodeId(NO_NODE_ID),
            parent_id: NO_PARENT_ID,
            parent_generation: 0,
            child_count: 0,
            _pad: [0; 28],
        }
    }
}

#[repr(C)]
struct ResourceEntry {
    state: AtomicU8,
    cap_type: CapType,
    base: PhysicalAddress,
    size_bytes: u64,
    allocatable: bool,
    allocated_bytes: u64,
    owner: Option<ProcessId>,
    root_delegation: DelegationNodeId,
    active_cap_count: AtomicU32,
}

impl ResourceEntry {
    const fn free() -> Self {
        Self {
            state: AtomicU8::new(RESOURCE_FREE),
            cap_type: CapType::PhysicalMemory,
            base: PhysicalAddress(0),
            size_bytes: 0,
            allocatable: false,
            allocated_bytes: 0,
            owner: None,
            root_delegation: DelegationNodeId(NO_NODE_ID),
            active_cap_count: AtomicU32::new(0),
        }
    }
}

#[repr(C)]
struct DelegationNode {
    state: AtomicU8,
    resource_id: ResourceId,
    holder: ProcessId,
    holder_slot: u16,
    _reserved: u16,
    parent: DelegationNodeId,
    first_child: AtomicU32,
    next_sibling: AtomicU32,
}

impl DelegationNode {
    const fn free() -> Self {
        Self {
            state: AtomicU8::new(NODE_FREE),
            resource_id: ResourceId(0),
            holder: ProcessId(0),
            holder_slot: 0,
            _reserved: 0,
            parent: DelegationNodeId(NO_NODE_ID),
            first_child: AtomicU32::new(NO_NODE_ID),
            next_sibling: AtomicU32::new(NO_NODE_ID),
        }
    }
}

/// Per-process bootstrap capability table backed by a fixed slot array.
#[repr(C)]
pub struct CapabilityTable {
    slots: [CapSlot; MAX_CAPS_PER_PROCESS],
    free_stack: [u16; MAX_CAPS_PER_PROCESS],
    free_len: usize,
    active_count: u32,
    owner: ProcessId,
}

impl CapabilityTable {
    const fn empty() -> Self {
        Self {
            slots: [const { CapSlot::free() }; MAX_CAPS_PER_PROCESS],
            free_stack: [0; MAX_CAPS_PER_PROCESS],
            free_len: 0,
            active_count: 0,
            owner: ProcessId(0),
        }
    }

    fn reset(&mut self, owner: ProcessId) {
        self.owner = owner;
        self.active_count = 0;
        self.free_len = MAX_CAPS_PER_PROCESS;

        let mut index = 0usize;
        while index < MAX_CAPS_PER_PROCESS {
            self.slots[index].generation.store(0, Ordering::Relaxed);
            self.slots[index].state.store(SLOT_FREE, Ordering::Relaxed);
            self.slots[index].cap_type = CapType::PhysicalMemory;
            self.slots[index].permissions = CapPermissions::empty();
            self.slots[index].resource_id = ResourceId(0);
            self.slots[index].delegation_node = DelegationNodeId(NO_NODE_ID);
            self.slots[index].parent_id = NO_PARENT_ID;
            self.slots[index].parent_generation = 0;
            self.slots[index].child_count = 0;
            self.free_stack[index] = (MAX_CAPS_PER_PROCESS - 1 - index) as u16;
            index += 1;
        }
    }

    fn alloc_slot(&mut self) -> Option<usize> {
        if self.free_len == 0 {
            return None;
        }
        self.free_len -= 1;
        Some(self.free_stack[self.free_len] as usize)
    }

    fn free_slot(&mut self, index: usize) {
        self.free_stack[self.free_len] = index as u16;
        self.free_len += 1;
    }

    fn verify(
        &self,
        handle: CapHandle,
        required: CapPermissions,
    ) -> Result<CapabilityView, CapError> {
        let Some(slot) = self.slots.get(handle.id as usize) else {
            return Err(CapError::InvalidHandle);
        };

        let generation = slot.generation.load(Ordering::Acquire);
        if generation != handle.generation {
            return Err(CapError::GenerationMismatch);
        }
        if slot.state.load(Ordering::Acquire) != SLOT_ACTIVE {
            return Err(CapError::InvalidHandle);
        }
        if !slot.permissions.contains(required) {
            return Err(CapError::PermissionDenied);
        }

        Ok(CapabilityView {
            handle,
            cap_type: slot.cap_type,
            permissions: slot.permissions,
            resource_id: slot.resource_id,
            delegation_node: slot.delegation_node,
        })
    }

    fn list_into(&self, out: &mut [CapInfo]) -> (usize, usize) {
        let total = self.active_count as usize;
        let mut written = 0usize;
        let mut slot_index = 0usize;
        while slot_index < self.slots.len() {
            let slot = &self.slots[slot_index];
            if slot.state.load(Ordering::Acquire) == SLOT_ACTIVE && written < out.len() {
                out[written] = CapInfo {
                    handle: CapHandle {
                        id: slot_index as u32,
                        generation: slot.generation.load(Ordering::Acquire),
                    },
                    cap_type: slot.cap_type,
                    permissions: slot.permissions,
                    parent: CapHandle {
                        id: slot.parent_id,
                        generation: slot.parent_generation,
                    },
                    has_parent: u8::from(slot.parent_id != NO_PARENT_ID),
                    child_count: slot.child_count,
                    reserved: [0; 3],
                };
                written += 1;
            }
            slot_index += 1;
        }
        (written, total)
    }
}

struct CapabilityCell(UnsafeCell<CapabilityTable>);
struct ResourceRegistryCell(UnsafeCell<[ResourceEntry; MAX_RESOURCES]>);
struct DelegationTreeCell(UnsafeCell<[DelegationNode; MAX_DELEGATION_NODES]>);
struct FreeListCell(UnsafeCell<[u16; MAX_DELEGATION_NODES]>);
struct CountCell(UnsafeCell<(usize, usize)>);

unsafe impl Sync for CapabilityCell {}
unsafe impl Sync for ResourceRegistryCell {}
unsafe impl Sync for DelegationTreeCell {}
unsafe impl Sync for FreeListCell {}
unsafe impl Sync for CountCell {}

static BOOTSTRAP_TABLE: CapabilityCell = CapabilityCell(UnsafeCell::new(CapabilityTable::empty()));
static RESOURCE_REGISTRY: ResourceRegistryCell =
    ResourceRegistryCell(UnsafeCell::new([const { ResourceEntry::free() }; MAX_RESOURCES]));
static DELEGATION_TREE: DelegationTreeCell =
    DelegationTreeCell(UnsafeCell::new([const { DelegationNode::free() }; MAX_DELEGATION_NODES]));
static DELEGATION_FREE_STACK: FreeListCell = FreeListCell(UnsafeCell::new([0; MAX_DELEGATION_NODES]));
static BOOTSTRAP_COUNTS: CountCell = CountCell(UnsafeCell::new((0, 0)));

#[cfg(test)]
static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn table() -> &'static CapabilityTable {
    unsafe { &*BOOTSTRAP_TABLE.0.get() }
}

fn table_mut() -> &'static mut CapabilityTable {
    unsafe { &mut *BOOTSTRAP_TABLE.0.get() }
}

fn resources() -> &'static [ResourceEntry; MAX_RESOURCES] {
    unsafe { &*RESOURCE_REGISTRY.0.get() }
}

fn resources_mut() -> &'static mut [ResourceEntry; MAX_RESOURCES] {
    unsafe { &mut *RESOURCE_REGISTRY.0.get() }
}

fn nodes() -> &'static [DelegationNode; MAX_DELEGATION_NODES] {
    unsafe { &*DELEGATION_TREE.0.get() }
}

fn nodes_mut() -> &'static mut [DelegationNode; MAX_DELEGATION_NODES] {
    unsafe { &mut *DELEGATION_TREE.0.get() }
}

fn delegation_free_stack_mut() -> &'static mut [u16; MAX_DELEGATION_NODES] {
    unsafe { &mut *DELEGATION_FREE_STACK.0.get() }
}

fn counts() -> &'static (usize, usize) {
    unsafe { &*BOOTSTRAP_COUNTS.0.get() }
}

fn counts_mut() -> &'static mut (usize, usize) {
    unsafe { &mut *BOOTSTRAP_COUNTS.0.get() }
}

fn resource_count() -> usize {
    counts().0
}

fn delegation_free_len() -> usize {
    counts().1
}

fn set_resource_count(count: usize) {
    counts_mut().0 = count;
}

fn set_delegation_free_len(count: usize) {
    counts_mut().1 = count;
}

fn alloc_node() -> Option<DelegationNodeId> {
    let free_len = delegation_free_len();
    if free_len == 0 {
        return None;
    }
    set_delegation_free_len(free_len - 1);
    Some(DelegationNodeId(
        delegation_free_stack_mut()[free_len - 1] as u32,
    ))
}

fn free_node(node_id: DelegationNodeId) {
    let free_len = delegation_free_len();
    delegation_free_stack_mut()[free_len] = node_id.0 as u16;
    set_delegation_free_len(free_len + 1);
}

fn register_resource(
    cap_type: CapType,
    base: PhysicalAddress,
    size_bytes: u64,
    allocatable: bool,
) -> Result<ResourceId, CapError> {
    let index = resource_count();
    if index >= MAX_RESOURCES {
        return Err(CapError::OutOfMemory);
    }

    let entry = &mut resources_mut()[index];
    entry.state.store(RESOURCE_ACTIVE, Ordering::Relaxed);
    entry.cap_type = cap_type;
    entry.base = base;
    entry.size_bytes = size_bytes;
    entry.allocatable = allocatable;
    entry.allocated_bytes = 0;
    entry.owner = None;
    entry.root_delegation = DelegationNodeId(NO_NODE_ID);
    entry.active_cap_count.store(0, Ordering::Relaxed);
    set_resource_count(index + 1);
    Ok(ResourceId(index as u32))
}

fn resource_entry(id: ResourceId) -> Result<&'static ResourceEntry, CapError> {
    let Some(entry) = resources().get(id.0 as usize) else {
        return Err(CapError::ResourceNotFound);
    };
    if entry.state.load(Ordering::Acquire) != RESOURCE_ACTIVE {
        return Err(CapError::ResourceNotFound);
    }
    Ok(entry)
}

fn attach_child(parent: DelegationNodeId, child: DelegationNodeId) {
    let parent_node = &mut nodes_mut()[parent.0 as usize];
    let current_first = parent_node.first_child.load(Ordering::Acquire);
    nodes_mut()[child.0 as usize]
        .next_sibling
        .store(current_first, Ordering::Release);
    parent_node
        .first_child
        .store(child.0, Ordering::Release);
}

fn detach_child(parent: DelegationNodeId, child: DelegationNodeId) {
    let mut previous = NO_NODE_ID;
    let mut current = nodes()[parent.0 as usize].first_child.load(Ordering::Acquire);
    while current != NO_NODE_ID {
        if current == child.0 {
            let next = nodes()[current as usize].next_sibling.load(Ordering::Acquire);
            if previous == NO_NODE_ID {
                nodes_mut()[parent.0 as usize]
                    .first_child
                    .store(next, Ordering::Release);
            } else {
                nodes_mut()[previous as usize]
                    .next_sibling
                    .store(next, Ordering::Release);
            }
            nodes_mut()[current as usize]
                .next_sibling
                .store(NO_NODE_ID, Ordering::Release);
            return;
        }
        previous = current;
        current = nodes()[current as usize].next_sibling.load(Ordering::Acquire);
    }
}

fn mint_slot_for_resource(
    resource_id: ResourceId,
    permissions: CapPermissions,
    parent: Option<CapHandle>,
    parent_node: DelegationNodeId,
    holder: ProcessId,
) -> Result<CapHandle, CapError> {
    let entry = resource_entry(resource_id)?;
    let Some(slot_index) = table_mut().alloc_slot() else {
        return Err(CapError::OutOfMemory);
    };
    let Some(node_id) = alloc_node() else {
        table_mut().free_slot(slot_index);
        return Err(CapError::OutOfMemory);
    };

    let slot = &mut table_mut().slots[slot_index];
    slot.cap_type = entry.cap_type;
    slot.permissions = permissions;
    slot.resource_id = resource_id;
    slot.delegation_node = node_id;
    slot.parent_id = parent.map_or(NO_PARENT_ID, |handle| handle.id);
    slot.parent_generation = parent.map_or(0, |handle| handle.generation);
    slot.child_count = 0;
    slot.state.store(SLOT_ACTIVE, Ordering::Release);
    table_mut().active_count += 1;

    let node = &mut nodes_mut()[node_id.0 as usize];
    node.state.store(NODE_ACTIVE, Ordering::Release);
    node.resource_id = resource_id;
    node.holder = holder;
    node.holder_slot = slot_index as u16;
    node.parent = parent_node;
    node.first_child.store(NO_NODE_ID, Ordering::Release);
    node.next_sibling.store(NO_NODE_ID, Ordering::Release);

    if parent_node.0 != NO_NODE_ID {
        attach_child(parent_node, node_id);
        let parent_slot_index = nodes()[parent_node.0 as usize].holder_slot as usize;
        table_mut().slots[parent_slot_index].child_count += 1;
    } else {
        resources_mut()[resource_id.0 as usize].root_delegation = node_id;
        resources_mut()[resource_id.0 as usize].owner = Some(holder);
    }
    resources_mut()[resource_id.0 as usize]
        .active_cap_count
        .fetch_add(1, Ordering::AcqRel);

    Ok(CapHandle {
        id: slot_index as u32,
        generation: slot.generation.load(Ordering::Acquire),
    })
}

fn release_subtree(node_id: DelegationNodeId) {
    let mut child = nodes()[node_id.0 as usize].first_child.load(Ordering::Acquire);
    while child != NO_NODE_ID {
        let next = nodes()[child as usize].next_sibling.load(Ordering::Acquire);
        release_subtree(DelegationNodeId(child));
        child = next;
    }

    let node = &nodes()[node_id.0 as usize];
    let slot_index = node.holder_slot as usize;
    let resource_id = node.resource_id;
    let parent = node.parent;

    if parent.0 != NO_NODE_ID {
        detach_child(parent, node_id);
        let parent_slot_index = nodes()[parent.0 as usize].holder_slot as usize;
        table_mut().slots[parent_slot_index].child_count =
            table_mut().slots[parent_slot_index].child_count.saturating_sub(1);
    } else {
        resources_mut()[resource_id.0 as usize].root_delegation = DelegationNodeId(NO_NODE_ID);
        resources_mut()[resource_id.0 as usize].owner = None;
    }

    let slot = &mut table_mut().slots[slot_index];
    slot.state.store(SLOT_FREE, Ordering::Release);
    slot.generation.fetch_add(1, Ordering::Release);
    slot.permissions = CapPermissions::empty();
    slot.resource_id = ResourceId(0);
    slot.delegation_node = DelegationNodeId(NO_NODE_ID);
    slot.parent_id = NO_PARENT_ID;
    slot.parent_generation = 0;
    slot.child_count = 0;

    table_mut().active_count = table_mut().active_count.saturating_sub(1);
    table_mut().free_slot(slot_index);

    resources_mut()[resource_id.0 as usize]
        .active_cap_count
        .fetch_sub(1, Ordering::AcqRel);

    let node_mut = &mut nodes_mut()[node_id.0 as usize];
    node_mut.state.store(NODE_FREE, Ordering::Release);
    node_mut.resource_id = ResourceId(0);
    node_mut.holder = ProcessId(0);
    node_mut.holder_slot = 0;
    node_mut.parent = DelegationNodeId(NO_NODE_ID);
    node_mut.first_child.store(NO_NODE_ID, Ordering::Release);
    node_mut.next_sibling.store(NO_NODE_ID, Ordering::Release);
    free_node(node_id);
}

/// Initializes the bootstrap process capability system.
pub fn init_bootstrap_process(owner: ProcessId) {
    table_mut().reset(owner);

    let mut index = 0usize;
    while index < MAX_RESOURCES {
        resources_mut()[index] = ResourceEntry::free();
        index += 1;
    }
    index = 0;
    while index < MAX_DELEGATION_NODES {
        nodes_mut()[index] = DelegationNode::free();
        delegation_free_stack_mut()[index] = (MAX_DELEGATION_NODES - 1 - index) as u16;
        index += 1;
    }
    set_resource_count(0);
    set_delegation_free_len(MAX_DELEGATION_NODES);
}

/// Registers one physical-memory resource for the bootstrap registry.
pub fn register_bootstrap_memory_resource(base: PhysicalAddress, size_bytes: u64) -> Result<ResourceId, CapError> {
    register_resource(CapType::PhysicalMemory, base, size_bytes, false)
}

/// Registers one physical-memory resource and marks whether it may hand out bootstrap allocations.
pub fn register_bootstrap_memory_resource_with_kind(
    base: PhysicalAddress,
    size_bytes: u64,
    allocatable: bool,
) -> Result<ResourceId, CapError> {
    register_resource(CapType::PhysicalMemory, base, size_bytes, allocatable)
}

/// Registers one storage device as a resource so the storage ABI lane
/// can mint a `CapType::StorageDevice` root capability over it. The
/// base/size pair currently records the controller's BAR mapping for
/// future introspection; v2 dispatch only checks the `cap_type` tag.
pub fn register_bootstrap_storage_device_resource(
    bar_base: PhysicalAddress,
    bar_size: u64,
) -> Result<ResourceId, CapError> {
    register_resource(CapType::StorageDevice, bar_base, bar_size, false)
}

/// Seeds the registry from the boot memory map and returns the number of registered resources.
pub fn seed_bootstrap_resources_from_handoff(
    regions: &[crate::bootabi::MemoryRegion],
) -> Result<usize, CapError> {
    let mut registered = 0usize;
    for region in regions {
        if matches!(region.kind, MemoryRegionKind::Usable | MemoryRegionKind::Kernel) {
            register_bootstrap_memory_resource_with_kind(
                PhysicalAddress(region.start.as_u64()),
                region.end.as_u64().saturating_sub(region.start.as_u64()),
                matches!(region.kind, MemoryRegionKind::Usable),
            )?;
            registered += 1;
        }
    }
    Ok(registered)
}

/// Returns the bootstrap process owner.
#[must_use]
pub fn owner() -> ProcessId {
    table().owner
}

/// Returns the number of active capabilities in the bootstrap table.
#[must_use]
pub fn active_count() -> usize {
    table().active_count as usize
}

/// Returns the number of registered resources.
#[must_use]
pub fn resource_count_public() -> usize {
    resource_count()
}

/// Returns one resource view, if present.
#[must_use]
pub fn resource(id: ResourceId) -> Option<ResourceView> {
    let entry = resources().get(id.0 as usize)?;
    if entry.state.load(Ordering::Acquire) != RESOURCE_ACTIVE {
        return None;
    }
    Some(ResourceView {
        id,
        cap_type: entry.cap_type,
        base: entry.base,
        size_bytes: entry.size_bytes,
        owner: entry.owner,
        active_cap_count: entry.active_cap_count.load(Ordering::Acquire),
        allocatable: entry.allocatable,
        allocated_bytes: entry.allocated_bytes,
    })
}

/// Mints a bootstrap root capability for a registered resource.
pub fn mint_bootstrap_root_capability(
    resource_id: ResourceId,
    permissions: CapPermissions,
) -> Result<CapHandle, CapError> {
    mint_slot_for_resource(resource_id, permissions, None, DelegationNodeId(NO_NODE_ID), owner())
}

fn align_up(value: u64, alignment: u64) -> u64 {
    if alignment <= 1 {
        value
    } else {
        let remainder = value % alignment;
        if remainder == 0 {
            value
        } else {
            value + (alignment - remainder)
        }
    }
}

/// Fulfills the first bootstrap `cap_request` slice.
pub fn request_bootstrap_capability(request: &CapRequest) -> Result<CapHandle, CapError> {
    match *request {
        CapRequest::PhysicalPages { num_pages, flags } => request_physical_pages(num_pages, flags),
        _ => Err(CapError::ResourceNotFound),
    }
}

fn request_physical_pages(num_pages: usize, flags: PageFlags) -> Result<CapHandle, CapError> {
    if num_pages == 0 {
        return Err(CapError::OutOfMemory);
    }
    if flags.contains(PageFlags::HUGE_2M) || flags.contains(PageFlags::HUGE_1G) {
        return Err(CapError::ResourceNotFound);
    }

    let requested_bytes = (num_pages as u64).saturating_mul(crate::bootabi::PAGE_SIZE);
    let alignment = if flags.contains(PageFlags::CONTIGUOUS) {
        crate::bootabi::PAGE_SIZE
    } else {
        1
    };

    let mut index = 0usize;
    while index < resource_count() {
        let entry = &mut resources_mut()[index];
        if entry.state.load(Ordering::Acquire) == RESOURCE_ACTIVE
            && entry.cap_type == CapType::PhysicalMemory
            && entry.allocatable
        {
            let start = align_up(entry.allocated_bytes, alignment);
            if start.saturating_add(requested_bytes) <= entry.size_bytes {
                let child_base = entry.base.0.saturating_add(start);
                entry.allocated_bytes = start.saturating_add(requested_bytes);
                let child = register_resource(
                    CapType::PhysicalMemory,
                    PhysicalAddress(child_base),
                    requested_bytes,
                    false,
                )?;
                return mint_bootstrap_root_capability(
                    child,
                    CapPermissions::READ | CapPermissions::WRITE | CapPermissions::REVOKE,
                );
            }
        }
        index += 1;
    }
    Err(CapError::OutOfMemory)
}

/// Verifies a handle against the bootstrap capability table.
pub fn verify_bootstrap_handle(
    handle: CapHandle,
    required: CapPermissions,
) -> Result<CapabilityView, CapError> {
    table().verify(handle, required)
}

/// Delegates one capability to the target process.
pub fn delegate_bootstrap_handle(
    handle: CapHandle,
    target_pid: ProcessId,
    mask: CapPermissions,
) -> Result<CapHandle, CapError> {
    let parent = verify_bootstrap_handle(handle, CapPermissions::DELEGATE)?;
    if !parent.permissions.contains(mask) {
        return Err(CapError::PermissionEscalation);
    }
    if target_pid != owner() {
        return Err(CapError::ResourceBusy);
    }
    mint_slot_for_resource(
        parent.resource_id,
        mask,
        Some(handle),
        parent.delegation_node,
        target_pid,
    )
}

/// Releases one bootstrap capability, cascading through any children.
pub fn release_bootstrap_handle(handle: CapHandle) -> Result<(), CapError> {
    let view = verify_bootstrap_handle(handle, CapPermissions::REVOKE)?;
    release_subtree(view.delegation_node);
    Ok(())
}

/// Writes active capability metadata into `out` and returns `(written, total)`.
pub fn list_bootstrap_capabilities(out: &mut [CapInfo]) -> (usize, usize) {
    table().list_into(out)
}

/// Translates a capability handle to its backing physical base address
/// and length. Used by lanes that need raw physical access (e.g. the
/// storage ABI's DMA buffer translation).
///
/// Accepts `CapType::PhysicalMemory` today and will accept
/// `CapType::DmaPool` once that resource type is minted. Other types
/// (DeviceBar, IpcEndpoint, ...) are rejected with
/// `CapError::PermissionDenied` because they don't name a contiguous
/// DMA-safe physical range.
pub fn cap_to_phys_base(
    handle: CapHandle,
    required: CapPermissions,
) -> Result<(PhysicalAddress, u64), CapError> {
    let view = verify_bootstrap_handle(handle, required)?;
    match view.cap_type {
        CapType::PhysicalMemory => {}
        _ => return Err(CapError::PermissionDenied),
    }
    let res = resource(view.resource_id).ok_or(CapError::InvalidHandle)?;
    Ok((res.base, res.size_bytes))
}

#[cfg(test)]
/// Acquires the capability test lock so global bootstrap state is isolated per test.
pub fn acquire_test_lock() -> std::sync::MutexGuard<'static, ()> {
    TEST_LOCK.lock().expect("capability test lock poisoned")
}

#[cfg(test)]
mod tests {
    use super::{
        CapSlot, CapabilityTable, MAX_CAPS_PER_PROCESS, acquire_test_lock, active_count,
        delegate_bootstrap_handle, init_bootstrap_process, list_bootstrap_capabilities,
        mint_bootstrap_root_capability, register_bootstrap_memory_resource,
        register_bootstrap_memory_resource_with_kind, release_bootstrap_handle, resource,
        request_bootstrap_capability, resource_count_public, verify_bootstrap_handle,
    };
    use core::mem::size_of;
    use feox_asi::{CapError, CapPermissions, CapRequest, PageFlags, PhysicalAddress, ProcessId};

    #[test]
    fn cap_slot_stays_cache_line_sized() {
        let _guard = acquire_test_lock();
        assert_eq!(size_of::<CapSlot>(), 64);
    }

    #[test]
    fn capability_table_tracks_capacity() {
        let _guard = acquire_test_lock();
        assert!(size_of::<CapabilityTable>() >= MAX_CAPS_PER_PROCESS * size_of::<CapSlot>());
        assert_eq!(MAX_CAPS_PER_PROCESS, 256);
    }

    #[test]
    fn registering_resources_populates_registry() {
        let _guard = acquire_test_lock();
        init_bootstrap_process(ProcessId(0));
        let id = register_bootstrap_memory_resource(PhysicalAddress(0x1000), 0x4000)
            .expect("resource should register");
        let view = resource(id).expect("resource should exist");
        assert_eq!(view.base, PhysicalAddress(0x1000));
        assert_eq!(view.size_bytes, 0x4000);
        assert_eq!(resource_count_public(), 1);
    }

    #[test]
    fn mint_verify_release_round_trip_updates_generation() {
        let _guard = acquire_test_lock();
        init_bootstrap_process(ProcessId(7));
        let resource_id =
            register_bootstrap_memory_resource(PhysicalAddress(0x2000), 0x1000).expect("resource");
        let handle =
            mint_bootstrap_root_capability(resource_id, CapPermissions::all()).expect("root cap");

        let view = verify_bootstrap_handle(handle, CapPermissions::READ)
            .expect("fresh handle should verify");
        assert_eq!(view.resource_id, resource_id);

        release_bootstrap_handle(handle).expect("release should succeed");
        assert_eq!(
            verify_bootstrap_handle(handle, CapPermissions::READ),
            Err(CapError::GenerationMismatch)
        );
    }

    #[test]
    fn cap_list_reports_total_even_when_buffer_is_small() {
        let _guard = acquire_test_lock();
        init_bootstrap_process(ProcessId(0));
        let first_resource =
            register_bootstrap_memory_resource(PhysicalAddress(0x1000), 0x1000).expect("first");
        let second_resource =
            register_bootstrap_memory_resource(PhysicalAddress(0x2000), 0x1000).expect("second");
        let first =
            mint_bootstrap_root_capability(first_resource, CapPermissions::all()).expect("first");
        let second = mint_bootstrap_root_capability(
            second_resource,
            CapPermissions::READ | CapPermissions::REVOKE,
        )
        .expect("second");

        let mut infos = [feox_asi::CapInfo::default(); 1];
        let (written, total) = list_bootstrap_capabilities(&mut infos);
        assert_eq!(written, 1);
        assert_eq!(total, 2);
        assert!(infos[0].handle == first || infos[0].handle == second);
        assert_eq!(active_count(), 2);
    }

    #[test]
    fn verify_requires_requested_permission_bits() {
        let _guard = acquire_test_lock();
        init_bootstrap_process(ProcessId(0));
        let resource_id =
            register_bootstrap_memory_resource(PhysicalAddress(0x3000), 0x1000).expect("resource");
        let handle =
            mint_bootstrap_root_capability(resource_id, CapPermissions::READ).expect("handle");

        assert_eq!(
            verify_bootstrap_handle(handle, CapPermissions::WRITE),
            Err(CapError::PermissionDenied)
        );
    }

    #[test]
    fn delegation_creates_child_and_release_cascades() {
        let _guard = acquire_test_lock();
        init_bootstrap_process(ProcessId(0));
        let resource_id =
            register_bootstrap_memory_resource(PhysicalAddress(0x4000), 0x1000).expect("resource");
        let parent =
            mint_bootstrap_root_capability(resource_id, CapPermissions::all()).expect("parent");
        let child = delegate_bootstrap_handle(
            parent,
            ProcessId(0),
            CapPermissions::READ | CapPermissions::REVOKE,
        )
        .expect("child should delegate");

        let mut infos = [feox_asi::CapInfo::default(); 2];
        let (_, total) = list_bootstrap_capabilities(&mut infos);
        assert_eq!(total, 2);
        let parent_info = infos
            .iter()
            .find(|info| info.handle == parent)
            .copied()
            .expect("parent info");
        let child_info = infos
            .iter()
            .find(|info| info.handle == child)
            .copied()
            .expect("child info");
        assert_eq!(parent_info.child_count, 1);
        assert_eq!(child_info.has_parent, 1);
        assert_eq!(resource(resource_id).expect("resource").active_cap_count, 2);

        release_bootstrap_handle(parent).expect("parent release");
        assert_eq!(
            verify_bootstrap_handle(parent, CapPermissions::READ),
            Err(CapError::GenerationMismatch)
        );
        assert_eq!(
            verify_bootstrap_handle(child, CapPermissions::READ),
            Err(CapError::GenerationMismatch)
        );
        assert_eq!(resource(resource_id).expect("resource").active_cap_count, 0);
    }

    #[test]
    fn physical_page_request_consumes_allocatable_resource() {
        let _guard = acquire_test_lock();
        init_bootstrap_process(ProcessId(0));
        let root = register_bootstrap_memory_resource_with_kind(
            PhysicalAddress(0x8000),
            crate::bootabi::PAGE_SIZE * 4,
            true,
        )
        .expect("allocatable resource");

        let first = request_bootstrap_capability(&CapRequest::PhysicalPages {
            num_pages: 2,
            flags: PageFlags::CONTIGUOUS,
        })
        .expect("first request");
        let second = request_bootstrap_capability(&CapRequest::PhysicalPages {
            num_pages: 1,
            flags: PageFlags::empty(),
        })
        .expect("second request");

        let first_view =
            verify_bootstrap_handle(first, CapPermissions::READ).expect("first handle verifies");
        let second_view =
            verify_bootstrap_handle(second, CapPermissions::READ).expect("second handle verifies");

        let first_resource = resource(first_view.resource_id).expect("first child resource");
        let second_resource = resource(second_view.resource_id).expect("second child resource");
        let root_resource = resource(root).expect("root resource");

        assert_eq!(first_resource.base, PhysicalAddress(0x8000));
        assert_eq!(first_resource.size_bytes, crate::bootabi::PAGE_SIZE * 2);
        assert_eq!(
            second_resource.base,
            PhysicalAddress(0x8000 + crate::bootabi::PAGE_SIZE * 2)
        );
        assert_eq!(second_resource.size_bytes, crate::bootabi::PAGE_SIZE);
        assert_eq!(root_resource.allocated_bytes, crate::bootabi::PAGE_SIZE * 3);
        assert_eq!(
            request_bootstrap_capability(&CapRequest::PhysicalPages {
                num_pages: 2,
                flags: PageFlags::empty(),
            }),
            Err(CapError::OutOfMemory)
        );
    }
}
