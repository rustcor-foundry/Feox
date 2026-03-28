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
- added `tools/check-host.ps1` so host readiness for QEMU and firmware is explicit instead of implicit
- added `docs/MEMORY_OWNERSHIP_PHASES.md` to define when memory becomes Feox-owned instead of just firmware-described
- added `docs/PAGE_TABLE_PLAN.md` to define the first x86 page-table mechanism layer after boot
- added explicit physical reservation support to the early frame allocator and now reserve the active PML4 frame during bootstrap accounting
- strengthened the early reservation model into typed bootstrap categories so kernel-image, page-table, and future per-core ownership can be tracked explicitly
- added a first x86 bootstrap paging helper so newly allocated page-table frames are recorded as `BootstrapPageTables` instead of consuming memory ad hoc
- added the first x86 page-table query layer with a root wrapper, entry model, and 4 KiB translation walk over a supplied frame source
- added the first x86 `map_4k`-style primitive so Feox can allocate missing intermediate tables and install a 4 KiB mapping in tests without a broader VM layer
- added the first x86 `unmap_4k`-style primitive and a passing `map -> translate -> unmap` lifecycle test for the bootstrap paging layer
- brought the x86 host boot rail to life on this workstation and captured the first successful loader-to-kernel trace under QEMU
- fixed the x86 kernel image type so the loader now boots an `ET_EXEC` kernel instead of rejecting an `ET_DYN` artifact
- fixed the x86 loader-to-kernel ABI handoff so the boot info pointer is passed in `rdi` as intended
- added a QEMU debug-console path at port `0x402` for both loader and kernel so early boot traces are captured reliably on this host
- fixed the live x86 bootstrap allocator path so the first usable frame is now a sane post-kernel address instead of `0x0`
- added richer live bootstrap memory diagnostics, including region-kind and reservation-kind summaries in the QEMU boot trace
- taught the live x86 bootstrap path to allocate and record one real `BootstrapPageTables` frame during bring-up
- added a bootstrap identity-mapped page-table source for low bootstrap-owned frames without assuming the firmware-owned active root is directly accessible
- taught the live x86 bootstrap path to build and retain a standalone transition page-table root, perform a real `map -> translate` step against it, and prove the result in the QEMU trace
- extended the live x86 transition-root trace with concrete high-half entry and stack aliases so the first root-switch step has explicit target coordinates
- completed the first real x86 CR3 handoff under QEMU by switching onto the retained transition root and logging success from inside the new address space
- debugged the handoff boundary with raw debug-console markers and confirmed the original failure was a writable-stack issue in the transition root
- moved the working CR3 handoff onto kernel-owned transition stack pages instead of the original boot stack, and verified the post-switch path still completes under QEMU
- extended the post-switch path so it moves from the identity handoff stack onto the mapped high-half transition stack alias and still completes cleanly under QEMU
- extended the post-switch path again so it now jumps into the mapped high-half code alias as well, giving Feox its first real higher-half code-and-stack execution slice under QEMU
- added a kernel-owned transition data page and verified that code, stack, and data now all survive the higher-half post-switch path under QEMU
- added a named bootstrap runtime layout so the higher-half window, stack window, data window, and handler delta are derived through one explicit model instead of scattered constants
- added `runtime_context.rs` as the first shared retained runtime-state service, with a published higher-half runtime snapshot instead of boot-local-only transition state
- retained the first per-core bootstrap context so the active core, root, stack, and entry survive the handoff as queryable runtime state
- retained a short bootstrap event timeline so the major transition milestones are visible after the handoff
- reworked breakpoint handling so higher-half exception validation returns through `iretq` instead of terminating the run immediately
- extended the post-validation path into a tiny `runtime-active` service that reports retained runtime state and then idles cleanly
- wired exception and panic reporting to include the retained runtime snapshot, core context, and event history
- added retained runtime-service state so the first post-handoff service now tracks its owner core, current phase, iteration count, and last action
- verified the live QEMU trace now shows the retained service entering `poll`, reporting its retained state, and then transitioning into `idle`
- turned the retained runtime service into a tiny FIFO command loop with explicit `refresh-snapshot`, `refresh-accounting`, `report-timeline`, `update-heartbeat`, and `enter-idle` phases
- added retained runtime accounting and a retained heartbeat record so the post-handoff loop now mutates shared runtime state instead of only reporting it
- made the retained runtime loop state-driven, with the heartbeat deciding whether the service performs one more refresh cycle before it settles into idle
- re-verified `cargo test`, `cargo kernel`, and `cargo loader` after the cleanup
- updated `README.md` to expose the real build, validation, and bootstrap entrypoints
- installed the missing Rust targets with `rustup target add x86_64-unknown-none x86_64-unknown-uefi`
- re-verified `cargo loader` and `tools/stage-efi.ps1`
- completed the host setup on this workstation so `tools/check-host.ps1 -Architecture x86_64` and bounded QEMU boot captures now work end-to-end
- isolated `runtime_context` tests from shared retained globals with an explicit test reset path
- added retained-runtime queue overflow/recovery coverage and rolling event-buffer coverage
- corrected stale docs so the workstation entry reflects the live x86 QEMU rail and the README's ARM aliases match `.cargo/config.toml`

## Next Focus

- grow the retained `runtime-active` slice into a broader long-lived runtime layout with clearer ownership boundaries
- keep improving the serial/debug trace so paging, retained runtime, heartbeat, and runtime-service changes are obvious under QEMU
- add one more real retained runtime mutation on top of the shared runtime context while keeping the mechanism layer tight
