# Feox Current Status

Last updated: 2026-03-27

## Posture

- status: early implementation with a real bootstrap path
- classification: core product, early-stage platform lane
- product center: `no_std` Rust exokernel bring-up and hardware-shaped runtime work
- current source of truth: this repo root, its Rust workspace, and the boot harness under `tools/`

## What Is True Right Now

- the repo has moved beyond design-only material into a working Rust workspace
- `cargo test` passes
- `cargo kernel` passes
- `cargo loader` passes
- `tools/stage-efi.ps1` succeeds and produces a staged EFI tree under `target/feox-efi`
- the current machine is now provisioned with the required Rust targets for the kernel and UEFI loader
- the x86 host boot rail is now working on this workstation, including QEMU launch, OVMF discovery, loader execution, and kernel handoff
- the first architecture-neutral cleanup pass is now in progress, with generic kernel code no longer reaching directly into `arch::x86_64` for core bootstrap/logging hooks
- early console and logging now also pass through a kernel-level facade instead of tying generic paths directly to the x86 serial module
- the loader ELF check and PowerShell build/staging harnesses are now architecture-aware, even though `x86_64` remains the only implemented kernel lane today
- `tools/check-host.ps1` now gives one explicit host-readiness entrypoint for the x86 and future ARM lanes
- the next x86-side design steps are now written down explicitly in `docs/MEMORY_OWNERSHIP_PHASES.md` and `docs/PAGE_TABLE_PLAN.md`
- the early frame allocator now supports explicit physical reservations and the bootstrap path reserves the kernel image plus active page-table root before handing frames out
- early kernel-owned memory is now tracked with typed bootstrap reservation categories so upcoming page-table and per-core state work has a disciplined place to land
- the first x86 paging ownership helper now exists, with a bootstrap paging-frame allocator that records new page-table frames under typed kernel reservations
- the first x86 page-table query layer now exists, with a root wrapper, entry model, and 4 KiB translation walk over a supplied frame source
- the first x86 4 KiB mapping primitive now exists, allocating intermediate tables through a supplied paging allocator and rejecting remaps or huge-page cases
- the first x86 unmap path now exists, and the repo has a passing `map -> translate -> unmap` lifecycle test for the 4 KiB bootstrap mechanism layer
- the loader and kernel now mirror early output to QEMU's debug console, giving the repo a reliable first-boot trace path on this host

## Current Strengths

- clear crate separation between boot ABI, async runtime work, NVMe primitives, loader, and kernel bootstrap
- honest low-level direction centered on ownership, per-core execution, and allocator-free early boot
- real bootstrap path from UEFI loader into a bare-metal kernel image
- useful host scripts for staging and QEMU launch instead of a purely conceptual boot plan
- clear strategic role as the lowest-level systems product in the portfolio, not just a side research repo

## Current Risks

- the current x86 bootstrap still halts in the early known-good loop, so successful boot visibility now outpaces real subsystem bring-up
- first usable-frame selection during the live handoff path still needs another look because the current bootstrap log reports `first_usable_frame=0x0`
- product framing had been underplaying the repo as a research lane instead of a core early-stage product

## Recommended Entry Points

- `cargo test`
- `cargo kernel`
- `cargo loader`
- `powershell -ExecutionPolicy Bypass -File .\tools\stage-efi.ps1`

Use those before deeper kernel or loader changes.

## Immediate Next Focus

1. document the first successful loader-to-kernel boot trace from this workstation
2. investigate why the live bootstrap allocator path reports `first_usable_frame=0x0`
3. surface the new paging lifecycle through more explicit serial/debug bootstrap checks
4. keep separating generic kernel plumbing from x86-specific implementation details before starting an ARM64 lane
