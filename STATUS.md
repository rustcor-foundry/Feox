# Feox Status

Checkpoint date: March 27, 2026

## Project Direction

Feox is being built as a lean `no_std` Rust exokernel with a bias toward:

- explicit ownership
- per-core execution
- fixed-size structures on hot paths
- allocator-free early boot
- abstractions that do not hide hardware shape

## What Exists Now

The repository has moved from design-only documents into a real Rust workspace
with a proven end-to-end boot path on this workstation.

Current crates and layers:

- `feox-asi`: shared ASI-facing types
- `feox-async`: executor state machine primitives
- `feox-boot`: shared boot ABI and physical memory map types
- `feox-nvme`: NVMe inflight tracking primitives
- `feox-xokernel`: bare-metal kernel bootstrap
- `feox-loader-uefi`: thin UEFI loader for the kernel ELF

Kernel/bootstrap capabilities in place:

- explicit `_start` entrypoint
- dedicated bootstrap stack
- x86_64 COM1 early serial output and ISA debug console at `0x402`
- panic-to-serial path
- bootstrap GDT install
- bootstrap IDT install
- fatal exception stubs for vectors `0-31`
- linker-backed kernel image bounds
- active `CR3` / PML4 introspection on the current x86_64 lane
- early physical memory region model
- linear 4 KiB frame allocator over boot-supplied usable regions with explicit
  physical reservations
- typed bootstrap reservation categories: kernel image, active page-table root,
  legacy low memory, bootstrap page tables, per-core state
- first x86 4 KiB map / translate / unmap lifecycle implemented and tested
- architecture-neutral cleanup pass: generic kernel code routes through an arch
  facade instead of reaching directly into `arch::x86_64`
- kernel-level early console facade

Boot path capabilities in place:

- shared `BootInfo` / `BootHandoff` ABI in `rdi`
- UEFI loader that reads `\EFI\BOOT\FEOXKERN.ELF`
- ELF validation for the active UEFI loader architecture
- PT_LOAD segment loading at linked addresses
- UEFI memory map translation into Feox memory regions
- kernel image range marked as `Kernel`
- loader logs mirrored to both UEFI console and the current x86 serial path

Higher-half handoff capabilities in place:

- bootstrap-owned transition page-table root separate from firmware-owned root
- CR3 handoff into the kernel-owned transition root under QEMU
- kernel-owned transition stack pages for the post-switch path
- higher-half code, stack, and data all survive the CR3 handoff
- GDT and IDT reloaded from higher-half aliases after the stack switch
- non-fatal breakpoint validation via `iretq` proving the higher-half exception
  path before entering the runtime service
- named `BootstrapRuntimeLayout` with explicit kernel, stack, and data windows

Retained runtime capabilities in place:

- retained shared `RuntimeSnapshot` published after the higher-half handoff
- retained per-core `BootstrapCoreContext` with active root, stack, and entry
- retained bootstrap event timeline (8-slot rolling buffer)
- retained `RuntimeServiceState` with owner core, phase, iteration, and last action
- retained `RuntimeServiceReport` with derived accounting of window and stack sizes
- retained `RuntimeServiceHeartbeat` with beat count and event observation
- retained `RuntimeReadinessState` and `RuntimeReadySummary` published when the
  service loop settles
- state-driven FIFO command queue driving the runtime service through:
  `RefreshSnapshot → RefreshAccounting → ReportTimeline → UpdateHeartbeat`
  with heartbeat-gated retry before settling into `PublishReady → EnterIdle`
- 5-stage bootstrap runtime stage model:
  `Prepared → IdentityActive → AliasActive → ExceptionValidated → RuntimeActive`

## Review Fixes Already Baked In

The original review findings were turned into code, not just doc edits:

- wake-during-poll is preserved in the async task state machine
- NVMe command IDs are not reused while still owned by a live future
- NVMe futures are core-local and intentionally not `Send`
- fail-all paths complete even not-yet-polled inflight commands

## What Has Been Verified

Verified on this checkpoint:

- `cargo test`
- `cargo kernel`
- `cargo loader`
- `powershell -ExecutionPolicy Bypass -File .\tools\stage-efi.ps1`
- `tools/check-host.ps1 -Architecture x86_64` passes on this workstation
- `tools/run-qemu.ps1` launches QEMU, reaches the Feox loader/kernel path, and
  captures a full bootstrap trace including higher-half runtime-active state

Artifacts and harness:

- staged EFI tree is produced under `target\feox-efi\EFI\BOOT`
- `BOOTX64.EFI` is the current x86_64 UEFI loader artifact
- `FEOXKERN.ELF` is the kernel image
- `tools\stage-efi.ps1` and `tools\run-qemu.ps1` are parameterized by architecture
- the run script supports x86_64 firmware discovery and has scaffolding for future ARM64

## Current Posture

The current working QEMU boot trace proves:

- UEFI boot manager reaches `BOOTX64.EFI`
- loader opens and validates `FEOXKERN.ELF`
- control transfers into kernel `_start`
- kernel accepts boot handoff and logs bootstrap state
- bootstrap page-table transition succeeds under QEMU
- higher-half code, stack, and data are all live after the CR3 handoff
- breakpoint validation returns cleanly via `iretq`
- retained runtime service enters `runtime-active`, drains the command queue,
  reports retained state, and settles into idle

## Next Focus

See [docs/CURRENT_STATUS.md](docs/CURRENT_STATUS.md) for the current recommended
next steps.

## Important Files

- [README.md](README.md)
- [Cargo.toml](Cargo.toml)
- [crates/feox-boot/src/lib.rs](crates/feox-boot/src/lib.rs)
- [kernel/feox-xokernel/src/main.rs](kernel/feox-xokernel/src/main.rs)
- [kernel/feox-xokernel/src/boot.rs](kernel/feox-xokernel/src/boot.rs)
- [kernel/feox-xokernel/src/memory.rs](kernel/feox-xokernel/src/memory.rs)
- [kernel/feox-xokernel/src/paging.rs](kernel/feox-xokernel/src/paging.rs)
- [kernel/feox-xokernel/src/runtime_context.rs](kernel/feox-xokernel/src/runtime_context.rs)
- [loader/feox-loader-uefi/src/main.rs](loader/feox-loader-uefi/src/main.rs)
- [tools/stage-efi.ps1](tools/stage-efi.ps1)
- [tools/run-qemu.ps1](tools/run-qemu.ps1)
