# riscv64 Port Plan

Status: **Milestone 6d complete** — Feox boots in S-mode under QEMU `virt`,
prints its banner over the SBI console, installs a supervisor trap vector,
recovers from a deliberate `ebreak`, brings up sv39 paging, parses the device
tree for the real RAM map, stands up a physical frame allocator, builds a
fine-grained kernel address space with per-section W^X permissions, drives the
portable `feox-async` executor, enumerates PCIe over ECAM, brings the NVMe
controller to ready, identifies it and its namespace, creates an I/O queue
pair, and verifies real block I/O (writes a pattern to LBA 0 and reads it back),
then parks the boot hart.

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

7. **SMP** — secondary harts via the SBI HSM extension (`hart_start`),
   replacing the x86 AP trampoline. **(next)**

## Interrupt-controller note

x86 LAPIC/IOAPIC → riscv64 **PLIC** (external interrupts) + **CLINT/aclint**
(timer/IPI, or the SBI timer + IPI extensions). These come in with the
trap-handling and SMP passes.
