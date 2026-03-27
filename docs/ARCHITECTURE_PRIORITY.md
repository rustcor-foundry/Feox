# Feox Architecture Priority

Last updated: 2026-03-27

This document answers a practical question:

Which architecture should Feox prioritize first if the goal is the strongest
performance-oriented lane with the least architectural drift?

## Decision

Feox should prioritize `x86_64` first.

ARM64 should remain an intentional second lane, not the first performance lane.

## Why

There is no universal "ARM vs x86" winner in the abstract. Performance depends
on the workload, the specific silicon, the memory and I/O complex, and how much
of the system Feox can actually exploit.

For Feox specifically, the first priority should be the architecture that gives
the best combination of:

- fastest path to a real high-performance bootstrap
- strongest direct-I/O ecosystem alignment
- most immediately usable exokernel-adjacent research base
- least architectural churn from the current repo

On those criteria, `x86_64` wins clearly.

## Research Read

### 1. The exokernel and direct-hardware research baseline is still heavily x86-centered

The most relevant performance-oriented references Feox is already drawing from
are strongly aligned with x86 server-class assumptions:

- Dune provides direct but safe access to privileged CPU features and was
  implemented for 64-bit x86 Linux:
  https://www.usenix.org/conference/osdi12/dune-safe-user-level-access-privileged-cpu-features
- Arrakis explicitly frames the OS as a control plane around direct hardware and
  virtualized I/O:
  https://www.usenix.org/conference/osdi14/technical-sessions/presentation/peter
- Corey is directly about making many-core sharing explicit:
  https://www.usenix.org/event/osdi08/tech/full_papers/boyd-wickizer/boyd_wickizer.pdf

This matters because Feox is not just choosing an ISA. It is choosing the first
lane where its exokernel ideas can be proven against the strongest nearby body
of systems work.

### 2. x86_64 matches Feox's current hardware and software assumptions

Today Feox already assumes:

- x86_64 bootstrap
- x86_64 page-table structure
- PCIe-first direct device thinking
- MSI-X language
- Intel VT-d flavored IOMMU language
- QEMU `x86_64` + OVMF tooling

That means the shortest path to real performance is not to switch architectures.
It is to finish the lane that already matches the code and the current design
documents.

### 3. ARM64 is compelling, but for a different reason

ARM's current server direction is real and serious:

- Arm says Neoverse V2 is designed for cloud, HPC, and ML performance
  leadership:
  https://www.arm.com/products/silicon-ip-cpu/neoverse/neoverse-v2
- Arm's reference designs show scalable PCIe, MSI routing through ITS, and
  SMMU-based I/O translation on large infrastructure systems:
  https://documentation-service.arm.com/static/641a84c08df5201251bf37e0

So ARM64 is not "slow." It is a strong long-term lane, especially for:

- core density
- throughput per watt
- future platform breadth

But it is still the second Feox lane because it requires much more bootstrap
and spec cleanup before Feox can exploit that hardware seriously.

## What "Performance" Means For Feox

For Feox, early performance should mean:

- minimal kernel mediation on the fast path
- direct device queue ownership
- strong per-core locality
- predictable interrupt and completion delivery
- explicit memory and DMA ownership
- fast path validation on real or emulated server-style hardware

That is a stronger fit for the current `x86_64` lane because Feox can get there
sooner and validate more of the design with less architecture-port work.

## Architecture Comparison For Feox

### x86_64 first-lane advantages

- current code already targets it
- current docs and ASI design language are largely written around it
- strongest overlap with Dune, Arrakis, and adjacent direct-I/O research
- mature PC-server development path with PCIe, MSI-X, and IOMMU expectations
- shortest path to proving the exokernel data path rather than just the boot path

### x86_64 first-lane drawbacks

- less future-facing than ARM64 on power efficiency and some scale-out deployments
- easier to accidentally inherit old PC assumptions too deeply if Feox is not careful

### ARM64 second-lane advantages

- strong long-term server and infrastructure relevance
- excellent core density and efficiency potential
- good future fit for a disciplined capability-first system
- modern interrupt and I/O translation blocks are serious, not toy features

### ARM64 second-lane drawbacks

- much larger current port cost
- current Feox docs and code are not architecture-neutral enough yet
- first deliverable would mostly be a bootstrap port, not a performance proof
- immediate direct-I/O parity with the x86-oriented design docs would take longer

## Recommendation

Feox should define the architecture priority like this:

1. `x86_64`
   This is the first performance lane and the first architecture where Feox
   should prove:
   - boot
   - handoff
   - memory ownership
   - page-table growth
   - capability boundaries
   - direct device path

2. `ARM64`
   This is the second lane and should be brought up after the x86 path is real
   enough that Feox is porting an architecture, not porting unresolved concepts.

## What This Means In Practice

The next architecture order should be:

1. finish first `x86_64` QEMU boot
2. harden the x86 memory and capability path
3. prove one direct-I/O performance-oriented subsystem on x86
4. then back-port the cleaned architecture boundaries into ARM64

That gives Feox the highest chance of reaching meaningful performance sooner.

## Bottom Line

If the question is "which architecture is more important to Feox's first real
performance story," the answer is `x86_64`.

If the question is "which second architecture should Feox invest in once the
first lane is real," the answer is `ARM64`.
