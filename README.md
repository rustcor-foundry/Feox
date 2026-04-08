# Feox

Feox is a `no_std` Rust exokernel product focused on explicit
ownership, per-core execution, allocator-free early boot, and hardware-shaped
abstractions.

Feox is part of the core product family in this workspace, but it is still in
an early implementation stage.

## Current Maturity

Current state: early implementation with a real bootstrap path.

Feox already has a compileable workspace, a thin UEFI loader, a bare-metal
kernel bootstrap, shared boot ABI types, and early memory/serial bring-up. It
is not yet a broader kernel environment, but it is well past pure concept
material.

## Portfolio Role

Feox is the kernel-and-runtime foundation product in the broader portfolio.

It owns the lowest-level lane in the family:

- explicit hardware-shaped execution
- capability-oriented kernel direction
- storage and runtime primitives close to the metal
- the bring-up path for a system that does not depend on a conventional host OS

## Workspace Layout

- `crates/` contains the shared runtime and device-support crates
- `kernel/` contains the bare-metal kernel crate and linker script
- `loader/` contains the thin UEFI loader
- `tools/` contains EFI staging and QEMU launch helpers
- `docs/` contains workstation entry, testing, and current-status docs
- root design docs capture the deeper architecture and research direction

## What It Is

The repository has moved beyond design-only status into a real Rust workspace
with foundational crates for:

- `feox-asi`: shared ASI-facing types
- `feox-async`: executor state-machine primitives
- `feox-boot`: shared boot ABI and physical memory map types
- `feox-nvme`: NVMe inflight tracking and queue ownership primitives
- `feox-xokernel`: top-level kernel facade crate
- `feox-loader-uefi`: thin UEFI loader for the kernel ELF

## Current Direction

The first implementation pass focuses on correctness in the places that matter
most for a low-level runtime:

- wake-during-poll is preserved instead of dropped
- NVMe command IDs are not reused while an owning future still exists
- NVMe futures remain core-local and intentionally `!Send`
- fail-all paths complete every in-flight operation, even before waker
  registration
- the x86_64 lane now has a real ASI `SYSCALL` / `SYSRET` transport with
  shared opcode and batch ABI types
- the first bootstrap capability table now exists, with generation-checked
  handles and initial `cap_list` / `cap_release` syscall coverage
- bootstrap capabilities are now backed by a real resource registry and
  delegation tree, with first `cap_delegate` coverage
- bootstrap `cap_request` can now mint physical-page capabilities from
  allocatable memory resources through the x86_64 syscall lane
- verified physical-memory capabilities can now drive a real 4 KiB mapping
  through the bootstrap paging layer in tests

## Workstation Entry

Use this order when picking up the repo on a Windows workstation:

```powershell
powershell -ExecutionPolicy Bypass -File .\tools\check-host.ps1 -Architecture x86_64
cargo test
cargo kernel
cargo loader
powershell -ExecutionPolicy Bypass -File .\tools\stage-efi.ps1
```

Those are the real entrypoints for validating the current bootstrap lane.

## Current Bootstrap Status

Current active implementation lane: `x86_64`.

Current early-boot capabilities include:

- explicit `_start` entrypoint
- dedicated bootstrap stack
- bootstrap GDT and IDT installation on the current x86 lane
- COM1 serial logging on the current x86 lane
- serial exception reporting
- kernel image and active PML4 introspection
- custom boot handoff ABI for physical memory regions in `rdi`
- panic-to-serial path
- retained higher-half runtime handoff with shared runtime context
- retained per-core bootstrap context and event timeline
- non-fatal higher-half breakpoint validation before entering a runtime service
- retained runtime-service state with phase, owner core, and iteration tracking
- retained runtime command queue with state-driven follow-up work
- retained runtime heartbeat and derived retry-before-idle behavior
- deterministic runtime-service idle loop after early initialization
- UEFI loader support for loading `\EFI\BOOT\FEOXKERN.ELF` and transferring
  control into the kernel entrypoint

## Build

Run the host-side checks:

```powershell
cargo test
```

Build the bare-metal kernel image:

```powershell
cargo kernel
```

Build the UEFI loader:

```powershell
cargo loader
```

Future architecture-aware aliases already exist for planning and scaffolding:

```powershell
cargo kernel-arm
cargo loader-arm
```

Those aliases expand to:

```powershell
cargo build -p feox-xokernel --bin feox-xokernel --target aarch64-unknown-none-softfloat
cargo build -p feox-loader-uefi --bin feox-loader-uefi --target aarch64-unknown-uefi
```

## Testing And QA

Automated baseline:

```powershell
cargo test
cargo kernel
cargo loader
```

Manual bootstrap rail:

```powershell
powershell -ExecutionPolicy Bypass -File .\tools\check-host.ps1 -Architecture x86_64
powershell -ExecutionPolicy Bypass -File .\tools\stage-efi.ps1
powershell -ExecutionPolicy Bypass -File .\tools\run-qemu.ps1
```

See [docs/TESTING.md](docs/TESTING.md) for the real operator checklist and
host prerequisites.

## Boot Harness

Stage the EFI directory without launching QEMU:

```powershell
powershell -ExecutionPolicy Bypass -File .\tools\stage-efi.ps1
```

Run Feox under QEMU + OVMF with serial output on stdio:

```powershell
powershell -ExecutionPolicy Bypass -File .\tools\run-qemu.ps1
```

The harnesses are now architecture-aware and accept:

```powershell
powershell -ExecutionPolicy Bypass -File .\tools\stage-efi.ps1 -Architecture x86_64
powershell -ExecutionPolicy Bypass -File .\tools\run-qemu.ps1 -Architecture x86_64
```

The run script will try common Windows install paths for QEMU and OVMF, and
you can override them with:

```powershell
$env:FEOX_QEMU = 'C:\Program Files\qemu\qemu-system-x86_64.exe'
$env:FEOX_OVMF_CODE = 'C:\Program Files\qemu\share\OVMF_CODE.fd'
$env:FEOX_OVMF_VARS = 'C:\Program Files\qemu\share\OVMF_VARS.fd'
```

## Packaging And Runtime Reality

- release binaries: real for the kernel and UEFI loader build artifacts
- installer or packaging flow: not applicable yet
- emulated boot flow: real in-repo harness with a working x86_64 QEMU + OVMF validation lane on this workstation
- current host posture on this workstation: `tools/check-host.ps1 -Architecture x86_64` passes and bounded QEMU boot captures work end-to-end
- current live runtime posture: Feox reaches a retained higher-half runtime-active state, validates the exception path, runs a tiny retained runtime command loop with heartbeat-driven follow-up work, and then idles cleanly

## Source-Of-Truth Docs

- [Current Status](docs/CURRENT_STATUS.md)
- [Architecture Priority](docs/ARCHITECTURE_PRIORITY.md)
- [Architecture Checklist](docs/ARCHITECTURE_CHECKLIST.md)
- [Bootstrap Runtime](docs/BOOTSTRAP_RUNTIME.md)
- [Virtual Address Layout](docs/VIRTUAL_ADDRESS_LAYOUT.md)
- [ARM64 Port Plan](docs/ARM64_PORT_PLAN.md)
- [Boot Memory Review](docs/BOOT_MEMORY_REVIEW.md)
- [Memory Ownership Phases](docs/MEMORY_OWNERSHIP_PHASES.md)
- [Page-Table Plan](docs/PAGE_TABLE_PLAN.md)
- [Exokernel Research Framework](docs/EXOKERNEL_RESEARCH_FRAMEWORK.md)
- [Code Review](docs/CODE_REVIEW.md)
- [Portfolio Positioning](docs/PORTFOLIO_POSITIONING.md)
- [Shared Product Doctrine](docs/SHARED_PRODUCT_DOCTRINE.md)
- [Workstation Entry](docs/WORKSTATION_ENTRY.md)
- [Testing](docs/TESTING.md)
- [Development Progress](DEV_PROGRESS.md)
- [ASI Spec](ASI-SPEC.md)
- [Async Runtime Notes](ASYNC-RUNTIME.md)
- [Capability System](CAPABILITY-SYSTEM.md)
- [NVMe Driver Notes](NVME-DRIVER.md)
