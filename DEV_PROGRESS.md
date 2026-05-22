# Feox Development Progress

This document is the running development record for Feox.

## Working Rules

- update this file with each meaningful checkpoint or commit
- keep entries implementation-focused
- use `docs/CURRENT_STATUS.md` for the current overall posture and milestone summary

## Entries

### 2026-05-22 (AP boot v1: trampoline alive)

- bumped Boot ABI to v3 (`BOOT_INFO_VERSION = 3`): adds
  `BootInfo::ap_trampoline_phys: u64` (zero if loader can't allocate
  one) and `BootHandoff::ap_trampoline_phys() -> Option<u64>`
- UEFI loader now allocates a single 4 KiB page below 1 MiB via
  `boot::allocate_pages(AllocateType::MaxAddress(0x100000), ...)`
  before `exit_boot_services`. SIPI vectors address sub-1-MiB
  physical memory, so the kernel can't get this through its normal
  capability allocator (which only sees Usable RAM above the BIOS
  area)
- new `kernel/feox-xokernel/src/lapic.rs`: maps the LAPIC MMIO at the
  MADT-reported address (typically `0xFEE00000`) through
  `mmio_map_bootstrap` (uncached). Exposes `read_id`, `read_version`,
  `send_init(dst_apic_id)`, `send_startup(dst_apic_id, vector)`.
  Polls ICR delivery-status bit between sends
- new `kernel/feox-xokernel/src/smp.rs`: writes a hand-assembled
  14-byte 16-bit real-mode trampoline at the loader-allocated frame.
  Trampoline does `cli; mov ds, cs; mov word [0xFF0], 0xCAFE; hlt`.
  `bring_up_first_ap(trampoline_phys, target_apic_id)` clears the
  magic, copies the bytes, sends INIT-SIPI-SIPI with crude busy
  delays (10M iter post-INIT, 200K between SIPIs), then polls the
  magic word for up to 5M iterations
- new `run_ap_boot_probe` (boot probe) runs after
  `run_acpi_smp_probe`. Picks the first non-BSP LAPIC, initializes
  the LAPIC driver, prints LAPIC id + version, calls
  `bring_up_first_ap`. With QEMU `-smp 4`, bounded smoke reports:
  `ap-boot-probe: lapic id=0 version=0x00050014`
  `ap-boot-probe: target_apic_id=1 trampoline_phys=0x9f000 sipi_vector=0x9f`
  `ap-boot-probe: AP alive (magic observed)`
- scope note: the trampoline does NOT enter Rust on the AP. It runs
  ~6 instructions of real-mode asm and halts. v2 will transition
  real -> protected -> long mode, share the BSP's CR3, load per-AP
  GS_BASE, and jump to a Rust `ap_entry`
- 101 host tests pass (was 97; +1 feox-boot ap_trampoline accessor,
  +1 lapic delivery-poll constant, +3 smp trampoline layout/magic
  tests); `cargo kernel` and `cargo loader` clean

### 2026-05-22 (SMP discovery: ACPI MADT + per-core infra)

- bumped Boot ABI to v2 (`BOOT_INFO_VERSION = 2`): adds
  `BootInfo::rsdp_phys: u64` (zero if loader can't provide one) and
  `BootHandoff::rsdp_phys() -> Option<u64>` accessor
- UEFI loader walks the configuration table (`uefi::system::
  with_config_table`) before `exit_boot_services` and forwards the
  ACPI 2.0 GUID's RSDP (falling back to ACPI 1.0). Stash kept inside
  `BootInfo::new(memory_map, rsdp_phys)`
- new `kernel/feox-xokernel/src/acpi.rs` parser: maps ACPI tables
  through `mmio_map_bootstrap` (ACPI ranges aren't in the direct
  map), validates RSDP signature + checksum, follows
  XSDT (revision ≥ 2) or RSDT (legacy), walks SDT entries to find
  `"APIC"` (MADT). Returns a fixed-capacity `AcpiTopology` with up
  to 32 `LapicEntry { processor_uid, apic_id, flags }` and the
  MADT-reported `local_apic_address`. RAII `AcpiMap` cleans up
  mappings on drop
- transition root prebuild now covers *all* `PER_CORE_MAX_CORES = 32`
  strides at `PER_CORE_BASE + core_id * PER_CORE_STRIDE`, not just
  core 0. Each stride reserves `PER_CORE_PREBUILT_PER_CORE_SIZE =
  2 MiB` of intermediates so future AP per-core inserts don't need
  a runtime frame allocator
- new `run_acpi_smp_probe` boot probe reads RSDP via a private
  `RSDP_PHYS: AtomicU64`, parses topology, and prints LAPIC count +
  per-CPU APIC ID / flags. Stashed in `bootstrap()` immediately
  after handoff
- bumped the QEMU smoke launch to `-smp 4` so the multi-AP discovery
  path runs on every boot; bounded smoke now reports:
  `acpi-smp-probe: lapics=4 enabled=4 local_apic_addr=0xfee00000`
  followed by per-CPU lines
- APs are *not* started yet (the locked slot just has PML4
  intermediates ready). AP boot (INIT/SIPI + 16-bit trampoline +
  per-AP entry) is the next focus
- 97 host tests pass (was 93; +1 for feox-boot rsdp accessor, +3 for
  acpi entry/flag tests); cargo kernel and cargo loader clean

### 2026-05-22 (per-core slot mapped at PER_CORE_BASE)

- added `memory::PER_CORE_PREBUILT_PER_CORE_SIZE = 2 MiB`. Sized for
  one core's PerCoreData footprint plus headroom for IST/TSS/GDT
  once those move per-core
- transition root construction now calls `prepare_4k_pages_with(
  PER_CORE_BASE, PER_CORE_PREBUILT_PER_CORE_SIZE)` alongside the
  existing MMIO prebuild. Allocates PML4/PDPT/PD/PT chains for core
  0's stride so post-handoff leaf inserts don't need a frame
  allocator. Failure produces a new transition_per_core_prebuild_failed
  error
- `per_core::initialize_core0` now uses
  `map_bootstrap_physical_capability_4k` to install the freshly
  allocated PhysicalPages cap at `PER_CORE_BASE` (writable,
  non-user, NX). `PerCoreData::self_ptr` is written to
  `PER_CORE_BASE`, and `IA32_GS_BASE` is loaded with the same
  address — so `gs:[0]` returns the locked VA, not the direct-map
  alias. Host build still falls back to the direct-map alias so
  cargo test compiles without page tables
- run_per_core_probe trace now reports
  `self_ptr=0xffffe00000000000` (was `0xffffc0000000XXXX`)
- only core 0's stride is prebuilt; secondary cores need their own
  prebuild plus AP boot + per-core GS_BASE load (next focus)
- 93 host tests pass; cargo kernel and cargo loader clean

### 2026-05-22 (per-core data area for core 0)

- new `kernel/feox-xokernel/src/per_core.rs` module:
  - `PerCoreData { self_ptr, magic, core_id, _reserved }` with
    `#[repr(C)]`; `self_ptr` at offset 0 so `mov rax, gs:[0]`
    materializes the area's own kernel virtual address
  - `PER_CORE_MAGIC = 0xFE0C_0DEF_ACEC_0FE1` (non-zero so a zeroed
    page is distinguishable from initialized state)
  - `initialize_core0()` requests a 4 KiB `PhysicalPages` cap, writes
    the struct through the direct-map alias, loads `IA32_GS_BASE`
    (`0xC000_0101`) with the alias virtual address, and records the
    base in a private static for the host-test fallback
  - `current() -> &'static PerCoreData` uses a `mov {0}, gs:[0]`
    inline asm load on `target_os = "none"`; falls back to the
    recorded static on host so tests compile without GS support
- registered as `pub mod per_core` in `lib.rs`
- new `run_per_core_probe` runs after the MMIO probe and before the
  NVMe probe: calls `initialize_core0`, reads the area through
  `current()`, verifies magic + core_id + self_ptr round-trip. Trace:
  `per-core-probe: ok via GS (core_id=0, self_ptr=0xffffc00000004000)`
- note: the area lives at its direct-map alias for this session; the
  locked virtual slot at `PER_CORE_BASE` (`0xFFFF_E000_0000_0000`)
  remains reserved but unmapped. Explicit per-core PML4
  intermediates + the eventual "all cores see per-CPU at the same VA"
  property are deferred to a follow-up
- 93 host tests pass (was 91; +2 for `per_core_data_layout_is_frozen`
  + `magic_is_non_zero`); `cargo kernel` and `cargo loader` clean

### 2026-05-21 (storage ABI v2: device capability enforced)

- added `register_bootstrap_storage_device_resource(bar_base,
  bar_size) -> Result<ResourceId, CapError>` in `capability.rs`,
  mirroring the memory-resource path but registering with
  `CapType::StorageDevice`
- extended `BlockDeviceState` with `device_cap: Option<CapHandle>`
  and surfaced:
  - `block::register_device_capability(bar_base, bar_size) ->
    Result<CapHandle, BlockError>` — registers the resource, mints
    the root cap with full bootstrap permissions, stashes it on the
    live device. Called by the boot probe right after
    `block::initialize`
  - `block::storage_device_cap() -> Option<CapHandle>` accessor
  - `block::shutdown` now releases the device cap (if any) before
    failing in-flight futures and clearing the static
- `dispatch_storage_submit_read` now requires `args.device` to
  resolve to a `CapType::StorageDevice` with `READ | WRITE`. Anything
  else (including the old sentinel zero handle) returns
  `0xFFFF_0500 + InvalidCapability`
- self-test grew a negative path: it submits with a zero CapHandle
  and asserts the dispatch returns a non-zero error code, then
  submits the positive path with the real `device_cap` from
  `block::register_device_capability`. Bounded smoke now reports:
  - `nvme-async-probe: device cap minted (id=17, gen=0)`
  - `storage-abi-probe: negative path rejected as expected
    (code=0xffff0500)`
  - `storage-abi-probe: ready sct=0 sc=0 dnr=0 polls=1` (positive)
- threaded `bar_phys + bar_size` through `run_nvme_admin_probe` so
  the device cap resource has accurate BAR metadata (8 KiB at the
  controller's BAR0 phys)
- updated `docs/STORAGE_ABI.md` with the v2 evolution; next focus is
  v3 (DmaPool flow + EventSlot variant)
- 91 host tests pass; `cargo kernel` and `cargo loader` clean

### 2026-05-21 (storage ABI v1: capability-backed buffer)

- added `CapType::StorageDevice` enum variant to `feox-asi` (marker for
  future enforcement; PCI enumeration doesn't mint one yet)
- swapped `StorageSubmitReadArgs::buffer_phys: PhysicalAddress` for
  `{ buffer: CapHandle, buffer_offset: u64 }` — wire size 40 → 48
  bytes; v0 was explicitly not stable
- added `crate::capability::cap_to_phys_base(handle, required_perms) ->
  Result<(PhysicalAddress, u64), CapError>` that verifies the cap and
  returns its backing resource's `(base, size_bytes)`. Currently
  accepts `CapType::PhysicalMemory`; structured to accept
  `CapType::DmaPool` once that resource type is minted
- `dispatch_storage_submit_read` now verifies the buffer cap (requires
  READ + WRITE), checks `buffer_offset + 4096 <= size_bytes`, and
  computes `buffer_phys = base + buffer_offset` before handing off to
  `block::storage_submit_read`. Rejects with `InvalidCapability` on
  any failure
- `run_nvme_admin_probe` self-test now requests its own
  `PhysicalPages` capability via `request_bootstrap_capability` (so it
  holds the handle, not just the phys) and passes
  `buffer: handle, buffer_offset: 0` through the syscall. Bounded
  smoke still reports `storage-abi-probe: ready sct=0 sc=0 dnr=0
  polls=1` and decodes the same `FEOX-NVME-SMOKE-LBA0`
- updated `docs/STORAGE_ABI.md` with v0 → v1 → v2 evolution and the
  current v1 args layout
- 91 host tests pass; `cargo kernel` and `cargo loader` clean

### 2026-05-21 (storage ABI v0)

- locked the first-cut storage syscall surface in `docs/STORAGE_ABI.md`:
  block-style ABI (protocol-agnostic), Submit+Poll semantics, opcode
  range `0x0500`-`0x05FF` reserved for storage, v0 takes raw
  `buffer_phys` with a sentinel device capability (v1 will replace
  both with `CapType::StorageDevice` + `CapType::DmaPool` flowed
  end-to-end)
- added `AsiOp::StorageSubmitRead = 0x0500` and
  `AsiOp::StoragePoll = 0x0501` to `crates/feox-asi`, plus shared
  types: `StorageToken`, `StorageSubmitReadArgs`, `StoragePollArgs`,
  `StorageCompletion`, `StoragePollResult`, `StorageError`
- extended `crate::block` with a small 8-slot inflight-submissions
  table keyed by `(slot_idx, generation)`. `storage_submit_read`
  reserves a slot and stashes the `NvmeIoFuture<8>` from
  `block::read`; `storage_poll` polls the future with a noop waker and
  either reports `Ok(None)` (still pending) or `Ok(Some(completion))`
  on Ready. Stale generations are rejected as `InvalidToken`
- wired `dispatch_storage_submit_read` + `dispatch_storage_poll` in
  `arch/x86_64/syscall.rs`, plus an unconditional `block::drain()` at
  the top of `feox_syscall_dispatch` so user space sees fresh
  completions on the next poll. Storage error codes return in the
  high-half band `0xFFFF_0500 + StorageError`
- exposed `feox_syscall_dispatch` as `pub extern "C"` so the boot
  self-test can invoke the exact same entry point the SYSCALL/SYSRET
  trampoline uses
- new storage-abi self-test runs in `run_nvme_admin_probe` after the
  existing NVMe async probe completes: allocates a fresh DMA page,
  walks `StorageSubmitRead` → `StoragePoll` (loop) → decode buffer.
  Bounded smoke now reports `storage-abi-probe: ready sct=0 sc=0
  dnr=0 polls=1` and confirms the same `'FEOX-NVME-SMOKE-LBA0'` data
- 91 host tests pass (was 90; +1 for `storage_abi_types_keep_expected_sizes`);
  `cargo kernel` and `cargo loader` clean; CI green on `lx-ws01`

### 2026-05-21 (background drainer task)

- added `pub async fn drainer_task()` to `kernel/feox-xokernel/src/block.rs`:
  an infinite `loop { drain(); yield_now().await; }` that pumps the I/O
  CQ on every executor pass and cooperatively yields between passes via
  a small `YieldNow` helper (self-wakes once, returns `Pending`, then
  `Ready`)
- migrated `run_nvme_admin_probe` off the hand-rolled drive loop:
  - spawns the drainer task on a second `TaskCell` and enqueues it
    onto the same `SingleCoreExecutor<4>` alongside the read task
  - replaced the alternating `executor.run_until_idle()` +
    `block::drain()` pump with a single `poll_one` loop that breaks
    when the read task reaches `TaskState::Complete`
  - `run_until_idle` can no longer be used here — the drainer self-wakes
    on every poll, so it would spin forever. `poll_one` polls one
    task per pass instead
- bounded smoke now reports `nvme-async-probe: task complete (polls=4)`
  with the LBA0 data still decoded correctly
  (`'FEOX-NVME-SMOKE-LBA0'`), and reaches `stage: runtime service idle`
- 90 host tests pass; `cargo kernel` and `cargo loader` clean

### 2026-05-21 (kernel block API)

- factored the NVMe submit/drain plumbing out of the boot probe and into
  a new `kernel/feox-xokernel/src/block.rs` module (registered as
  `pub mod block` in `lib.rs`)
- exposed a free-function surface so kernel code can do
  `crate::block::read(nsid, lba, buf_phys).await` without touching
  `feox_nvme::SubmissionQueueEntry` or controller registers:
  - `BlockDeviceConfig` (registers + SQ/CQ pointers + queue depths + I/O QID)
  - `initialize(config) -> Result<(), BlockError>` — sets up the static device
  - `read(nsid, lba, buf_phys) -> Result<NvmeIoFuture<8>, BlockError>` —
    reserves a CID via `NvmeQueuePair::submit`, builds the NVM Read SQE,
    writes it to the I/O SQ, rings the SQ tail doorbell, returns the
    NVMe future
  - `drain() -> bool` — pumps CQEs from the I/O CQ into
    `NvmeQueuePair::complete` (which fires the bound waker dispatcher)
  - `shutdown()` — calls `NvmeQueuePair::fail` to release outstanding
    futures, then clears the static
  - `is_initialized()` — diagnostic accessor
- migrated `run_nvme_admin_probe` onto the new API. After the
  controller bring-up + Identify + Create I/O CQ/SQ admin commands
  succeed, the probe calls `block::initialize`, spawns an async task
  that does `block::read(...).await`, and uses `block::drain` in the
  drive loop. `block::shutdown` runs after the I/O queue teardown
  finishes so the static state is cleared before the controller's
  CC.EN goes back to 0
- removed the duplicate `NvmeAsyncState` / `nvme_async_submit_read` /
  `nvme_async_drain` from `boot.rs`
- bounded smoke now reports
  `nvme-async-probe: submitting NVM Read via block::read` followed by
  the same successful completion + decoded data, still in
  `poll_passes=2 drain_passes=1`
- 90 host tests pass; `cargo kernel` and `cargo loader` clean

### 2026-05-21 (executor enqueue gap closed)

- closed the executor "enqueue gap" in `feox_async::TASK_HEADER_WAKER_VTABLE`.
  The waker now pushes the task back into the owning run queue when
  `wake()` transitions a parked task to ready
- added `wake_target: AtomicPtr<u8>` and `wake_dispatcher: AtomicPtr<c_void>`
  fields to `TaskHeader`, plus an `unsafe fn bind_wake_target(target,
  dispatcher: WakeDispatcher)` method that the executor calls on
  `enqueue`. The vtable's `wake` and `wake_by_ref` closures invoke the
  bound dispatcher on `WakeDisposition::Enqueue`; the dispatcher is a
  monomorphized `run_queue_push_dispatcher::<CAP>` that casts the
  type-erased target back to `*mut RunQueue<CAP>` and pushes the task
- `SingleCoreExecutor::enqueue` now calls `bind_wake_target` so every
  task added to an executor has an enqueue path for external wakes
- new unit test `waker_re_enqueues_parked_task_after_external_wake`
  verifies the closed gap: spawn a future that stashes its waker on
  first poll and parks, run to idle, fire the stashed waker, and
  confirm the executor is no longer idle (the task has been re-queued)
  before the next `run_until_idle` polls it to completion
- removed the manual `executor.enqueue(header)` re-enqueue from
  `run_nvme_admin_probe`: the waker handles it now
- bounded smoke still reports `task complete (poll_passes=2
  drain_passes=1)` for the NVMe LBA read, confirming the path still
  closes after one drain + one re-poll
- 90 host tests pass (89 + the new external-wake test); `cargo kernel`
  and `cargo loader` clean

### 2026-05-21 (async NVMe driver path)

- wired the live NVMe controller through `feox-async`'s executor + the
  existing `feox_nvme::NvmeQueuePair<N>` / `InflightMap<N>` async
  primitives. The boot probe now reads LBA 0 through a real
  `async`/`await` task instead of busy-polling
- new `NvmeAsyncState` static in `boot.rs` holds the live controller
  state (registers, SQ/CQ pointers + indices + phase, queue_pair);
  `nvme_async_submit_read` reserves a CID via `NvmeQueuePair::submit`,
  builds the NVM Read SQE, writes it to the I/O SQ and rings the
  doorbell; `nvme_async_drain` walks the I/O CQ and delivers each
  completion to `NvmeQueuePair::complete`
- the async task uses `feox_async::TaskCell::new(CoreId(0)).spawn(...)`
  with an `async move` block that awaits the read future, decodes the
  completion, and prints the ASCII + hex prefix of the read buffer; the
  drive loop alternates `executor.run_until_idle()` with a drainer pass
- **three stack/layout issues uncovered and fixed**:
  - `BOOT_STACK` (low-half stack used before the CR3 handoff) overflowed
    silently into the adjacent `.data` and corrupted the `GDT` static.
    The post-handoff `lgdt` then reloaded a zeroed GDT and locked up.
    Bumped from 16 KiB → 64 KiB
  - `TRANSITION_STACK_PAGES` (higher-half runtime stack) needed more
    room once the async runtime + task cell + future state machine
    were live; bumped from 4 → 8 pages (16 KiB → 32 KiB)
  - `EarlyKernelReservations::MAX_REGIONS` bumped from 96 → 256 to
    accommodate the larger boot-time allocation footprint
- **executor "enqueue gap"** documented in
  `feox_async::TASK_HEADER_WAKER_VTABLE` is real: the waker transitions
  the task atomic state but does NOT push the task back into the run
  queue. The probe re-enqueues the task manually after each successful
  drain; closing the gap is the next focus item
- bounded smoke now reports
  `nvme-async-probe: completion cid=0 sct=0 sc=0 dnr=false` followed by
  `nvme-async-probe: LBA0 ascii='FEOX-NVME-SMOKE-LBA0' hex=...` and
  `nvme-async-probe: task complete (poll_passes=2 drain_passes=1)` —
  two polls and one drain to round-trip a real LBA 0 read through the
  async runtime
- 89 host tests pass; `cargo kernel` and `cargo loader` clean

### 2026-05-21 (NVMe I/O queues + LBA read)

- extended `feox_nvme::SubmissionQueueEntry` with builders for the
  admin queue-management commands and one I/O command:
  `create_io_completion_queue`, `create_io_submission_queue`,
  `delete_io_completion_queue`, `delete_io_submission_queue`,
  `nvm_read`
- generalized the doorbell helpers in `feox_nvme::ControllerRegisters`
  from `ring_admin_*_doorbell` to `ring_sq_tail_doorbell(qid, tail)` /
  `ring_cq_head_doorbell(qid, head)`; the offset math computes the
  correct admin (qid=0) or I/O (qid>=1) doorbell location for any
  controller with `CAP.DSTRD = 0`
- factored a tiny `NvmeProbeQueueState` (sq_tail, cq_head, phase) and a
  `nvme_submit_and_wait` helper in `boot.rs` so the probe can pump
  multiple commands through the admin and I/O queues with consistent
  doorbell + phase-tracking logic
- extended `run_nvme_admin_probe` into a full NVMe lifecycle: reset →
  enable → Identify Controller → Create I/O CQ → Create I/O SQ → NVM
  Read LBA 0 → Delete I/O SQ → Delete I/O CQ → shutdown. The probe
  decodes the model/serial/firmware (as before) and the 20-byte
  ASCII / 32-byte hex prefix of the read buffer
- updated `tools/run-qemu-smoke.sh` to stamp the ASCII pattern
  `FEOX-NVME-SMOKE-LBA0` into LBA 0 of the smoke disk image each run, so
  the trace shows recognizable bytes coming back from the controller
- **subtle bit-layout bug fix**: `CC.IOSQES` is at bits 19:16 and
  `CC.IOCQES` is at bits 23:20 of the Controller Configuration
  register, not 23:20 / 27:24 as the spec text suggested. QEMU's
  `nvme_create_cq` rejects Create I/O CQ with the misleadingly-named
  `NVME_MAX_QSIZE_EXCEEDED` status when these fields don't match
  `NVME_SQES=6` / `NVME_CQES=4`. The boot CC programming now uses the
  correct shifts (`(4u32 << 20) | (6u32 << 16) | 1`)
- bounded smoke trace now reports:
  `nvme-io-probe: NVM Read LBA 0 ok`,
  `nvme-io-probe: LBA0 ascii='FEOX-NVME-SMOKE-LBA0' hex=46454f58...`,
  confirming a real round-trip through the device
- 89 host tests pass; `cargo kernel` and `cargo loader` clean; smoke
  reaches `stage: runtime service idle`

### 2026-05-21 (NVMe admin queue)

- extended `feox_nvme::ControllerRegisters` with the admin-queue
  register helpers: `cc` / `set_cc`, `csts` (decoded via new `Csts`),
  `set_aqa` (encodes the zero-based field), `set_asq`, `set_acq`,
  `ring_admin_sq_tail_doorbell`, `ring_admin_cq_head_doorbell`
- added `feox_nvme::SubmissionQueueEntry` (64-byte `#[repr(C, align(64))]`
  layout) with a `SubmissionQueueEntry::identify_controller` builder
  (opcode 0x06, CNS=0x01)
- added `feox_nvme::CompletionQueueEntry` (16-byte) plus decoders for
  command id, phase, status field, and SQ head pointer
- extended the boot probe with `run_nvme_admin_probe`: allocates three
  4 KiB DMA pages through the existing capability minter (admin SQ,
  admin CQ, identify buffer), reaches them via the direct map, resets
  the controller, programs AQA/ASQ/ACQ, sets `CC.EN`, polls
  `CSTS.RDY`, submits `Identify Controller` with cid=1, polls the
  admin CQ for phase change, decodes the model / serial / firmware
  ASCII fields, and shuts the controller back down
- doubled the NVMe BAR mapping in the probe from 4 KiB to 8 KiB so the
  admin doorbell page at offset 0x1000 is reachable
- bounded smoke now reports the full handshake:
  `nvme-admin-probe: completion cid=1 sq_head=1 status=0x0` and
  `model='QEMU NVMe Ctrl' serial='feox-smoke' firmware='10.0.8'`
  (the `feox-smoke` serial is the same string passed in the QEMU
  command-line, confirming a live round-trip)
- 89 host tests pass; `cargo kernel` and `cargo loader` clean; smoke
  reaches `stage: runtime service idle`

### 2026-05-21 (NVMe wired into MMIO)

- added 32-bit port I/O helpers `outl` / `inl` to `arch/x86_64/cpu.rs`
- added legacy x86 PCI configuration-space reader in
  `arch/x86_64/pci.rs` (single `config_read32` over the 0xCF8/0xCFC
  port pair)
- added `kernel/feox-xokernel/src/pci.rs` (registered as `pub mod pci`)
  with `PciDevice`, `scan_for_class`, `bar64`, and the constant
  `PCI_CLASS_NVME = 0x010802`. The scanner walks the 256×32×8 bus space
  and returns the first matching function
- added `feox_nvme::ControllerRegisters` plus decoded
  `feox_nvme::Cap` and `feox_nvme::Vs` views. `ControllerRegisters`
  wraps a raw pointer to a mapped BAR and exposes
  `cap()` / `vs()` reads via `read_volatile`
- promoted the kernel's default features to `["runtime", "storage"]` so
  the default `cargo kernel` build pulls in `feox-async` and
  `feox-nvme`; the crate exposes `feox_xokernel::nvme as feox_nvme`
  (`#[cfg(feature = "storage")]`)
- added `run_nvme_mmio_probe` to the boot self-test: PCI-scan for the
  NVMe class, read BAR0, map the first 4 KiB through
  `mmio_map_bootstrap` (UC), confirm the page-table walk reports the
  same phys + UC flags, then construct a `ControllerRegisters` over the
  mapped BAR and print `CAP` (with `mqes`, `dstrd`, `mpsmin`, `mpsmax`)
  and `VS` (`major.minor.tertiary`). The probe prints "no NVMe
  controller found" and continues cleanly when no device is present
- updated `tools/run-qemu-smoke.sh` to lazily create a 16 MiB sparse
  backing file at `target/feox-qemu/x86_64-nvme-disk.debug.img` and
  attach it as an emulated NVMe device
  (`-device nvme,drive=feox-nvme-disk,serial=feox-smoke`)
- bounded smoke now traces:
  `nvme-mmio-probe: found 00:03.0 vid=0x1b36 did=0x0010 class=0x010802`,
  `BAR0 phys=0x000000c000000000`,
  `CAP=0x004008200f0107ff mqes=2047 dstrd=0 mpsmin=0 mpsmax=4`,
  `VS=0x00010400 version=1.4.0`, and reaches
  `stage: runtime service idle`
- 89 host tests pass (added 2 unit tests covering `Cap` / `Vs`
  decoders); `cargo kernel` and `cargo loader` clean

### 2026-05-21 (MMIO bring-up)

- added `FLAG_WRITE_THROUGH` (bit 3, PWT) and `FLAG_CACHE_DISABLE` (bit 4,
  PCD) public constants on `PageTableEntry`, plus `is_cache_disabled` /
  `is_write_through` accessors
- added `MMIO_PREBUILT_SIZE` constant (64 MiB) in `memory.rs` — the size
  of the MMIO sub-window whose page-table intermediates are prebuilt at
  boot
- added retained MMIO state in `runtime_context.rs`:
  `BootstrapMmioMapping`, a 16-slot retained mapping array, a bump-offset
  cursor, and `allocate_mmio_range` / `record_mmio_mapping` /
  `remove_mmio_mapping` / `mmio_mappings` / `mmio_bump_offset` helpers
- created `kernel/feox-xokernel/src/mmio.rs` (registered as `pub mod
  mmio` in `lib.rs`) with `mmio_map_bootstrap(phys, length, writable,
  uncached)` and `mmio_unmap_bootstrap(region)`. Internal kernel API;
  no syscall ABI yet
- transition root builder in `boot.rs` now calls `prepare_4k_pages_with`
  over `[MMIO_BASE, MMIO_BASE + MMIO_PREBUILT_SIZE)` after the direct
  map install, so device drivers can install MMIO leaf entries without a
  runtime allocator
- extended the boot self-test with `run_mmio_cycle_probe`: maps the
  LAPIC base (`0xFEE00000`) into the MMIO zone with UC, walks the active
  root to verify the leaf entry's phys + PCD + PWT bits, then unmaps
- 4 new unit tests in `mmio.rs` exercise alignment validation and
  retained-table round-trip; all 87 host tests pass
- bounded smoke trace shows
  `mmio-probe: walk phys=0x00000000fee00000 writable=true
  cache_disabled=true write_through=true` and reaches
  `stage: runtime service idle`
- updated `docs/VIRTUAL_ADDRESS_LAYOUT.md` (MMIO row promoted from policy
  marker to live for the prebuilt sub-window) and `docs/CURRENT_STATUS.md`

### 2026-05-21 (broadened self-test)

- broadened the bootstrap VM self-test in `boot.rs`:
  - request a 4-page contiguous physical capability instead of a single page
  - issue one `mem_map_bootstrap` for the whole 16 KiB region
  - single `mem_vtop_bootstrap` on the first page (preserves the
    single-address code path)
  - `mem_vtop_batch_bootstrap` on all 4 pages with a stack-allocated input
    and output buffer
  - verify contiguity: `phys_batch[i+1] == phys_batch[i] + PAGE_SIZE`
  - cross-check: `phys_batch[0] == single_vtop_result`
  - one `mem_unmap_bootstrap` releases the whole region
- the smoke trace now logs every per-page translation, so a regression in
  the live VM lane shows up in the bounded-CI log immediately
- 83 host tests pass; bounded smoke reaches `stage: runtime service idle`;
  `cargo kernel` and `cargo loader` clean

### 2026-05-21 (access window retirement)

- retired the bootstrap page-table access window mechanism now that
  `DirectMapPageTables` is the live source for `mem_map` / `mem_unmap` /
  `mem_vtop`
- removed `BootstrapPageTableAccessWindow`, `BootstrapPageTableAccessReservation`,
  `BootstrapPageTableAccessSource`, `PageTableAccessError`, and
  `bootstrap_page_table_access_control_table_mut` from `paging.rs`
- removed `BootstrapPageTableAccessSlot`, `BOOTSTRAP_PAGE_TABLE_ACCESS_SLOT_CAPACITY`,
  the slot static, `acquire_page_table_access_slot`, `release_page_table_access_slot`,
  `page_table_access_slots`, and the access-window reset from
  `runtime_context.rs`
- removed the access-window prebuild and self-map install from the
  transition root builder in `boot.rs`
- removed the three access-window unit tests; the parallel-test race on
  `BOOTSTRAP_PAGE_TABLE_ACCESS_SLOTS` is gone as a side effect (no shared
  static remains)
- kept `BOOTSTRAP_PAGE_TABLE_ACCESS_WINDOW_BASE` / `_SIZE` in `memory.rs`
  as a reserved 20 KiB address-space slot at `0xFFFF_9000_0800_0000` —
  doc comment updated to reflect the slot-only status
- updated `docs/PAGE_TABLE_ACCESS_PLAN.md` (Resolution → Retirement) and
  `docs/VIRTUAL_ADDRESS_LAYOUT.md` (live-status table marks the access
  window retired)
- 83 host tests pass across 3 parallel runs without flake; smoke reaches
  `stage: runtime service idle`; `cargo kernel` and `cargo loader` clean

### 2026-05-21 (direct map)

- added `PageTableRoot::map_2m_with` to `paging.rs` — installs a 2 MiB
  huge-page leaf at the PD level (PS=1), allocating missing intermediates
- added `DirectMapPageTables` source in `paging.rs` that derefs page-table
  frames at `DIRECT_MAP_BASE + phys` (no slots, no LRU, no per-frame
  `invlpg`)
- transition root builder in `boot.rs` now installs the permanent direct
  map of physical RAM after the bootstrap windows: one 2 MiB huge-page
  entry per aligned interior chunk of every Usable region, with 4 KiB
  head/tail entries to cover the unaligned bytes; non-Usable phys
  (kernel image, MMIO, BIOS ROM, ACPI) is deliberately excluded
- cut over `mem_map_bootstrap`, `mem_unmap_bootstrap`, and
  `mem_vtop_bootstrap` from `BootstrapPageTableAccessSource::active()` to
  `DirectMapPageTables`
- simplified the boot probe: dropped the multi-step access-window
  primitive probe (Phase A — served its purpose during the bug hunt);
  added a single direct-map read of the active root frame; the live
  `mem_map` / `mem_vtop` / `mem_unmap` cycle still runs every boot
- bounded smoke now traces `paging: direct_map_pages_2m=100 pages_4k=1490
  coverage_bytes=0xcdd2000` (200 MiB of bulk + ~5.8 MiB of head/tail on
  the QEMU 256 MiB target) and reaches `stage: runtime service idle`
- 86 host tests pass; `cargo kernel` and `cargo loader` clean
- updated `docs/VIRTUAL_ADDRESS_LAYOUT.md` (direct-map region promoted
  from policy marker to live) and `docs/CURRENT_STATUS.md`

### 2026-05-21 (layout lock)

- locked the permanent kernel virtual layout in `docs/VIRTUAL_ADDRESS_LAYOUT.md`:
  user/kernel split is the standard x86_64 canonical 128 TiB/128 TiB; kernel
  image stays at `0xFFFF_9000_0000_0000` (bootstrap window becomes permanent);
  direct map at `0xFFFF_C000_0000_0000` (32 TiB); per-core data at
  `0xFFFF_E000_0000_0000` (1 TiB stride × 32 cores); MMIO at
  `0xFFFF_F000_0000_0000` (8 TiB); kernel vmalloc / capability tables at
  `0xFFFF_F800_0000_0000` (8 TiB)
- added policy-marker constants in `memory.rs` for each locked region
  (`DIRECT_MAP_BASE`, `DIRECT_MAP_SIZE`, `PER_CORE_BASE`, `PER_CORE_STRIDE`,
  `PER_CORE_MAX_CORES`, `MMIO_BASE`, `MMIO_SIZE`, `KERNEL_VMALLOC_BASE`,
  `KERNEL_VMALLOC_SIZE`) so callers can reference the locked addresses
  before live mappings exist
- updated `docs/CURRENT_STATUS.md` so Immediate Next Focus is direct-map
  bring-up rather than layout policy work

### 2026-05-21

- diagnosed and fixed the post-handoff bootstrap VM blocker called out in `docs/PAGE_TABLE_ACCESS_PLAN.md` Latest Finding
- root cause was a release-path off-by-N bug in `BootstrapPageTableAccessReservation::release`: the control PT alias was computed as `slot.virtual_base - PAGE_SIZE`, which only equals `window_base` for slot index 0 and silently dereferenced the previous slot's alias for any later slot; the second release in a multi-frame walk then faulted on a slot whose own alias had just been cleared
- stored `window_base` on `BootstrapPageTableAccessReservation` and used it directly in `release()`
- raised `BOOTSTRAP_PAGE_TABLE_ACCESS_SLOT_CAPACITY` from 3 to 4 and the access window size from 16 KiB to 20 KiB so one `map_4k_with` / `unmap_4k_with` / `translate_with` walk can hold simultaneous aliases for PML4, PDPT, PD, and PT
- pubbed `BOOTSTRAP_PAGE_TABLE_ACCESS_SLOT_CAPACITY` and threaded it through `BootstrapPageTableAccessSource` so the const is the single source of truth
- cut over `mem_map_bootstrap`, `mem_unmap_bootstrap`, and `mem_vtop_bootstrap` in `vm.rs` from `BootstrapIdentityMappedPageTables` to `BootstrapPageTableAccessSource::active()`
- added a bootstrap VM self-test in `boot.rs` that runs in the retained runtime command queue: a low-level access-window primitive probe followed by a full `mem_map` → `mem_vtop` → `mem_unmap` cycle against a real one-page capability
- bounded QEMU smoke now reaches `stage: runtime service idle` with the live VM cycle exercised on every boot
- added `tools/run-qemu-smoke.sh` as a bash equivalent of `tools/run-qemu-smoke.ps1` so contributors on a Linux host without `pwsh` can still run the bounded smoke locally; the PowerShell harness remains the authoritative CI entry point
- updated `docs/CURRENT_STATUS.md` and `docs/PAGE_TABLE_ACCESS_PLAN.md` to mark Bootstrap VM hardening resolved
- all 86 host tests pass (`cargo test`); `cargo kernel` and `cargo loader` clean

### 2026-04-08

- locked a bootstrap capability-backed VM window at `0xFFFF_9000_0400_0000` with a fixed 64 MiB bootstrap-only mapping arena in `memory.rs` and `docs/VIRTUAL_ADDRESS_LAYOUT.md`
- added shared ASI memory-mapping ABI types to `feox-asi`: `MapFlags`, `MemMapArgs`, `MappedRegion`, and `MemError`
- extended `paging::PageTableRoot` with `prepare_4k_pages_with` so the bootstrap path can prebuild the first VM window without installing leaf mappings
- added retained bootstrap VM mapping records to `runtime_context.rs`
- added bootstrap `mem_map` / `mem_unmap` helpers in `kernel/feox-xokernel/src/vm.rs` for physical-memory capabilities inside the retained bootstrap VM window
- wired the x86_64 ASI syscall lane to handle `MemMap` and `MemUnmap`, including batch-path support and first syscall tests
- taught the transition-root build path to prebuild the bootstrap VM window page-table structures before the higher-half handoff
- extended the shared ASI memory ABI with `MemVtoPArgs` and `MemVtoPBatchArgs`
- added retained bootstrap mapping lookup by handle and virtual address, then wired bootstrap `mem_vtop` / `mem_vtop_batch` helpers and x86_64 syscall dispatch support
- added host-safe positive tests for bootstrap virtual-to-physical translation and exposed the retained-runtime test reset path so shared bootstrap state stays isolated
- expanded `.gitea/workflows/ci.yml` so `lx-ws01` now runs host tests, real target builds, target lint/check coverage, and a bounded x86_64 QEMU smoke boot
- taught `tools/check-host.ps1` and `tools/run-qemu.ps1` to discover common Linux QEMU and OVMF paths, then added `tools/run-qemu-smoke.ps1` as the normal bounded CI boot wrapper
- attempted a live bootstrap VM self-test in the higher-half runtime path and confirmed the current VM helpers still rely on page-table-access assumptions that are safe in host tests but not yet hardened for live post-handoff use; reverted that probe and documented the limitation explicitly
- added `docs/PAGE_TABLE_ACCESS_PLAN.md` to define the next narrow design step: a bootstrap page-table access window for live higher-half paging operations without committing to a permanent direct map yet
- reserved a 16 KiB bootstrap page-table access window at `0xFFFF_9000_0800_0000` in `memory.rs` and `docs/VIRTUAL_ADDRESS_LAYOUT.md` so the next implementation slice has an explicit live paging access target
- taught the transition-root builder in `boot.rs` to prebuild the reserved page-table access window alongside the bootstrap VM window so the live accessor layer has address-space scaffolding ready before the higher-half handoff
- added retained page-table access slot bookkeeping in `runtime_context.rs` and a first `BootstrapPageTableAccessWindow` helper in `paging.rs` so live higher-half page-table-frame aliases now have an explicit reservation model before the VM callers are switched over
- reattempted a live bootstrap VM self-test and narrowed the remaining blocker: the first post-handoff alias of the active root frame still faults because the reservation prototype needs one non-identity foothold for the access window's own control page before live `mem_map` can stop depending on bootstrap identity access
- extended that prototype by self-mapping the access-window control PT page during transition-root construction and confirmed in QEMU that the runtime can install dynamic slot PTEs after handoff, but the first live write through the aliased root frame still page-faults, so the new access source remains a prototype rather than the default live VM path

### 2026-04-07

- closed A-03 by adding shared ASI syscall transport types to `feox-asi`: `PhysicalAddress`, `PciAddress`, `ProcessId`, `ThreadId`, `CoreSet`, `AsiOp`, `SyscallResult`, `BatchOp`, and `BatchError`
- expanded the x86_64 GDT with ring-3 code/data segments and exposed selectors needed for `SYSCALL` / `SYSRET`
- added `kernel/feox-xokernel/src/arch/x86_64/syscall.rs` with real `IA32_STAR` / `IA32_LSTAR` / `IA32_FMASK` setup, a dedicated syscall stack, and a minimal `SYSCALL` entry stub
- wired `arch::early_init()` to install the syscall transport after GDT/IDT, NXE, and CR4 security-bit setup
- added the first typed syscall dispatcher with raw-opcode validation, `ProcYield` as a minimal success path, and working `AsiBatch` validation plus per-op result reporting
- started A-04 with shared `CapType`, `CapPermissions`, `CapError`, and `CapInfo` metadata in `feox-asi`
- added `kernel/feox-xokernel/src/capability.rs` with a 256-slot bootstrap capability table, 64-byte `CapSlot`s, generation-checked handle verification, root-cap minting, `cap_list`, and `cap_release`
- wired bootstrap capability minting for discovered memory regions and extended the syscall path with working `cap_list` and `cap_release` handling
- extended A-04 with a bootstrap resource registry, delegation tree, cascade release, and first `cap_delegate` syscall handling
- extended A-04 again with shared `CapRequest` / `PageFlags` ABI, allocatable physical-memory resources, bootstrap `cap_request` handling for physical pages, and end-to-end syscall coverage
- added `kernel/feox-xokernel/src/vm.rs` with the first capability-backed 4 KiB map helper, so verified physical-memory capabilities now drive a real page-table install in host tests
- added test-only capability-state locking so shared bootstrap globals stay isolated across parallel unit tests
- re-verified `cargo test`, `cargo kernel`, and `cargo loader`
- updated `README.md`, `docs/CURRENT_STATUS.md`, and `docs/WORKSTATION_ENTRY.md` so the repo no longer points at a missing `STATUS.md`

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

### 2026-03-27 (A-05 frame-tree sidecar)

- closed A-05: added `PageTableEdges` — a fixed-capacity 32-pair `(parent_phys, child_phys)` sidecar for bootstrap page-table frame-tree tracking; BFS `frames_reachable_from` enables future reclaim of all intermediate frames once the transition root is replaced
- added `record_edge` default no-op method to `PageTableFrameAllocator` trait; `BootstrapPagingAllocator` overrides it to call `PageTableEdges::record`
- wired `allocator.record_edge(table_frame, child)` call into `ensure_child_table` immediately after every successful intermediate-table allocation
- added `edges()` accessor on `BootstrapPagingAllocator` to expose the accumulated sidecar after a mapping sequence
- added 3 tests: `page_table_edges_records_and_reports_count`, `frames_reachable_from_traverses_tree`, `bootstrap_paging_allocator_records_edges_on_map`
- all 47 tests pass; `cargo kernel` and `cargo loader` clean

### 2026-03-27 (status doc pass)

- updated `STATUS.md` to reflect post-code-review state: all 21 findings resolved or deferred, 47 tests, full capability inventory including TSS/IST stacks, EFER.NXE, CR4 security bits, TLB invalidation, console guard, executor scaffold, and PageTableEdges sidecar
- updated `docs/CURRENT_STATUS.md` to summarize all code review fixes by finding ID and call out the two deferred items (A-03 syscall entry, A-04 capability table)

## Next Focus

- extend the block layer to handle multiple devices / namespaces / queue pairs
- bring up the per-core data zone at `0xFFFF_E000_0000_0000` when SMP work begins
- evolve storage ABI from v0 (raw `buffer_phys`, sentinel device cap, Submit+Poll) toward v1 (capability-backed device + DMA, EventSlot/park variant)
