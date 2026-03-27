# Feox Boot Memory Review

Last updated: 2026-03-27

This is the first subsystem review using
[ARCHITECTURE_CHECKLIST.md](ARCHITECTURE_CHECKLIST.md).

Scope reviewed:

- boot handoff ABI in `crates/feox-boot`
- UEFI memory-map translation in `loader/feox-loader-uefi`
- early kernel bootstrap in `kernel/feox-xokernel`
- current frame allocator and page-table visibility in `kernel/feox-xokernel/src/memory.rs`

## Verdict

The current boot memory layer is disciplined and appropriately small for this
stage.

It is not yet a real virtual-memory subsystem, but it is a good bootstrap
foundation because it does the important early things correctly:

- preserves an explicit boot handoff ABI
- translates firmware memory types into Feox-owned region semantics
- marks the kernel image explicitly in the memory map
- keeps the early allocator linear, bounded, and easy to reason about
- exposes enough serial diagnostics to validate the handoff at first boot

This is the right shape for an early core product. The next step should be to
extend this foundation carefully, not replace it with a broader allocator or
VM layer too early.

## What Is Already Strong

### 1. The handoff is explicit and typed

`BootInfo`, `BootHandoff`, `MemoryRegion`, and `MemoryRegionKind` form a clean
ownership boundary between loader and kernel.

Why this is good:

- it keeps the boot contract narrow
- it avoids loader-private assumptions leaking into the kernel
- it gives Feox a stable place to hang later memory and capability decisions

### 2. Kernel memory ownership starts from real region semantics

The loader translates UEFI memory types into Feox-specific categories like:

- `Usable`
- `Kernel`
- `Reserved`
- `Mmio`
- `BootloaderReclaimable`

Why this is good:

- the kernel is not operating on raw firmware categories forever
- the memory model is already moving toward Feox-owned semantics
- `Kernel` is separated from future allocatable memory early

### 3. The allocator is intentionally simple

`FrameAllocator` is a linear allocator over usable regions only.

Why this is good:

- it is easy to reason about in bring-up
- it preserves deterministic behavior
- it avoids premature allocator complexity before the boot path is proven

### 4. Serial visibility is already useful

The kernel currently logs:

- kernel image bounds
- active PML4 frame
- memory-map region count
- usable MiB
- highest physical address
- first usable frame

Why this is good:

- these are exactly the right facts to validate first boot under QEMU
- it supports the "bring-up discipline beats speculative architecture" rule

## Gaps And Risks

These are not failures. They are the next real design gaps.

### 1. No reclaim path for bootloader-reclaimable memory yet

The loader marks `BootloaderReclaimable` correctly, but the kernel does not yet
have a phase transition where those regions become allocatable under Feox
control.

Why it matters:

- this is the first real ownership transition after boot
- it will eventually affect page-table allocation and capability-backed memory
  ownership

### 2. The allocator does not exclude all future self-hosted kernel structures yet

Today the allocator relies on the memory map to avoid obviously non-usable
regions, which is correct for now, but there is not yet a richer reservation
story for future kernel-owned structures beyond the kernel image itself.

Why it matters:

- page tables, capability tables, IOMMU structures, and per-core data will need
  explicit reservation and accounting
- that reservation model should be designed before the allocator grows broader

### 3. Page-table work is still observational, not operational

The kernel can inspect CR3 and derive indices from virtual addresses, but it
cannot yet:

- allocate paging structures
- map or unmap pages intentionally
- describe a Feox-owned virtual memory layout

Why it matters:

- this is the next real architectural milestone after first boot
- it should be designed with ownership and revocation in mind, not as a generic
  VM convenience layer

### 4. No explicit direct-map or kernel virtual layout policy yet

The current code reads the active PML4 root but does not yet define whether
Feox will use:

- a direct map of physical memory
- a fixed-offset physical map
- separate higher-half regions for core kernel structures
- capability-relevant mapping zones

Why it matters:

- this decision will shape memory authority, DMA setup, and later ASI mapping behavior

## Checklist Outcome

### Memory checklist

- protect vs manage: pass for current stage
- ownership explicitness: partial pass
- per-core locality: neutral so far, not yet stressed
- revocation story: not ready yet
- debug visibility: pass

### Bring-up discipline checklist

- current scope improves bring-up instead of distracting from it: pass
- current layer is still serial-debuggable: pass
- the next step should still follow verified QEMU boot: pass

## Recommended Next Sequence

This is the most disciplined order from here.

### 1. Prove first QEMU boot and capture serial output

Do not deepen the memory subsystem before the current handoff is proven end to end.

### 2. Define memory ownership phases

Write down the transition model for:

- firmware-owned memory
- kernel image memory
- bootloader-reclaimable memory
- Feox-owned allocatable memory

This should become a small design doc before allocator expansion.

### 3. Add an explicit reservation model

Before building richer allocators, define how Feox reserves memory for:

- paging structures
- per-core state
- capability-system tables
- IOMMU structures
- driver bootstrap allocations

### 4. Design page-table management as mechanism, not policy

The next page-table layer should focus on:

- allocate paging frames
- install mappings
- tear mappings down
- preserve narrow ownership and accounting

It should not jump straight to a broad VM abstraction model.

## Concrete Near-Term Questions

The next memory design pass should answer:

1. When does `BootloaderReclaimable` become Feox-owned?
2. What memory is reserved permanently at early boot?
3. Will Feox adopt a direct-map region, and if so, where?
4. What is the smallest page-table API the kernel needs next?
5. Which memory objects should be capability-governed earliest?

## Bottom Line

Feox's current boot memory layer is in good shape.

It is intentionally narrow, already uses Feox-owned semantics instead of raw
firmware language, and supports the right next milestone. The disciplined move
now is not "add more memory features." The disciplined move is:

- prove first boot
- define memory ownership phases
- add reservation discipline
- then build the first real page-table management layer
