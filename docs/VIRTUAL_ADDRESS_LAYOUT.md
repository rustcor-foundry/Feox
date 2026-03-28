# Feox Virtual Address Space Layout

Last updated: 2026-03-27

This document captures the current virtual address space layout decisions for
the x86_64 bootstrap lane and identifies the open decisions that need to be
made before the layout grows further.

## Context

`docs/PAGE_TABLE_PLAN.md` explicitly deferred the direct-map and virtual layout
decision until the first operational mapping layer was proven. That milestone
is now past. The bootstrap runtime uses a real higher-half layout and the
addresses are locked in code. This document captures what is decided and what
is still open.

## What Is Decided

The bootstrap runtime owns three named higher-half windows. These are defined
as constants in `kernel/feox-xokernel/src/memory.rs` and are part of the
`BootstrapRuntimeLayout` struct.

```
0xFFFF_9000_0000_0000   BOOTSTRAP_KERNEL_WINDOW_BASE
    Kernel image alias window.
    The loaded kernel ELF image is aliased here, page-for-page, starting at
    this base. Window size equals the kernel image size in bytes. The
    higher-half code entry, GDT alias, and IDT alias all fall inside this
    window.

0xFFFF_9000_0200_0000   BOOTSTRAP_STACK_WINDOW_BASE
    Bootstrap stack window.
    The bootstrap transition stack pages are mapped here. The higher-half
    stack top (alias_stack) is derived from this base plus the stack size.
    Currently 4 pages (16 KiB) deep.

0xFFFF_9000_0300_0000   BOOTSTRAP_DATA_WINDOW_BASE
    Bootstrap runtime data window.
    The kernel-owned transition data page is mapped here. This page carries
    the BootstrapRuntimeState struct across the CR3 switch until its
    contents are promoted into the retained context statics.
```

These three windows are not yet the permanent kernel virtual layout. They are
the bootstrap layout — the minimal higher-half footprint that the transition
root needs to carry the kernel from the identity-mapped post-switch entry point
into a stable higher-half `runtime-active` state.

### Address Space Region

All three windows sit in the same `0xFFFF_9000_xxxx_xxxx` region. This places
them in the upper canonical half (above `0xFFFF_8000_0000_0000`) and within a
`0x0000_0001_0000_0000` (4 GiB) span starting at `0xFFFF_9000_0000_0000`.

This region was chosen to be:

- clearly in the kernel half (bit 63 set, canonical)
- well separated from the low-address kernel load region
- far from common higher-half conventions like `0xFFFF_8000_0000_0000` used
  for direct maps, to avoid collisions when those regions are defined

### Alias Derivation

The kernel image alias window maps the kernel image starting at
`BOOTSTRAP_KERNEL_WINDOW_BASE`. Any address inside the kernel image is aliased
as:

```
alias = BOOTSTRAP_KERNEL_WINDOW_BASE + (address - kernel_image.start)
```

The handler delta (used to retarget IDT stub entries from their link-time
low addresses into the higher-half window) is:

```
handler_delta = BOOTSTRAP_KERNEL_WINDOW_BASE - kernel_image.start
```

Both are computed by `BootstrapRuntimeLayout` in `memory.rs`.

## What Is Not Decided

The following decisions are deferred but must be made before the virtual layout
grows beyond the bootstrap slice.

### 1. Direct map of physical memory

A direct map provides a fixed virtual-to-physical translation for all
(or most) physical RAM. It is the standard approach for higher-half kernels
that need to access arbitrary physical frames without per-frame mappings.

Common choices for x86_64:

- `0xFFFF_8000_0000_0000` — used by Linux, leaves the full lower half for
  user processes
- `0xFFFF_C000_0000_0000` — used when a larger guard gap is wanted below
  the direct map

Feox has not yet committed to a direct-map region or size. This decision
should be made before the early frame allocator or page-table layer needs
to access arbitrary physical frames through a virtual window.

### 2. Permanent kernel virtual layout

The three bootstrap windows are transition-time only. A permanent kernel
virtual layout needs to define:

- where the loaded kernel image lives permanently (versus the bootstrap alias)
- where per-core data is mapped
- where capability tables and IOMMU structures live
- what guard regions exist between zones

### 3. User-space / kernel split

x86_64 canonical address space gives roughly `0x0000_8000_0000_0000` for
user space and the upper half for the kernel. Feox should explicitly choose
and document the split before any process address space work begins.

### 4. MMIO windows

Device BARs and IOMMU-mapped regions need virtual address space. The layout
for kernel-owned MMIO mappings should be defined before the first direct
device access path is built.

## Recommended Next Step

Before the retained runtime service grows any further, define the permanent
kernel virtual layout. The minimum useful decision is:

1. Choose a direct-map base and maximum size.
2. Define where per-core data will live relative to the direct map.
3. Decide whether the bootstrap windows become part of the permanent layout
   or are replaced when the permanent layout is established.

Capture that decision in an update to this document. The implementation should
follow from the decision, not the other way around.

## Relationship To Other Docs

- `docs/PAGE_TABLE_PLAN.md` — the paging mechanism layer; the layout doc is
  orthogonal (mechanism vs. policy)
- `docs/MEMORY_OWNERSHIP_PHASES.md` — physical memory ownership phases; the
  direct-map decision connects phases 3 and 4 to virtual address allocation
- `docs/BOOTSTRAP_RUNTIME.md` — the bootstrap runtime context that uses these
  windows

## Bottom Line

Three windows are currently locked in code and proven under QEMU:

| Window | Base | Purpose |
|--------|------|---------|
| Kernel image alias | `0xFFFF_9000_0000_0000` | Higher-half code, GDT, IDT |
| Stack window | `0xFFFF_9000_0200_0000` | Bootstrap transition stack |
| Data window | `0xFFFF_9000_0300_0000` | Runtime state data page |

Everything above that — direct map, permanent kernel layout, user split, MMIO
windows — is still open and should be decided before the layout grows.
