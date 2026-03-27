# Feox Memory Ownership Phases

Last updated: 2026-03-27

This document defines the intended ownership transitions for physical memory in
the x86-first Feox bring-up path.

It answers the most immediate question raised in
[BOOT_MEMORY_REVIEW.md](BOOT_MEMORY_REVIEW.md):

When does memory stop being "firmware or bootloader memory" and become Feox-owned?

## Purpose

Feox should not jump directly from a firmware memory map to a broad allocator
with unclear ownership. Memory needs a staged transition model.

This document defines those stages.

## Phase 0: Firmware-Owned

State:

- UEFI firmware still owns the machine
- Feox loader is executing in the firmware environment
- the firmware memory map is authoritative

Feox constraints:

- Feox may inspect the UEFI memory map
- Feox may allocate loader-owned pages through UEFI services
- Feox must not treat `ConventionalMemory` as already kernel-owned

Output of this phase:

- the loader identifies the kernel image range
- the loader translates firmware memory types into Feox region kinds
- the loader builds `BootInfo`

## Phase 1: Handoff-Owned But Not Yet Reclaimed

State:

- `ExitBootServices()` has completed
- firmware ownership is gone
- the kernel has received `BootInfo`
- Feox now has a static description of physical memory, but not all regions are
  equally ready for allocation

Region meanings in this phase:

- `Kernel`: permanently reserved for the loaded kernel image
- `Reserved`: not allocatable
- `Mmio`: not allocatable
- `Usable`: allocatable immediately by the earliest frame allocator
- `BootloaderReclaimable`: visible to Feox but not yet allocatable

Why `BootloaderReclaimable` is special:

- the loader may still have left meaningful state in those pages
- Feox has not yet established a clear reservation model for self-hosted kernel
  structures
- reclaiming too early risks blurring ownership before basic bootstrap is proven

Current Feox status:

- this is the current effective kernel phase today

## Phase 2: Early Feox-Owned Core Memory

Entry condition:

- first boot path is proven
- boot diagnostics are stable enough to reason about reservations
- Feox explicitly transitions selected regions into kernel ownership

Feox actions in this phase:

1. reserve the kernel image permanently
2. reserve the active bootstrap page tables currently in use
3. reserve any early per-core state
4. reserve space needed for future capability tables, resource registries, and
   IOMMU/bootstrap metadata
5. convert eligible `BootloaderReclaimable` regions into Feox-owned allocatable
   memory

Important rule:

`BootloaderReclaimable` should only become allocatable after the kernel has
recorded all reservations it still depends on.

Output of this phase:

- Feox has its first trustworthy self-owned physical frame pool

## Phase 3: Feox-Owned Structured Pools

Entry condition:

- Feox can reserve and account for its own internal structures
- the page-table layer exists at least in a minimal operational form

Feox actions in this phase:

- split raw allocatable memory into named internal pools where justified
- carve pools for:
  - paging structures
  - capability-system metadata
  - per-core data
  - IOMMU structures
  - device bootstrap buffers

Important rule:

This is still kernel mechanism work, not a rich general-purpose allocator policy
layer.

## Phase 4: Capability-Governed Memory

Entry condition:

- capability machinery is real enough to express memory authority
- Feox can distinguish kernel-owned memory from delegated memory explicitly

Feox actions in this phase:

- represent allocatable physical memory as resources with explicit authority
- allow controlled allocation of memory-backed capabilities
- support mapping and eventual revocation rules on top of those owned resources

Important rule:

This phase should not collapse back into ambient allocator authority.

## Ownership Rules

These rules should hold across all phases.

### Rule 1: `Kernel` is never allocatable

The kernel image range is permanently reserved from the moment the loader marks
it.

### Rule 2: `Mmio` is never general-purpose memory

MMIO ranges are resources, not RAM.

### Rule 3: `BootloaderReclaimable` is not automatically `Usable`

It is eligible for later Feox ownership, not immediate early allocation.

### Rule 4: Feox-owned memory starts only after explicit reservation

If Feox still depends on a region for bootstrap structures, that dependency
must be recorded before the region can join the allocatable pool.

### Rule 5: Capability-backed memory should grow out of Feox-owned pools

Memory capabilities should be granted from explicitly owned and accounted-for
Feox pools, not directly from a raw firmware map.

## Immediate Implementation Guidance

For the next x86 lane steps:

1. keep the current `Usable`-only frame allocator for the earliest bootstrap
2. add a reservation model before reclaiming `BootloaderReclaimable`
3. define one explicit transition point where reclaimable memory becomes Feox-owned
4. only then let broader page-table and capability work depend on reclaimed memory

## Bottom Line

The important distinction is:

- firmware-supplied memory map semantics are input
- Feox-owned allocatable memory is a later state

That transition should be explicit, staged, and visible in the implementation.
