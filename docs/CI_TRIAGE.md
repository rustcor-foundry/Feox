# Feox CI Triage

This is the first-response checklist for Feox CI failures on the Linux runner.

## 1. Identify The Lane

The normal lanes are:

- `host-tests`
- `build`
- `lint-and-check`
- `qemu-smoke`

Start with the first failing lane, not the last one.

## 2. If `host-tests` Fails

Run:

```bash
cargo test
```

Usually this means:

- a new host-test regression
- shared bootstrap test state not being reset
- an ABI/layout assertion drifted

## 3. If `build` Fails

Run:

```bash
cargo kernel
cargo loader
```

Usually this means:

- target-specific code stopped compiling
- linker-script or target-rustflags drifted
- the kernel or loader now depends on a host-only assumption

## 4. If `lint-and-check` Fails

Run:

```bash
cargo check -p feox-asi -p feox-async -p feox-boot -p feox-nvme -p feox-xokernel --target x86_64-unknown-none
cargo check -p feox-loader-uefi --target x86_64-unknown-uefi
cargo clippy -p feox-asi -p feox-async -p feox-boot -p feox-nvme -p feox-xokernel --target x86_64-unknown-none -- -D warnings
cargo clippy -p feox-loader-uefi --target x86_64-unknown-uefi -- -D warnings
```

Usually this means:

- target-only dead code or warning drift
- a clippy warning promoted to an error
- one crate compiles under `cargo build` but not under the exact target `check` lane

## 5. If `qemu-smoke` Fails

Read the job output in this order:

1. `Print runner tool versions`
2. `Check x86_64 boot-host prerequisites`
3. `Print resolved firmware paths`
4. `Run bounded x86_64 QEMU smoke boot`
5. `Dump QEMU smoke logs`

Then rerun locally on the runner:

```bash
pwsh -File ./tools/check-host.ps1 -Architecture x86_64
pwsh -File ./tools/run-qemu-smoke.ps1 -Architecture x86_64 -TimeoutSeconds 20
```

## 6. Most Likely `qemu-smoke` Failure Modes

### `pwsh` missing

Fix:

- install PowerShell on the Linux runner

### `qemu-system-x86_64` missing

Fix:

- install QEMU on the Linux runner
- or set `FEOX_QEMU` to the real binary path

### OVMF code or vars file missing

Fix:

- install OVMF on the Linux runner
- or set `FEOX_OVMF_CODE` and `FEOX_OVMF_VARS`

### QEMU launches but success marker is missing

Check:

- `target/feox-qemu/x86_64-debug.debug.log`
- `target/feox-qemu/x86_64-stdout.debug.log`
- `target/feox-qemu/x86_64-stderr.debug.log`

Usually this means:

- boot got far enough to launch but not far enough to settle into runtime idle
- the timeout is too short for the runner
- a real kernel or loader regression landed

### QEMU exits immediately

Usually this means:

- malformed firmware path
- unsupported QEMU drive syntax on that host
- missing firmware file permissions

## 7. Runner Contract Docs

Use these docs together:

- `docs/CI_RUNNER_SETUP.md`
- `docs/TESTING.md`
- `.gitea/workflows/ci.yml`

The goal is to keep the same PowerShell harnesses working on both the Windows
workstation and the Linux Gitea runner, with that runner treated as part of the
normal Feox validation rail rather than a special-case environment.
