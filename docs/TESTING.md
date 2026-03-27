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

These validate the current workspace, bare-metal kernel image, and UEFI loader
build path.

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
- `tools/run-qemu.ps1`: currently fails on this workstation because QEMU is not installed or not discoverable yet

## Boot Success Criteria

When QEMU and OVMF are available, the first useful success bar is:

- UEFI loader starts
- loader reports image and entrypoint details
- control transfers into kernel `_start`
- serial output shows the bootstrap banner or early handoff trace
- the machine reaches the known halt loop instead of dying silently

## Current Host Prerequisites

- `rustup target add x86_64-unknown-none x86_64-unknown-uefi`
- install or point to QEMU
- install or point to OVMF firmware files
