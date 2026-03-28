# Feox Workstation Entry

This is the practical pickup path for Feox on a Windows workstation.

## What Feox Is

Feox is a `no_std` Rust exokernel product in an early implementation stage. It is not a packaged desktop
product and it should not be approached like one. The real front door is the
Cargo workspace plus the EFI staging and QEMU harness under `tools/`.

## Recommended Pickup Order

Run these from the repo root:

```powershell
powershell -ExecutionPolicy Bypass -File .\tools\check-host.ps1 -Architecture x86_64
cargo test
cargo kernel
cargo loader
powershell -ExecutionPolicy Bypass -File .\tools\stage-efi.ps1
```

If those all pass, the repo is in a good local state for deeper kernel or
loader work.

## Host Prerequisites

Rust targets required for the current bootstrap lane:

```powershell
rustup target add x86_64-unknown-none x86_64-unknown-uefi
```

Host emulation tooling required for first full boot attempts:

- a working `qemu-system-x86_64.exe`
- OVMF code firmware
- OVMF vars firmware

The launch script checks common Windows install paths and also supports:

```powershell
$env:FEOX_QEMU = 'C:\Program Files\qemu\qemu-system-x86_64.exe'
$env:FEOX_OVMF_CODE = 'C:\Program Files\qemu\share\OVMF_CODE.fd'
$env:FEOX_OVMF_VARS = 'C:\Program Files\qemu\share\OVMF_VARS.fd'
```

The repo also ships a local MSYS2 and QEMU tree under `tools/host/` which the
scripts will discover automatically if no system-wide install is found.

## Current Validation Rail

```powershell
powershell -ExecutionPolicy Bypass -File .\tools\check-host.ps1 -Architecture x86_64
cargo test
cargo kernel
cargo loader
powershell -ExecutionPolicy Bypass -File .\tools\stage-efi.ps1
```

First full boot validation:

```powershell
powershell -ExecutionPolicy Bypass -File .\tools\run-qemu.ps1
```

The QEMU boot rail is working on this workstation as of March 27, 2026.
`tools/check-host.ps1 -Architecture x86_64` passes and bounded QEMU boot
captures work end-to-end. The debug-console log under `target/feox-qemu/`
is the current source of truth for first-boot validation.

## Expected Boot Trace

A successful run shows:

- UEFI boot manager reaches `BOOTX64.EFI`
- loader opens and validates `FEOXKERN.ELF`
- control transfers into kernel `_start`
- kernel logs the bootstrap banner and early handoff state
- bootstrap page-table transition completes
- higher-half code, stack, and data all become live
- breakpoint validation returns via `iretq`
- retained runtime service enters `runtime-active` and idles cleanly

## Source Of Truth

- [README.md](../README.md)
- [docs/CURRENT_STATUS.md](CURRENT_STATUS.md)
- [docs/TESTING.md](TESTING.md)
- [STATUS.md](../STATUS.md)
- [DEV_PROGRESS.md](../DEV_PROGRESS.md)
