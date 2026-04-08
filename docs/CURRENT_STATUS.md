# Feox Current Status

Last updated: 2026-04-08

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
- **A-04b**: bootstrap ASI memory lane now has shared `MemMap` / `MemVtoP` ABI types, a fixed 64 MiB bootstrap VM window, retained bootstrap mapping records, bootstrap-scoped `mem_map` / `mem_unmap` syscall handling, and first `mem_vtop` / `mem_vtop_batch` translation support for active physical-memory mappings
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
- permanent virtual address layout decisions are not yet locked (direct-map base, per-core zones, MMIO windows), though the bootstrap layout now reserves a dedicated page-table access window for the next live paging pass
- the capability system is still bootstrap-scoped — there is no multi-process table set, no device-resource population beyond physical memory, and the memory lane is still bootstrap-window-only rather than a real per-process VM subsystem
- the new bootstrap VM lane is proven in host tests and syscall/unit coverage, but live post-handoff use still depends on page-table-access assumptions that are not yet hardened for the higher-half runtime path

## Recommended Entry Points

```
cargo test
cargo kernel
cargo loader
powershell -ExecutionPolicy Bypass -File .\tools\stage-efi.ps1
```

Use those before deeper kernel or loader changes.

## Immediate Next Focus

1. **Bootstrap VM hardening** — validate the widened bootstrap memory lane under QEMU, especially `mem_vtop` / `mem_vtop_batch`, and decide how page-table-access mappings should evolve beyond the current bootstrap assumptions
2. **Virtual address layout** — lock the permanent direct-map base and per-core/MMIO zones in `docs/VIRTUAL_ADDRESS_LAYOUT.md`

The next concrete design document for item 1 is `docs/PAGE_TABLE_ACCESS_PLAN.md`.
