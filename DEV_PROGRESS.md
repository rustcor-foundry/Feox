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

### 2026-03-27 (doc pass)

- completed a full doc review against the current implementation
- updated `STATUS.md` to reflect March 27 state including all higher-half handoff and retained runtime capabilities
- updated `docs/WORKSTATION_ENTRY.md` to reflect that the QEMU boot rail is now working on this workstation
- added `docs/BOOTSTRAP_RUNTIME.md` to document the 5-stage bootstrap stage model and the full retained context layer (`RuntimeSnapshot`, `BootstrapCoreContext`, `RuntimeServiceState`, `RuntimeServiceReport`, `RuntimeServiceHeartbeat`, `RuntimeReadinessState`, `RuntimeReadySummary`, command queue, event timeline)
- added `docs/VIRTUAL_ADDRESS_LAYOUT.md` to capture the three bootstrap windows now locked in code (`0xFFFF_9000_0000_0000` kernel alias, `0xFFFF_9000_0200_0000` stack, `0xFFFF_9000_0300_0000` data) and to call out the open decisions (direct map, permanent layout, user split, MMIO windows) that must be made before the layout grows

### 2026-03-27 (code review pass)

- completed a deep cross-referenced code review of all kernel source, crate source, and design docs
- added `docs/CODE_REVIEW.md` with 21 classified findings (5 correctness, 5 safety, 2 performance, 5 architecture gaps, 4 research alignment)
- immediate priority findings: no TLB invlpg in `map_4k_with`/`unmap_4k_with` (C-02), debugcon markers in production CR3 path (P-01), console re-init inside exception handler (C-03)
- structural gap: `feox-async` executor/reactor/waker infrastructure does not exist (A-02); everything in the async and device I/O stack is blocked on it
- `feox-nvme` blocked from kernel integration by `alloc::vec::Vec` dependency (A-01); fix is const-generic `InflightMap<const N: usize>`
- research alignment: strong Engler SOSP95 and Corey fit; Dune and Arrakis alignment requires SYSCALL entry path and capability enforcement

### 2026-03-27 (code review fix pass)

- fixed P-01: split `switch_page_table_root_and_jump` into `cfg(debug_assertions)` / `cfg(not(debug_assertions))` bodies; release build is now 5 clean instructions with no debugcon I/O
- fixed C-02: added `invalidate_page` (invlpg) to `arch::cpu` and called it after every PTE write in `map_4k_with` and `unmap_4k_with`; gated behind `cfg(target_os = "none")` so host tests are unaffected
- fixed C-03: added `CONSOLE_READY: AtomicBool` in `console.rs` and guarded exception handler console-init calls with `is_ready()` to close the double-fault re-entry window
- fixed A-01: replaced `alloc::vec::Vec` in `feox-nvme` with a const-generic `InflightMap<const N: usize>` backed by `[InflightEntry; N]` and a fixed-size CID free-stack; crate is now fully `no_std` with no allocator requirement
- fixed S-05: widened `CoreId` from `u16` to `u32` in `feox-asi` to match the ASI spec; updated all downstream uses
- fixed S-02: added `rdmsr`/`wrmsr` helpers and `enable_nxe()` in `arch::x86_64::cpu`; `early_init()` now sets EFER.NXE before any `FLAG_NO_EXECUTE` PTE is live; added `FLAG_NO_EXECUTE` constant and applied it to bootstrap stack and data pages
- fixed A-02 (partial): added `RunQueue<const CAP: usize>` (fixed-capacity FIFO ring buffer of `NonNull<TaskHeader>`) and a static `RawWakerVTable` with `make_task_waker` to `feox-async`; executor poll loop and reactor remain pending
- fixed C-01: added `AtomicU32 CONTEXT_OWNER` and `claim_bootstrap_context` to `runtime_context.rs`; all 9 mutation functions now carry `assert_context_claimed()` debug guards; `bootstrap()` calls `claim_bootstrap_context` immediately after `early_init`; tests call claim before any store
- fixed S-01: added a 64-bit TSS with dedicated 4 KiB IST stacks for NMI (IST1) and double-fault (IST2) in `gdt.rs`; expanded the GDT from 3 to 5 entries to hold the 128-bit TSS descriptor; `gdt::init()` installs IST stack tops, writes the TSS descriptor, and loads the Task Register via `ltr`; `idt::init()` and `relocate_and_reload()` now set `ist=1` on vector 2 (NMI) and `ist=2` on vector 8 (#DF)
- all 39 tests pass (27 xokernel + 6 feox-async + 3 feox-nvme + 3 feox-boot); `cargo kernel` and `cargo loader` clean

### 2026-03-27 (second fix pass)

- fixed S-03: added `enable_cr4_security_bits()` in `arch::x86_64::cpu` — reads CPUID leaf 7 sub-leaf 0 and conditionally sets SMEP (CR4.20), SMAP (CR4.21), and UMIP (CR4.11); called from `early_init()` after GDT/IDT and NXE; uses push/pop rbx around CPUID to work around LLVM's reserved-register constraint
- fixed S-04: added explicit doc comment to `hlt_loop` documenting that `cli` is intentional — NMIs are not masked, maskable reboot/QEMU-exit signals are deliberately refused from the panic halt path
- fixed C-04: added 4 KiB alignment `debug_assert` to `identity_mapped_table_mut` and a doc comment naming the non-aliasing invariant upheld by `BootstrapPagingAllocator` never reusing frames
- fixed C-05: added sequential-borrow explanation in `ensure_child_table` documenting why the two `table_mut` calls cannot alias
- fixed P-02: added doc comment to the PRESENT|WRITABLE intermediate-entry install in `ensure_child_table` flagging it as a bootstrap-only policy that must be tightened before per-process address spaces are introduced
- all 41 tests pass; `cargo kernel` and `cargo loader` clean

### 2026-03-27 (A-02 executor scaffold)

- closed A-02: added `PollFn` type alias, `poll_fn: UnsafeCell<Option<PollFn>>` field to `TaskHeader`, and `install_poll_fn` / `poll` methods to complete the type-erased poll path
- added `TaskCell<F>` — `#[repr(C)]` pinned per-task future storage; `spawn()` writes the future, installs the poll trampoline, and returns a `NonNull<TaskHeader>` ready for enqueueing; `#[repr(C)]` with `header` first guarantees the header pointer equals the cell pointer so the trampoline can cast back without offset arithmetic
- added `SingleCoreExecutor<const CAP>` — drives a `RunQueue<CAP>` of type-erased tasks; `poll_one` dequeues, calls `begin_poll`, constructs the waker, invokes the poll trampoline, and handles `Parked` vs `Requeue` epilogue; `run_until_idle` loops to empty
- added 3 executor tests: immediately-ready task completes in one pass; double-spawn returns `None`; self-waking countdown future runs through multiple requeue cycles and completes
- all 44 tests pass; `cargo kernel` and `cargo loader` clean

## Next Focus

- decide the permanent kernel virtual address layout and update `docs/VIRTUAL_ADDRESS_LAYOUT.md` with the direct-map base and per-core/MMIO zone choices
- A-03: add the ASI syscall entry path (SYSCALL/SYSRET stub + ring-3 entry point)
- A-05: add intermediate page-table frame ownership tracking to `BootstrapPagingAllocator`
