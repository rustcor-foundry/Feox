# Aether Capability System -- Kernel Implementation Design

## Version 0.1.0 -- Draft

---

## 0. Scope and Audience

This document describes the kernel-internal implementation of the capability
system specified in ASI-SPEC.md. It covers data structures, algorithms,
concurrency strategies, and IOMMU integration. It is intended for kernel
developers working on the Aether exokernel.

This is a design document, not a code walkthrough. Rust snippets define
structure layouts and key interfaces; they are not final implementation.

---

## 1. Internal Data Structures

### 1.1 Per-Process Capability Table

Each process owns a `CapabilityTable` that maps integer handles to capability
entries. User-space holds `CapHandle` values containing a slot index and a
generation counter. The kernel validates both on every use.

**Design choice: slot array vs. hash map.** A flat array indexed by slot ID
gives O(1) lookup with no hashing overhead. The tradeoff is a fixed maximum
capability count per process, but in practice exokernel processes hold tens of
capabilities (one per device BAR, DMA pool, MSI-X vector), not thousands. A
256-slot table at 128 bytes per entry costs 32 KiB per process -- acceptable.

```rust
/// Per-process capability table. Lives in kernel memory, one per process.
/// Indexed by the `id` field of CapHandle.
#[repr(C)]
pub struct CapabilityTable {
    /// Fixed-size array of capability slots.
    slots: [CapSlot; MAX_CAPS_PER_PROCESS],
    /// Number of slots currently in use (for cap_list iteration).
    active_count: u32,
    /// Owning process.
    owner: ProcessId,
}

/// Maximum capabilities per process. 256 is generous for an exokernel
/// where each cap represents a discrete hardware resource.
pub const MAX_CAPS_PER_PROCESS: usize = 256;

/// A single slot in the capability table.
#[repr(C)]
pub struct CapSlot {
    /// Current generation counter for this slot. Incremented on every
    /// revocation. A CapHandle is valid only if its generation matches.
    generation: AtomicU32,
    /// Slot state: Free, Active, or PendingRevocation.
    state: AtomicU8,
    /// Type of resource this capability grants access to.
    cap_type: CapType,
    /// Permission bits (READ, WRITE, DELEGATE, REVOKE).
    permissions: CapPermissions,
    /// Index into the global ResourceRegistry. Identifies the underlying
    /// physical resource (memory region, device BAR, MSI-X vector, etc.).
    resource_id: ResourceId,
    /// Index into the DelegationTree. Links this cap to its parent and
    /// children for cascading revocation.
    delegation_node: DelegationNodeId,
    /// Padding to align each slot to 64 bytes (one cache line) to avoid
    /// false sharing between slots accessed by different cores.
    _pad: [u8; 32],
}

/// Slot states.
const SLOT_FREE: u8 = 0;
const SLOT_ACTIVE: u8 = 1;
const SLOT_PENDING_REVOCATION: u8 = 2;
```

**Slot allocation.** Free slots are tracked via a per-table free list
(a stack of indices). Allocation pops from the stack; deallocation pushes.
The free list itself is a simple array of `u16` indices with an atomic
stack pointer, so allocation is lock-free in the common case (single owner
process). Cross-process delegation requires the kernel to allocate in the
*target* process's table, which takes a short spinlock on that table's
free list.

**Generation counter mechanics.** When a slot is freed (via `cap_release`
or cascading revocation), the generation counter is incremented atomically.
The slot transitions to `SLOT_FREE`. Any subsequent `cap_verify` with a
stale generation returns `CapError::GenerationMismatch` in constant time --
no tree walk, no lock, just a single atomic load and comparison.

### 1.2 Global Resource Registry

The resource registry tracks every physical resource the kernel knows about:
physical memory regions, PCI BAR mappings, MSI-X vectors, IPC endpoints. It
is the single source of truth for "what exists and who owns it."

```rust
/// Global registry of all physical resources known to the kernel.
/// Populated at boot by PCI enumeration and physical memory detection.
#[repr(C)]
pub struct ResourceRegistry {
    /// All registered resources, indexed by ResourceId.
    entries: [ResourceEntry; MAX_RESOURCES],
    /// Number of registered resources.
    count: u32,
    /// Lock for registration/deregistration (boot-time and hot-plug only).
    /// Not taken on the read path.
    write_lock: SpinLock,
}

pub const MAX_RESOURCES: usize = 4096;

/// Opaque index into the ResourceRegistry.
#[repr(transparent)]
pub struct ResourceId(pub u32);

/// A single resource tracked by the kernel.
#[repr(C)]
pub struct ResourceEntry {
    /// What kind of resource this is.
    resource_type: ResourceType,
    /// Resource-specific data.
    data: ResourceData,
    /// Which process currently holds the root capability for this resource.
    /// None if unclaimed.
    owner: Option<ProcessId>,
    /// Root delegation node for this resource's delegation tree.
    root_delegation: DelegationNodeId,
    /// Reference count of active capabilities (root + all delegates).
    /// Used for debugging and cap_list; not on the verification hot path.
    active_cap_count: AtomicU32,
}

#[repr(C)]
pub enum ResourceType {
    PhysicalMemory,
    DeviceBar,
    DmaPool,
    MsixVector,
    IpcEndpoint,
}

/// Resource-specific metadata. Sized as a union to keep ResourceEntry
/// at a fixed size for array layout.
#[repr(C)]
pub union ResourceData {
    pub physical_memory: PhysicalMemoryResource,
    pub device_bar: DeviceBarResource,
    pub dma_pool: DmaPoolResource,
    pub msix_vector: MsixVectorResource,
    pub ipc_endpoint: IpcEndpointResource,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct PhysicalMemoryResource {
    pub base: PhysicalAddress,
    pub size_bytes: usize,
    pub flags: PageFlags,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct DeviceBarResource {
    pub pci_addr: PciAddress,
    pub bar_index: u8,
    pub base_physical: PhysicalAddress,
    pub size: usize,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct DmaPoolResource {
    pub base_physical: PhysicalAddress,
    pub size_bytes: usize,
    pub device: PciAddress,
    /// IOMMU domain ID assigned to this device.
    pub iommu_domain: u16,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct MsixVectorResource {
    pub pci_addr: PciAddress,
    pub vector: u16,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct IpcEndpointResource {
    pub name: AsiString,
}
```

**Why a flat array?** Resources are enumerated at boot and rarely change
(hot-plug is a future extension). A flat array gives cache-friendly
sequential access during enumeration and O(1) lookup by `ResourceId`.
4096 entries at ~128 bytes each is 512 KiB -- a fixed, predictable
kernel memory cost.

**Ownership model.** The `owner` field records which process holds the
root capability. Only one process can be the root owner of a resource
(exclusive ownership for BARs and MSI-X vectors; shared ownership for
physical memory is handled by delegation). When the root owner releases
or exits, cascading revocation clears all delegates.

### 1.3 Delegation Tree

Capability delegation forms a forest of trees (one tree per resource).
Each tree is rooted at the process that holds the root capability. Child
nodes represent delegated capabilities in other processes.

The tree must support two operations efficiently:

1. **Parent lookup** (for permission validation during delegation): O(1).
2. **Subtree walk** (for cascading revocation): O(n) in the number of
   descendants, which is unavoidable.

```rust
/// Pool-allocated delegation tree nodes. All trees for all resources
/// share one global pool to avoid per-resource allocation overhead.
#[repr(C)]
pub struct DelegationTree {
    /// Node pool. Indexed by DelegationNodeId.
    nodes: [DelegationNode; MAX_DELEGATION_NODES],
    /// Free list head (singly-linked via next_sibling when free).
    free_head: AtomicU32,
    /// Total nodes in use.
    active_count: AtomicU32,
}

pub const MAX_DELEGATION_NODES: usize = 8192;

/// Opaque index into the DelegationTree node pool.
#[repr(transparent)]
#[derive(Copy, Clone, PartialEq, Eq)]
pub struct DelegationNodeId(pub u32);

/// Sentinel value meaning "no node."
pub const DELEGATION_NODE_NONE: DelegationNodeId = DelegationNodeId(u32::MAX);

/// A node in the delegation tree. Uses a left-child/right-sibling
/// representation for memory efficiency and bounded allocation.
#[repr(C)]
pub struct DelegationNode {
    /// The resource this delegation refers to.
    resource_id: ResourceId,
    /// Process that holds this delegated capability.
    holder: ProcessId,
    /// Slot index in the holder's CapabilityTable.
    holder_slot: u16,
    /// Parent node (the capability this was delegated from).
    parent: DelegationNodeId,
    /// First child (first process this was delegated to).
    first_child: AtomicU32,  // DelegationNodeId stored as u32 for atomics
    /// Next sibling (next delegate from the same parent).
    next_sibling: AtomicU32, // DelegationNodeId stored as u32 for atomics
    /// Node state: Free, Active, Revoking.
    state: AtomicU8,
}
```

**Left-child/right-sibling representation.** Each node stores a pointer to
its first child and its next sibling. This is the standard technique for
representing trees with arbitrary branching factor using only two pointers
per node. Walking all children of a node means following `first_child` then
iterating `next_sibling`. Walking the full subtree is a depth-first traversal.

**Why a global pool?** Per-resource allocation would require either a
heap allocator (unacceptable in a `no_std` kernel) or per-resource pools
(wastes memory when delegation depth varies across resources). A single
global pool with free-list allocation is simple, bounded, and predictable.
8192 nodes at ~48 bytes each costs ~384 KiB.

**Tradeoff: pool sizing.** 8192 nodes supports a maximum of 8192
simultaneous delegations across all resources and all processes. If the
system runs 32 processes each delegating 10 capabilities on average, that
is 320 delegation nodes -- well within budget. The constant can be tuned
at compile time.

### 1.4 Generation Counters: Preventing Use-After-Revoke

The generation counter is the keystone of stale-handle detection. The
mechanism works as follows:

1. When a capability is created in slot `i`, the slot's generation counter
   `g` is recorded in the returned `CapHandle { id: i, generation: g }`.

2. On every ASI call that takes a `CapHandle`, the kernel performs:
   ```rust
   fn cap_verify(table: &CapabilityTable, handle: CapHandle) -> Result<&CapSlot, CapError> {
       let slot = &table.slots[handle.id as usize];
       let current_gen = slot.generation.load(Ordering::Acquire);
       if current_gen != handle.generation {
           return Err(CapError::GenerationMismatch);
       }
       if slot.state.load(Ordering::Acquire) != SLOT_ACTIVE {
           return Err(CapError::InvalidHandle);
       }
       Ok(slot)
   }
   ```

3. When a capability is revoked, the kernel atomically increments the
   generation counter and sets the state to `SLOT_FREE`. Any racing
   `cap_verify` either sees the old generation (and proceeds -- the
   operation completes before the revocation takes effect) or sees the
   new generation (and fails immediately).

**Why Acquire ordering?** The `Acquire` load on the generation counter
synchronizes-with the `Release` store done during revocation. This ensures
that if `cap_verify` sees the pre-revocation generation, it also sees the
complete, consistent slot data that was written before the capability was
created. Without this ordering, the verifier could read a stale
`resource_id` from a recycled slot.

**Overflow.** The generation counter is `u32`, giving 4 billion
revocations per slot before wraparound. At 1 million revocations per
second (implausible in practice), this takes ~72 minutes. In reality,
capabilities are acquired at setup time and rarely revoked. If paranoia
demands it, the counter can be widened to `u64` at the cost of 4 bytes
per slot.

---

## 2. Lock-Free Design for the Hot Path

### 2.1 Identifying the Hot Path

The ASI is designed as a setup-time interface. However, `cap_verify` is
called internally on every ASI operation that references a capability:
`mem_map`, `mem_unmap`, `mem_vtop`, `irq_attach`, `irq_detach`,
`cap_delegate`, `cap_release`. During application startup, these may be
called in rapid succession (especially via `asi_batch`).

The critical insight: `cap_verify` is read-only. It reads a generation
counter and a state byte. It never writes. This is the ideal case for
lock-free concurrent access.

### 2.2 Read-Side: Atomic Loads Only

`cap_verify` performs two atomic loads with `Acquire` ordering:

1. `slot.generation.load(Ordering::Acquire)` -- 4-byte aligned read, single
   instruction on x86-64 (`mov`).
2. `slot.state.load(Ordering::Acquire)` -- 1-byte aligned read.

On x86-64, all aligned loads are already acquire-ordered due to the TSO
memory model. The `Acquire` annotation generates no additional fence
instructions. **cap_verify is two plain memory loads on x86-64.**

There are no locks, no CAS loops, no retries. Multiple cores can verify
capabilities concurrently with zero contention.

### 2.3 Write-Side: Revocation

Revocation (the write side) is rare relative to verification. When it
occurs, the kernel must:

1. Set `slot.state` to `SLOT_PENDING_REVOCATION` (Relaxed store is fine;
   readers that see ACTIVE can complete their operation).
2. Drain in-flight DMA (see Section 3).
3. Tear down IOMMU mappings (see Section 4).
4. Increment `slot.generation` with `Release` ordering.
5. Set `slot.state` to `SLOT_FREE` with `Release` ordering.

The `Release` stores in steps 4-5 ensure that all cleanup (DMA drain,
IOMMU teardown) is visible to any core that subsequently reads the new
generation or free state.

**No read-side lock or RCU required.** Because `cap_verify` is two loads
and the write side uses release-ordered stores, we get natural
synchronization from the x86-64 TSO model without any explicit
synchronization primitive on the read path. This is simpler and faster
than RCU (no grace period tracking, no callback queues) and simpler than
seqlocks (no retry loops on the read path).

The key correctness argument: if a reader sees the old generation, it
proceeds with a valid capability. The DMA drain and IOMMU teardown have
not yet completed, so the resource is still accessible. If a reader sees
the new generation, it fails fast. There is no window where a reader
could see a valid generation but access a torn-down resource, because the
generation increment is release-ordered after all cleanup.

### 2.4 Per-Core Capability Caching

For processes that call `mem_vtop` or `mem_vtop_batch` during setup (to
build scatter-gather lists for DMA), the same DMA pool capability is
verified repeatedly. A per-core cache avoids redundant memory loads to
the capability table.

```rust
/// Per-core cache of the most recently verified capability.
/// One instance per core, stored in the per-core data area.
#[repr(C, align(64))]
pub struct CapCache {
    /// Cached process ID. Invalidated on context switch.
    pid: ProcessId,
    /// Cached slot index.
    slot_id: u64,
    /// Cached generation at time of verification.
    generation: u32,
    /// Cached resource_id (the lookup result we want to avoid repeating).
    resource_id: ResourceId,
    /// Cached permissions.
    permissions: CapPermissions,
    /// Valid flag. Cleared on context switch and revocation IPI.
    valid: bool,
    _pad: [u8; 27],
}
```

**Cache invalidation.** The cache is invalidated in two cases:

1. **Context switch.** When a core switches to a different process, the
   cache is cleared (the `pid` will not match, or `valid` is set to
   false). This is a single store in the context switch path.

2. **Revocation.** When a capability is revoked, the revoking core sends
   an IPI (inter-processor interrupt) to all cores that might have cached
   it. The IPI handler clears the cache. This is the only cost of
   revocation that touches other cores, and it only matters if the
   revoked capability was recently used on another core.

**Tradeoff: cache size.** A single-entry cache is shown here. It handles
the common case (repeated `mem_vtop` calls with the same DMA handle).
A 4-entry set-associative cache would cover batched setups using multiple
capabilities, at the cost of 256 bytes per core and slightly more complex
invalidation. Starting with 1 entry and benchmarking before expanding.

### 2.5 Concurrency Summary

| Operation      | Mechanism               | Cost on x86-64          |
|:---------------|:------------------------|:------------------------|
| cap_verify     | 2 atomic loads          | 2 MOV instructions      |
| cap_verify (cached) | 1 struct compare   | ~3 MOV + 1 branch       |
| cap_release    | Atomic inc + stores     | ~10 instructions + DMA drain |
| cap_delegate   | Spinlock on target table| Short critical section   |
| Revocation IPI | Cross-core interrupt    | ~1 us per target core   |

---

## 3. Cascading Revocation Algorithm

### 3.1 Overview

When `cap_release(handle)` is called, or a process exits, all capabilities
delegated from the released capability must be recursively revoked. This
section describes the complete algorithm, including the interaction with
in-flight DMA and IOMMU teardown.

### 3.2 Algorithm

```
cap_revoke(node_id: DelegationNodeId):
    node = delegation_tree.nodes[node_id]

    // Phase 1: Mark the subtree as revoking (top-down BFS/DFS).
    // This prevents new operations from starting on any node in the subtree.
    mark_subtree_revoking(node_id)

    // Phase 2: Drain in-flight DMA for all DmaPool capabilities in the
    // subtree. This must complete before IOMMU teardown.
    drain_dma_for_subtree(node_id)

    // Phase 3: Tear down IOMMU mappings for all DmaPool capabilities.
    teardown_iommu_for_subtree(node_id)

    // Phase 4: Increment generation counters and free slots (bottom-up).
    // Children are freed before parents so that a concurrent cap_list
    // never shows a parent as free while children are still active.
    finalize_revocation(node_id)
```

### 3.3 Phase 1: Mark Subtree as Revoking

```
mark_subtree_revoking(node_id):
    node = delegation_tree.nodes[node_id]
    slot = get_slot(node.holder, node.holder_slot)
    slot.state.store(SLOT_PENDING_REVOCATION, Release)

    child = node.first_child.load(Acquire)
    while child != DELEGATION_NODE_NONE:
        mark_subtree_revoking(child)
        child = delegation_tree.nodes[child].next_sibling.load(Acquire)
```

Setting `SLOT_PENDING_REVOCATION` causes `cap_verify` to return
`InvalidHandle` for any new ASI call that references this capability.
Operations already past `cap_verify` may still be in progress; this is
handled in Phase 2.

**Race with in-progress ASI calls.** Consider a core that has just
passed `cap_verify` and is about to program the IOMMU for a new DMA
mapping. Phase 1 sets `SLOT_PENDING_REVOCATION`, but the other core
does not see it because it already passed verification. This is safe
because:

- Phase 2 (DMA drain) waits for all in-flight DMA to complete, which
  includes any DMA that the racing operation might set up.
- Phase 3 (IOMMU teardown) happens after drain, so the racing
  operation's IOMMU mapping will be torn down.
- The racing operation's caller will observe the revocation on its
  next ASI call (generation mismatch).

The window between the racing core passing `cap_verify` and the
revocation completing is bounded by the DMA drain time (typically
microseconds for NVMe, milliseconds in the worst case for RDMA).

### 3.4 Phase 2: DMA Drain

For DmaPool capabilities, the kernel must ensure no device is performing
DMA to/from the associated physical pages before those pages can be
reclaimed.

```
drain_dma_for_subtree(node_id):
    node = delegation_tree.nodes[node_id]
    resource = resource_registry.entries[node.resource_id]

    if resource.resource_type == ResourceType::DmaPool:
        domain_id = resource.data.dma_pool.iommu_domain
        // Issue an IOMMU invalidation wait descriptor.
        // This blocks until all in-flight DMA transactions for this
        // domain have completed.
        iommu_drain_domain(domain_id)

    // Recurse into children. Each child may reference the same or
    // different DMA pools.
    child = node.first_child.load(Acquire)
    while child != DELEGATION_NODE_NONE:
        drain_dma_for_subtree(child)
        child = delegation_tree.nodes[child].next_sibling.load(Acquire)
```

**IOMMU drain mechanism.** Intel VT-d provides the "Invalidation Wait
Descriptor" in the invalidation queue. The kernel submits an invalidation
request for the IOMMU domain and spins (or schedules away) until the
completion status is written. This guarantees that all PCIe transactions
initiated by the device before the invalidation have reached their
destination.

**Optimization: deduplicate drains.** If multiple nodes in the subtree
reference the same IOMMU domain (common when a parent delegated its DMA
pool to children), the drain is performed once per unique domain. A small
stack-allocated bitset of domain IDs (max 256 domains) tracks which
domains have already been drained.

### 3.5 Phase 3: IOMMU Teardown

After DMA is drained, the IOMMU mappings can be safely removed.

```
teardown_iommu_for_subtree(node_id):
    node = delegation_tree.nodes[node_id]
    resource = resource_registry.entries[node.resource_id]

    if resource.resource_type == ResourceType::DmaPool:
        pool = resource.data.dma_pool
        iommu_unmap_pages(pool.iommu_domain, pool.base_physical, pool.size_bytes)
        // IOTLB flush to ensure the device sees the unmapping.
        iommu_flush_iotlb(pool.iommu_domain)

    child = node.first_child.load(Acquire)
    while child != DELEGATION_NODE_NONE:
        teardown_iommu_for_subtree(child)
        child = delegation_tree.nodes[child].next_sibling.load(Acquire)
```

**Ordering: drain before unmap is mandatory.** Unmapping IOMMU entries
while DMA is in flight causes the device's transactions to fault. On
some hardware, this triggers a machine check exception. The drain-then-
unmap ordering prevents this.

### 3.6 Phase 4: Finalize (Bottom-Up)

Finalization frees slots and increments generation counters, starting
from the leaves and working up to the root of the revoked subtree.

```
finalize_revocation(node_id):
    node = delegation_tree.nodes[node_id]

    // Recurse into children first (bottom-up).
    child = node.first_child.load(Acquire)
    while child != DELEGATION_NODE_NONE:
        next = delegation_tree.nodes[child].next_sibling.load(Acquire)
        finalize_revocation(child)
        child = next

    // Now free this node.
    slot = get_slot(node.holder, node.holder_slot)
    slot.generation.fetch_add(1, Release)
    slot.state.store(SLOT_FREE, Release)
    // Unmap any virtual memory mappings for this capability.
    unmap_all_regions(node.holder, node.holder_slot)

    // Send cache invalidation IPI to all cores.
    send_cap_cache_invalidation_ipi()

    // Return the delegation node to the free pool.
    delegation_tree.free_node(node_id)
```

**Why bottom-up?** A parent's `cap_list` might be called concurrently.
If we freed the parent first, `cap_list` would see the parent as free
but children still active -- an inconsistent view. Bottom-up ensures
children vanish before parents.

### 3.7 Stack Depth Concern

The recursive descent is bounded by the maximum delegation depth. In
practice, delegation chains are shallow (init -> driver -> sub-driver,
depth 3). However, to guard against pathological cases, the kernel
limits delegation depth to 16 at `cap_delegate` time. Any attempt to
delegate beyond depth 16 returns `CapError::PermissionDenied`.

This bounds the revocation stack usage to ~16 frames * ~64 bytes = ~1 KiB,
well within the kernel stack (typically 8-16 KiB).

---

## 4. IOMMU Integration

### 4.1 Architecture

The kernel manages the IOMMU (Intel VT-d) as an implementation detail
invisible to user-space. The ASI spec mandates this: user-space never
programs the IOMMU directly.

### 4.2 Per-Device IOMMU Domains

Each PCI device that is granted a DmaPool capability gets its own IOMMU
domain (address space). This provides device-level isolation: even if
two devices are used by the same process, they cannot access each
other's DMA buffers unless explicitly configured.

```rust
/// Per-device IOMMU domain state. Stored in kernel memory, one per
/// PCI device that has active DMA capabilities.
#[repr(C)]
pub struct IommuDomain {
    /// VT-d domain ID (programmed into the context table).
    pub domain_id: u16,
    /// The PCI device this domain is assigned to.
    pub device: PciAddress,
    /// Page table root for this domain (physical address of the
    /// VT-d second-level page table).
    pub page_table_root: PhysicalAddress,
    /// Active mapping count. When this reaches zero, the domain
    /// can be reclaimed.
    pub mapping_count: AtomicU32,
    /// State: Uninitialized, Active, Draining, TornDown.
    pub state: AtomicU8,
}

/// IOMMU domain states.
const IOMMU_DOMAIN_UNINITIALIZED: u8 = 0;
const IOMMU_DOMAIN_ACTIVE: u8 = 1;
const IOMMU_DOMAIN_DRAINING: u8 = 2;
const IOMMU_DOMAIN_TORN_DOWN: u8 = 3;
```

### 4.3 DmaPool Grant: Automatic IOMMU Programming

When the kernel fulfills a `CapRequest::DmaPool`, the following sequence
executes:

1. **Allocate physical pages** from the kernel's physical page allocator.
   Pages must be contiguous if `PageFlags::CONTIGUOUS` is set (required
   for some device DMA engines).

2. **Allocate or look up IOMMU domain** for the specified device. If the
   device already has a domain (from a previous DmaPool grant), reuse it.
   Otherwise, allocate a new domain ID and program the VT-d context table
   to associate the device's PCI address with the new domain.

3. **Program VT-d page tables.** Create second-level page table entries
   mapping the device-virtual addresses (which we set equal to the
   physical addresses for simplicity -- identity mapping within the IOMMU
   domain) to the allocated physical pages. Set read/write permissions
   based on the capability's permissions.

4. **Flush IOTLB.** Issue a domain-selective IOTLB invalidation to ensure
   the device sees the new mappings immediately.

5. **Create the capability** in the requesting process's table and return
   the handle.

**Identity mapping within IOMMU domains.** We map device-virtual address
== physical address within each IOMMU domain. This means `mem_vtop`
returns the physical address directly, and user-space can use these
physical addresses in DMA descriptors without translation. The IOMMU
domain isolation comes from each device only having mappings for its
own DMA pools, not from address randomization.

**Tradeoff: identity mapping vs. remapped addresses.** Remapped addresses
would provide defense-in-depth (a device exploit cannot predict physical
addresses), but would require the kernel to maintain a device-virtual-to-
physical translation table that `mem_vtop` would have to consult. Since
the IOMMU already prevents the device from accessing unmapped pages, the
additional security benefit is marginal, and the performance cost on
`mem_vtop` (a setup-time hot path for scatter-gather list construction)
is not worth it.

### 4.4 Teardown Safety

The drain-before-unmap ordering described in Section 3.4-3.5 is
implemented using VT-d hardware capabilities:

1. **Invalidation Queue.** The kernel submits context-cache and IOTLB
   invalidation descriptors to the VT-d invalidation queue.

2. **Invalidation Wait Descriptor.** After the invalidation descriptors,
   the kernel submits a wait descriptor with a status address. The
   hardware writes to this address when all preceding invalidations are
   complete and all in-flight DMA transactions that used the old mappings
   have finished.

3. **Spin or schedule.** The kernel spins on the status address (for
   short drains expected to complete in microseconds) or schedules
   the revoking thread away and checks periodically (for long drains).

4. **Unmap.** Only after the wait descriptor completes does the kernel
   clear the page table entries and reclaim the physical pages.

### 4.5 VT-d Data Structure Layout

The kernel pre-allocates VT-d root and context tables at boot, sized
for the maximum PCI topology (256 buses * 32 devices * 8 functions).
Second-level page tables are allocated from a dedicated page pool.

```
VT-d Root Table (4 KiB, 256 entries -- one per bus)
  -> Context Table (4 KiB per bus, 256 entries -- one per devfn)
       -> Second-Level Page Table (per-domain, 4-level, x86-64 paging)
            -> Physical pages (DMA pool memory)
```

The root and context tables are wired into the VT-d hardware at boot
via the Root Table Address Register (RTADDR). They are never deallocated.
Only the context entries and second-level page tables change as DMA
capabilities are granted and revoked.

---

## 5. Init Process Bootstrap

### 5.1 The Capability Mint

Only the kernel can create root capabilities (capabilities with no
parent in the delegation tree). There is no ASI call that creates root
capabilities -- `cap_request` is the public interface, but it is gated
by an internal authority check.

The "capability mint" is the kernel-internal function that creates root
capabilities:

```rust
/// Kernel-internal only. Not exposed via any ASI call.
/// Creates a root capability for a resource and assigns it to a process.
fn mint_root_capability(
    pid: ProcessId,
    resource_id: ResourceId,
    permissions: CapPermissions,
) -> CapHandle {
    let table = get_capability_table(pid);
    let slot_idx = table.alloc_slot();
    let slot = &table.slots[slot_idx];

    slot.cap_type = resource_registry.entries[resource_id].resource_type.to_cap_type();
    slot.permissions = permissions;
    slot.resource_id = resource_id;
    slot.state.store(SLOT_ACTIVE, Release);

    let node_id = delegation_tree.alloc_node();
    let node = &delegation_tree.nodes[node_id];
    node.resource_id = resource_id;
    node.holder = pid;
    node.holder_slot = slot_idx as u16;
    node.parent = DELEGATION_NODE_NONE;  // root: no parent
    node.first_child.store(DELEGATION_NODE_NONE.0, Release);
    node.next_sibling.store(DELEGATION_NODE_NONE.0, Release);
    node.state.store(NODE_ACTIVE, Release);

    slot.delegation_node = node_id;
    resource_registry.entries[resource_id].root_delegation = node_id;
    resource_registry.entries[resource_id].owner = Some(pid);

    CapHandle {
        id: slot_idx as u64,
        cap_type: slot.cap_type,
        generation: slot.generation.load(Acquire),
    }
}
```

### 5.2 Boot Sequence

The boot sequence for capability initialization:

1. **Physical memory detection.** The kernel reads the memory map from
   the bootloader (e.g., UEFI memory map or Multiboot2 memory map).
   Each usable region is registered in the ResourceRegistry as a
   `PhysicalMemory` resource.

2. **PCI enumeration.** The kernel walks the PCI configuration space
   (using ECAM for PCIe). Each discovered device's BARs and MSI-X
   capability are registered as `DeviceBar` and `MsixVector` resources
   in the ResourceRegistry.

3. **IOMMU initialization.** The kernel locates the VT-d hardware via
   ACPI DMAR table, initializes the root and context tables, and enables
   DMA remapping. All devices start with empty IOMMU domains (no DMA
   permitted).

4. **Init process creation.** The kernel loads the init ELF binary
   (built into the kernel image or loaded by the bootloader) and creates
   the first process.

5. **Root capability minting.** The kernel calls `mint_root_capability`
   for every discovered resource, granting the init process full
   (READ | WRITE | DELEGATE | REVOKE) capabilities for all hardware.

6. **Transfer to user-space.** The kernel transfers control to init's
   entry point. The init process's `CapabilityTable` is fully populated.
   From this point, all hardware access flows through the ASI.

### 5.3 Authority Chain

After boot, the authority chain is:

```
Kernel (mints root caps at boot, then never again)
  -> init process (holds root caps for all hardware)
       -> driver processes (receive delegated caps from init)
            -> application processes (receive further-delegated caps)
```

The kernel never creates new root capabilities after boot (except during
hot-plug events, which are a future extension). The init process is the
sole authority for distributing hardware access. This is the exokernel
model: the kernel enforces isolation; policy is in user-space.

### 5.4 cap_request Authority Check

When a non-init process calls `cap_request`, the kernel checks:

1. Is this resource unclaimed (no root owner)? If so, grant a root
   capability. This only happens for resources like IPC endpoints
   that are created on demand.

2. Is this resource already claimed? Return `CapError::ResourceBusy`.
   The caller must obtain access via `cap_delegate` from the current
   owner.

3. Exception: `PhysicalPages` requests allocate new physical memory
   (not claiming an existing resource), so they are always granted
   if memory is available. The kernel mints a root capability for the
   newly allocated region.

---

## 6. Memory Layout

### 6.1 Kernel Memory Map

The capability system's data structures are allocated in the kernel's
direct-mapped physical memory region. On x86-64, the kernel typically
identity-maps all physical memory (or uses a fixed offset mapping) so
that physical addresses are directly accessible.

```
Kernel Virtual Address Space:
    ...
    [CapabilityTables]     -- Per-process cap tables
    [ResourceRegistry]     -- Global resource registry
    [DelegationTree]       -- Global delegation node pool
    [IommuDomains]         -- Per-device IOMMU domain state
    [VtdRootTable]         -- VT-d root table (4 KiB, page-aligned)
    [VtdContextTables]     -- VT-d context tables (up to 256 * 4 KiB)
    [VtdPageTablePool]     -- Pool for second-level page tables
    [PerCoreData]          -- Per-core data including CapCache
    ...
```

### 6.2 Sizing Calculations

| Structure            | Parameters                           | Size        |
|:---------------------|:-------------------------------------|:------------|
| CapabilityTable      | 256 slots * 64 B/slot                | 16 KiB      |
| All cap tables       | 64 processes * 16 KiB                | 1 MiB       |
| ResourceRegistry     | 4096 entries * 128 B/entry           | 512 KiB     |
| DelegationTree       | 8192 nodes * 48 B/node               | 384 KiB     |
| IommuDomains         | 256 domains * 64 B/domain            | 16 KiB      |
| VT-d root table      | 1 page                               | 4 KiB       |
| VT-d context tables  | 256 buses * 1 page                   | 1 MiB       |
| VT-d page table pool | 1024 pages                           | 4 MiB       |
| PerCoreData          | 256 cores * 128 B (CapCache + misc)  | 32 KiB      |
| **Total**            |                                      | **~7 MiB**  |

7 MiB of kernel memory for the entire capability system. This is a fixed,
compile-time-known cost with no dynamic allocation.

### 6.3 Compile-Time Constants

```rust
/// Maximum number of processes the kernel supports.
pub const MAX_PROCESSES: usize = 64;

/// Maximum capabilities per process.
pub const MAX_CAPS_PER_PROCESS: usize = 256;

/// Maximum distinct hardware resources tracked.
pub const MAX_RESOURCES: usize = 4096;

/// Maximum delegation tree nodes (shared across all resources).
pub const MAX_DELEGATION_NODES: usize = 8192;

/// Maximum IOMMU domains (one per device with active DMA).
pub const MAX_IOMMU_DOMAINS: usize = 256;

/// Maximum supported CPU cores.
pub const MAX_CORES: usize = 256;

/// Maximum delegation depth (limits cascading revocation stack usage).
pub const MAX_DELEGATION_DEPTH: u8 = 16;
```

**All constants are tunable at compile time.** The values above are
chosen for a high-end server with up to 256 cores, 64 isolated driver
processes, and hundreds of NVMe/RDMA devices. A smaller embedded
deployment could reduce these significantly.

### 6.4 Cache Alignment

All frequently-accessed structures are aligned to cache line boundaries
(64 bytes on x86-64) to prevent false sharing:

- `CapSlot` is padded to 64 bytes.
- `CapCache` is aligned to 64 bytes.
- `EventSlot` (from the ASI spec) is aligned to 64 bytes.
- `DelegationNode` is 48 bytes; two nodes share a cache line, which is
  acceptable because they are rarely accessed concurrently (a delegation
  tree walk is single-threaded).

### 6.5 No Dynamic Allocation

The entire capability system operates without a heap allocator. All
memory is statically sized and allocated at compile time (in `.bss`)
or at boot time from the physical page allocator. This is essential
for `no_std` compatibility and provides deterministic memory usage --
the kernel will never OOM due to capability operations.

The cost is inflexibility: the maximum number of processes, capabilities,
resources, and delegation nodes is fixed. Exceeding any limit returns
an appropriate error (`OutOfMemory`, `TooManyThreads`, etc.) rather
than attempting dynamic growth.

---

## 7. Design Tradeoff Summary

### Flat arrays vs. hash maps for capability lookup

Flat arrays were chosen for O(1) indexing with no hash computation, at
the cost of fixed maximum sizes. Hash maps would allow unbounded growth
but introduce variable-time lookups, cache-unfriendly access patterns,
and require a heap allocator. Given that exokernel processes hold few
capabilities, the fixed-size approach is clearly better.

### Global delegation pool vs. per-resource trees

A global pool was chosen to avoid per-resource memory waste. The cost is
a global free-list contention point, but delegation and revocation are
rare operations (setup/teardown time only), so this is not a concern.

### Identity IOMMU mapping vs. randomized device-virtual addresses

Identity mapping simplifies `mem_vtop` to a trivial physical address
return. Randomized addresses would add defense-in-depth but penalize
the scatter-gather setup path. Since IOMMU domain isolation already
prevents cross-device access, the marginal security benefit does not
justify the complexity.

### Single-entry per-core cap cache vs. multi-entry

A single entry handles the dominant pattern (repeated `mem_vtop` with
one DMA handle). Multi-entry adds complexity and cache footprint. The
design starts simple with a measured upgrade path.

### Generation counters (u32) vs. epoch-based reclamation

Generation counters provide per-slot, constant-time stale detection
with no global synchronization. Epoch-based reclamation (like crossbeam)
would defer slot reuse until all readers have exited a critical section,
which adds latency to slot recycling. Since capabilities are recycled
infrequently, the generation counter approach is simpler and sufficient.

---

## 8. Correctness Invariants

The following invariants must hold at all times. Violations indicate
kernel bugs.

1. **Handle-generation consistency.** For every active `CapSlot`, the
   generation counter in the slot matches the generation in every valid
   `CapHandle` that references it.

2. **Delegation tree acyclicity.** The delegation tree is a forest of
   trees (no cycles). Every node's parent chain terminates at a root
   node (parent == DELEGATION_NODE_NONE).

3. **Permission monotonicity.** A delegated capability's permissions are
   a subset of its parent's permissions.

4. **IOMMU completeness.** Every active `DmaPool` capability has a
   corresponding IOMMU domain with correct page table entries. No
   physical page is DMA-accessible by a device without a corresponding
   active capability.

5. **Drain-before-unmap.** IOMMU page table entries are never removed
   while DMA transactions referencing those entries may be in flight.

6. **Bottom-up finalization.** During cascading revocation, a child's
   generation counter is incremented before its parent's.

7. **No root cap creation after boot.** Except for newly allocated
   physical pages and on-demand IPC endpoints, the kernel never mints
   root capabilities after the init process is started.
