# Feox Page-Table Plan

Last updated: 2026-03-27

This document defines the first page-table management plan for the current
x86-first Feox lane.

The goal is not to design a full VM subsystem. The goal is to define the
smallest useful page-table mechanism layer Feox needs after first boot.

## Current State

Today Feox can:

- read the active top-level page-table root
- inspect x86_64 virtual-address indices
- allocate 4 KiB frames from `Usable` memory
- log enough state to reason about the boot handoff

Today Feox cannot yet:

- create new paging structures
- install new mappings
- remove mappings
- define a Feox-owned virtual layout

## Design Goal

The first page-table layer should provide mechanism only.

It should answer:

- how to allocate paging frames
- how to walk paging structures
- how to create mappings
- how to remove mappings
- how to account for those structures as kernel-owned memory

It should not yet try to be:

- a rich VM policy layer
- a userspace mapping interface
- a demand-paging system
- a final architecture-neutral API

## First x86_64 Scope

The first implementation scope should cover only 4 KiB mappings on x86_64.

Why:

- it keeps the first operational layer small
- it matches the current frame allocator
- it avoids large-page policy decisions too early

Large-page support can follow later once the ownership and reservation model is
solid.

## Proposed Layering

### 1. Paging frame allocation

Add a page-table allocator that draws from explicitly reserved Feox-owned memory.

Requirements:

- uses 4 KiB frames
- records frame ownership as paging-structure memory
- does not silently consume untracked frames

### 2. Page-table walker

Add a minimal walker that can:

- start from a known PML4 root
- follow PML4 -> PDPT -> PD -> PT
- create missing intermediate tables when requested
- reject unsupported huge-page cases for now

### 3. Mapping operation

Add one explicit mapping primitive:

`map_4k(root, virt, phys, flags)`

Responsibilities:

- allocate missing intermediate tables
- install a leaf mapping
- reject remapping unless an explicit replace path exists

### 4. Unmapping operation

Add one explicit unmapping primitive:

`unmap_4k(root, virt)`

Responsibilities:

- remove the leaf mapping
- return enough information to support later accounting and teardown
- leave higher-level table reclamation as a later optimization

### 5. Translation / query

Add one explicit query primitive:

`translate(root, virt) -> Option<(phys, flags)>`

Responsibilities:

- verify that the mapping machinery is producing coherent results
- support debug output and later mapping validation

## Proposed Data Model

The first page-table layer should define:

- page-table entry flags
- page-table frame ownership marker
- a root wrapper for the active paging tree
- a small error enum for walk/map/unmap failures

It should avoid:

- hidden global mutable state
- generic "allocator object" sprawl
- architecture-neutral abstractions that are still too early to justify

## x86_64 Virtual Layout Questions

Before implementing mappings broadly, Feox should make one x86-first decision:

Will the early kernel grow toward:

1. a higher-half kernel plus direct-map region
2. a fixed-offset physical mapping
3. a very small bootstrap mapping model first, with layout deferred

Recommended answer for now:

- choose the smallest bootstrap mapping model first
- reserve the direct-map decision until the first operational mapping layer works

That reduces the risk of locking Feox into a broad layout policy too early.

## Required Reservation Inputs

The page-table plan depends on the memory ownership phases doc.

Before implementing mapping, Feox should know:

- which frames are permanently reserved for the current page-table root
- which frames are available for new paging structures
- whether reclaimed bootloader memory has entered the Feox-owned pool yet

Without that, page-table growth risks stealing memory from unclear ownership
regions.

## Interaction With Capabilities

The first page-table layer is kernel-internal only.

But it should already be built in a way that later capability-backed mapping can
reuse:

- explicit ownership
- deterministic teardown
- narrow mapping primitives
- clear flags and accounting

This means the page-table layer should not assume "kernel can map anything
whenever it wants" as a permanent design stance.

## Recommended Build Order

1. prove first x86 QEMU boot
2. define memory ownership phases
3. add reservation bookkeeping for kernel-owned structures
4. implement `PageTableRoot` plus `translate`
5. implement `map_4k`
6. implement `unmap_4k`
7. validate with serial-debuggable mapping tests before broadening scope

## Good First Validation

After the first implementation slice, Feox should be able to:

- report the active PML4 root
- map one known test virtual address to one known physical frame
- translate it back correctly
- unmap it
- confirm over serial that the mapping lifecycle behaved as expected

That is enough to prove the mechanism before building policy above it.

## Bottom Line

The first Feox page-table layer should be:

- x86-first
- 4 KiB only
- mechanism-only
- ownership-aware
- serial-debuggable

If it grows broader than that before first success, it is probably too large.
