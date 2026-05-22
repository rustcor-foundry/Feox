# Feox Virtual Address Space Layout

Last updated: 2026-05-21

This document captures the locked virtual address space layout for the
x86_64 lane. The decisions here are policy: code follows the layout, not the
other way around.

## Context

`docs/PAGE_TABLE_PLAN.md` deferred the direct-map and virtual layout decision
until the first operational mapping layer was proven and the live VM lane
was hardened. Both milestones are now past:

- The bootstrap runtime has a working higher-half stack, data, kernel image,
  and capability-backed `mem_map` lane.
- Live `mem_map` / `mem_unmap` / `mem_vtop` walk the active root through a
  bootstrap page-table access window — raw identity dereference is gone from
  the live paging callers (`docs/PAGE_TABLE_ACCESS_PLAN.md`).

This document now records the permanent layout commitments.

## User / Kernel Split

Feox uses the standard x86_64 canonical 48-bit split:

```
0x0000_0000_0000_0000 .. 0x0000_7FFF_FFFF_FFFF   USER HALF   (128 TiB)
0xFFFF_8000_0000_0000 .. 0xFFFF_FFFF_FFFF_FFFF   KERNEL HALF (128 TiB)
```

The kernel half is shared across processes (PML4 entries 256..511 are
cloned). The user half is per-process (PML4 entries 0..255). Bit 47 selects
the half; bits 63..48 are sign-extended copies of bit 47.

## Locked Kernel-Half Regions

Each PML4 entry covers 512 GiB.

| Region | Base | PML4 | Size | Status |
|--------|------|------|------|--------|
| Reserved-low | `0xFFFF_8000_0000_0000` | 256–287 | 16 TiB | reserved |
| **Bootstrap + permanent kernel image** | `0xFFFF_9000_0000_0000` | 288–319 | 16 TiB slot | in use |
| Reserved-bridge | `0xFFFF_A000_0000_0000` | 320–383 | 32 TiB | reserved |
| **Direct map of physical memory** | `0xFFFF_C000_0000_0000` | 384–447 | 32 TiB | locked, not implemented |
| **Per-core kernel data** | `0xFFFF_E000_0000_0000` | 448–479 | 32 TiB (1 TiB/core × 32 cores) | locked, not implemented |
| **MMIO** | `0xFFFF_F000_0000_0000` | 480–495 | 8 TiB | locked, not implemented |
| **Kernel vmalloc / capability tables** | `0xFFFF_F800_0000_0000` | 496–511 | 8 TiB | locked, not implemented |

### Bootstrap + permanent kernel image (0xFFFF_9000)

This is both the working bootstrap window and the permanent home for the
kernel image. The linker continues to build the ELF at low virtual addresses
(`. = 1M` in `linker.ld`); the boot path installs a higher-half alias of
the loaded image at `0xFFFF_9000_0000_0000`. The phrase "bootstrap window"
in earlier docs implied this would be replaced later — that intent is now
withdrawn. This IS the permanent kernel home.

Inside this 16 TiB slot the live sub-windows are:

```
0xFFFF_9000_0000_0000   BOOTSTRAP_KERNEL_WINDOW_BASE
    Kernel image alias. Window size equals the kernel image size in bytes.
    Higher-half code entry, GDT alias, and IDT alias all fall inside this
    window.

0xFFFF_9000_0200_0000   BOOTSTRAP_STACK_WINDOW_BASE
    Bootstrap stack window. Currently 4 pages (16 KiB) deep.

0xFFFF_9000_0300_0000   BOOTSTRAP_DATA_WINDOW_BASE
    Runtime data window for the BootstrapRuntimeState struct.

0xFFFF_9000_0400_0000   BOOTSTRAP_VM_WINDOW_BASE
    Capability-backed VM arena. 64 MiB. 4 KiB mappings only. Backed by
    DirectMapPageTables for all live paging operations.

0xFFFF_9000_0800_0000   BOOTSTRAP_PAGE_TABLE_ACCESS_WINDOW_BASE
    Page-table access window (retired). 20 KiB address-space slot kept
    reserved in case a future tactical mechanism wants this footprint;
    no live mappings, no helper code.
```

Headroom inside the 16 TiB slot is intentionally large; the bootstrap
sub-windows together consume less than 256 MiB.

Alias derivation:

```
alias = BOOTSTRAP_KERNEL_WINDOW_BASE + (address - kernel_image.start)
handler_delta = BOOTSTRAP_KERNEL_WINDOW_BASE - kernel_image.start
```

Both are computed by `BootstrapRuntimeLayout` in `memory.rs`.

### Direct map (0xFFFF_C000)

A 32 TiB direct map of physical memory. Translation rule:

```
direct_map_va = DIRECT_MAP_BASE + phys_addr
phys_addr     = direct_map_va - DIRECT_MAP_BASE
```

Properties:
- Read-write, kernel-only, NX
- Covers any physical RAM Feox is likely to see in practice (32 TiB ceiling)
- Sized at a clean PML4 boundary (64 PML4 entries, 0xC000..0xE000)
- Lives well above the bootstrap windows at 0xFFFF_9000 — no overlap

The direct map is **live as of 2026-05-21**. The transition root builder in
`boot.rs` walks `BootMemoryMap` and installs:

- one 2 MiB huge-page entry per aligned interior chunk of every `Usable` region
- 4 KiB entries for the unaligned head (between region start and the first
  2 MiB boundary) and tail (between the last 2 MiB boundary and region end)

Non-Usable phys ranges (kernel image, MMIO, BIOS ROM, ACPI) are intentionally
not covered through the direct map. Callers that need those use the
bootstrap kernel-image alias or a dedicated mapping in the locked MMIO zone.

`DirectMapPageTables` in `paging.rs` is the live `PageTableFrameSource` /
`PageTableFrameMutSource` for `mem_map` / `mem_unmap` / `mem_vtop` walks. It
simply offsets a physical frame to `DIRECT_MAP_BASE + phys` — no slot
reservation, no LRU.

The bootstrap page-table access window mechanism was retired on 2026-05-21.
The address-space slot at `0xFFFF_9000_0800_0000` is preserved but the
helper types (`BootstrapPageTableAccessWindow`, `BootstrapPageTableAccessSource`,
`BootstrapPageTableAccessReservation`, retained slot state, and
control-PT self-map prebuild) are all removed.

### Per-core kernel data (0xFFFF_E000)

One 1 TiB slot per logical core, indexed by `CoreId`:

```
core_base(core_id) = 0xFFFF_E000_0000_0000 + (core_id as u64 * (1 << 40))
```

Per-core sub-allocations (sub-windows TBD, but committed to live inside
this slot):
- per-core bootstrap stack
- per-core IDT, GDT, TSS, IST stacks
- per-core retained context
- per-core executor `RunQueue` storage when multi-core support lands

Sized for up to 32 cores at 1 TiB each (32 TiB total, PML4[448..480]).
This is the upper bound the lock guarantees; if Feox needs more cores
later the layout must be revisited.

The current single-core bootstrap path puts its GDT/IDT/TSS/IST inside
the kernel image window. When SMP is brought up, those per-core artifacts
move into this region; the bootstrap window keeps only globally shared
kernel state.

### MMIO (0xFFFF_F000)

8 TiB reserved for kernel-owned MMIO mappings. Commitments:
- Device BARs and IOMMU-mapped regions go here, not in the direct map
- Caching attributes are per-region (UC for typical MMIO, WC for
  framebuffers, write-back for coherent DMA buffers)
- Allocation is bump-style at first, with a freelist when device hot-plug
  is supported

**Status (2026-05-21):** Live for the prebuilt sub-window.

The transition root builder reserves `MMIO_PREBUILT_SIZE` (64 MiB) starting
at `MMIO_BASE` by walking `prepare_4k_pages_with` over that range; all
intermediate PML4/PDPT/PD/PT structures are in place before the higher-half
handoff. The `mmio::mmio_map_bootstrap(phys, length, writable, uncached)`
helper bump-allocates a virtual range inside this sub-window and installs
the leaf entries directly through `DirectMapPageTables` — no allocator is
needed because the intermediates are prebuilt.

UC mappings set both PCD (cache-disable, bit 4) and PWT (write-through,
bit 3) on every leaf, producing strict UC per the Intel SDM PAT table.
Non-UC mappings (passing `uncached = false`) leave both bits clear and
inherit write-back caching. Write-combining via PAT bits is not yet
exposed; that comes when the first WC consumer (a framebuffer) arrives.

When a device subsystem maps a BAR larger than the prebuilt sub-window can
hold, the prebuild must be grown first — there is no runtime page-table
allocator in the post-handoff path yet.

### Kernel vmalloc / capability tables (0xFFFF_F800)

8 TiB for kernel-side dynamic mappings:
- Capability table backing storage when it grows beyond the bootstrap
  256-slot fixed table
- Kernel heap (if and when a real allocator is introduced beyond the
  bootstrap allocator-free model)
- Generic kernel virtual mappings

## Reserved Regions

The reserved entries (`0xFFFF_8000_0000_0000` and `0xFFFF_A000_0000_0000`)
are intentionally empty. They are guard regions: any access to them is a
bug. The kernel must never install a leaf mapping inside a reserved region
without first updating this document and the corresponding constants in
`memory.rs`.

## Address Space Region Summary

```
USER
  0x0000_0000_0000_0000 .. 0x0000_7FFF_FFFF_FFFF   per-process

KERNEL (shared)
  0xFFFF_8000_0000_0000 .. 0xFFFF_8FFF_FFFF_FFFF   reserved-low

  0xFFFF_9000_0000_0000 .. 0xFFFF_9FFF_FFFF_FFFF   bootstrap + kernel image
    0xFFFF_9000_0000_0000   kernel image alias
    0xFFFF_9000_0200_0000   bootstrap stack
    0xFFFF_9000_0300_0000   bootstrap data
    0xFFFF_9000_0400_0000   bootstrap VM window (64 MiB)
    0xFFFF_9000_0800_0000   page-table access window (20 KiB, reserved slot only)

  0xFFFF_A000_0000_0000 .. 0xFFFF_BFFF_FFFF_FFFF   reserved-bridge

  0xFFFF_C000_0000_0000 .. 0xFFFF_DFFF_FFFF_FFFF   DIRECT MAP (32 TiB)

  0xFFFF_E000_0000_0000 .. 0xFFFF_EFFF_FFFF_FFFF   per-core kernel data
    core_id * 1 TiB stride

  0xFFFF_F000_0000_0000 .. 0xFFFF_F7FF_FFFF_FFFF   MMIO (8 TiB)

  0xFFFF_F800_0000_0000 .. 0xFFFF_FFFF_FFFF_FFFF   kernel vmalloc + cap tables (8 TiB)
```

## Implementation Status

| Region | Constants in memory.rs | Live mappings | Callers |
|--------|------------------------|---------------|---------|
| Bootstrap kernel image | yes | yes | boot.rs transition root |
| Bootstrap stack | yes | yes | boot.rs |
| Bootstrap data | yes | yes | boot.rs |
| Bootstrap VM window | yes | prebuilt; populated by `mem_map` | vm.rs |
| Page-table access window | yes (address-space slot only) | no (retired 2026-05-21) | none |
| Direct map | yes | yes (2 MiB bulk + 4 KiB head/tail per Usable region) | `DirectMapPageTables` in paging.rs; consumed by vm.rs live `mem_map` / `mem_unmap` / `mem_vtop` |
| Per-core kernel data | yes (policy markers) | no | none yet |
| MMIO | yes | prebuilt sub-window (`MMIO_PREBUILT_SIZE`, 64 MiB) ready for device BARs; bump-allocated, UC by default | `mmio::mmio_map_bootstrap` / `mmio::mmio_unmap_bootstrap` |
| Kernel vmalloc | yes (policy markers) | no | none yet |

The "policy marker" constants exist so callers can reference the locked
addresses today even though no live mappings are installed. When a region
is brought up, the implementation pass adds the mapping logic and updates
this table.

## Relationship To Other Docs

- `docs/PAGE_TABLE_PLAN.md` — the paging mechanism layer; this document
  is the policy layer (mechanism vs. policy)
- `docs/PAGE_TABLE_ACCESS_PLAN.md` — the bootstrap page-table access
  window mechanism, now fully implemented
- `docs/MEMORY_OWNERSHIP_PHASES.md` — physical memory ownership phases;
  the direct map connects ownership phases 3 and 4 to virtual address
  allocation
- `docs/BOOTSTRAP_RUNTIME.md` — the bootstrap runtime context that uses
  the 0xFFFF_9000 windows
