# Feox Workstation Entry

This is the practical pickup path for Feox on a Windows workstation.

## What Feox Is

Feox is a `no_std` Rust exokernel product in an early implementation stage. It is not a packaged desktop
product and it should not be approached like one. The real front door is the
Cargo workspace plus the EFI staging and QEMU harness under `tools/`.

## Recommended Pickup Order

Run these from the repo root:

```powershell
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

## Current Validation Rail

- `cargo test`
- `cargo kernel`
- `cargo loader`
- `tools/stage-efi.ps1`

First full boot validation is:

```powershell
powershell -ExecutionPolicy Bypass -File .\tools\run-qemu.ps1
```

At the current checkpoint, that run is still blocked on missing QEMU host
tooling on this machine.

## Source Of Truth

- [README.md](../README.md)
- [docs/CURRENT_STATUS.md](CURRENT_STATUS.md)
- [docs/TESTING.md](TESTING.md)
- [STATUS.md](../STATUS.md)
- [DEV_PROGRESS.md](../DEV_PROGRESS.md)
