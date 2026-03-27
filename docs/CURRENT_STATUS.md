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
- the remaining first-boot blocker on this workstation is host emulation tooling, not Feox repo wiring
- the first architecture-neutral cleanup pass is now in progress, with generic kernel code no longer reaching directly into `arch::x86_64` for core bootstrap/logging hooks
- early console and logging now also pass through a kernel-level facade instead of tying generic paths directly to the x86 serial module
- the loader ELF check and PowerShell build/staging harnesses are now architecture-aware, even though `x86_64` remains the only implemented kernel lane today

## Current Strengths

- clear crate separation between boot ABI, async runtime work, NVMe primitives, loader, and kernel bootstrap
- honest low-level direction centered on ownership, per-core execution, and allocator-free early boot
- real bootstrap path from UEFI loader into a bare-metal kernel image
- useful host scripts for staging and QEMU launch instead of a purely conceptual boot plan
- clear strategic role as the lowest-level systems product in the portfolio, not just a side research repo

## Current Risks

- no verified QEMU boot on this workstation yet because QEMU is not installed or not discoverable
- OVMF availability has not been rechecked after the QEMU lookup failure because the run harness stops at the first missing prerequisite
- product framing had been underplaying the repo as a research lane instead of a core early-stage product

## Recommended Entry Points

- `cargo test`
- `cargo kernel`
- `cargo loader`
- `powershell -ExecutionPolicy Bypass -File .\tools\stage-efi.ps1`

Use those before deeper kernel or loader changes.

## Immediate Next Focus

1. install or point the host at a working QEMU binary
2. confirm OVMF firmware paths on this workstation
3. run the first real UEFI boot attempt under `tools/run-qemu.ps1`
4. capture and document the first serial-output handoff from loader to kernel
5. keep separating generic kernel plumbing from x86-specific implementation details before starting an ARM64 lane
