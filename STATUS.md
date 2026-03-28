# Feox Status

Checkpoint date: March 27, 2026

## Project Direction

Feox is being built as a lean `no_std` Rust exokernel with a bias toward:

- explicit ownership
- per-core execution
- fixed-size structures on hot paths
- allocator-free early boot
- abstractions that do not hide hardware shape

## What Exists Now

The repository has a proven end-to-end boot path on this workstation and has
completed a full code review pass that hardened every identified correctness,
safety, performance, and architecture gap before continuing.

Current crates and layers:

- `feox-asi`: shared ASI-facing types (`CoreId` as `u32` per ASI spec)
- `feox-async`: executor state machine — `RunQueue`, `TaskHeader`, `TaskCell<F>`, `SingleCoreExecutor`
- `feox-boot`: shared boot ABI and physical memory map types
- `feox-nvme`: NVMe inflight tracking — fully `no_std`, const-generic `InflightMap<const N>`
- `feox-xokernel`: bare-metal kernel bootstrap
- `feox-loader-uefi`: thin UEFI loader for the kernel ELF

Kernel/bootstrap capabilities in place:

- explicit `_start` entrypoint with dedicated bootstrap stack
- x86_64 COM1 early serial output and ISA debug console at `0x402`
- kernel-level early console facade with `CONSOLE_READY` guard (prevents double-fault re-entry)
- panic-to-serial path with retained runtime snapshot, core context, and event history
- bootstrap GDT: 5-entry table with a 64-bit TSS descriptor; Task Register loaded via `ltr`
- bootstrap IDT: IST1 on NMI (vector 2), IST2 on double-fault (vector 8)
- dedicated 4 KiB IST stacks for NMI and double-fault (safe NMI/DF delivery at any stack depth)
- fatal exception stubs for vectors 0–31
- EFER.NXE enabled before any NX-flagged PTE is live
- `FLAG_NO_EXECUTE` applied to bootstrap stack and data pages
- CR4 security bits conditionally enabled via CPUID leaf 7: SMEP, SMAP, UMIP
- architecture-neutral kernel bootstrap routing through an arch facade
- linker-backed kernel image bounds
- active `CR3`/PML4 introspection on the current x86_64 lane
- early physical memory region model
- linear 4 KiB frame allocator with explicit typed reservations:
  `KernelImage`, `ActivePageTableRoot`, `LegacyLow`, `BootstrapPageTables`, `PerCoreState`
- x86 4 KiB map / translate / unmap lifecycle, tested end-to-end
- TLB invalidation (`invlpg`) after every PTE write; gated behind `cfg(target_os = "none")`
- `PageTableEdges` frame-tree sidecar: records parent→child edges during allocation for future reclaim
- single-writer ownership claim on `RuntimeContext` via `AtomicU32 CONTEXT_OWNER`

Boot path capabilities in place:

- shared `BootInfo` / `BootHandoff` ABI in `rdi`
- UEFI loader that reads `\EFI\BOOT\FEOXKERN.ELF`
- ELF validation for the active UEFI loader architecture
- PT_LOAD segment loading at linked addresses
- UEFI memory map translation into Feox memory regions
- loader logs mirrored to both UEFI console and the current x86 serial path

Higher-half handoff capabilities in place:

- bootstrap-owned transition page-table root separate from firmware-owned root
- CR3 handoff into the kernel-owned transition root under QEMU
- kernel-owned transition stack and data pages for the post-switch path
- higher-half code, stack, and data all survive the CR3 handoff
- GDT and IDT reloaded from higher-half aliases after the stack switch
- non-fatal breakpoint validation via `iretq` proving the higher-half exception path
- named `BootstrapRuntimeLayout` with explicit kernel, stack, and data windows
- CR3 switch path emits debug markers in `debug_assertions` builds only; release is 5 clean instructions

Retained runtime capabilities in place:

- retained shared `RuntimeSnapshot` published after the higher-half handoff
- retained per-core `BootstrapCoreContext` with active root, stack, and entry
- retained bootstrap event timeline (8-slot rolling buffer)
- retained `RuntimeServiceState` with owner core, phase, iteration, and last action
- retained `RuntimeServiceReport` with derived accounting of window and stack sizes
- retained `RuntimeServiceHeartbeat` with beat count and event observation
- retained `RuntimeReadinessState` and `RuntimeReadySummary` published when the service loop settles
- state-driven FIFO command queue driving the runtime service through:
  `RefreshSnapshot → RefreshAccounting → ReportTimeline → UpdateHeartbeat`
  with heartbeat-gated retry before settling into `PublishReady → EnterIdle`
- 5-stage bootstrap runtime stage model:
  `Prepared → IdentityActive → AliasActive → ExceptionValidated → RuntimeActive`

Async executor capabilities in place:

- `RunQueue<const CAP>`: fixed-capacity FIFO ring buffer of type-erased task pointers
- `TaskHeader`: type-erased task state with `begin_poll` / `complete` / `wake_pending` transitions
- `TaskCell<F>`: `#[repr(C)]` pinned future storage; `spawn()` CAS-claims and installs poll trampoline
- `SingleCoreExecutor<const CAP>`: `poll_one` and `run_until_idle` driving the run queue
- waker vtable: `make_task_waker` produces `Waker` from `NonNull<TaskHeader>` without heap allocation

## What Has Been Verified

Verified on this checkpoint:

- `cargo test` — 47 tests pass across all crates
- `cargo kernel` — clean
- `cargo loader` — clean
- `powershell -ExecutionPolicy Bypass -File .\tools\stage-efi.ps1`
- `tools/check-host.ps1 -Architecture x86_64` passes on this workstation
- `tools/run-qemu.ps1` launches QEMU, reaches the Feox loader/kernel path, and
  captures a full bootstrap trace including higher-half runtime-active state

Artifacts and harness:

- staged EFI tree is produced under `target\feox-efi\EFI\BOOT`
- `BOOTX64.EFI` is the current x86_64 UEFI loader artifact
- `FEOXKERN.ELF` is the kernel image
- `tools\stage-efi.ps1` and `tools\run-qemu.ps1` are parameterized by architecture

## Current Posture

The current working QEMU boot trace proves:

- UEFI boot manager reaches `BOOTX64.EFI`
- loader opens and validates `FEOXKERN.ELF`
- control transfers into kernel `_start`
- kernel accepts boot handoff and logs bootstrap state
- bootstrap page-table transition succeeds under QEMU
- higher-half code, stack, and data are all live after the CR3 handoff
- breakpoint validation returns cleanly via `iretq`
- retained runtime service enters `runtime-active`, drains the command queue,
  reports retained state, and settles into idle

The code review pass is complete. All 21 findings have been resolved or explicitly
deferred (A-03 syscall entry, A-04 capability table).

## Next Focus

- **A-03**: ASI syscall entry path — `SYSCALL`/`SYSRET` stub and ring-3 entry point
- **Virtual address layout**: decide the permanent direct-map base and per-core/MMIO zones;
  update `docs/VIRTUAL_ADDRESS_LAYOUT.md` with locked decisions

## Important Files

- [README.md](README.md)
- [Cargo.toml](Cargo.toml)
- [DEV_PROGRESS.md](DEV_PROGRESS.md)
- [crates/feox-boot/src/lib.rs](crates/feox-boot/src/lib.rs)
- [crates/feox-async/src/lib.rs](crates/feox-async/src/lib.rs)
- [kernel/feox-xokernel/src/main.rs](kernel/feox-xokernel/src/main.rs)
- [kernel/feox-xokernel/src/boot.rs](kernel/feox-xokernel/src/boot.rs)
- [kernel/feox-xokernel/src/memory.rs](kernel/feox-xokernel/src/memory.rs)
- [kernel/feox-xokernel/src/paging.rs](kernel/feox-xokernel/src/paging.rs)
- [kernel/feox-xokernel/src/runtime_context.rs](kernel/feox-xokernel/src/runtime_context.rs)
- [kernel/feox-xokernel/src/arch/x86_64/gdt.rs](kernel/feox-xokernel/src/arch/x86_64/gdt.rs)
- [kernel/feox-xokernel/src/arch/x86_64/idt.rs](kernel/feox-xokernel/src/arch/x86_64/idt.rs)
- [kernel/feox-xokernel/src/arch/x86_64/cpu.rs](kernel/feox-xokernel/src/arch/x86_64/cpu.rs)
- [loader/feox-loader-uefi/src/main.rs](loader/feox-loader-uefi/src/main.rs)
- [tools/stage-efi.ps1](tools/stage-efi.ps1)
- [tools/run-qemu.ps1](tools/run-qemu.ps1)
