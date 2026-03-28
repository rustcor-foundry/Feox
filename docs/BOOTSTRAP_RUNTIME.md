# Feox Bootstrap Runtime

Last updated: 2026-03-27

This document describes the retained bootstrap runtime: the stage model that
governs the CR3 handoff sequence, and the retained context layer that survives
into the post-handoff `runtime-active` service.

## Purpose

The bootstrap runtime serves two jobs:

1. **Handoff record.** The boot path produces a verified higher-half execution
   environment through a multi-stage transition. Each stage produces state that
   must survive into the next stage. Rather than scattering that state across
   boot-local variables that disappear after the transition, it is captured in
   a named, retained context that later services and diagnostics can read.

2. **Minimal runtime service.** After the handoff is complete and the exception
   path is validated, the kernel enters a tiny `runtime-active` service loop
   instead of halting immediately. That loop drives the first mutations of
   shared retained state, proves the command queue mechanism, and settles into
   idle cleanly. This is the seed of the broader long-lived runtime layout.

## Stage Model

The bootstrap runtime defines five stages. Each is written into the
`BootstrapRuntimeState` data page and mirrored into the retained
`BootstrapCoreContext`.

```
Prepared
  │  Transition page-table root built. Windows, stacks, and data page
  │  mapped. Identity-mapped jump target prepared. Root not yet active.
  │
  ▼
IdentityActive
  │  CR3 switch complete. Kernel is executing on the new root from the
  │  identity-mapped entry and stack. Higher-half aliases are present
  │  in the active root but not yet used.
  │
  ▼
AliasActive
  │  Stack has switched to the higher-half alias. Code is executing from
  │  the higher-half code alias. GDT and IDT have been reloaded from
  │  higher-half aliases. Data page is live at its higher-half alias.
  │
  ▼
ExceptionValidated
  │  A controlled `int3` breakpoint was raised and returned through
  │  `iretq`. The active IDT and exception stubs are confirmed working
  │  in the higher-half environment.
  │
  ▼
RuntimeActive
     The kernel has entered the retained runtime service. The bootstrap
     transition is complete. The service loop is running.
```

The stage label is encoded as a `u64` in the `BootstrapRuntimeState` data page
(which lives in a kernel-owned physical frame mapped at the
`BOOTSTRAP_DATA_WINDOW_BASE` higher-half alias). This lets the stage survive
the CR3 switch and stack change without depending on Rust statics, which are
not reliably accessible until the higher-half window is active.

## Retained Context Layer

After `AliasActive`, the runtime state is promoted from the boot-local data
page into a set of retained kernel statics in `runtime_context.rs`. These
survive for the lifetime of the bootstrap runtime service and are visible to
exception handlers, panic paths, and any future runtime layers.

The retained layer consists of the following records.

### RuntimeSnapshot

A read-only snapshot of the higher-half transition result. Captured once after
the stack switch and updated only when the stage label changes.

Fields:

- `active_root` — physical address of the active PML4 root after the CR3 switch
- `kernel_window_base` / `kernel_window_end` — bounds of the kernel image alias window
- `kernel_pages_mapped` — number of kernel image pages carried into the transition root
- `identity_stack` / `alias_stack` — identity and higher-half stack tops
- `stack_pages` — number of stack pages in the transition root
- `data_page` — identity-mapped address of the runtime data page
- `alias_entry` / `alias_gdt` / `alias_idt` — higher-half aliases for the code entry,
  GDT, and IDT
- `stage` — current stage label string

### BootstrapCoreContext

Records the active core's view at each stage transition.

Fields:

- `core_id` — bootstrap processor identifier
- `active_root` — CR3 value observed at this stage
- `stack_pointer` — RSP at this stage
- `alias_entry` — higher-half code alias address
- `stage` — stage label at the time of recording

### RuntimeServiceState

Tracks the current state of the post-handoff runtime service.

Fields:

- `owner_core` — core running the service
- `phase` — current service phase label
- `iterations` — number of completed command-loop iterations
- `last_action` — label of the last completed action

### RuntimeServiceReport

Derived accounting computed during the `RefreshAccounting` command.

Fields:

- `kernel_window_bytes` — size of the active kernel image alias window
- `stack_bytes` — retained bootstrap stack footprint
- `retained_events` — number of events currently in the event timeline

### RuntimeServiceHeartbeat

Updated on each `UpdateHeartbeat` command. Drives the retry decision.

Fields:

- `beats` — total number of heartbeat updates completed
- `last_iteration` — service iteration that produced this heartbeat
- `observed_events` — event count visible when the heartbeat ran

### RuntimeReadinessState

Published once the service loop settles (beats >= 2).

Fields:

- `ready` — true when the service has settled
- `published_iteration` — iteration at which readiness was published
- `settled_beats` — heartbeat count at publish time

### RuntimeReadySummary

A final summary snapshot published alongside readiness.

Fields:

- `active_root` — root at the time of the ready publish
- `kernel_pages_mapped` — page count at the time of the ready publish
- `retained_events` — event count at the time of the ready publish

### Event Timeline

A fixed-capacity (8-slot) rolling string buffer. Events are pushed at each
major milestone and read back as an ordered timeline by the service loop.

Current events pushed during a successful boot:

```
transition-handoff-entered
identity-handoff-active
higher-half-alias-active
descriptor-tables-reloaded
higher-half-exception-validation
higher-half-runtime-active
runtime-service-entered
runtime-service-poll
runtime-service-accounting
runtime-service-timeline
runtime-service-heartbeat
runtime-service-ready
```

## Command Queue

The runtime service is driven by a 7-slot FIFO command queue. The service loop
dequeues one command per iteration and each command decides what to enqueue
next. This makes the flow explicit and easy to extend without restructuring the
loop body.

Current command sequence for a nominal boot:

```
RefreshSnapshot
  └─ snapshot present → enqueue RefreshAccounting
     └─ events > 0 → enqueue ReportTimeline
        └─ report present → enqueue UpdateHeartbeat
           └─ beats < 2 → enqueue RefreshSnapshot  (retry cycle)
           └─ beats >= 2 → enqueue PublishReady
              └─ enqueue PublishReadySummary
                 └─ enqueue EnterIdle
```

The heartbeat gating (beats < 2 triggers one more RefreshSnapshot cycle)
ensures the service performs at least two full passes before declaring itself
ready. This is intentionally minimal and will grow as the retained runtime
layout gains more real work to do.

## Relationship To The Broader Runtime

The bootstrap runtime is not the final kernel runtime. It is the seed.

The current retained service is intentionally tiny: it proves the retained
state mechanism, the command queue, the heartbeat, and the idle path. The next
evolution is to grow the `runtime-active` slice into a broader long-lived
runtime layout with clearer ownership boundaries and real mutation work beyond
heartbeat updates.

The retained context layer in `runtime_context.rs` is designed to grow. New
service records follow the same pattern: a typed struct, a static `Option` slot,
a store function, and a read function. The command queue capacity (7) and event
timeline capacity (8) are conservative starting points and can be increased once
the retained layout stabilizes.

## Source Files

- `kernel/feox-xokernel/src/runtime_context.rs` — retained context types and
  store/read functions
- `kernel/feox-xokernel/src/boot.rs` — stage model, transition sequence, and
  runtime service loop
- `kernel/feox-xokernel/src/memory.rs` — `BootstrapRuntimeLayout` and the
  three window base addresses

## Design Gates

Before growing this layer further, each new addition should answer:

1. Is this state retained because something else needs to read it, or just
   for logging? Logging-only state should stay ephemeral.
2. Does this need to be a separate retained record, or does it extend an
   existing one?
3. Does adding this command make the service loop's next step clearer or
   more obscure?
4. Is there a serial-visible diagnostic for the new state?
