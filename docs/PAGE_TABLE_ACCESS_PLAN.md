# Feox Page-Table Access Plan

Last updated: 2026-04-08

This document defines the next narrow design step for Feox paging work:
how the kernel should access live page-table frames after the higher-half
handoff.

## Why This Exists

Feox now has:

- a working x86_64 bootstrap paging layer
- a retained higher-half runtime
- bootstrap `mem_map` / `mem_unmap`
- bootstrap `mem_vtop` / `mem_vtop_batch`

But the first live attempt to use that VM lane from the higher-half runtime
failed with a page fault.

The reason is not that the VM API shape is wrong. The reason is that the
current paging helpers still depend on a bootstrap-only assumption:

- page-table frames can be dereferenced through their physical addresses as if
  low physical memory were still identity mapped

That assumption is valid for early bootstrap construction and for host-side
tests, but it is not a safe permanent rule once the retained higher-half
runtime is active on its own transition root.

## Current Mechanism

Today live paging edits use:

- `PageTableRoot::active()`
- `BootstrapIdentityMappedPageTables`
- `identity_mapped_table()` / `identity_mapped_table_mut()`

That source treats a page-table frame's physical address as a directly usable
virtual pointer.

This is intentionally narrow and was enough to prove:

- page-table walking
- mapping
- unmapping
- transition-root construction

It is no longer enough for post-handoff VM services.

## The Real Problem

After higher-half handoff, Feox can still know the active root frame
physically, but it does not yet have a durable rule for how to *access the
memory contents* of arbitrary page-table frames in the live address space.

That missing rule blocks:

- safe live `mem_map`
- safe live `mem_unmap`
- safe live `mem_vtop` through actual hardware page tables
- later reclaim or mutation of transition-root descendants

## Design Goal

Add one explicit, bootstrap-scoped page-table access model for live runtime
use.

It should:

- stay mechanism-only
- avoid a full direct-map decision
- avoid heap allocation
- keep ownership explicit
- be serial-debuggable

It should not yet:

- solve the permanent direct map
- define final MMIO layout
- define final per-core kernel layout
- broaden into a full VM subsystem

## Recommended Answer

Use a dedicated **page-table access window** inside the bootstrap VM region.

That means:

1. reserve a small higher-half virtual range specifically for temporary
   page-table-frame access
2. map one page-table frame at a time into that window
3. read or mutate entries through that temporary alias
4. unmap or reuse the slot deterministically

## Why This Is Better Than Reusing Identity Access

It keeps the transition explicit:

- physical frame identity is still the ownership truth
- virtual access becomes a deliberate mapping decision
- the live runtime no longer depends on an undocumented leftover identity map

It also avoids solving the full direct-map policy too early.

## Proposed Bootstrap-Scoped Shape

### New Window

Add a small page-table access window distinct from the 64 MiB bootstrap VM
mapping arena.

Suggested initial size:

- 16 KiB
- four 4 KiB slots

That is enough for:

- current root frame
- one child table
- one sibling table
- one scratch slot

### New Access Helper

Introduce a dedicated helper with semantics like:

```rust
with_page_table_frame_mut(frame, |table| { ... })
with_page_table_frame(frame, |table| { ... })
```

Responsibilities:

- acquire one slot in the page-table access window
- map the requested physical frame into that slot
- expose `&[u64; 512]` or `&mut [u64; 512]`
- unmap or recycle the slot after the closure returns

### New Source Type

Replace `BootstrapIdentityMappedPageTables` in live higher-half VM paths with a
source that uses the page-table access window instead of raw physical-address
dereference.

Keep `BootstrapIdentityMappedPageTables` only for:

- early transition-root construction
- bootstrap-only tests that intentionally model identity access

## Expected Code Split

### Keep

- `PageTableRoot`
- `translate_with`
- `map_4k_with`
- `unmap_4k_with`
- `prepare_4k_pages_with`

### Add

- bootstrap page-table access window constants in `memory.rs`
- retained page-table access-slot state
- one mapped-frame accessor in `paging.rs`
- one new page-table frame source for live runtime use

### Retire From Live Paths

- direct use of `BootstrapIdentityMappedPageTables` from:
  - `vm.rs` live mapping
  - `vm.rs` live unmapping
  - `vm.rs` live translation

## Suggested Build Order

1. define the new page-table access window in the virtual layout docs and code
2. add one helper that can temporarily map a page-table frame into that window
3. implement a `PageTableFrameSource` / `PageTableFrameMutSource` backed by that helper
4. switch live bootstrap VM helpers in `vm.rs` to the new source
5. reattempt the live runtime self-test
6. only after that, broaden the smoke lane to assert the VM self-test marker

## Validation Target

The next successful validation should be:

- higher-half runtime enters normally
- bootstrap VM self-test requests one page capability
- `mem_map` succeeds
- `mem_vtop` succeeds
- `mem_vtop_batch` succeeds
- `mem_unmap` succeeds
- runtime still reaches `stage: runtime service idle`

That is the right proof point before any broader direct-map or per-process VM
design work.

## Bottom Line

Feox does not need a permanent direct map yet.

It does need one explicit post-handoff rule for page-table-frame access.

The narrowest disciplined answer is a small bootstrap page-table access window
that replaces raw identity dereference in live higher-half VM operations.
