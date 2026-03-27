# Feox Architecture Checklist

Last updated: 2026-03-27

This document turns the research framework and current Feox design docs into a
practical review checklist for implementation work.

Use it before introducing a new subsystem and during review of any major kernel,
loader, or runtime design change.

Primary companion docs:

- [Exokernel Research Framework](EXOKERNEL_RESEARCH_FRAMEWORK.md)
- [ASI Spec](../ASI-SPEC.md)
- [Capability System](../CAPABILITY-SYSTEM.md)
- [NVMe Driver](../NVME-DRIVER.md)

## Global Design Gate

Every major design should answer these first:

1. Is this kernel mechanism or higher-level policy?
2. Does this need to exist in the kernel right now?
3. What authority does the caller need?
4. What is core-local and what is intentionally shared?
5. What does revocation or teardown look like?
6. What does the fast path cost once setup is complete?
7. What does the bring-up and debug story look like on serial output?

If those answers are vague, the design is probably too early or too broad.

## Memory Checklist

Use this for boot memory, page-table work, physical allocators, and future
mapping APIs.

### Scope and placement

- Does the kernel only own protection, mapping, accounting, and revocation?
- Are richer allocation policies kept out of the kernel unless they are truly required?
- Is early boot still allocator-free where it matters?

### Ownership and authority

- Is ownership of physical memory explicit?
- Is every memory grant backed by a clear capability or equivalent authority token?
- Can the authority chain for mapping, DMA, or delegation be explained simply?

### Structure and locality

- Is memory state organized to preserve per-core locality where possible?
- Are shared global structures justified, bounded, and contention-aware?
- Are hot-path structures fixed-size or otherwise predictable?

### Revocation and teardown

- Is it clear how mappings are removed or invalidated?
- Can stale handles or stale mappings fail fast?
- Is there a deterministic path for reclaiming memory after revocation?

### Debug and verification

- Can the boot and mapping state be inspected over serial without a debugger?
- Are invariants documented for alignment, region typing, and page ownership?
- Is there a minimal test or proof path before layering more virtual-memory complexity on top?

## Capability Checklist

Use this for capability handles, delegation, process authority, and syscall
boundaries.

### Authority model

- Does each capability represent concrete authority, not a vague reference?
- Are rights narrow enough to explain in one sentence?
- Does the design avoid ambient authority?

### Delegation model

- Is delegation monotonic, with rights only narrowing?
- Is parent-child authority clear?
- Is cascading revocation or an equivalent containment model defined up front?

### Verification path

- Is capability verification constant-time or predictably cheap on the common path?
- Are generation, epoch, or stale-handle checks explicit?
- Does the design avoid dynamic lookup or heap-heavy metadata on the hot path?

### Scope and layering

- Is the kernel enforcing authority while leaving policy to higher layers?
- Does the syscall/API surface stay small and typed?
- Are convenience wrappers above the kernel clearly separated from the trusted base?

### Failure handling

- Are failure modes typed and explicit?
- Does revocation fail closed rather than limping through partial authority?
- Can the system explain why access was denied, stale, or revoked?

## NVMe And Device-I/O Checklist

Use this for NVMe first, then for any future direct device queue work.

### Queue ownership

- Is queue ownership core-local by default?
- Are command IDs, inflight state, and completions tied to one queue owner?
- Is any sharing across cores explicitly justified and measured?

### Fast-path discipline

- Is setup-time kernel work separated from the I/O path?
- Does the hot path avoid syscalls unless the core is idle or parking?
- Are MMIO, DMA, and completion paths direct and legible?

### DMA and mapping

- Are DMA buffers pinned, bounded, and associated with explicit device authority?
- Is the IOMMU model mandatory and automatic where needed?
- Is drain-before-unmap preserved for teardown?

### Completion and wake model

- Is completion delivery minimal and composable with the async model?
- Are wakeups core-local unless there is a strong reason otherwise?
- Are spurious wakes, fail-all paths, and late completions handled explicitly?

### Error model

- Does the design distinguish queue-full, device-fault, stale-command, and revocation paths?
- Can failure be contained without poisoning unrelated queues?
- Is there a clear controller-reset or recovery boundary?

## Interrupt And Event Delivery Checklist

Use this for MSI-X, interrupt routing, event slots, IPIs, and wake paths.

### Routing model

- Is interrupt ownership explicit and capability-guarded?
- Is the target core chosen deliberately?
- Does the interrupt route align with queue and executor locality?

### Event delivery

- Is the kernel's interrupt-side work as small as possible?
- Does the event mechanism compose cleanly with async polling?
- Is there a defined difference between notification and actual completion processing?

### Cross-core cost

- Are cross-core wakeups rare and justified?
- Is any use of IPIs bounded and visible in the design?
- Can the normal completion path stay on one core?

### Recovery

- What happens to parked or waiting threads if the device is removed or revoked?
- Can all waiters be failed deterministically?
- Is event state still coherent after reset or teardown?

## Process And Runtime Checklist

Use this for early process creation, thread models, scheduler hooks, and any
future user-mode runtime layers.

### Kernel boundary

- Does the kernel provide only the minimum process/thread mechanisms needed?
- Are scheduling and affinity semantics explicit?
- Are higher-level runtime policies staying above the kernel when possible?

### Locality and execution

- Is per-core execution preserved by default?
- Are thread migration and shared scheduling decisions deliberate instead of automatic?
- Are runtime objects marked or designed to prevent accidental cross-core movement when needed?

### Startup and composition

- Can a process acquire exactly the capabilities it needs at startup?
- Is bootstrap state understandable from the loader into the first runtime?
- Are batch setup operations preferred over many tiny kernel crossings?

## Bring-Up Discipline Checklist

Use this whenever a tempting new subsystem appears before the current base is
proven.

- Does this come after a verified boot milestone, not before it?
- Do we already have serial visibility for the state this subsystem depends on?
- Have the current prerequisites been proven in QEMU before growing the design?
- Would shipping this now improve the bootstrap path or distract from it?

If the answer to the last question is "distract," defer it.

## Recommended Near-Term Review Order

For the current Feox checkpoint, the best order is:

1. loader-to-kernel handoff proof under QEMU
2. boot memory and page-table review with the memory checklist
3. capability-boundary review against `ASI-SPEC.md` and `CAPABILITY-SYSTEM.md`
4. NVMe ownership and event-delivery review against `NVME-DRIVER.md`
5. only then broader runtime and process-surface growth

## Definition Of A Good Feox Design

A good Feox design:

- exports mechanisms low in the stack
- keeps authority explicit
- preserves locality
- keeps kernel trust narrow
- has deterministic teardown
- improves bring-up instead of distracting from it

If a proposal does not improve at least one of those without weakening the
others, it probably needs another pass.
