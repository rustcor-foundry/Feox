# Feox Testing

This is the current validation rail for Feox.

## Automated Baseline

Run from the repo root:

```powershell
powershell -ExecutionPolicy Bypass -File .\tools\check-host.ps1 -Architecture x86_64
cargo test
cargo kernel
cargo loader
```

These validate the current workspace, bare-metal kernel image, UEFI loader
build path, and host readiness for the x86 boot rail.

## Gitea CI Rail

The normal Feox CI runner is `lx-ws01`.

The Gitea workflow now validates four lanes on that runner:

- `cargo test`
- `cargo kernel`
- `cargo loader`
- bounded `x86_64` QEMU smoke boot through `pwsh -File ./tools/run-qemu-smoke.ps1`

For `lx-ws01` to stay green, it must provide:

- `pwsh`
- `qemu-system-x86_64`
- x86_64 OVMF firmware code and vars files
- Rust targets `x86_64-unknown-none` and `x86_64-unknown-uefi`

The PowerShell harnesses now search common Linux paths for QEMU and OVMF in
addition to the existing Windows paths, so the same scripts are used on both
the workstation and the CI runner.

## Manual Bootstrap Rail

Stage the EFI tree:

```powershell
powershell -ExecutionPolicy Bypass -File .\tools\stage-efi.ps1
```

Attempt a QEMU boot:

```powershell
powershell -ExecutionPolicy Bypass -File .\tools\run-qemu.ps1
```

## Expected Current Results

Verified on 2026-03-27:

- `cargo test`: passes
- `cargo kernel`: passes
- `cargo loader`: passes
- `tools/stage-efi.ps1`: passes and stages `BOOTX64.EFI` plus `FEOXKERN.ELF`
- `tools/check-host.ps1 -Architecture x86_64`: passes
- `tools/run-qemu.ps1`: launches QEMU and reaches the Feox loader/kernel path on this workstation
- bounded debug-log validation now shows:
  - UEFI boot manager reaches `BOOTX64.EFI`
  - the loader opens `FEOXKERN.ELF`
  - control transfers into the kernel
  - the kernel accepts the boot handoff and logs bootstrap state

## Boot Success Criteria

The current useful success bar is:

- UEFI loader starts
- loader reports image and entrypoint details
- control transfers into kernel `_start`
- debug or serial output shows the bootstrap banner and early handoff trace
- the machine reaches the known halt loop instead of dying silently

## Current Host Prerequisites

- `rustup target add x86_64-unknown-none x86_64-unknown-uefi`
- QEMU and firmware must be discoverable either from the system install paths or the repo-local `tools/host/msys64` tree

## Debug Capture Note

`tools/run-qemu.ps1` now writes a debug-console log under `target/feox-qemu/`
using the ISA debug console at `0x402`.

That log is the current source of truth for first-boot validation on this
workstation because it captures both loader and kernel output even when COM1
behavior is noisy or inconsistent.

For CI, `tools/run-qemu-smoke.ps1` uses the same harness in bounded mode and
asserts that the boot log reaches the configured success marker.
