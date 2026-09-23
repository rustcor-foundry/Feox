# Feox CI Runner Setup

This document defines the expected Feox runner posture for Gitea CI.

## Primary Runner

- runner label: `build-linux`
- role: normal Feox CI runner for host tests, target builds, lint/check, and
  bounded x86_64 QEMU smoke boot

## Required Host Tools

The runner must provide:

- `pwsh`
- `cargo`
- `rustup`
- `qemu-system-x86_64`
- x86_64 OVMF firmware code and vars files

The current workflow installs Rust toolchains and targets inside the job, but
the base runner still needs working `cargo`, `rustup`, and `pwsh`.

## Linux Path Assumptions

The Feox PowerShell harnesses now search these common Linux locations:

### x86_64 QEMU

- `/usr/bin/qemu-system-x86_64`
- `/usr/local/bin/qemu-system-x86_64`

### x86_64 OVMF code

- `/usr/share/OVMF/OVMF_CODE.fd`
- `/usr/share/OVMF/OVMF_CODE_4M.fd`
- `/usr/share/edk2/x64/OVMF_CODE.fd`
- `/usr/share/edk2-ovmf/x64/OVMF_CODE.fd`
- `/usr/share/qemu/OVMF_CODE.fd`

### x86_64 OVMF vars

- `/usr/share/OVMF/OVMF_VARS.fd`
- `/usr/share/OVMF/OVMF_VARS_4M.fd`
- `/usr/share/edk2/x64/OVMF_VARS.fd`
- `/usr/share/edk2-ovmf/x64/OVMF_VARS.fd`
- `/usr/share/qemu/OVMF_VARS.fd`

If the runner uses different locations, set:

- `FEOX_QEMU`
- `FEOX_OVMF_CODE`
- `FEOX_OVMF_VARS`

at the runner or job environment level.

## Current CI Lanes

The normal Feox workflow on the Linux runner runs:

- `cargo test`
- `cargo kernel`
- `cargo loader`
- target `cargo check`
- target `cargo clippy`
- bounded x86_64 QEMU smoke boot through `tools/run-qemu-smoke.ps1`

## Smoke-Boot Success Marker

The current bounded smoke lane succeeds when the boot log contains:

```text
stage: runtime service idle
```

That marker means the loader ran, the kernel completed the higher-half
transition, and the retained runtime settled into the expected idle loop.

## Suggested Linux Packages

On a Debian or Ubuntu style runner, the needed host pieces are typically:

```bash
sudo apt-get update
sudo apt-get install -y powershell qemu-system-x86 ovmf
```

Exact package names may differ by distro. The important contract is that the
paths above resolve, or the `FEOX_*` environment variables point to valid
alternatives.

## Triage

If a Feox CI lane fails on the Linux runner, start with:

- `docs/CI_TRIAGE.md`
