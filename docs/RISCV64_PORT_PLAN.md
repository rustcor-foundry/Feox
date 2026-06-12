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

## Toward apps (Route B — capability-based U-mode, hand-rolled)

Goal: a network OS on the RV2, as isolated U-mode capability apps over the ASI.
Ladder: M10 heap ✅ -> M11 VM abstraction ✅ -> M12 U-mode execution ✅ ->
M13 ASI syscall dispatch + capability table ✅ -> M14 threads + preemptive
scheduler ✅ -> M15 ELF loader + per-process address spaces ✅ -> M16 libOS +
app delivery (toolchain-built user crates through the M15 loader) -> M17 first
U-mode app; then NIC-as-capability + hand-rolled TCP in the network-service
app, then the RV2 hardware tail.

## Other follow-ups

- Coalesce freed heap regions + reclaim alignment padding in `heap.rs`.
- **PLIC** (external interrupts) — claim/complete, route a device IRQ
  (virtio-net/UART) so RX/completions are interrupt-driven instead of polled.
- Integrate `feox-nvme` (enable the `storage` feature) to replace the
  hand-rolled NVMe queue logic.
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
