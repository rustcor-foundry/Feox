# Feox Development Progress

This document is the running development record for Feox.

## Working Rules

- update this file with each meaningful checkpoint or commit
- keep entries implementation-focused
- use `STATUS.md` for the current overall posture and milestone summary

## Entries

### 2026-03-25

- confirmed the repo has moved from design-only status into a real Rust workspace
- verified `cargo test`, `cargo kernel`, `cargo loader`, and EFI staging
- confirmed the current blocker for first QEMU boot attempts is host tooling availability, not in-repo wiring
- documented the current early-boot and loader capabilities in `STATUS.md`

### 2026-03-27

- tightened Feox onboarding around a real workstation pickup flow instead of a docs-only concept surface
- added `docs/CURRENT_STATUS.md`, `docs/WORKSTATION_ENTRY.md`, and `docs/TESTING.md`
- added `docs/EXOKERNEL_RESEARCH_FRAMEWORK.md` to anchor Feox on primary exokernel and capability-system literature
- added `docs/ARCHITECTURE_CHECKLIST.md` to turn the research rail into an implementation-review framework for memory, capabilities, NVMe, and interrupts
- reviewed the current boot memory and handoff layer against the checklist and captured the outcome in `docs/BOOT_MEMORY_REVIEW.md`
- reviewed what ARM64 support would actually require and captured the phased plan in `docs/ARM64_PORT_PLAN.md`
- reviewed the architecture research and locked the first performance lane as `x86_64`, with ARM64 as the second lane in `docs/ARCHITECTURE_PRIORITY.md`
- started the architecture-neutral cleanup pass by adding a generic arch facade for early init, panic, console output, halt, and page-root access
- updated the generic kernel bootstrap path to call the arch facade instead of reaching directly into `arch::x86_64`
- added `kernel/feox-xokernel/src/console.rs` as a kernel-level early console facade and moved panic/exception logging to use it
- made the UEFI loader's expected kernel ELF machine target-aware and parameterized the PowerShell staging/QEMU harnesses by architecture
- added `kernel-arm` and `loader-arm` cargo aliases to make the future ARM64 lane explicit in repo tooling
- re-verified `cargo test`, `cargo kernel`, and `cargo loader` after the cleanup
- updated `README.md` to expose the real build, validation, and bootstrap entrypoints
- installed the missing Rust targets with `rustup target add x86_64-unknown-none x86_64-unknown-uefi`
- re-verified `cargo loader` and `tools/stage-efi.ps1`
- confirmed the current first-boot blocker on this workstation is still missing QEMU host tooling

## Next Focus

- install or point the host at working QEMU and OVMF paths
- run the first real UEFI boot attempt under QEMU
- capture serial output and validate the loader-to-kernel handoff
