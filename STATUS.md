# Feox Status

Checkpoint date: March 25, 2026

## Project Direction

Feox is being built as a lean `no_std` Rust exokernel with a bias toward:

- explicit ownership
- per-core execution
- fixed-size structures on hot paths
- allocator-free early boot
- abstractions that do not hide hardware shape

## What Exists Now

The repository has moved from design-only documents into a real Rust workspace.

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
- x86_64 COM1 early serial output
- panic-to-serial path
- bootstrap GDT install
- bootstrap IDT install
- fatal exception stubs for vectors `0-31`
- linker-backed kernel image bounds
- active `CR3` / PML4 introspection on the current x86_64 lane
- early physical memory region model
- linear 4 KiB frame allocator over boot-supplied usable regions
- first architecture-neutral cleanup pass started around bootstrap and console facades

Boot path capabilities in place:

- shared `BootInfo` / `BootHandoff` ABI in `rdi`
- UEFI loader that reads `\EFI\BOOT\FEOXKERN.ELF`
- ELF validation for the active UEFI loader architecture
- PT_LOAD segment loading at linked addresses
- UEFI memory map translation into Feox memory regions
- kernel image range marked as `Kernel`
- loader logs mirrored to both UEFI console and the current x86 serial path

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

Artifacts and harness:

- staged EFI tree is produced under `target\feox-efi\EFI\BOOT`
- `BOOTX64.EFI` is the current x86_64 UEFI loader artifact
- `FEOXKERN.ELF` is the kernel image
- `tools\stage-efi.ps1` and `tools\run-qemu.ps1` are now parameterized by architecture
- the run script currently supports x86_64 firmware discovery directly and has the first scaffolding for future ARM64 firmware selection

## Current Stopping Point

The project is ready for first real UEFI boot attempts under QEMU.

The only blocker on this machine at checkpoint time is host tooling:

- QEMU was not installed or not found in the expected Windows paths
- current x86 firmware was not available in the expected Windows paths

The wiring in-repo is ready for the next step once those are installed.

## Next Step After QEMU Setup

Run:

```powershell
powershell -ExecutionPolicy Bypass -File .\tools\run-qemu.ps1
```

Expected early success criteria:

- UEFI loader starts
- loader prints kernel entry/image range
- control transfers into kernel `_start`
- kernel serial output shows bootstrap banner
- kernel reports descriptor tables and memory handoff summary
- machine ends in the known-good halt loop

## Likely Next Milestones

After first boot under QEMU, the next good sequence is:

1. Prove the loader-to-kernel handoff under emulation and capture serial logs.
2. Add page-table management on top of the real boot memory map.
3. Build the first direct-map / physical memory management layer.
4. Add per-core bootstrap state and interrupt-controller groundwork.
5. Start capability-kernel core and syscall boundary bring-up.

## Important Files

- [README.md](D:\Paul\Software%20Projects\Feox\README.md)
- [Cargo.toml](D:\Paul\Software%20Projects\Feox\Cargo.toml)
- [crates/feox-boot/src/lib.rs](D:\Paul\Software%20Projects\Feox\crates\feox-boot\src\lib.rs)
- [kernel/feox-xokernel/src/main.rs](D:\Paul\Software%20Projects\Feox\kernel\feox-xokernel\src\main.rs)
- [kernel/feox-xokernel/src/boot.rs](D:\Paul\Software%20Projects\Feox\kernel\feox-xokernel\src\boot.rs)
- [kernel/feox-xokernel/src/memory.rs](D:\Paul\Software%20Projects\Feox\kernel\feox-xokernel\src\memory.rs)
- [loader/feox-loader-uefi/src/main.rs](D:\Paul\Software%20Projects\Feox\loader\feox-loader-uefi\src\main.rs)
- [tools/stage-efi.ps1](D:\Paul\Software%20Projects\Feox\tools\stage-efi.ps1)
- [tools/run-qemu.ps1](D:\Paul\Software%20Projects\Feox\tools\run-qemu.ps1)
