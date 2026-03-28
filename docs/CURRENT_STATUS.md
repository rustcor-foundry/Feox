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
- the x86 host boot rail is now working on this workstation, including QEMU launch, OVMF discovery, loader execution, and kernel handoff
- the first architecture-neutral cleanup pass is now in progress, with generic kernel code no longer reaching directly into `arch::x86_64` for core bootstrap/logging hooks
- early console and logging now also pass through a kernel-level facade instead of tying generic paths directly to the x86 serial module
- the loader ELF check and PowerShell build/staging harnesses are now architecture-aware, even though `x86_64` remains the only implemented kernel lane today
- `tools/check-host.ps1` now gives one explicit host-readiness entrypoint for the x86 and future ARM lanes
- the next x86-side design steps are now written down explicitly in `docs/MEMORY_OWNERSHIP_PHASES.md` and `docs/PAGE_TABLE_PLAN.md`
- the early frame allocator now supports explicit physical reservations and the bootstrap path reserves the kernel image plus active page-table root before handing frames out
- early kernel-owned memory is now tracked with typed bootstrap reservation categories so upcoming page-table and per-core state work has a disciplined place to land
- the first x86 paging ownership helper now exists, with a bootstrap paging-frame allocator that records new page-table frames under typed kernel reservations
- the first x86 page-table query layer now exists, with a root wrapper, entry model, and 4 KiB translation walk over a supplied frame source
- the first x86 4 KiB mapping primitive now exists, allocating intermediate tables through a supplied paging allocator and rejecting remaps or huge-page cases
- the first x86 unmap path now exists, and the repo has a passing `map -> translate -> unmap` lifecycle test for the 4 KiB bootstrap mechanism layer
- the loader and kernel now mirror early output to QEMU's debug console, giving the repo a reliable first-boot trace path on this host
- the live x86 boot trace now reports a sane first allocatable frame (`0x0000000000111000`) instead of falling back to address zero
- the live bootstrap path now performs one real paging-structure allocation and records it as `BootstrapPageTables` in the early reservation model
- the live x86 bootstrap now builds and retains a bootstrap-owned transition root under QEMU, proving a first kernel-owned `map -> translate` paging step without mutating firmware-owned active page tables
- the live x86 bootstrap now reports concrete transition entry and stack aliases under QEMU, so a future root switch has real handoff coordinates instead of guessed addresses
- the live x86 bootstrap now completes the first real CR3 handoff under QEMU by switching onto the retained transition root and logging success from inside the new address space
- the first CR3 handoff now lands on kernel-owned transition stack pages rather than reusing the original boot stack, reducing another dependency on the pre-switch environment
- the post-switch path now climbs from the identity handoff stack onto the high-half transition stack alias under QEMU, proving one real higher-half runtime step after the CR3 switch
- the post-switch path now also jumps into the mapped high-half code alias under QEMU, so Feox has its first real higher-half code-and-stack execution slice after the CR3 handoff
- the post-switch path now carries a kernel-owned transition data page into the higher-half slice as well, so code, stack, and data all survive the current QEMU handoff
- the higher-half handoff now publishes a retained shared runtime snapshot instead of treating the transition state as boot-local only
- the bootstrap path now retains a per-core bootstrap context so the active core, root, stack, and entry are queryable after the handoff
- the bootstrap runtime now keeps a short retained event timeline covering the major transition milestones
- higher-half breakpoint validation is now non-fatal, returns through `iretq`, and proves the active higher-half exception path before continuing
- the post-validation path now reaches a small retained runtime service and idles from `runtime-active` instead of halting immediately after validation
- the retained runtime service now also keeps its own shared service state, including owner core, current phase, iteration count, and last action
- the retained runtime service now drains a tiny command queue instead of following a single hardcoded sequence
- the retained runtime loop now keeps a shared heartbeat record and uses it to run one more refresh cycle before settling into idle

## Current Strengths

- clear crate separation between boot ABI, async runtime work, NVMe primitives, loader, and kernel bootstrap
- honest low-level direction centered on ownership, per-core execution, and allocator-free early boot
- real bootstrap path from UEFI loader into a bare-metal kernel image
- useful host scripts for staging and QEMU launch instead of a purely conceptual boot plan
- clear strategic role as the lowest-level systems product in the portfolio, not just a side research repo

## Current Risks

- the current x86 bootstrap still settles into a retained idle loop, so successful runtime behavior now outpaces broader subsystem bring-up
- the live boot path now proves a retained bootstrap-owned transition root, a real CR3 handoff, kernel-owned transition stack pages, post-switch higher-half code-plus-stack execution, a surviving higher-half data page, and a state-driven retained runtime loop, but broader runtime structures are still intentionally small
- the current boot trace is strong enough to guide real VM bring-up, but bootstrap-oriented diagnostics and tiny retained runtime services still outweigh sustained runtime behavior

## Recommended Entry Points

- `cargo test`
- `cargo kernel`
- `cargo loader`
- `powershell -ExecutionPolicy Bypass -File .\tools\stage-efi.ps1`

Use those before deeper kernel or loader changes.

## Immediate Next Focus

1. grow the retained `runtime-active` slice from a tiny command loop into a broader long-lived runtime layout with clearer ownership boundaries
2. extend the serial/debug trace so reservation, paging, retained runtime, heartbeat, and runtime-service changes stay easy to verify under QEMU
3. let the retained runtime loop make one more useful mutation beyond heartbeat updates and reporting
4. keep separating generic kernel plumbing from x86-specific implementation details before starting an ARM64 lane
