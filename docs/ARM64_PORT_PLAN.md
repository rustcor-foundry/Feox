# Feox ARM64 Port Plan

Last updated: 2026-03-27

This document captures what it would take to bring Feox up on 64-bit Arm
(`aarch64`) while keeping the project disciplined.

The goal is not "support every ARM board." The goal is a clean first supported
ARM bring-up path with the smallest number of moving parts.

## Short Answer

Feox can be developed for ARM64, but not as a small target-triple tweak.

The current repo is strongly `x86_64`-specific in:

- kernel bootstrap entry
- descriptor-table setup
- page-table inspection
- serial path
- linker script
- QEMU launch harness
- parts of the current ASI draft that assume x86-64 syscall and interrupt language

The right first ARM lane is:

- `aarch64-unknown-uefi` for the loader
- `aarch64-unknown-none-softfloat` or `aarch64-unknown-none` for the kernel
- QEMU `virt` as the first emulated machine
- UEFI firmware from ArmVirtPkg / edk2 rather than OVMF

## External Baseline

These are the most relevant primary references for an ARM64 bring-up lane.

### Rust target support

- Rust supports `aarch64-unknown-uefi` as a Tier 2 UEFI target:
  https://doc.rust-lang.org/beta/rustc/platform-support/unknown-uefi.html
- Rust supports `aarch64-unknown-none` and `aarch64-unknown-none-softfloat`
  as bare-metal Armv8-A targets:
  https://doc.rust-lang.org/beta/rustc/platform-support/aarch64-unknown-none.html

Important implication:

- Feox can keep its Rust-first UEFI loader model on ARM64
- the kernel will need its own linker script and startup path for AArch64

### QEMU ARM64 baseline

- QEMU documents `qemu-system-aarch64` for 64-bit Arm guests:
  https://qemu.readthedocs.io/en/v9.2.4/system/target-arm.html
- QEMU's `virt` machine is the generic first platform and supports PCI,
  virtio, recent CPUs, and large amounts of RAM:
  https://qemu.readthedocs.io/en/master/system/arm/virt.html

Important implication:

- `virt` is the right first emulation target for Feox
- it gives Feox a practical path to UEFI boot, memory-map intake, and later PCI
  device experimentation

### ARM UEFI firmware path

- Tianocore lists `ArmVirtPkg` as the EDK II platform for ARM emulation:
  https://github.com/tianocore/tianocore.github.io/wiki/EDK-II-Platforms

Important implication:

- the current OVMF assumptions in Feox's tooling are `x86_64`-specific
- ARM64 boot harnessing should be built around ArmVirtPkg firmware artifacts

## What Carries Over Cleanly

These parts of Feox are already close to portable.

### Boot ABI

`crates/feox-boot` is mostly architecture-neutral:

- `BootInfo`
- `BootHandoff`
- `MemoryRegion`
- `MemoryRegionKind`
- physical-address and region semantics

This is good news. The loader-to-kernel contract does not need to be reinvented
for ARM64.

### High-level product direction

The core Feox ideas still map cleanly:

- explicit ownership
- per-core execution
- capability direction
- narrow kernel mechanism surface

Those are not x86-specific.

### UEFI loader model

The loader is currently UEFI-based, and Rust supports `aarch64-unknown-uefi`.
That means Feox can preserve:

- UEFI file loading
- UEFI memory-map translation
- an explicit handoff structure
- a thin loader instead of a heavy bootloader dependency

## What Is Explicitly x86_64-Specific Today

These are the real blockers.

### Kernel bootstrap

Current kernel entry assumes:

- `_start` in x86_64 assembly
- stack setup in x86_64 assembly
- boot argument passed in `rdi`

For ARM64 this becomes:

- AArch64 entry assembly
- stack setup using ARM64 registers
- UEFI/kernel handoff passed according to the AArch64 boot convention Feox chooses

### Early architecture layer

Current `kernel/feox-xokernel/src/arch/x86_64` assumes:

- GDT
- IDT
- CR3
- x86 exception stubs
- x86 port-I/O serial

ARM64 equivalents will instead center on:

- exception vectors
- exception-level setup
- translation-table base registers instead of CR3
- MMIO UART, not port I/O

### Linker and image assumptions

Current linker script is `elf64-x86-64` and the loader validates:

- `ET_EXEC`
- `EM_X86_64`

ARM64 needs:

- a new linker script
- ELF validation for AArch64
- entry and segment assumptions reviewed for the QEMU `virt` memory map

### ASI draft language

The current ASI draft includes x86-specific assumptions like:

- `syscall` / `sysret`
- Local APIC wording
- Intel VT-d wording
- MSI-X narrative written from a PC-server point of view

That does not make the ASI invalid, but it means the spec is currently
architecture-colored rather than architecture-neutral.

### Tooling

Current scripts assume:

- `x86_64-unknown-none`
- `x86_64-unknown-uefi`
- `qemu-system-x86_64`
- OVMF firmware names and paths

These need to become target-aware.

## Recommended First ARM64 Scope

Do not try to solve "all ARM" first.

The clean first scope is:

1. boot Feox on QEMU `virt`
2. use UEFI on ARM64
3. keep serial logging
4. prove boot handoff and halt loop
5. defer advanced interrupt, PCI, and IOMMU work until the basic lane is real

That gives Feox a second architecture without pretending the whole exokernel
stack is portable on day one.

## Phase Plan

### Phase 0: Architecture-neutral cleanup

Before writing ARM64 code, split out the parts that should stop saying
"x86_64" everywhere.

Recommended cleanup:

- move generic boot logic out of arch-specific modules where possible
- make the loader's ELF validation architecture-aware
- separate generic serial/logging interfaces from current x86 implementation
- update docs so "x86_64 today" is explicit instead of implicit

Exit criteria:

- repo structure makes it obvious what is generic vs arch-specific

### Phase 1: ARM64 UEFI loader lane

Bring up:

- `aarch64-unknown-uefi`
- ARM64 build target in Cargo/scripts
- ARM64 ELF acceptance in the loader
- ArmVirtPkg firmware support in the QEMU harness

Exit criteria:

- ARM64 loader builds
- loader runs under QEMU `virt`
- serial or UEFI console logs confirm loader startup

### Phase 2: ARM64 kernel bootstrap lane

Bring up:

- `aarch64-unknown-none-softfloat` or `aarch64-unknown-none`
- AArch64 `_start`
- bootstrap stack
- early exception-vector install
- UART logging on QEMU `virt`
- boot handoff acceptance
- known-good halt loop

Exit criteria:

- kernel receives handoff from the loader
- kernel prints bootstrap logs
- machine reaches the known halt loop

### Phase 3: ARM64 memory foundation

Bring up:

- page-size and translation-table assumptions documented for ARM64
- equivalent of current x86 page-root visibility
- first frame allocator validation against the ARM64 handoff
- architecture-aware boot memory review

Exit criteria:

- ARM64 memory bring-up is at parity with current x86 bootstrap depth

### Phase 4: Spec cleanup

Refactor ASI and capability docs so they distinguish:

- architecture-neutral concepts
- x86-specific implementation notes
- ARM64-specific implementation notes

Exit criteria:

- Feox docs stop reading like the architecture is x86-only by default

## Practical Implementation Notes

### Target choice

The most conservative kernel target is probably `aarch64-unknown-none-softfloat`
because Rust notes that kernel-like environments may prefer the soft-float ABI.
That should be validated against how Feox wants to treat floating-point/SIMD
state during early bring-up.

### Serial

The current COM1 path is not portable. For QEMU `virt`, Feox should adopt a
proper MMIO UART path behind an architecture-neutral serial interface.

### Interrupts

Do not let interrupt-controller work block the first ARM64 milestone. The first
target should still be "boot, log, halt."

### PCI / NVMe / IOMMU

QEMU `virt` supports PCI, which is helpful long-term, but Feox should avoid
assuming immediate parity with current NVMe and VT-d-flavored docs.

Near-term ARM64 focus should be:

- loader
- bootstrap
- memory

not:

- full PCIe exokernel data plane on day one

## Current Recommendation

Feox should support ARM64 eventually, and it is a good strategic move.

But the disciplined way to do it is:

1. finish the first x86_64 QEMU boot proof
2. clean up the generic-vs-arch boundaries
3. bring up ARM64 as a second bootstrap lane on QEMU `virt`

That sequence keeps the project honest and avoids turning a good early x86
bootstrap into a two-architecture half-bootstrap.
