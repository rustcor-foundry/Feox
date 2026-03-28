# Feox Code Review

Last updated: 2026-03-27

This document is a cross-referenced review of all current Feox source files
against the research framework (`docs/EXOKERNEL_RESEARCH_FRAMEWORK.md`), the
architecture checklist (`docs/ARCHITECTURE_CHECKLIST.md`), and the published
design specs (`ASYNC-RUNTIME.md`, `CAPABILITY-SYSTEM.md`, `NVME-DRIVER.md`,
`ASI-SPEC.md`).

Every finding is classified by severity:

- **C** — Correctness: the code is wrong or has defined UB under reachable conditions
- **S** — Safety: the code is technically sound today but the abstraction does not
  prevent a future caller from producing UB
- **P** — Performance: the code does extra work that will matter at runtime
- **A** — Architecture gap: an interface or mechanism promised by the design does
  not yet exist in source
- **R** — Research alignment: an aspect of the implementation diverges from or has
  not yet caught up to the exokernel and capability literature

Findings that are already covered by a clear "deferred" note in the
architecture checklist are marked **(deferred)** and not treated as bugs.

---

## 1. Correctness

### C-01 — `static mut` in `runtime_context.rs` has no single-writer enforcement

**File:** `kernel/feox-xokernel/src/runtime_context.rs`

All eight retained context slots are bare `static mut`:

```rust
static mut RUNTIME_SNAPSHOT: Option<RuntimeSnapshot> = None;
static mut BOOTSTRAP_CORE_CONTEXT: Option<BootstrapCoreContext> = None;
// ... six more
```

Every store and read uses a raw `unsafe` block with no spinlock, no
`OnceCell`, and no `CoreId` check. The code works today because the
bootstrap path is strictly single-core and all stores happen before the
service loop reads anything back. But there is no type-level enforcement of
that invariant.

**Risk.** If a second core comes up (even accidentally, via a stray SIPI),
writes to these slots produce a data race — which is undefined behavior in
Rust even on targets where the hardware would give a torn write.
`KernelConfig::max_cores: 1` is a runtime value, not a compile-time bound,
so the type system does not prevent multi-core use.

**Recommended fix.** Wrap each slot in a `UnsafeCell<Option<T>>` and
enforce single-writer access via an explicit compile-time or boot-time core
check in each store path. A lightweight pattern that fits the `no_std`
environment:

```rust
// Store only from core 0 during single-core bootstrap
fn store_runtime_snapshot(s: RuntimeSnapshot) {
    debug_assert!(current_core_id() == CoreId(0));
    unsafe { RUNTIME_SNAPSHOT = Some(s); }
}
```

This is not a fix that needs to wait for multi-core support — the
`debug_assert` encodes the invariant that is already expected.

---

### C-02 — No TLB invalidation in `map_4k_with` / `unmap_4k_with`

**File:** `kernel/feox-xokernel/src/paging.rs`

`map_4k_with` installs a PTE by writing directly to the page-table entry.
`unmap_4k_with` clears one. Neither calls `invlpg` on the target virtual
address after the write.

On x86_64, a PTE write takes effect for future page-table walks, but any
existing TLB entry for that virtual address is not invalidated until an
`invlpg` or CR3 reload. The bootstrap path today does a full CR3 switch
after all mappings are installed, which implicitly flushes the TLB. But
`map_4k_with` is a general primitive — any future call after CR3 is live
will silently use a stale TLB entry.

**Severity.** This is a correctness bug in the general case. The bootstrap
path happens to be safe today only because CR3 is written after all mapping
work is done.

**Recommended fix.** Add `invlpg` at the end of `map_4k_with` and
`unmap_4k_with`:

```rust
// After installing the PTE:
unsafe { core::arch::asm!("invlpg [{}]", in(reg) virt_addr, options(nostack)); }
```

Alternatively, make the caller responsible and document it explicitly at the
trait level. The former is safer since misuse by a caller is silent.

---

### C-03 — `dispatch_exception` calls `console::init()` inside an exception handler

**File:** `kernel/feox-xokernel/src/arch/x86_64/exceptions.rs`

The fatal exception path calls `console::init()` to reinitialize the serial
port before printing the exception dump. The intent is to guarantee serial
output even if the console was not yet initialized when the exception fired.

The risk is **recursive exception handling**. If `console::init()` faults
(e.g., a null-pointer dereference in console state, a GPF on the port I/O
access under a restrictive I/O permission bitmap), the CPU raises a second
exception. With IST=0 for all entries (see S-01), the second handler reuses
the same stack pointer. If that stack is already corrupted, the result is a
triple fault with no diagnostic output — the worst possible failure mode.

**Recommended fix.** Track console readiness with a `static AtomicBool`
initialized by `console::init()`. In the exception path, call `init()` only
if the flag is not set; if it is set, skip the re-init entirely. This
eliminates the re-entry window without changing behavior in the common path.

---

### C-04 — `BootstrapIdentityMappedPageTables` creates aliased mutable references

**File:** `kernel/feox-xokernel/src/paging.rs`

`BootstrapIdentityMappedPageTables::frame_mut` constructs a mutable slice
over a physical frame by treating the frame's physical address as a virtual
address (identity map assumption):

```rust
fn frame_mut(&mut self, addr: PhysicalAddress) -> &mut [PageTableEntry; 512] {
    unsafe {
        &mut *(ptr::with_exposed_provenance_mut::<[PageTableEntry; 512]>(
            addr.0 as usize
        ))
    }
}
```

If the caller holds two references to different levels of the same page-table
tree (e.g., a PML4 entry and a PDPT entry), and one of those frames happens
to be the same physical address (a degenerate but not impossible case during
bootstrap), both references alias the same memory. Rust's aliasing model
forbids two simultaneously live `&mut` references to overlapping memory.

The bootstrap allocator prevents this in practice because it never reuses
frames. But there is no type-level assertion of this and no runtime check.

**Recommended fix.** Add a debug assertion in `frame_mut` that the returned
address has not been returned before during the same traversal. This is
easily done with a small visited-set in the `BootstrapIdentityMappedPageTables`
struct (an array of at most 4 entries covers all levels of a 4-level walk).

---

### C-05 — `ensure_child_table` aliasing window in `paging.rs`

**File:** `kernel/feox-xokernel/src/paging.rs`

`map_4k_with` calls `ensure_child_table` to allocate or locate intermediate
page-table frames. The pattern reads the parent entry, installs a new frame
if absent, then calls `frame_mut` on the new frame to descend. There is a
window where the parent entry is read as a shared reference while the child
frame pointer is simultaneously derived and passed as a mutable reference.
If the source implementation of `PageTableFrameMutSource` returns overlapping
frame addresses from successive calls, both references are live at the same
time.

This is the same aliasing category as C-04 but in the intermediate-table
allocation path rather than the direct access path.

---

## 2. Safety (Abstraction Gaps)

### S-01 — All IDT entries use IST=0; NMI and double-fault have no dedicated stack

**File:** `kernel/feox-xokernel/src/arch/x86_64/idt.rs`

All 256 IDT entries are installed with `ist: 0` (the `IdtEntry` struct sets
IST=0 by default). IST=0 means the CPU uses the current RSP at the time the
exception fires, adjusted by the hardware frame push.

For most exceptions this is correct. For three vectors it is a known problem:

- **Vector 2 (NMI):** NMIs can fire at any time, including during another
  exception handler or with a completely invalid RSP. Without a dedicated IST
  stack, an NMI during a page fault with a bad stack causes a triple fault.
- **Vector 8 (Double Fault):** A double fault by definition fires when the
  primary exception handler itself faults. The RSP at that point is
  potentially the same corrupted stack that caused the first fault.
- **Machine Check (Vector 18):** Same category as NMI.

**Recommended fix.** Allocate two dedicated 4-KiB IST stacks (one for NMI,
one for double fault) during `early_init`. Set IST=1 for vector 2 and IST=2
for vector 8 in the TSS and IDT. This is a prerequisite before any real
interrupt handling work begins.

---

### S-02 — No execute-disable (NX) bit set in any page-table entry

**File:** `kernel/feox-xokernel/src/paging.rs`

`PageTableEntry` exposes a `HUGE_PAGE` flag constant and the address mask,
but no `NO_EXECUTE` constant (bit 63). All current mappings install pages
with NX=0, meaning every mapped page is executable.

For an exokernel that intends to expose controlled hardware access to
applications, an executable data stack or writable code page is a critical
attack surface. The Engler SOSP95 threat model explicitly assumes the
exokernel enforces protection even when it does not enforce management — and
NX enforcement is the minimum viable protection for the kernel's own stack
and data pages.

**Recommended fix.** Add `const NO_EXECUTE: u64 = 1 << 63;` to
`PageTableEntry`. Set NX on the stack window and data window at map time.
Verify that `EFER.NXE` is set during `early_init` (x86_64 requires this bit
to be set in the IA32_EFER MSR before NX takes effect).

---

### S-03 — CR4 security bits (SMEP, SMAP, UMIP) not set

**File:** `kernel/feox-xokernel/src/arch/x86_64/cpu.rs` (implicitly)

The current `early_init` path (via `arch::early_init`) does not set:

- **SMEP** (Supervisor Mode Execution Prevention, CR4.20): prevents the
  kernel from executing user-space pages.
- **SMAP** (Supervisor Mode Access Prevention, CR4.21): prevents the kernel
  from inadvertently reading user-space pages without explicit `stac`/`clac`
  bracketing.
- **UMIP** (User Mode Instruction Prevention, CR4.11): prevents user-space
  from reading `SGDT`, `SIDT`, `SMSW`, `SLDT`, `STR` — which otherwise leak
  kernel addresses.

These are not relevant until user-mode is live, but establishing them during
`early_init` is the right time — before any code path could accidentally
create a user-accessible mapping.

**Recommended fix.** Add a CR4 setup step to `early_init` that sets SMEP,
SMAP, and UMIP if the CPU supports them (checked via CPUID leaf 7). This is
a single MSR read + OR + write sequence.

---

### S-04 — `hlt_loop` masks NMIs

**File:** `kernel/feox-xokernel/src/arch/x86_64/cpu.rs`

```rust
pub fn hlt_loop() -> ! {
    loop {
        disable_interrupts(); // cli
        core::arch::asm!("hlt", options(nomem, nostack));
    }
}
```

The `cli` before `hlt` prevents maskable interrupts from waking the core,
which is the intent. But `cli` does **not** mask NMIs. On some implementations
(particularly virtualized environments), NMIs arrive as watchdog or
machine-check signals. A core spinning in `cli + hlt` will take the NMI with
a valid RSP (the halt loop stack frame), but with `IF=0` in RFLAGS at the
time of the `iretq` — which means the NMI handler's `iretq` will return into
the halt loop with interrupts still disabled, which is correct.

The real risk is the opposite: the current panic path calls `hlt_loop`
directly after printing, which means a panicked kernel will refuse to take
any maskable interrupt. This makes it impossible to trigger a soft reboot or
QEMU exit via IRQ in the test harness. For the bootstrap phase this is
acceptable, but it should be documented as a deliberate choice rather than
implicit behavior.

**Recommended fix (minimal):** Document the `cli` in a comment explaining
why it is intentional (prevent spurious wake, accept NMI). Add a note in the
panic path that `hlt_loop` does not accept maskable reboot signals.

---

### S-05 — `CoreId` width inconsistency between spec and implementation

**File:** `kernel/feox-xokernel/src/lib.rs`, `ASI-SPEC.md`

`ASI-SPEC.md` describes `CoreId` as a `u32` field in the thread creation
and affinity interfaces. In the kernel, `CoreId` is used as a newtype around
what appears to be a narrower unsigned integer (the bootstrap path initializes
it from a static `u16` cast). This is not a current bug since `max_cores: 1`
means only `CoreId(0)` is ever used, but the width mismatch will become a
real problem when the ASI syscall layer is wired to the kernel.

**Recommended fix.** Define `CoreId(pub u32)` explicitly in `feox-boot` or
a shared types crate and import it consistently across the kernel, async
crate, and future ASI shim. This is a one-line change now and a painful find-
and-replace later.

---

## 3. Performance

### P-01 — Production `switch_page_table_root_and_jump` contains debug-console markers

**File:** `kernel/feox-xokernel/src/arch/x86_64/cpu.rs`

The CR3 switch function contains approximately 12 inline `out 0x402, al`
instructions (debugcon port writes) directly in the production code path.
This was the correct debugging approach during the CR3 switch development
(the markers proved that each sub-step was reached). They should now be
conditioned on a `cfg(debug_assertions)` or `cfg(feature = "debugcon")`
gate.

Each port write is a serializing instruction that prevents instruction
reordering across the boundary. On a real x86 processor, serializing I/O
instructions add tens to hundreds of nanoseconds each. For a CR3 switch that
should be a handful of cycles, this is a significant overhead that is also
invisible in profiling output.

**Recommended fix.** Wrap all `out 0x402` calls in `#[cfg(debug_assertions)]`
or extract a `debugcon_write!(byte)` macro that compiles to nothing in
release. The actual port write should remain available for future boot
debugging without polluting the production path.

---

### P-02 — Intermediate page-table entries always installed with `WRITABLE`

**File:** `kernel/feox-xokernel/src/paging.rs`

`ensure_child_table` installs new intermediate table frames with `PRESENT |
WRITABLE`. This is correct for the bootstrap identity-mapped and transition
roots where the kernel must be able to modify all page-table levels.

However, once per-process address spaces are introduced, installing
intermediate entries with `WRITABLE` for user-space subtrees means a
privileged (ring 0) write to a virtual address in that subtree can modify
the page table itself if the PTE accidentally maps a page-table frame at that
address. This is not a current bug but is a design assumption that will need
to be revisited when the capability-controlled address-space model from
`CAPABILITY-SYSTEM.md` is implemented.

**Recommended fix (design note).** Document the current flag policy explicitly
in `paging.rs` and in `docs/PAGE_TABLE_PLAN.md`. Flag the `WRITABLE`
intermediate entry assumption as a bootstrap-only policy that will be tightened
when the permanent kernel layout is established.

---

## 4. Architecture Gaps

### A-01 — `feox-nvme` uses `extern crate alloc` and `Vec` — blocks kernel integration

**File:** `crates/feox-nvme/src/lib.rs`

`feox-nvme` imports `alloc::vec::Vec` for the `InflightMap` slot storage.
The kernel crate (`feox-xokernel`) is `no_std` with no global allocator. This
means `feox-nvme` cannot be used from the kernel today, even though the
kernel's `lib.rs` has a `cfg(feature = "nvme")` guard that would re-export it.

The `InflightMap` capacity is bounded at construction time (`new(queue_depth)`),
so the `Vec` is used only for its initial allocation, not for dynamic growth.
This is exactly the use case for a fixed-size array or a static pool.

**Recommended fix.** Parameterize `InflightMap<const N: usize>` with a
const-generic depth. Replace `Vec<InflightEntry>` with `[MaybeUninit<InflightEntry>; N]`
plus a `usize` length counter. This removes the alloc dependency entirely and
makes the crate usable from `no_std` without a global allocator. The caller
(`KernelConfig::nvme_queue_depth: 64`) provides the compile-time depth.

---

### A-02 — `feox-async` executor infrastructure does not exist

**File:** `crates/feox-async/src/lib.rs`, `ASYNC-RUNTIME.md`

`feox-async` has a correct and well-tested `TaskHeader` state machine
(`Ready`, `Polling`, `WakePending`, `Parked`, `Complete`) with proper
wake-during-poll handling via `WakePending`. What does not exist:

- **Executor**: the per-core run queue that drives `poll()` calls
- **Reactor**: the bridge between hardware completion events and task wakers
- **TimerWheel**: the sleep/timeout mechanism
- **Waker implementation**: `TaskHeader::wake()` exists but there is no
  concrete `Waker` that the async runtime and NVMe futures can hold

`ASYNC-RUNTIME.md` describes all four components in detail. The task state
machine is the hardest part and it is done correctly. The executor scaffold
is the next structural piece before any real I/O or async work can be built.

**Priority.** High. Without the executor, `feox-async` is a type definition
library. Every other async component (NVMe futures, ASI `thread_park`, per-
core event loops) depends on this.

---

### A-03 — ASI syscall entry path does not exist in kernel source

**File:** `ASI-SPEC.md` vs. kernel source

`ASI-SPEC.md` defines 18 syscall operations across 5 groups (Capability
Management, Memory Mapping, Interrupt Routing, Process/Thread Management,
Device Discovery). The kernel has no syscall entry path: no `SYSCALL`
handler, no MSR setup for `IA32_STAR`/`IA32_LSTAR`/`IA32_FMASK`, and no
dispatch table.

This is explicitly deferred in the architecture checklist, but noting it here
because the exokernel value proposition (Engler: "securely multiplexing
hardware resources") is not demonstrable until the application-facing
interface exists.

**(deferred)** — tracked in `docs/ARCHITECTURE_CHECKLIST.md`.

---

### A-04 — No capability table implementation in kernel source

**File:** `CAPABILITY-SYSTEM.md` vs. kernel source

`CAPABILITY-SYSTEM.md` specifies `CapabilityTable` (256-slot flat array),
`CapSlot` (64 bytes, cache-line aligned), `ResourceRegistry` (4096 entries),
and `DelegationTree`. None of these exist in kernel source. The paging layer
has no integration point for capability-checked page mappings.

**(deferred)** — tracked in `docs/ARCHITECTURE_CHECKLIST.md`.

---

### A-05 — No intermediate page-table frame ownership tracking

**File:** `kernel/feox-xokernel/src/memory.rs`, `kernel/feox-xokernel/src/paging.rs`

`EarlyKernelReservations` tracks five typed reservation categories
(`KernelImage`, `ActivePageTableRoot`, `LegacyLowMemory`, `BootstrapPageTables`,
`BootstrapPerCoreState`). When `ensure_child_table` allocates a new PDPT,
PD, or PT frame, it is recorded as `BootstrapPageTables`, but there is no
link between the intermediate frame and the parent PML4 entry that points to
it.

This means the reservation model cannot answer: "which frames make up the
transition page-table tree rooted at physical address X?" When the transition
root is eventually replaced by the permanent kernel root, there is no
mechanism to determine which frames should be returned to the allocator and
which should be retained.

**Recommended fix (design note).** Add a small frame-tree sidecar to the
reservation model: a fixed-capacity array of `(parent_phys, child_phys)`
pairs recorded by `BootstrapPagingAllocator::allocate_frame`. This does not
need to be a full tree structure — a flat list of 32 pairs covers all
intermediate frames reachable in a 4-level walk over a modest kernel image.

---

## 5. Research Framework Alignment

### R-01 — Strong alignment with Engler SOSP95 protection / management separation

The core paging layer (`paging.rs`) and reservation model (`memory.rs`)
correctly separate mechanism from policy:

- `map_4k_with`, `translate_with`, `unmap_4k_with` are pure mechanism —
  they take any `PageTableFrameSource` and make no policy decisions about
  which process can access which frame.
- `BootstrapPagingAllocator` is one policy implementation of the allocator
  trait, not the only possible one.

This is the right shape for an exokernel that will eventually expose physical
frame allocation directly to applications (Engler's "libOS" model). The trait
boundary is the correct exokernel protection boundary.

**Gap.** The protection boundary is not yet enforced. Any kernel code can
call `map_4k_with` with any frame address and any flags. The capability
system (A-04) is the missing piece that turns this mechanism layer into a
protected exokernel primitive.

---

### R-02 — Correct alignment with Corey explicit-sharing model

`feox-async`'s per-core affinity model (`CoreId` field in `TaskHeader`,
`!Send` on `NvmeIoFuture`) and `KernelConfig::max_cores: 1` both reflect the
Corey (OSDI08) insight that per-core data structures eliminate cross-core
coherence traffic. The `static mut` globals in `runtime_context.rs` are
written by one core by design.

The gap is that per-core structures are asserted by convention (single-core
bootstrap) rather than type-level enforcement (see C-01). When SMP is
introduced, the type-level enforcement must be in place before the first
second-core bring-up.

---

### R-03 — Partial alignment with Dune privileged-hardware-access model

Dune (OSDI12) grants user processes access to privileged hardware state
(page tables, exception delivery) by running them in ring 0 inside a VM
guest. Feox's exokernel direction is to expose physical frame allocation and
interrupt routing directly to library OS components running at ring 3, rather
than via a hypervisor layer.

The current paging trait design is consistent with this: the mechanism layer
does not enforce ring-level restrictions, only capability checks (once those
exist). The IDT/GDT setup is kernel-only today, which is correct for the
bootstrap phase.

**Gap.** The SYSCALL path (A-03) is required before any Dune-style direct
hardware exposure can be tested. The CR4 security bits (S-03) should be set
before any ring-3 code runs.

---

### R-04 — Early alignment with Arrakis control/data plane separation

Arrakis (OSDI14) separates the control plane (kernel-mediated resource
allocation) from the data plane (direct application access to device queues).
The `feox-nvme` design has the right shape for this: `NvmeQueuePair` is an
owned, per-core structure that could be handed to a library OS component as a
capability-protected data-plane handle.

**Gap.** `feox-nvme` cannot currently be linked into the kernel (A-01), the
NVMe register MMIO layer does not exist yet, and the capability check that
would gate queue ownership delegation has not been designed. The data-plane
shape is correct but the control-plane handoff mechanism is missing.

---

### R-05 — Exception handling model does not yet meet architecture checklist gate

From `docs/ARCHITECTURE_CHECKLIST.md`, the interrupt checklist requires:

> "Does the kernel have a clear IDT with explicit vector assignments, and are
> all vectors explicitly dispatched rather than falling through to a generic
> handler?"

The current `dispatch_exception` routes all 256 vectors through a single
function that logs the vector number. The vector-specific handlers
(`handle_breakpoint`, CR2 read for page fault) are present but the
architecture checklist gate — "are all vectors explicitly dispatched" — is
not yet met. Vectors 0–7, 9–12, 16–19 all hit the same generic fatal path
with no vector-specific recovery or reporting.

This is acceptable for bootstrap validation but should be called out as a gap
before the interrupt routing layer of the ASI spec is implemented.

---

## Summary Table

| ID | Severity | Area | Status |
|----|----------|------|--------|
| C-01 | Correctness | `runtime_context.rs` static mut no core enforcement | Fix before SMP |
| C-02 | Correctness | No TLB invalidation in `map_4k_with`/`unmap_4k_with` | Fix now |
| C-03 | Correctness | `console::init()` inside exception handler | Fix now |
| C-04 | Correctness | `BootstrapIdentityMappedPageTables` aliased mut refs | Fix before alloc reuse |
| C-05 | Correctness | `ensure_child_table` aliasing window | Fix before alloc reuse |
| S-01 | Safety | IST=0 for NMI/double-fault | Fix before real interrupts |
| S-02 | Safety | No NX bit on stack/data pages | Fix before user-mode |
| S-03 | Safety | SMEP/SMAP/UMIP not set in CR4 | Fix before user-mode |
| S-04 | Safety | `hlt_loop` NMI mask undocumented | Document now |
| S-05 | Safety | `CoreId` width mismatch vs spec | Fix before ASI wiring |
| P-01 | Performance | Debugcon markers in production CR3 path | Fix now |
| P-02 | Performance | Intermediate entries always `WRITABLE` | Document policy now |
| A-01 | Arch gap | `feox-nvme` alloc dependency | Fix before kernel integration |
| A-02 | Arch gap | Executor/Reactor/Waker do not exist | Next implementation lane |
| A-03 | Arch gap | ASI syscall entry path missing | Deferred |
| A-04 | Arch gap | Capability table missing | Deferred |
| A-05 | Arch gap | No intermediate frame ownership tracking | Fix before root handoff |
| R-01 | Research | Strong Engler alignment; capability enforcement missing | Track |
| R-02 | Research | Corey alignment correct; needs type enforcement for SMP | Track |
| R-03 | Research | Dune alignment partial; needs SYSCALL + CR4 | Track |
| R-04 | Research | Arrakis data-plane shape correct; control plane missing | Track |
| R-05 | Research | Exception dispatch not fully vector-specific | Fix before ASI interrupts |

---

## Immediate Action Priority

The three findings that should be addressed before any new implementation
work begins:

1. **C-02** — Add `invlpg` to `map_4k_with` and `unmap_4k_with`. One-line
   fix, prevents silent correctness failures on any future non-bootstrap
   mapping call.

2. **P-01** — Gate debugcon markers behind `cfg(debug_assertions)` in
   `switch_page_table_root_and_jump`. The markers served their purpose and
   should not appear in a profiled or timed boot.

3. **C-03** — Track console readiness with an `AtomicBool` so
   `dispatch_exception` does not re-enter `console::init()`. Eliminates the
   recursive-exception silent triple-fault risk.

The next structural piece after those three is **A-02** — the executor
scaffold in `feox-async`. Everything else in the async and device I/O stack
is blocked on it.
