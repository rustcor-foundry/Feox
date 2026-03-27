# Feox Exokernel Research Framework

Last updated: 2026-03-27

This document captures the external research baseline Feox should use as a
discipline rail while the product is still early.

The goal is not to imitate one historical system wholesale. The goal is to use
the strongest ideas from exokernel, library-OS, capability, and many-core
systems work to keep Feox architecturally honest.

## Core Reading Set

### Foundational exokernel model

1. Engler, Kaashoek, O'Toole, "Exokernel: An Operating System Architecture for
   Application-Level Resource Management" (SOSP 1995)
   https://pdos.csail.mit.edu/6.828/2007/readings/engler95exokernel.pdf

Why it matters:

- this is the canonical statement of protection-vs-management separation
- it defines the low-level export model Feox should stay close to
- it introduces secure bindings, visible revocation, and abort protocols as
  real design tools rather than slogans

### Practical direct hardware access

2. Belay et al., "Dune: Safe User-level Access to Privileged CPU Features"
   (OSDI 2012)
   https://www.usenix.org/system/files/conference/osdi12/osdi12-final-117.pdf

Why it matters:

- shows how to expose privileged mechanisms safely without collapsing all
  protection boundaries
- useful as a modern reminder that Feox should expose mechanisms narrowly and
  deliberately, not just dump raw privilege everywhere

### Exokernel ideas for many-core scaling

3. Boyd-Wickizer et al., "Corey: An Operating System for Many Cores"
   (OSDI 2008)
   https://www.usenix.org/event/osdi08/tech/full_papers/boyd-wickizer/boyd_wickizer.pdf

Why it matters:

- Feox already leans per-core; Corey gives stronger justification for making
  sharing explicit instead of accidental
- it is especially relevant for kernel object scope, locality, and
  contention control

### Data plane vs control plane split

4. Peter et al., "Arrakis: The Operating System is the Control Plane"
   (OSDI 2014)
   https://www.dougwoos.com/papers/arrakis-osdi14.pdf

Why it matters:

- Feox should keep the distinction between global coordination and fast-path
  execution sharp
- Arrakis is one of the best modern references for direct device access without
  giving up system structure

### Modern library-OS framing

5. Schatzberg et al., "EbbRT: A Framework for Building Per-Application Library
   Operating Systems" (OSDI 2016)
   https://www.usenix.org/system/files/conference/osdi16/osdi16-schatzberg.pdf

Why it matters:

- useful for thinking about how much runtime, driver, or policy logic belongs
  in reusable libraries above a minimal kernel substrate
- keeps Feox from reinventing everything inside the kernel just because it can

### Capability-system lineage

6. CapROS / EROS lineage
   https://www.capros.org/
   https://repository.upenn.edu/handle/20.500.14332/7087

Why it matters:

- Feox already has capability direction in-repo
- capability systems answer a different question than exokernels, but the two
  fit well together if authority and resource ownership stay explicit

### Hardware capability direction

7. CHERI project overview
   https://www.cl.cam.ac.uk/research/security/ctsrd/cheri/

Why it matters:

- CHERI is not a requirement for Feox
- it is a strong reference for capability-aware hardware/software co-design and
  for thinking about long-horizon protection models without abandoning
  pragmatic bring-up

## Rust-Specific Reference Set

There is not a large mature field of "Rust exokernels" specifically. The best
Rust material today is adjacent work: kernels, unikernels, library OSes, and
OS-dev foundations that help answer how Feox should be built, even when their
architecture is not exokernel-first.

### Rust OSDev ecosystem

1. Rust OSDev organization
   https://github.com/rust-osdev

Why it matters:

- this is the strongest general ecosystem hub for Rust low-level tooling
- Feox already touches the same space through UEFI, x86_64, serial, and boot
  mechanics
- these crates are more relevant to Feox's immediate implementation discipline
  than most "toy kernel" repos

Most relevant current pieces for Feox:

- `uefi-rs`
- `x86_64`
- `uart_16550`
- `acpi`
- `ovmf-prebuilt`

### Hermit

2. Hermit kernel
   https://github.com/hermit-os/kernel

Why it matters:

- a serious Rust-based unikernel effort with real build discipline
- useful reference for low-level Rust ergonomics, architecture support, and
  kernel-facing crate organization

What to take:

- repo discipline
- build/test/runtime ergonomics
- crate and architecture partitioning

What not to copy blindly:

- Hermit is a unikernel, not an exokernel
- it optimizes for a different deployment model than Feox

### Tock

3. Tock OS
   https://github.com/tock/tock
   https://www.tockos.org/

Why it matters:

- one of the strongest real Rust OS codebases
- strong authority, isolation, and interface design thinking
- especially useful for syscall boundary discipline and capsule-style structure

What to take:

- interface discipline
- isolation thinking
- API shape around untrusted code and constrained authority

What not to copy blindly:

- Tock is an embedded OS, not a server-class exokernel
- MPU-centered isolation and microcontroller assumptions do not map directly to
  Feox

### Redox

4. Redox OS
   https://www.redox-os.org/
   https://github.com/redox-os/kernel

Why it matters:

- one of the longest-running Rust operating-system efforts
- useful reference for system decomposition, userspace drivers, and an
  opinionated kernel/userspace split

What to take:

- practical OS-in-Rust lessons
- boundary-setting around services and drivers
- long-horizon project discipline

What not to copy blindly:

- Redox is a full Unix-like microkernel OS
- Feox should not drift into "general-purpose Rust OS" thinking too early

### Asterinas

5. Asterinas / OSDK
   https://asterinas.github.io/

Why it matters:

- interesting modern Rust systems direction with a small privileged core and a
  larger safe-Rust surface
- particularly useful as a reference for minimizing unsafe regions and for
  reusable kernel-development infrastructure

What to take:

- the idea of confining unsafe code aggressively
- reusable kernel-development tooling
- explicit thought about trusted vs untrusted kernel regions

What not to copy blindly:

- Asterinas is not aiming for exokernel architecture
- its framekernel structure solves a different problem than Feox

### LiteBox

6. LiteBox
   https://github.com/microsoft/litebox

Why it matters:

- modern Rust library-OS work with clear "North" and "South" interface
  separation
- strong reference for keeping upper interfaces narrow and explicit

What to take:

- interface layering discipline
- library-OS thinking
- narrow host/platform boundary design

What not to copy blindly:

- LiteBox is a library OS and sandboxing system, not a bare-metal kernel
- its host-facing model is much higher in the stack than Feox's current stage

## Working Synthesis For Feox

Feox should not simply be:

- "MIT exokernel, but in Rust"
- "a capability OS with exokernel branding"
- "a hobby kernel with fewer abstractions"

The disciplined Feox synthesis is:

- a minimal kernel that protects and multiplexes hardware resources
- explicit authority and ownership modeled with capabilities
- per-core execution as a first-class design assumption
- fast-path mechanisms exported low in the stack
- higher-level policy, richer runtime behavior, and compatibility layers kept
  outside the minimal kernel where possible

Inferred from the Rust ecosystem above:

- Feox should borrow implementation discipline from Rust kernels
- borrow tooling and crate practices from Rust OSDev
- borrow interface narrowness from library-OS work
- but keep its own exokernel and capability identity instead of collapsing into
  "Rust microkernel" or "Rust unikernel" by accident

## Non-Negotiable Design Rules

These should guide design review going forward.

### 1. Protect first, manage second

If a feature requires the kernel to guess policy for every workload, Feox is
probably drifting too high.

Kernel responsibilities should stay close to:

- protection
- multiplexing
- handoff
- revocation
- capability enforcement
- interrupt, memory, and device mediation

### 2. Keep authority explicit

Capabilities should represent real authority, not vague handles. If an object
or service can act, map memory, submit I/O, or signal a completion, its
authority chain should be explainable.

### 3. Preserve per-core locality

Feox should assume that cross-core sharing is expensive and should usually be
opt-in. Core-local futures, queues, and ownership are a feature, not a
temporary limitation.

### 4. Split control plane from data plane

System-wide naming, admission, revocation, and policy can live in a more
coordinating layer. Data movement, device rings, and latency-sensitive paths
should stay as direct and narrow as possible.

### 5. Do not over-kernelize convenience code

Driver helpers, protocol stacks, file abstractions, and richer runtimes should
have to justify being in the kernel. The default answer should be "library or
user-mode component unless protection or latency absolutely requires kernel
placement."

### 6. Bring-up discipline beats speculative architecture

A clean boot path, serial visibility, memory-map integrity, and deterministic
handoff are more important than speculative subsystem breadth. Feox should earn
each new layer.

## Immediate Architectural Implications

Based on the current Feox state, these are the most justified near-term moves.

### Near-term yes

- prove loader-to-kernel handoff under QEMU and capture serial traces
- harden the boot memory model and page-table management
- keep async and NVMe ownership models core-local unless there is a strong
  reason not to
- define capability boundaries early for memory ownership, I/O submission, and
  interrupt/event delivery

### Near-term no

- do not rush into POSIX-like abstractions
- do not add a broad kernel object model before the authority model is tighter
- do not hide device queues behind high-level policy too early
- do not flatten per-core execution into globally shared scheduler semantics

## Feox Review Questions

For any major new subsystem, ask:

1. is this mechanism or policy?
2. does this belong in the kernel, a library OS, or a higher runtime layer?
3. what authority does the caller need?
4. what is core-local and what is intentionally shared?
5. how is revocation handled?
6. what does the fast path look like with no extra abstraction penalty?

If those questions are hard to answer, the design probably needs another pass.

## Recommended Next Research Pass

After the first QEMU boot is real, the next worthwhile focused study areas are:

1. page-table and memory-ownership models in exokernel and capability systems
2. user-level interrupt and completion delivery
3. direct device queue ownership for NVMe and networking
4. capability transfer and revocation semantics
5. many-core naming and object-scope design

## Notes

This framework is intentionally selective. It is a discipline rail for Feox,
not an exhaustive exokernel bibliography.
