# riscv64 Port Plan

Status: **Milestone 7 complete** — Feox boots in S-mode under QEMU `virt`,
prints its banner over the SBI console, installs a supervisor trap vector,
recovers from a deliberate `ebreak`, brings up sv39 paging, parses the device
tree for the real RAM map, stands up a physical frame allocator, builds a
fine-grained kernel address space with per-section W^X permissions, drives the
portable `feox-async` executor, enumerates PCIe over ECAM, brings the NVMe
controller to ready and verifies real block I/O, brings up the secondary harts
via the SBI HSM extension, and parks all harts.

## Boot model (how riscv64 differs from x86_64)

x86_64 boots through the UEFI loader (`feox-loader-uefi`), which builds a
`BootInfo` handoff and jumps to `feox_entry(boot_info)` → `boot::bootstrap`.

riscv64 has **no UEFI loader**. The firmware stack is OpenSBI → (U-Boot on real
hardware) → S-mode kernel. The kernel is entered directly:

- QEMU: `qemu-system-riscv64 -machine virt -bios default -kernel <elf>` —
  QEMU's built-in OpenSBI enters the kernel at its link address in S-mode.
- Real Orange Pi RV: build a flat `Image`, deploy via the RVBOOT / U-Boot
  `extlinux` recipe (same path openSUSE uses).

In both cases the boot hart enters with `a0 = hartid`, `a1 = dtb` (flattened
device-tree physical address). `_start` (in `kernel/feox-xokernel/src/main.rs`)
sets up the boot stack, zeroes the frame pointer, and tail-calls
`feox_entry(hartid, dtb)` → `arch::riscv64::riscv_main`.

## What carries over unchanged

The arch-agnostic core is reused as-is: `feox-async` (executor), `feox-nvme`
(MMIO+DMA queues), `feox-asi`, `feox-boot` (ABI types), the capability system,
and the arch-boundary contract in `arch/mod.rs`. RISC-V slots in as
`arch/riscv64/`; no restructuring of the kernel facade was required.

## Build & run

```
cargo kernel-riscv                       # build (riscv64gc-unknown-none-elf, --no-default-features)
tools\run-qemu-riscv.ps1                 # build + boot under local QEMU virt (needs qemu-system-riscv64)
tools\run-qemu-riscv.ps1 -TimeoutSeconds 20   # bounded smoke run, checks for the banner marker
tools\run-qemu-riscv.ps1 -Remote lx-ws01      # build here, ship the ELF, boot on a RustyKey host
```

On Paul's setup the build host (Windows) has the Rust toolchain and repo while
the boot host (lx-ws01, Debian) has `qemu-system-riscv64`. `-Remote <host>`
builds locally, `rusk cp`s the self-contained ELF to the host, and runs QEMU
there (time-bounded, since the boot hart parks in `wfi`), then checks the
serial log for the banner marker. No Rust or repo checkout is needed remotely.

The riscv64 milestone-1 build uses `--no-default-features` so the still
x86-shaped `runtime`/`storage` crates are not pulled into the minimal
SBI-console bring-up. The x86-coupled top-level modules (`acpi`, `lapic`,
`boot`, `paging`, `memory`, `smp`, `pci`, `vm`, `block`, `mmio`, `per_core`,
`runtime_context`, `capability`) are gated to `target_arch = "x86_64"` in
`lib.rs` until their riscv64 backends land.

## Roadmap (incremental, each builds on the last)

1. **Banner boot** ✅ — S-mode `_start`, SBI `console_putchar`, `wfi` halt.
2. **Trap handling** ✅ — direct-mode `stvec` -> `trap_entry` (saves a 31-GPR
   `TrapFrame` + `sepc`/`sstatus`/`scause`/`stval`), Rust `trap_dispatch`
   decodes `scause`, handles the breakpoint exception (advancing `sepc` past
   the trapping instruction, RVC-length-aware) and resumes via `sret`;
   everything else panics. Proven by a deliberate `ebreak` in the boot path.
   Timer/external interrupts and the `ecall` syscall path build on this.
3. **sv39 paging** ✅ — single root table of 1 GiB leaf gigapages
   identity-mapping the low 4 GiB (device space RW, RAM RWX); `satp` switched to
   sv39 with an `sfence.vma`, execution continues translated. `flush_tlb_all` /
   `flush_tlb_page` provide the `sfence.vma` primitives behind a future
   `arch::invalidate_page`. Next: a multi-level walker + frame allocator + per
   section permissions arrive with the memory pass.
4. **Memory** — device-tree intake (parse `a1` DTB for RAM regions) replacing
   the x86 ACPI/RSDP + loader memory map; un-gate `memory`.
   - **4a** ✅ — hand-rolled FDT reader (`fdt.rs`) finds the `/memory` region;
     a bump + free-list physical frame allocator (`frame.rs`) manages the RAM
     between the kernel image and the DTB. Proven by an alloc/free self-check.
   - **4b** ✅ — multi-level sv39 walker (`map_one`/`map_region`, 4 KiB + 2 MiB
     superpages, intermediate tables from the frame allocator) builds a
     fine-grained kernel address space with per-section W^X (text R-X, rodata
     R--, data/bss RW-), plus frame pool RW, DTB R, and the UART page;
     `satp` is switched to it and `translate()` verifies the walk/permissions.
     Identity (VA==PA) is preserved.
   - **4c** (next) — high-half/physmap or begin un-gating the shared `memory`
     module for riscv64 (it carries x86 assumptions), and honor the FDT
     memory-reservation block in the allocator.
5. **Runtime** ✅ — `feox-async` (arch-agnostic, no-alloc, static-storage
   executor) builds for riscv64 with the `runtime` feature on; `runtime.rs`
   spawns two self-waking `Yield` tasks on a `SingleCoreExecutor` and runs them
   to completion, proving the executor schedules futures on riscv64.
6. **NVMe** — QEMU `virt` exposes an NVMe device (`-device nvme`); exercise it.
   - **6a** ✅ — memory-mapped ECAM config access (`pci.rs`); enumerate the root
     bus, find the NVMe controller (class `0x010802`), read vendor/device/class
     and the raw BAR0. The ECAM window is mapped R/W in the kernel address
     space. First non-SBI device MMIO on riscv64. (ECAM base hardcoded for QEMU
     virt; DTB-derived on real hardware.)
   - **6b** ✅ — assign the 64-bit BAR from the PCIe MMIO window (`nvme.rs` +
     `pci::size_bar64`/`set_bar64`/`enable_memory_and_bus_master`), map the
     window, read CAP/VS, run the reset/enable handshake (clear CC.EN -> wait
     CSTS.RDY=0 -> program AQA/ASQ/ACQ admin queues -> set CC.EN -> wait
     CSTS.RDY=1). Self-contained MMIO; no `feox-nvme` yet.
   - **6c** ✅ — admin Identify Controller round-trip: build a 64-byte SQE, ring
     the SQ doorbell, the controller DMAs the 4 KiB identify structure, poll the
     CQ phase bit, check status, advance + ring the CQ doorbell, and read back
     the model/serial strings. Proves the full command/completion/DMA cycle
     (doorbells at `BAR + 0x1000`, stride `4 << DSTRD`).
   - **6d** ✅ — a `Queue` abstraction (submit + phase-poll) shared by admin and
     I/O; Identify Namespace (LBA size + capacity), Create I/O CQ/SQ (queue id
     1), then a write-then-read-back-and-compare of LBA 0 proves real block I/O.
     Still hand-rolled MMIO/DMA — `feox-nvme` integration is a later cleanup.

7. **SMP** ✅ — secondary harts via the SBI HSM extension (`smp.rs`):
   `sbi_hart_start` enters each AP at `_ap_start` (S-mode, paging off), which
   loads a per-hart stack + the shared kernel `satp` from a boot block, turns
   paging on, installs the trap vector, and reports online. Serialized bring-up
   with an atomic online counter. Replaces the x86 AP trampoline. (CI runs QEMU
   `-smp 4`.)

8. **Networking** ✅ (QEMU) — virtio-net now (CI), JH7110 `dwmac` on real
   hardware, both behind the `net::NetDevice` trait so the stack is
   device-agnostic. ARP + IPv4/ICMP ping + UDP/DHCP lease all verified.
   - **8a** ✅ — virtio-net over virtio-mmio (`net.rs`): probe the transport,
     negotiate features (VERSION_1 + NET_MAC), set up split RX/TX virtqueues,
     and prove the link with an ARP round-trip (who-has the slirp gateway ->
     receive its MAC). Gated to QEMU; CI runs `-netdev user -device
     virtio-net-device`.
   - **8b** ✅ — minimal IPv4 + ICMP: build an IP/ICMP echo request (with
     Internet checksums) and ping the slirp gateway, matching the echo reply.
   - **8c** ✅ — UDP + DHCP: a DISCOVER/OFFER/REQUEST/ACK exchange (UDP/BOOTP)
     obtains a lease from slirp's DHCP server; `build_dhcp` + option parsing.
   - **8-hw** — JH7110 `dwmac` GMAC driver behind `NetDevice` (board-only; not
     QEMU-modellable).

## Real-hardware readiness (Orange Pi RV / JH7110)

Portability cleanups done (QEMU stays green; these no-op safely off-QEMU):

- Bootstrap identity map is all-R-W-X, so the kernel runs wherever it is loaded
  (JH7110 RAM is at `0x4000_0000`, not QEMU's `0x8000_0000`).
- The fixed QEMU device windows (UART/ECAM/PCIe-MMIO) and the PCIe/NVMe probe
  are gated behind a device-tree machine check (`fdt::machine_is_qemu()`),
  since the PCIe MMIO window even overlaps RAM on the JH7110.

Still required before it can boot on the RV:

- **Link/load address** — a JH7110 linker base (RAM `0x4000_0000`); we build
  static/non-PIE, so link address must equal U-Boot's load address.
- **Flat Image + RISC-V Image header** so U-Boot `booti` accepts it (we emit an
  ELF today, which only QEMU `-kernel` takes).
- **Deploy**: the RV boots openSUSE from the NVMe/Optane, so the target is to
  place the Image + DTB on the NVMe boot partition with an extlinux entry.
- StarFive PCIe controller bring-up (clocks/resets/PHY) before NVMe works on the
  RV — a device-tree-derived driver, not ECAM poking.

9. **Timer interrupts** ✅ — supervisor timer via SBI `set_timer` (`time.rs`):
   `enable()` arms the deadline and sets `sie.STIE` + `sstatus.SIE`;
   `trap_dispatch` handles the timer interrupt (cause 5), counting + re-arming;
   the timebase comes from the DTB `/cpus/timebase-frequency`. Proven by taking
   a few ticks. First interrupt source handled — foundation for preemption and
   interrupt-driven I/O.

10. **Kernel heap** ✅ — a hand-rolled first-fit linked-list allocator
    (`heap.rs`) over an 8 MiB region carved from the frame pool, wired as the
    `#[global_allocator]` (spinlock-guarded). `alloc` (Box/Vec/collections) now
    works on riscv64. Limitation: no free-coalescing / front-padding reclaim yet
    (documented follow-up). First rung of the Route-B exokernel build-out.

11. **VM abstraction** ✅ — `paging::AddressSpace` (`new`/`map`/`unmap`/
    `translate`/`activate`/`destroy`): build/teardown arbitrary sv39 spaces and
    walk them without activating. `translate` is now root-parameterized; PTE
    R/W/X flags are public. Proven by a scratch space (map non-identity VAs ->
    translate -> unmap -> destroy). Foundation for per-process U-mode spaces.

12. **U-mode execution** ✅ — `umode.rs`: `enter_user` saves a setjmp-style
    kernel context, sets `sstatus` (SPP=0 to return to U, SUM=1 so the trap path
    can use the user stack) and `sret`s to a user code+stack page (mapped `U`+R+X
    / `U`+R+W at an unused 4 GiB VA); the user's `ecall` traps and the dispatcher
    longjmps back via `resume_kernel`. Proves the S↔U round trip; the longjmp is
    the context-switch primitive for the scheduler (M14). (Uses SUM for now; a
    per-thread `sscratch` kernel-stack swap arrives with multiple processes.)
    M13 generalized this into `umode::run_user_program` (run raw instruction
    words until `ProcExit`, returning the exit value).

13. **ASI syscall dispatch + capability table** ✅ — the shared `capability.rs`
    is un-gated (its `MemoryRegionKind`/`PAGE_SIZE` imports now come from the
    portable `feox-boot` via `bootabi`), and `syscall.rs` adds the riscv64 ecall
    lane: `a7` = `AsiOp`, `a0`/`a1` = args pointer/length in, result code /
    value out; the trap dispatcher routes ecall-from-U into it (`ProcExit` tears
    down the excursion via `umode::exit_to_kernel`). Serves the capability ops
    (`CapRequest`/`CapRelease`/`CapDelegate`/`CapList`) + `ProcYield`; transport
    result codes moved into `feox-asi` so both arch lanes share one ABI. Proven
    by a kernel-side `cap_request` self-test plus a U-mode program that calls
    `CapList` over the real ABI and exits with the reported total (CI asserts it
    matches the kernel's count). Mem/storage lanes and dispatcher unification
    with x86_64 are follow-ups.

14. **Threads + preemptive scheduler** ✅ — `sched.rs`: a fixed TCB table of
    U-mode threads, context-switched at trap level (a thread's whole register
    state is a `TrapFrame`; a switch swaps the live frame, and the stub's
    restore path resumes whatever context the frame describes). Preemption is
    the M9 timer (100 Hz slices); `ProcYield`/`ProcExit` are thread-lifecycle
    events under the scheduler; entry/exit reuses the M12 longjmp. Prerequisite
    landed with it: the `sscratch` kernel trap-stack swap (traps from U-mode now
    run on a dedicated stack, with `sscratch` = trap-stack top in U / 0 in S and
    re-armed by destination mode on restore), plus a latent stub bug fixed (`t0`
    was clobbered before being saved, corrupting it in every interrupted
    context). Proven by 2 yielders (exit 100/101) + 1 spinner preempted until a
    12-tick budget stops the run; CI asserts the stats line. Threads still share
    the kernel address space — per-process isolated spaces arrive with the ELF
    loader.

15. **ELF loader + per-process address spaces** ✅ — `elf.rs`: a hand-rolled
    static ELF64 loader (validate header, copy `PT_LOAD` segments into fresh
    frames with per-segment U+R/W/X permissions, zero-filled `memsz > filesz`
    tails). Process spaces come from `AddressSpace::new_user()` — a root that
    clones the kernel's top-level entries (so the trap path works under any
    process satp) — with process VAs in a top-level slot the kernel never
    touches (0x2_0000_0000, slot 8), so each space grows a private table tree;
    `destroy_user()` frees exactly that. The scheduler is satp-aware: each
    thread carries its satp, switches write it when it changes, and `run()`
    restores the caller's space. Proven by loading ONE synthesized image into
    TWO processes, poking a different `.data` value into each (same VA,
    different frame), and scheduling both: distinct exit values (200/201) +
    a pre-run translate() comparison prove isolation. ET_EXEC only; M16 feeds
    toolchain-built executables through this same path.

16. **libOS + app delivery** ✅ — `apps/feox-libos` (the ASI ecall ABI as Rust
    functions over the shared `feox-asi` opcodes: `syscall`/`yield_now`/
    `cap_count`/`exit`, plus the panic handler) and `apps/feox-hello`, a real
    `no_std` Rust binary linked with its own user-space linker script (image at
    0x2_0000_0000, `code-model=medium`, 4 KiB-aligned segments). Delivery: the
    kernel's build.rs cross-builds the app (nested cargo into OUT_DIR; rustflags
    passed by env because env *replaces* config rustflags while config files
    *merge* — the kernel linker script must not leak into app links) and the
    kernel embeds it via `include_bytes!(env!(...))`. The app yields, queries
    the capability table, exercises rodata + bss, and exits with
    `fib(10) + 1 + cap_count` — predicted independently by the kernel, so a
    correct exit proves build, delivery, load, per-segment permissions,
    syscalls, and exit end to end. Apps stay outside the workspace (own
    `[workspace]` tables); build.rs only fires for riscv64 kernel builds.

17. **First real U-mode app** ✅ — the mem lane lands on riscv64:
    `MemMap`/`MemUnmap`/`MemVtoP` in the ecall dispatcher, operating on the
    *calling process's* address space (the trap leaves `satp` untouched, so
    `AddressSpace::from_active()` is the caller's space; mappings extend its
    private slot-8 subtree and die with `destroy_user`). VAs come from a
    kernel-chosen monotonic mmap window (0x2_1000_0000+); flags are validated
    (READ required, no EXEC/cache hints), capability permissions enforced via
    `cap_to_phys_base` (READ + WRITE when mapping writable), bounds and page
    alignment checked, with `feox-asi` `MemError` codes. `feox-libos` grows
    `cap_request_pages`/`cap_release`/`mem_map`/`mem_unmap`/`mem_vtop`, and
    `feox-hello` becomes the proof: cap_request 2 pages -> mem_map -> fill/sum
    i^2 across a reschedule -> mem_vtop -> mem_unmap -> cap_release -> verify
    the capability count is back to its starting value -> exit with
    `sum % 65521` (kernel predicts it via the closed form; failed steps exit
    distinct 0xbNN codes). The app's ELF now carries all three segment
    classes (R+X text, R rodata, R+W bss).

18. **IPC events** ✅ — futex-shaped `ThreadPark` over `feox-asi`'s
    `EventSlot`s in shared memory: the caller names a slot VA + the counter
    value it observed; the kernel returns immediately if the counter already
    moved (no lost wakeups), else blocks the thread, polling the slot's
    *physical* address (translated at park time) from the tick-driven wake
    scan, with optional timeout. The scheduler gains a Blocked state and an
    S-mode idle loop (entered when all live threads are parked; sret with
    SPP=1/SPIE=1 keeps ticks flowing, and the wake scan switches a thread
    back in — the idle frame is simply discarded). `spawn_at`/`enter_user`
    now deliver an `a0` argv0. Proven by `apps/feox-pingpong`: one image, two
    processes (roles via argv0), a kernel-provided shared R+W page at the
    same VA in both (the inverse of the M15 isolation proof) holding two
    EventSlots + a mailbox; producer and consumer ping-pong 4 payloads
    through real block/wake cycles and exit with sum-derived values the
    kernel predicts. `ThreadParkArgs` added to `feox-asi` (the x86_64 lane
    still reports ThreadPark unsupported).

19. **External interrupts (PLIC) + the IRQ lane** ✅ — `plic.rs` routes
    device interrupts to the boot hart's S-mode context (priority/enable/
    threshold + claim/complete, `sie.SEIE`; stale pendings from the polled
    bring-up are drained at init by claim -> device ack -> complete). The
    virtio-net device stays live after the M8 selftest; its ISR is acked on
    interrupt, with an RX used-index shadow distinguishing real RX progress
    from TX completions (virtio-mmio's ISR doesn't say which queue fired).
    `IrqAttach`/`IrqDetach` land in the ecall lane (`IrqAttachArgs` in
    `feox-asi`, source `IRQ_SOURCE_NET_RX`): the kernel signals the attached
    EventSlot (PA, translated at attach time) once per RX event, flushing
    pre-attach events so the attach race cannot lose a wakeup. The scheduler
    gains `on_event`: external interrupts wake parked threads immediately and
    leave the idle loop without waiting for a tick. Proven by pingpong role 2:
    attach a bss EventSlot to net RX, park; the kernel sends one ICMP echo
    pre-run and the reply's interrupt wakes the app (exit 0xACE). QEMU-only
    (fixed PLIC base + virtio irq mapping; DT-derived bases with the
    hardware tail).

20. **User-space NIC lanes** ✅ — the net device becomes a `CapType::NetDevice`
    capability (minted at ecall-lane init; apps discover it via `CapList`),
    driven through three new ASI ops: `NetSubmitTx` (transmit a frame from
    capability-backed memory), `NetPollRx` (receive into capability memory;
    value = length, 0 = none), `NetGetInfo` (MAC + MTU). `feox-asi` carries
    the arg structs + `NetError` codes; the kernel verifies device-cap type +
    permissions and bounds-checks buffer capabilities via `cap_to_phys_base`.
    Proven by `apps/feox-netapp` role 0: a COMPLETE ARP round trip from
    U-mode — the app builds the who-has itself in its mapped packet buffer,
    transmits, parks on the RX interrupt (observed-before-poll ordering, so
    frames can't be lost between poll and park), and parses the reply,
    exiting with a gateway-MAC checksum the kernel predicts from its own M8
    resolution.

21. **User-space TCP** ✅ — hand-rolled TCP client in `feox-netapp` role 1,
    entirely in U-mode over the M20 lanes: every Ethernet/IPv4/TCP frame is
    built and parsed by the app (RFC 1071 checksums incl. the TCP
    pseudo-header). Full lifecycle against the CI echo peer (QEMU
    `guestfwd=tcp:10.0.2.100:7777-cmd:cat`): ARP the gateway, three-way
    handshake (SYN -> SYN-ACK validation -> ACK), 8 bytes sent PSH+ACK,
    echo collected across arbitrary segmentation with cumulative ACKs (and
    duplicate/out-of-order re-ACK), active close (FIN -> FIN-ack or peer
    FIN + final ACK). Exits `0x4000 | (sum of echoed bytes & 0xFFF)`,
    predicted by the kernel. No retransmission (park timeouts surface
    failures as distinct 0xbNN exits) — flow/congestion control arrive with
    the real network-service app.

## Toward apps (Route B — capability-based U-mode, hand-rolled)

Goal: a network OS on the RV2, as isolated U-mode capability apps over the ASI.
Ladder: M10 heap ✅ -> M11 VM abstraction ✅ -> M12 U-mode execution ✅ ->
M13 ASI syscall dispatch + capability table ✅ -> M14 threads + preemptive
scheduler ✅ -> M15 ELF loader + per-process address spaces ✅ -> M16 libOS +
app delivery ✅ -> M17 first real U-mode app (mem lane over capabilities) ✅
-> M18 IPC events (EventSlot + ThreadPark) ✅ -> M19 external interrupts
(PLIC + IrqAttach -> EventSlot) ✅ -> M20 user-space NIC lanes (NetDevice
capability + NetSubmitTx/NetPollRx/NetGetInfo) ✅ -> M21 hand-rolled TCP in
user space ✅. The Route-B ladder is complete on QEMU; next: grow the
network-service app (retransmission, multiple connections, a real service)
and the RV2 hardware tail (DT-derived PLIC/net, JH7110 dwmac).

## The RFS arc (filesystem)

[RFS](https://github.com/rustcor-foundry/RFS) — the CoW filesystem (txg/ZIL/
snapshots) whose first target is Feox via an `rfs-feox` `BlockDevice` adapter
(see RFS `docs/FEOX-INTEGRATION.md`).

22. **feox-nvme data path** ✅ — the crate gains `QueueRing<N>`: submit takes
    a real `SubmissionQueueEntry`, assigns a ring-owned CID, writes the SQE,
    rings the (dstrd-aware) tail doorbell; `process_completions` drains
    phase-valid CQEs into the inflight map, resolving `NvmeIoFuture`s. Plus
    `nvm_write`/`nvm_flush`/`identify_namespace` builders, CQE status decode
    (SCT/SC/DNR), and `parse_identify_namespace` geometry. Host-tested end to
    end against a fake register bank (SQE/CID/doorbell asserted; hand-crafted
    CQE resolves the future). The riscv64 NVMe driver is re-based onto the
    crate (the `storage` feature is now on for riscv64), so the existing
    M6b-6d CI markers validate the crate's ring path against real QEMU NVMe —
    now including an NVM Flush barrier in the self-test. This closes RFS's
    gap list items (1)-(3) + (5); next: the `rfs-feox` adapter in the RFS
    repo, then mount an RFS volume on the CI NVMe disk.

23. **An RFS volume on the NVMe disk** ✅ — the cross-repo payoff. RFS M5
    shipped `rfs-feox` (`NvmeBlockDevice`: `BlockDevice` over `QueueRing`
    with a self-waking pump future + a page-aligned PRP1 bounce; always
    presents 4 KiB blocks by aggregating LBAs). The kernel pulls `rfs-core`
    (+`testkit` for the poll-loop `block_on`) and `rfs-feox` as git deps
    behind the new `rfs` feature, with a `[patch]` redirecting their
    `feox-nvme` git dep to the local path crate so the ring types unify.
    `rfs.rs`: format a filesystem on the QEMU NVMe namespace (16 MiB -> 4096
    4 KiB blocks, 1 MiB segments), create `/feox.txt`, write through the CoW
    tree, `sync` (txg commit), DROP the handle — then remount via
    `Filesystem::open` on a SECOND I/O queue pair (a fresh device handle
    sharing only the media) and read the payload back. CI asserts the
    marker. The riscv64 NVMe driver grew `create_extra_io_ring`/
    `into_io_ring`/`geometry` for this. Follow-ups: zero-copy registered
    buffers; expose the volume to U-mode via a storage service.

## The hardware tail (Orange Pi RV / RV2)

24. **Hardware boot enablement + packaging** ✅ — the kernel boots real
    boards through the RISC-V Linux boot protocol: a 64-byte boot image
    header (so U-Boot `booti` loads the objcopy'd flat Image and enters with
    a0=hartid/a1=dtb), bss self-zeroing in `_start` (ELF loaders zero bss;
    `booti` does not), per-board linker scripts (`linker-riscv64-jh7110.ld`
    @ 0x4020_0000 for the Orange Pi RV, `linker-riscv64-ky-x1.ld` @
    0x0020_0000 for the Orange Pi RV2) built via `tools/build-board.*` (env
    RUSTFLAGS — config rustflags would merge), SMP gated to QEMU (JH7110
    hart 0 is the MMU-less S7; DT-driven hart selection is the follow-up),
    and the frame pool clamped below the 4 GiB bootstrap identity map (RV2
    boards carry 8 GB). CI's `package` job ships `feox-boot-images`: both
    flat Images + the QEMU ELF + SHA256SUMS + `HARDWARE_BOOT.md` (boot
    procedure, bdinfo verification, expected output, troubleshooting). On
    hardware, the QEMU-window demos (PCIe/NVMe/net/PLIC/RFS) skip cleanly;
    everything else (traps through ELF apps and IPC) runs unchanged.

    Next on hardware: DT-driven SMP (honor `mmu-type`), native UART console,
    DT-derived PLIC, then the board NICs (JH7110 dwmac / Ky X1 ethernet) and
    storage behind the existing capability lanes — at which point the full
    QEMU ladder (TCP, RFS) runs on silicon.

25. **DT-driven console + SMP** ✅ — the two biggest first-boot risks
    retired. `fdt.rs` grew a parent-cells-aware walker: `uart()` finds the
    first `ns16550*`/`snps,dw-apb-uart` node (`reg` decoded with the parent's
    `#address-cells`; `reg-shift`/`reg-io-width` defaulted for QEMU, set for
    the JH7110/Ky X1) and `cpu_harts()` collects hart ids of cpu nodes that
    carry `mmu-type` and are not disabled. `uart.rs`: a write-only 16550
    driver that uses the UART exactly as U-Boot left it (poll LSR.THRE,
    write THR; no clock/baud programming) with a bounded poll that falls
    back to SBI if the UART is dead — so a wrong base can't hang the kernel.
    The console upgrades from SBI to native after the DT parse (the board
    UART page is mapped into the kernel space; QEMU's is already mapped),
    making CI prove the native driver on every run. SMP is re-based onto
    the DT hart list — QEMU and hardware now share one path, and the
    JH7110's MMU-less S7 hart is structurally unstartable. CI asserts the
    console marker; the SMP marker now passes via the DT path.

26. **DT-derived PLIC + interrupt numbers** ✅ — the last hardcoded QEMU
    interrupt assumption removed. `fdt.rs` gains `plic()` (compatible
    containing "plic": sifive,plic-1.0.0 / riscv,plic0; reg decoded with the
    parent's address AND size cells) and `interrupt_at(unit_base)` (the
    `interrupts` cell of the node whose reg base matches a directly-probed
    device — how the virtio-net transport's PLIC source is found without
    slot math). The PLIC driver binds to a runtime base; its window is
    mapped at discovery time instead of in the static QEMU device map. The
    net IRQ is DT-first with the slot computation as fallback. QEMU and the
    boards (JH7110: PLIC @ 0xc000000; Ky X1: elsewhere — the reason this
    can't be a constant) now share one interrupt bring-up path, proven by
    the existing M19 interrupt-wake demo + a new DT-PLIC CI marker.

27. **U-mode storage lane** ✅ — `StorageSubmitRead`/`StoragePoll` (the
    feox-asi storage ops, previously x86_64-only) land in the riscv64 ecall
    dispatcher over a dedicated NVMe queue pair (qid 3), gated by a
    `CapType::StorageDevice` capability minted at lane init (apps find it
    via CapList). Submit verifies device + buffer capabilities and bounds,
    issues `nvm_read` on the lane's `QueueRing`, and returns a token; poll
    drains completions and resolves the (single, for now) in-flight future
    into a `StorageCompletion`. libOS grows `storage_submit_read`/
    `storage_poll`; netapp role 2 is the proof: find the cap, map a buffer
    page, read LBA 0, poll-with-yields to completion, and exit with a
    checksum of the first four bytes — the M6 self-test's "FEOX" stamp
    (the demo runs before RFS reformats the disk). One in-flight op and
    read-only for now; multi-token tables and writes come with the storage
    service.

## Other follow-ups

- Coalesce freed heap regions + reclaim alignment padding in `heap.rs`.
- PLIC landed in M19 (net RX routed to an EventSlot); the kernel's own M8
  selftest exchanges still poll — move them (and NVMe completions) onto the
  interrupt path.
- Un-gate the shared `memory` module for riscv64 (`capability` un-gated in M13).
- Unify the x86_64 SYSCALL and riscv64 ecall dispatchers over one portable core
  (today the riscv64 lane mirrors the cap-op subset; mem/storage are x86-only).

## QA-identified hardening (deferred — none fire on current QEMU paths)

A review pass (all milestones) found no live bugs beyond the small fixes already
applied (sv39 `translate` PPN mask; net diagnostics/`&buf[..n]` cleanup;
frame-allocator invariant doc). These remain for when the relevant paths grow:

- **Frame allocator + SMP:** `FRAME_ALLOCATOR` is a `static mut` accessed only by
  the bootstrap hart. Add a spinlock before any secondary hart allocates.
- **`frame::free` robustness:** no double-free / multi-frame guard; freeing a
  frame from an `alloc_contiguous` block would underflow `in_use`. Add a
  `free_contiguous` / validation when freeing becomes common.
- **NVMe queue depth:** clamp admin/IO depths to `CAP.MQES` for real
  controllers (QEMU's max far exceeds our 64/8).
- **Traps for U-mode:** `trap_entry` carves the frame on the current `sp` and
  `instruction_len_at` reads the trapping PC; add an `sscratch` kernel-stack
  swap and care for faulting reads before user/page-fault traps land.
- **SMP probe:** distinguish `sbi_hart_start` error codes instead of treating any
  nonzero as "no more harts"; free the AP stack/boot-block on a failed start.

## Interrupt-controller note

x86 LAPIC/IOAPIC → riscv64 **PLIC** (external interrupts) + **CLINT/aclint**
(timer/IPI, or the SBI timer + IPI extensions). These come in with the
trap-handling and SMP passes.
