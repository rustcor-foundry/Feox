# Feox Current Status

Last updated: 2026-05-22

## Posture

- status: early implementation — proven boot path, full code review complete
- classification: core product, early-stage platform lane
- product center: `no_std` Rust exokernel bring-up and hardware-shaped runtime work
- current source of truth: this repo root, its Rust workspace, and the boot harness under `tools/`

## What Is True Right Now

- `cargo test` passes — 74 tests across all crates
- `cargo kernel` passes
- `cargo loader` passes
- `tools/stage-efi.ps1` succeeds and produces a staged EFI tree under `target/feox-efi`
- the x86 host boot rail is working on this workstation end-to-end (QEMU launch, OVMF, loader, kernel handoff, retained runtime-active loop)
- the Gitea CI rail now targets `lx-ws01` for host tests, target builds, target lint/check coverage, and a bounded x86_64 QEMU smoke boot
- the full code review pass from `docs/CODE_REVIEW.md` is complete — all findings are now either resolved in code or intentionally deferred as later architecture work

### Security and correctness hardening (from code review)

- **S-01**: 64-bit TSS with dedicated 4 KiB IST stacks; NMI on IST1, double-fault on IST2; Task Register loaded via `ltr`
- **S-02**: `EFER.NXE` enabled before any NX PTE is live; `FLAG_NO_EXECUTE` on bootstrap stack and data pages
- **S-03**: CR4 SMEP/SMAP/UMIP conditionally set via CPUID leaf 7 check during `early_init`
- **S-04**: `hlt_loop` behavior documented — `cli` is intentional; NMIs remain unmasked by design
- **S-05**: `CoreId` widened to `u32` to match the ASI spec; all downstream uses updated
- **C-01**: `AtomicU32 CONTEXT_OWNER` claim guard on all `RuntimeContext` mutation functions
- **C-02**: `invlpg` after every PTE write in `map_4k_with` and `unmap_4k_with`; gated `cfg(target_os = "none")`
- **C-03**: `CONSOLE_READY: AtomicBool` in `console.rs` prevents console re-init inside exception handlers
- **C-04**: alignment `debug_assert` and aliasing invariant documented in `identity_mapped_table_mut`
- **C-05**: sequential-borrow safety documented in `ensure_child_table`
- **P-01**: CR3 switch path split by `cfg(debug_assertions)` — release build has no I/O on the hot path
- **P-02**: PRESENT|WRITABLE intermediate-entry policy documented as bootstrap-only in `ensure_child_table`
- **A-01**: `feox-nvme` replaced `Vec` with const-generic `InflightMap<const N>`; crate is now fully `no_std`
- **A-02**: `feox-async` now has `RunQueue`, `TaskHeader` with type-erased poll, `TaskCell<F>`, `SingleCoreExecutor`
- **A-03**: x86_64 ASI transport now installs `SYSCALL` / `SYSRET`, programs `IA32_STAR` / `IA32_LSTAR` / `IA32_FMASK`, carries a dedicated syscall stack, and dispatches typed ASI opcodes through shared `feox-asi` syscall and batch types
- **A-04**: bootstrap capability layer now has a 256-slot table, a registered physical-memory resource registry, a delegation tree with cascade release, working `cap_request` for physical pages, first capability-backed 4 KiB map helper coverage, and first `cap_list` / `cap_delegate` / `cap_release` syscall handling
- **A-04b**: bootstrap ASI memory lane now has shared `MemMap` / `MemVtoP` ABI types, a fixed 64 MiB bootstrap VM window, retained bootstrap mapping records, bootstrap-scoped `mem_map` / `mem_unmap` syscall handling, and first `mem_vtop` / `mem_vtop_batch` translation support for active physical-memory mappings. As of 2026-05-21 the live `mem_map` / `mem_unmap` / `mem_vtop` callers walk the active root through `BootstrapPageTableAccessSource` (a 20 KiB / 4-slot bootstrap page-table access window) instead of `BootstrapIdentityMappedPageTables`, and the bounded smoke now reaches `stage: runtime service idle` after a live `mem_map → mem_vtop → mem_unmap` cycle against a real bootstrap capability
- **A-05**: `PageTableEdges` frame-tree sidecar records every intermediate allocation for future reclaim

### Deferred (next sessions)

- full capability system follow-through: capability-checked memory/device operations and multi-process capability ownership beyond the bootstrap process

## Current Strengths

- clear crate separation: boot ABI, async runtime, NVMe primitives, loader, kernel bootstrap
- all code review safety and correctness gaps resolved before new feature work
- end-to-end QEMU boot trace through higher-half handoff, exception validation, and retained runtime
- real x86_64 ASI syscall transport with ring-3 selectors, STAR/LSTAR/FMASK setup, and first typed dispatch path
- first kernel capability authority lane with stale-handle detection, registered resources, delegation links, and bootstrap capability enumeration
- fixed-capacity, allocator-free data structures on all hot paths (executor, NVMe, paging, runtime state)
- explicit ownership model with claim guards and typed reservation categories

## Current Risks

- the retained runtime still idles after the command queue drains — broader subsystem bring-up not yet started
- the permanent virtual address layout is locked in `docs/VIRTUAL_ADDRESS_LAYOUT.md` and `memory.rs`; the direct map at `0xFFFF_C000_0000_0000` is live and covers every Usable RAM region (2 MiB bulk + 4 KiB head/tail) — per-core, MMIO, and kernel vmalloc remain policy markers awaiting implementation
- the capability system is still bootstrap-scoped — there is no multi-process table set, no device-resource population beyond physical memory, and the memory lane is still bootstrap-window-only rather than a real per-process VM subsystem; the NVMe BAR is currently mapped without going through the capability layer (kernel-internal API only)
- the access window dynamic slot capacity is 4, sized for a single 4-level walk; any future caller that needs simultaneous aliases for more than four distinct frames must expand the window first
- `mem_vtop_batch` is now exercised by the bootstrap self-test on every boot (4-page contiguous capability, batch translation with contiguity check)
- per-core layout reserves 32 cores × 1 TiB each (`PER_CORE_MAX_CORES = 32`); SMP work beyond that ceiling must revisit the layout before scaling further

## Recommended Entry Points

```
cargo test
cargo kernel
cargo loader
powershell -ExecutionPolicy Bypass -File .\tools\stage-efi.ps1
```

Use those before deeper kernel or loader changes.

## Immediate Next Focus

1. **AP boot v2: Rust ap_entry** — v1 lands a 16-bit trampoline,
   sends INIT-SIPI-SIPI through the LAPIC, and confirms one AP runs
   the trampoline by polling a magic word it writes before halting.
   v2 extends the trampoline through real -> protected -> long mode,
   shares the BSP's CR3, loads each AP's own GS_BASE, and jumps to a
   Rust `ap_entry` that signals alive via a shared atomic. Arch-heavy.
2. **Multi-device / multi-namespace block layer** — the current
   `crate::block` module is hardcoded to one NVMe device with one I/O
   queue pair. A real block layer needs device enumeration, namespace
   handling, and per-queue scheduling.
3. **Storage ABI v3 (DmaPool + EventSlot)** — v2 enforces the device
   cap; remaining storage work is wiring `CapType::DmaPool` end-to-end
   through `cap_request` (today buffers must be `PhysicalMemory` caps,
   which works in bootstrap but doesn't generalize once an IOMMU is
   in the mix) and adding an EventSlot/park variant so callers can
   sleep on completion instead of spin-polling. See
   `docs/STORAGE_ABI.md`.

Per-core data exists for core 0 (`crate::per_core::PerCoreData` with
`self_ptr` / `magic` / `core_id` / `_reserved`). The page is mapped at
the locked virtual slot `PER_CORE_BASE` (`0xFFFF_E000_0000_0000`)
using PML4/PDPT/PD/PT intermediates prebuilt during transition root
construction for *all* `PER_CORE_MAX_CORES` (32) strides, so secondary
cores' per-core slots are ready to receive leaf inserts without a
runtime frame allocator. `IA32_GS_BASE` points at core 0's slot so
kernel code reaches its own per-CPU block via a `gs:[0]` load through
`crate::per_core::current()`. Validated on every boot by
`run_per_core_probe`.

SMP discovery + AP boot v1 are live: the loader forwards the ACPI
RSDP and a reserved sub-1-MiB trampoline frame through `BootInfo`
(ABI v3). `crate::acpi::parse_topology` walks RSDP → XSDT → MADT to
enumerate Local APIC entries. `crate::lapic` maps the LAPIC MMIO and
exposes `send_init` / `send_startup`. `crate::smp::bring_up_first_ap`
writes a hand-assembled 14-byte real-mode trampoline to the reserved
frame, sends INIT-SIPI-SIPI to the first non-BSP LAPIC, and observes
the AP running by polling a magic word the trampoline writes before
halting. `run_ap_boot_probe` exercises this on every boot — with
QEMU `-smp 4`, AP 1 reliably reports alive
(`ap-boot-probe: AP alive (magic observed)`). The trampoline does
not yet reach Rust on the AP; that's the next focus.

The executor "enqueue gap" is closed, the kernel block surface
(`crate::block::initialize` / `read` / `drain` / `shutdown`) wraps the
NVMe submit/drain primitives behind free functions, a long-lived
`crate::block::drainer_task` pumps completions on every executor pass,
and the storage ABI lane (`AsiOp::StorageSubmitRead` 0x0500 /
`AsiOp::StoragePoll` 0x0501) routes submit + poll through the syscall
dispatch with v2 device-cap enforcement (`block::register_device_capability`
mints a `CapType::StorageDevice` root cap over the controller's BAR;
`dispatch_storage_submit_read` verifies `args.device` against it) and
v1 capability-backed buffers (the `buffer` field is a `CapHandle`,
translated via `crate::capability::cap_to_phys_base` to a
`PhysicalMemory` resource's base address) — both the positive path
and a negative path (bogus device cap → `0xFFFF_0500`) are exercised
on every boot by the storage-abi self-test in `run_nvme_admin_probe`.

Bootstrap VM hardening, the permanent virtual address layout, the direct
map, the access-window retirement, broadened live self-test coverage,
MMIO bring-up, PCI enumeration, first-device BAR mapping (NVMe), the
NVMe admin queue handshake, the full NVMe I/O queue + LBA read
lifecycle, and the first storage ABI surface are all resolved — see
`docs/PAGE_TABLE_ACCESS_PLAN.md` Retirement section, the live status
table in `docs/VIRTUAL_ADDRESS_LAYOUT.md`, and `docs/STORAGE_ABI.md`.
