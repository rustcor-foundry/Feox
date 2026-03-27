# Aether Async Runtime: Executor, Reactor, and Task Design

## Version 0.1.0 -- Draft

---

## Table of Contents

1. [Architecture Overview](#1-architecture-overview)
2. [The Executor](#2-the-executor)
3. [The Reactor](#3-the-reactor)
4. [The Task](#4-the-task)
5. [The Sleep/Wake Mechanism](#5-the-sleepwake-mechanism)
6. [Timer Support](#6-timer-support)
7. [Multi-Core Coordination](#7-multi-core-coordination)
8. [Backpressure and Overload](#8-backpressure-and-overload)
9. [Key Data Structures](#9-key-data-structures)
10. [The Complete Poll Loop](#10-the-complete-poll-loop)

---

## 1. Architecture Overview

### 1.1 Per-Core Executor Model

The Aether async runtime runs one executor instance per physical core. There is
no global scheduler, no shared run queue, and no cross-core task migration by
default. Each core owns its executor, its reactor, its run queue, and its set of
hardware completion sources.

This model exists because Aether is an exokernel: user-space drivers interact
directly with hardware queues (NVMe submission/completion queues, RDMA
send/receive queues), and those queues are pinned to specific cores via MSI-X
interrupt affinity (see `irq_attach` with `target_core` in the ASI spec). Moving
a task from one core to another would break the locality contract -- the task
would poll a completion queue whose interrupts arrive on a different core,
causing cross-core cache line bouncing on the EventSlot and the CQ memory
itself.

The architecture looks like this:

```
Core 0                    Core 1                    Core 2
+-------------------+     +-------------------+     +-------------------+
| Executor          |     | Executor          |     | Executor          |
|  +- RunQueue      |     |  +- RunQueue      |     |  +- RunQueue      |
|  +- Reactor       |     |  +- Reactor       |     |  +- Reactor       |
|  |   +- NVMe CQ0  |     |  |   +- NVMe CQ1  |     |  |   +- RDMA CQ0  |
|  |   +- EventSlot0|     |  |   +- EventSlot1 |     |  |   +- EventSlot2|
|  +- TimerWheel    |     |  +- TimerWheel     |     |  +- TimerWheel    |
|  +- StealQueue    |     |  +- StealQueue     |     |  +- StealQueue    |
+-------------------+     +-------------------+     +-------------------+
```

Each executor is fully self-contained. It polls its own completion sources,
wakes its own tasks, manages its own timers, and parks its own thread via the
ASI `thread_park` syscall. Cross-core interaction is limited to two narrow
channels: work-stealing (Section 7) and remote spawn (Section 7.3).

### 1.2 Why Not Tokio

Tokio is designed for Linux/macOS/Windows, relies on `std`, uses `epoll`/`kqueue`/`IOCP`
as its I/O backend, and assumes a POSIX-like operating system underneath. None
of these apply to Aether:

- **No operating system.** Aether is the operating system. There is no `epoll`
  file descriptor. Hardware completion queues are memory-mapped directly into the
  process address space.
- **No `std`.** The runtime must be `no_std`. It uses `core::task::Waker`,
  `core::pin::Pin`, and `core::future::Future` -- the language-level async
  primitives, not library-level ones.
- **No file descriptors.** I/O readiness is not signaled through file
  descriptors. It is signaled through EventSlots -- cache-line-aligned atomic
  counters that the kernel increments on hardware interrupts.
- **No epoll.** The reactor does not call into the kernel to check for I/O
  readiness. It reads EventSlot counters (a single atomic load) and, if changed,
  directly polls the hardware completion queue in user-space memory.
- **No cross-core migration.** Tokio's work-stealing scheduler freely moves
  tasks between worker threads. This is incompatible with hardware queue affinity.

The Aether runtime is approximately 1000 lines of Rust. Tokio is approximately
70,000. The difference reflects the difference in scope.

### 1.3 The Three Components

The runtime consists of three components:

| Component    | Responsibility                                                |
|:-------------|:--------------------------------------------------------------|
| **Executor** | Owns the run queue. Polls ready tasks. Parks the thread.      |
| **Reactor**  | Bridges hardware completion events to `Waker` invocations.    |
| **Task**     | A pinned, heap-allocated `Future` with its `Waker` and state. |

The Executor drives the outer loop. When it has ready tasks, it polls them. When
it has no ready tasks, it asks the Reactor to check for hardware completions.
When the Reactor also has nothing, the Executor parks the thread via
`thread_park`, surrendering the core to the kernel scheduler until a hardware
interrupt or timer expiry wakes it.

---

## 2. The Executor

### 2.1 Per-Core Run Queue

Each executor maintains a local run queue -- a `VecDeque<TaskRef>` of task
references that are in the `Ready` state and need to be polled. This is a
single-producer, single-consumer structure for the common case: the local
executor is the only entity that pushes (via waker callbacks) and pops (via the
poll loop).

The exception is work-stealing (Section 7), which introduces a second data
structure -- a Chase-Lev deque -- that allows remote cores to steal tasks from
the tail while the local core pops from the head.

```rust
pub struct Executor {
    /// The core this executor is pinned to.
    core_id: CoreId,

    /// Tasks ready to be polled. Local push/pop from the head.
    local_queue: VecDeque<TaskRef>,

    /// Work-stealing deque. Local push to tail, remote steal from tail.
    /// Only used when work-stealing is enabled.
    steal_queue: WorkStealQueue<TaskRef>,

    /// The reactor that bridges hardware events to wakers.
    reactor: Reactor,

    /// Timer wheel for delayed futures.
    timer_wheel: TimerWheel,

    /// Monotonic tick counter for the timer wheel, driven by TSC.
    current_tick: u64,

    /// Total number of tasks owned by this executor (all states).
    task_count: usize,

    /// Maximum tasks before backpressure is applied.
    task_limit: usize,
}
```

### 2.2 The Core Poll Loop

The executor's main loop follows this priority order:

1. **Drain the local run queue.** If there are ready tasks, poll them. This is
   the fast path. No syscalls, no atomic operations beyond the waker.
2. **Advance timers.** Read the TSC, advance the timer wheel, and move any
   expired timer futures into the ready queue.
3. **Poll the reactor.** Ask the reactor to check all registered
   CompletionSources. Any completed I/O operations will trigger wakers, which
   push tasks onto the ready queue.
4. **Try to steal.** If the local queue is still empty after reactor polling,
   attempt to steal tasks from other cores' steal queues.
5. **Park.** If all of the above yielded nothing, park the thread via
   `thread_park`. The timeout is set to the next timer expiry (or infinite if
   no timers are pending).

This ordering ensures that CPU-bound work (ready tasks) is always serviced
before checking for I/O, and that the thread only parks as a last resort. The
reactor poll in step 3 is cheap -- it is a single atomic load per EventSlot,
and only touches the hardware CQ if the EventSlot counter has changed.

### 2.3 Task Pinning

Tasks are spawned on a specific core and stay there for their entire lifetime.
The `spawn` function takes no core parameter -- it always spawns on the current
core's executor:

```rust
/// Spawn a future on the current core's executor.
/// The task will never migrate to another core unless explicitly stolen
/// via the work-stealing mechanism.
pub fn spawn<F>(future: F) -> JoinHandle<F::Output>
where
    F: Future + 'static,
    F::Output: 'static,
{
    let executor = current_executor();
    let task = Task::allocate(future, executor.core_id);
    let join_handle = task.join_handle();
    executor.local_queue.push_back(task.as_ref());
    join_handle
}
```

To spawn on a specific remote core, use `spawn_on` (Section 7.3).

### 2.4 Task States

Each task is in exactly one of five states:

| State        | Meaning                                                     |
|:-------------|:------------------------------------------------------------|
| **Ready**    | In the run queue, waiting to be polled.                     |
| **Polling**  | Currently being polled by the executor (transient state).   |
| **Parked**   | Not in any queue. Waiting for a waker invocation.           |
| **WakePending** | A wake arrived while the task was in `Polling`.          |
| **Complete** | The future returned `Poll::Ready`. Output is available.     |

State transitions:

```
spawn -> Ready -> Polling -> Parked (future returned Pending)
                          -> Complete (future returned Ready)
         Parked -> Ready (waker invoked)
        Polling -> WakePending (waker invoked during poll)
    WakePending -> Ready (poll epilogue requeues task)
```

The `Polling` state exists to prevent double-polling: if a waker fires while a
task is already being polled (e.g., a synchronous completion inside `poll`), the
wake is recorded as `WakePending`. When the poll returns `Pending`, the executor
requeues the task instead of parking it. This preserves the wake without
allowing recursive polling.

### 2.5 Optional Work-Stealing

Work-stealing is the ONLY cross-core synchronization in the runtime. It is
discussed in detail in Section 7. The key design decision is that stolen tasks
lose their core affinity -- they execute on the stealing core, which means they
should not be polling hardware CQs that are interrupt-pinned to their original
core. Work-stealing is therefore only appropriate for compute-bound tasks, not
I/O-bound tasks.

This tradeoff is acceptable because:

- Compute-bound tasks do not touch EventSlots or hardware CQs.
- If an I/O-bound task is stolen and later needs to poll a CQ, the reactor on
  the new core simply will not have that CQ registered, and the task will remain
  parked until its original core processes the completion and invokes the waker
  remotely.

---

## 3. The Reactor

### 3.1 Bridging Hardware to Futures

The reactor is the translation layer between hardware completion events and
Rust's `Waker` mechanism. When an NVMe command completes, the hardware writes a
completion queue entry (CQE) to a memory-mapped completion queue. The reactor
reads that CQE, extracts the command ID, looks up the corresponding `Waker` in
its WakerMap, and invokes it.

The reactor does not own any threads. It is called synchronously by the executor
as part of the poll loop.

```rust
pub struct Reactor {
    /// All registered hardware completion sources.
    sources: ArrayVec<CompletionSourceEntry, MAX_SOURCES>,

    /// Snapshot of EventSlot counters from the last poll.
    /// Used to detect whether new completions have arrived.
    last_seen: [u64; MAX_SOURCES],
}

struct CompletionSourceEntry {
    /// The hardware completion source (NVMe CQ, RDMA CQ, etc.).
    source: &'static dyn CompletionSource,

    /// The EventSlot associated with this source's MSI-X vector.
    event_slot: &'static EventSlot,

    /// Maps hardware completion IDs to wakers.
    waker_map: WakerMap,
}
```

### 3.2 The CompletionSource Trait

Every hardware completion queue implements this trait:

```rust
/// A hardware completion source that can be polled for completed operations.
///
/// Implementors include NVMe completion queues, RDMA completion queues,
/// network receive queues, and any other hardware queue that produces
/// completion events.
///
/// The `reap` method is called by the reactor when the associated EventSlot
/// indicates new completions are available. It must:
/// 1. Read completion entries from the hardware queue.
/// 2. For each entry, extract the completion ID (e.g., NVMe command ID).
/// 3. Look up the waker in the provided WakerMap and invoke it.
/// 4. Advance the hardware CQ head pointer.
/// 5. Return the number of completions reaped.
pub trait CompletionSource {
    /// Reap completed operations from the hardware queue.
    /// Returns the number of completions processed.
    fn reap(&self, waker_map: &WakerMap) -> u32;
}
```

An NVMe completion queue implementation would look like:

```rust
impl CompletionSource for NvmeCq {
    fn reap(&self, waker_map: &WakerMap) -> u32 {
        let mut count = 0u32;
        loop {
            let cqe = self.peek_next_cqe();
            if cqe.is_none() || cqe.unwrap().phase != self.expected_phase {
                break;
            }
            let cqe = cqe.unwrap();
            let cmd_id = cqe.command_id;

            // Store the completion status where the future can read it.
            self.completion_status[cmd_id as usize].store(cqe.status, Relaxed);

            // Wake the future that submitted this command.
            if let Some(waker) = waker_map.take(cmd_id as u64) {
                waker.wake();
            }

            self.advance_head();
            count += 1;
        }
        // Ring the CQ doorbell to tell the controller we consumed entries.
        if count > 0 {
            self.ring_doorbell();
        }
        count
    }
}
```

### 3.3 The WakerMap

The WakerMap is a fixed-size array that maps hardware-specific completion IDs to
`Waker` instances. The size is determined by the maximum number of outstanding
commands for the associated hardware queue (e.g., NVMe queue depth, typically
64-1024).

```rust
/// Maps hardware completion IDs (u64) to Wakers.
///
/// This is a fixed-size, pre-allocated structure. The size matches the
/// hardware queue depth. Slots are claimed when a command is submitted
/// and released when the completion is reaped.
///
/// Thread safety: accessed only by the local executor's reactor. No
/// synchronization required.
pub struct WakerMap {
    /// Slot array. `None` means the slot is free.
    slots: Box<[Option<core::task::Waker>]>,

    /// Number of occupied slots.
    active: usize,

    /// Maximum slots (matches hardware queue depth).
    capacity: usize,
}

impl WakerMap {
    /// Register a waker for a completion ID.
    /// Called when a command is submitted to the hardware queue.
    pub fn insert(&mut self, completion_id: u64, waker: core::task::Waker) {
        debug_assert!((completion_id as usize) < self.capacity);
        debug_assert!(self.slots[completion_id as usize].is_none());
        self.slots[completion_id as usize] = Some(waker);
        self.active += 1;
    }

    /// Remove and return the waker for a completion ID.
    /// Called by CompletionSource::reap when a command completes.
    pub fn take(&mut self, completion_id: u64) -> Option<core::task::Waker> {
        let slot = &mut self.slots[completion_id as usize];
        let waker = slot.take();
        if waker.is_some() {
            self.active -= 1;
        }
        waker
    }

    /// Returns true if there are outstanding wakers (pending completions).
    pub fn has_pending(&self) -> bool {
        self.active > 0
    }
}
```

### 3.4 EventSlot Integration

The key optimization in the reactor is avoiding unnecessary hardware CQ polls.
Polling a completion queue involves reading from memory-mapped device memory,
which is uncacheable (UC) or write-combining (WC) -- significantly slower than
normal DRAM reads.

The reactor avoids this by checking the EventSlot counter first. The kernel
atomically increments `event_slot.counter` every time the associated MSI-X
interrupt fires. If the counter has not changed since the last reactor poll,
there are no new completions, and the CQ poll is skipped entirely.

```rust
impl Reactor {
    /// Poll all registered completion sources.
    /// Returns the total number of completions reaped across all sources.
    pub fn poll(&mut self) -> u32 {
        let mut total = 0u32;

        for (i, entry) in self.sources.iter_mut().enumerate() {
            // Fast path: check if any new interrupts arrived.
            let current = entry.event_slot.counter.load(Ordering::Acquire);
            if current == self.last_seen[i] {
                // No new interrupts. Skip the expensive CQ poll.
                continue;
            }
            self.last_seen[i] = current;

            // New completions are available. Reap them.
            let reaped = entry.source.reap(&mut entry.waker_map);
            total += reaped;
        }

        total
    }

    /// Register a new completion source with its associated EventSlot.
    pub fn register(
        &mut self,
        source: &'static dyn CompletionSource,
        event_slot: &'static EventSlot,
        queue_depth: usize,
    ) -> SourceId {
        let id = SourceId(self.sources.len() as u32);
        self.sources.push(CompletionSourceEntry {
            source,
            event_slot,
            waker_map: WakerMap::new(queue_depth),
        });
        self.last_seen[id.0 as usize] = event_slot.counter.load(Ordering::Acquire);
        id
    }

    /// Get a reference to the WakerMap for a specific source.
    /// Used by I/O futures to register their waker when submitting commands.
    pub fn waker_map(&mut self, source: SourceId) -> &mut WakerMap {
        &mut self.sources[source.0 as usize].waker_map
    }

    /// Collect references to all EventSlots for thread_park.
    pub fn event_slots(&self) -> ArrayVec<&EventSlot, MAX_SOURCES> {
        self.sources.iter().map(|e| e.event_slot).collect()
    }

    /// Returns true if any source has outstanding completions pending.
    pub fn has_pending(&self) -> bool {
        self.sources.iter().any(|e| e.waker_map.has_pending())
    }
}
```

The `Ordering::Acquire` on the EventSlot load is critical. It synchronizes with
the kernel's `Ordering::Release` store (the atomic increment in the ISR). This
guarantees that when user-space sees the incremented counter, all memory writes
from the hardware completion (the CQE data written by the device via DMA) are
visible.

---

## 4. The Task

### 4.1 Structure

A task is a pinned, heap-allocated future combined with the bookkeeping state
the executor needs to manage it.

```rust
/// A task in the executor. Pinned, heap-allocated, non-movable.
///
/// Tasks are allocated from a slab allocator (Section 4.4) to avoid
/// per-task heap allocation overhead and fragmentation.
#[repr(C)]
pub struct Task<F: Future> {
    /// The current state of this task.
    state: AtomicU8,

    /// The core this task was spawned on. Used for remote wakeups.
    core_affinity: CoreId,

    /// The raw waker vtable and data pointer, pre-built at spawn time.
    /// Stored inline to avoid an indirection on every poll.
    waker_data: WakerData,

    /// The user's future. Pinned in place -- never moved after allocation.
    future: UnsafeCell<F>,

    /// Storage for the future's output value once it completes.
    output: UnsafeCell<Option<F::Output>>,
}

/// Waker bookkeeping, stored inline in the Task.
struct WakerData {
    /// Pointer back to the owning Task (for the waker vtable functions).
    task_ptr: *const (),

    /// Pointer to the executor's ready queue (for pushing on wake).
    ready_queue: *const VecDeque<TaskRef>,

    /// The vtable for constructing a core::task::Waker.
    vtable: &'static core::task::RawWakerVTable,
}

/// Type-erased reference to a task. Used in the run queue.
#[derive(Clone, Copy)]
pub struct TaskRef {
    /// Pointer to the task header (state + core_affinity + waker_data).
    ptr: *const TaskHeader,

    /// Function pointer to poll the type-erased future.
    poll_fn: unsafe fn(*const TaskHeader, &mut core::task::Context<'_>) -> Poll<()>,
}
```

### 4.2 Type Erasure

The run queue (`VecDeque<TaskRef>`) must hold tasks with different future types.
We achieve this through manual type erasure: `TaskRef` stores a raw pointer to
the task header and a function pointer to a monomorphized `poll` wrapper.

```rust
impl<F: Future> Task<F> {
    /// Create a TaskRef that erases the future type.
    fn as_ref(&self) -> TaskRef {
        TaskRef {
            ptr: self as *const Self as *const TaskHeader,
            poll_fn: Self::poll_erased,
        }
    }

    /// Type-erased poll function. Called through the TaskRef function pointer.
    unsafe fn poll_erased(
        header: *const TaskHeader,
        cx: &mut core::task::Context<'_>,
    ) -> Poll<()> {
        let task = &*(header as *const Self);
        let future = Pin::new_unchecked(&mut *task.future.get());
        match future.poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(output) => {
                *task.output.get() = Some(output);
                Poll::Ready(())
            }
        }
    }
}
```

### 4.3 The Waker

The waker is the mechanism by which a task re-enters the run queue.
When a future returns `Poll::Pending`, it stores the `Waker` from the
`Context`. Later, when the event the future is waiting for occurs (e.g., an
NVMe completion), the reactor calls `waker.wake()`, which:

1. Transitions `Parked -> Ready` and enqueues the task immediately, or
2. Transitions `Polling -> WakePending` and lets the poll epilogue requeue it.

If the waker is invoked from a different core (e.g., a remote completion
callback), it pushes onto the target core's MPSC remote queue instead (Section
7.3).

```rust
/// Waker vtable implementation.
///
/// Safety: The waker holds a raw pointer to the Task, which is pinned
/// and lives for the duration of the executor. The pointer is valid as
/// long as the task has not been deallocated (state != Complete + collected).
static WAKER_VTABLE: core::task::RawWakerVTable = core::task::RawWakerVTable::new(
    waker_clone,
    waker_wake,
    waker_wake_by_ref,
    waker_drop,
);

unsafe fn waker_clone(data: *const ()) -> core::task::RawWaker {
    // Cloning a waker just copies the pointer. No reference counting.
    // The task outlives all its wakers because the executor owns it.
    core::task::RawWaker::new(data, &WAKER_VTABLE)
}

unsafe fn waker_wake(data: *const ()) {
    waker_wake_by_ref(data);
    // No drop needed -- we don't reference count.
}

unsafe fn waker_wake_by_ref(data: *const ()) {
    let header = data as *const TaskHeader;
    let task_header = &*header;

    loop {
        match task_header.state.load(Ordering::Acquire) {
            x if x == TaskState::Parked as u8 => {
                if task_header.state.compare_exchange(
                    TaskState::Parked as u8,
                    TaskState::Ready as u8,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ).is_ok() {
                    enqueue_task(task_header, header);
                    break;
                }
            }
            x if x == TaskState::Polling as u8 => {
                if task_header.state.compare_exchange(
                    TaskState::Polling as u8,
                    TaskState::WakePending as u8,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ).is_ok() {
                    // Do not enqueue here. The polling core will see
                    // WakePending and requeue the task when poll
                    // returns Pending.
                    break;
                }
            }
            x if x == TaskState::Ready as u8
              || x == TaskState::WakePending as u8
              || x == TaskState::Complete as u8 => break,
            _ => unreachable!(),
        }
    }
}

unsafe fn waker_drop(_data: *const ()) {
    // No-op. Wakers do not own the task.
}
```

**Design decision: no reference counting.** Tokio uses `Arc` for tasks because
tasks can be dropped from any thread. In Aether, the executor owns all tasks on
its core. A task is only deallocated when the executor processes its `Complete`
state, there is no outstanding `WakePending` state, and the `JoinHandle` has
been dropped or consumed. This eliminates the cost of atomic reference counting
on every waker clone/drop, which matters when wakers are cloned into WakerMap
slots on every `poll` call.

### 4.4 Memory Layout and Allocation

Tasks are allocated from a per-core slab allocator. Each slab contains
fixed-size slots. Because futures in Aether are typically small (an NVMe read
future is ~128 bytes), we use a small set of size classes:

| Size Class | Slot Size | Typical Use                       |
|:-----------|:----------|:----------------------------------|
| Small      | 256 bytes | Simple I/O futures, timers        |
| Medium     | 512 bytes | Chained futures, select! branches |
| Large      | 1024 bytes| Complex state machines            |

```rust
/// Per-core slab allocator for task storage.
///
/// Each size class is a linked list of fixed-size slots carved from
/// page-aligned memory regions. Allocation and deallocation are O(1).
///
/// No locking required -- each core has its own allocator.
pub struct TaskAllocator {
    small: SlabList,   // 256-byte slots
    medium: SlabList,  // 512-byte slots
    large: SlabList,   // 1024-byte slots
}

struct SlabList {
    free_head: *mut SlabSlot,
    slot_size: usize,
    allocated: usize,
    capacity: usize,
}

#[repr(C)]
struct SlabSlot {
    /// When free: pointer to the next free slot.
    /// When allocated: task data begins here.
    next_free: *mut SlabSlot,
}
```

**Why a slab allocator instead of a general-purpose allocator?** Three reasons:

1. **Deterministic allocation time.** Slab allocation is O(1) -- pop from a free
   list. A general-purpose allocator has variable latency due to coalescing,
   splitting, and searching.
2. **No fragmentation.** Fixed-size slots cannot fragment. Over the lifetime of
   a long-running driver, fragmentation from variable-size allocations would
   degrade performance.
3. **Cache efficiency.** Task headers are densely packed. When the executor
   iterates the run queue and touches task headers, they are likely to be in the
   same cache lines.

---

## 5. The Sleep/Wake Mechanism

### 5.1 The Lifecycle

When the executor has exhausted all ready tasks, polled the reactor, attempted
work-stealing, and still has nothing to do, it enters the sleep path. This is
the mechanism that prevents busy-waiting and allows the core to halt (enter a
low-power state) until meaningful work arrives.

The complete sequence:

```
1. Executor drains local run queue           (all tasks polled)
2. Executor advances timer wheel             (no timers expired)
3. Reactor polls EventSlots                  (no counter changes)
4. Executor attempts work-stealing           (no stealable tasks)
5. Executor computes timeout:
   - If timers are pending: timeout = next_timer_expiry - now
   - If no timers: timeout = None (infinite)
6. Executor calls: thread_park(&event_slots, timeout)
7. Kernel parks the thread, halts the core   (HLT or MWAIT)
   --- time passes ---
8. Hardware interrupt fires on this core
9. Kernel ISR: increments EventSlot.counter
10. Kernel sees parked thread watching this EventSlot, sends IPI (or resumes directly)
11. thread_park returns ParkResult::Woken { slot_index }
12. Executor resumes -> reactor polls the indicated CQ
13. Wakers fire -> tasks enter Ready state
14. Executor polls them -> back to step 1
```

Alternatively, if no interrupt fires but the timeout expires:

```
8. Timer hardware fires (LAPIC timer or TSC deadline)
9. Kernel wakes the parked thread
10. thread_park returns ParkResult::TimedOut
11. Executor resumes -> advances timer wheel -> expired timers fire
12. Timer futures become Ready
13. Executor polls them -> back to step 1
```

### 5.2 The thread_park Contract

From the ASI spec, `thread_park` accepts:

- `slots: &[&EventSlot]` -- the set of EventSlots to watch. The kernel wakes
  the thread when ANY of them is incremented.
- `timeout: Option<Duration>` -- maximum time to sleep. `None` means sleep
  indefinitely.

Returns:

- `ParkResult::Woken { slot_index }` -- an EventSlot was incremented.
- `ParkResult::TimedOut` -- the timeout expired.

**Critical correctness requirement:** There is a race between checking the
EventSlot counter and calling `thread_park`. If an interrupt fires between the
reactor's `poll()` (which reads the counter) and the `thread_park` call, the
thread would sleep despite having pending completions.

The kernel handles this by checking each EventSlot counter at entry to
`thread_park`. If any counter has changed since the user-space snapshot (which
the kernel stores from the last wake or park), `thread_park` returns immediately
with `Woken`. This is the standard level-triggered semantic -- the kernel does
not clear the counter, so the condition is persistent.

### 5.3 Why Not Busy-Poll

Some high-performance I/O systems (DPDK, SPDK) use busy-polling: the core never
sleeps and continuously checks the hardware CQ in a tight loop. This minimizes
latency (no wake-up delay) but wastes power and prevents other threads on the
core from running.

Aether's approach is a hybrid:

- **Spin briefly** before parking. The executor can optionally spin for N
  iterations (configurable) checking EventSlots before falling through to
  `thread_park`. This amortizes the wake-up latency for bursty workloads.
- **Park when idle.** When the spin budget is exhausted, actually park. This is
  essential for multi-tenant scenarios where multiple processes share cores.

The spin count is a tunable parameter. For dedicated I/O cores (core exclusively
owned by one driver), a higher spin count (or infinite spin, i.e., pure
busy-poll) may be appropriate. For shared cores, spin count should be zero or
very low.

---

## 6. Timer Support

### 6.1 Hierarchical Timer Wheel

The executor maintains a hierarchical timing wheel for delayed futures (`sleep`,
`timeout`). The wheel is structured as multiple levels of slots, where each
level covers a progressively coarser time range.

```rust
/// Hierarchical timer wheel.
///
/// Level 0: 256 slots, 1 microsecond per slot  (covers 256 us)
/// Level 1: 256 slots, 256 microseconds per slot (covers 65.5 ms)
/// Level 2: 256 slots, 65.5 ms per slot (covers 16.7 seconds)
/// Level 3: 256 slots, 16.7 seconds per slot (covers ~71 minutes)
///
/// Total coverage: ~71 minutes with microsecond resolution at the low end.
/// Timers beyond level 3 range are placed in an overflow list and
/// cascaded down as time advances.
pub struct TimerWheel {
    /// The four wheel levels.
    levels: [WheelLevel; 4],

    /// Overflow list for timers exceeding the wheel's range.
    overflow: LinkedList<TimerEntry>,

    /// Current tick (microseconds since executor start, driven by TSC).
    current_tick: u64,
}

struct WheelLevel {
    /// Circular buffer of timer slots. Each slot is a linked list of
    /// timer entries that expire at this tick.
    slots: [LinkedList<TimerEntry>; 256],

    /// Current position in the circular buffer.
    cursor: u8,
}

/// A single timer entry. Lives in the timer wheel until it expires.
struct TimerEntry {
    /// Absolute expiry tick (microseconds).
    expiry: u64,

    /// Waker to invoke when the timer expires.
    waker: core::task::Waker,

    /// Intrusive linked list pointers.
    next: *mut TimerEntry,
    prev: *mut TimerEntry,
}
```

### 6.2 Resolution and Time Source

Timer resolution is 1 microsecond, backed by the x86-64 TSC (Time Stamp
Counter). The TSC is:

- Per-core (but invariant TSC on modern processors is synchronized across cores).
- Monotonic and non-stopping on modern Intel/AMD processors.
- Readable without privilege (`rdtsc` instruction, no syscall).
- Sub-nanosecond resolution.

The TSC frequency is calibrated at boot time by the kernel and exposed to
user-space. The executor converts TSC ticks to microseconds using a
multiply-and-shift operation (no division):

```rust
/// Convert TSC ticks to microseconds.
/// `tsc_multiplier` and `tsc_shift` are calibrated at startup.
#[inline(always)]
fn tsc_to_micros(tsc: u64, cal: &TscCalibration) -> u64 {
    ((tsc as u128 * cal.multiplier as u128) >> cal.shift) as u64
}

struct TscCalibration {
    multiplier: u64,
    shift: u32,
}
```

### 6.3 Integration with thread_park

The timer wheel directly feeds the `thread_park` timeout parameter. When the
executor is about to park:

```rust
fn compute_park_timeout(&self) -> Option<asi::Duration> {
    let next_expiry = self.timer_wheel.next_expiry();
    match next_expiry {
        Some(expiry) => {
            let now = self.current_tick;
            if expiry <= now {
                // Timer already expired. Don't park.
                return Some(asi::Duration { nanos: 0 });
            }
            let delta_micros = expiry - now;
            Some(asi::Duration { nanos: delta_micros * 1000 })
        }
        None => {
            // No timers pending. Park indefinitely.
            None
        }
    }
}
```

### 6.4 Timer Future

The public API for timers:

```rust
/// Sleep for the specified duration.
///
/// # Example
/// ```
/// aether::time::sleep(Duration::from_millis(100)).await;
/// ```
pub fn sleep(duration: core::time::Duration) -> SleepFuture {
    let executor = current_executor();
    let expiry = executor.current_tick + duration.as_micros() as u64;
    SleepFuture {
        expiry,
        registered: false,
    }
}

pub struct SleepFuture {
    expiry: u64,
    registered: bool,
}

impl Future for SleepFuture {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut core::task::Context<'_>) -> Poll<()> {
        let this = self.get_mut();
        let executor = current_executor();

        if executor.current_tick >= this.expiry {
            return Poll::Ready(());
        }

        if !this.registered {
            executor.timer_wheel.insert(this.expiry, cx.waker().clone());
            this.registered = true;
        }

        Poll::Pending
    }
}
```

### 6.5 Timeout Combinator

Wrapping any future with a timeout:

```rust
/// Run a future with a timeout. Returns Err(Elapsed) if the timeout
/// expires before the future completes.
pub async fn timeout<F: Future>(
    duration: core::time::Duration,
    future: F,
) -> Result<F::Output, Elapsed> {
    // Implementation uses select! over the future and a sleep timer.
    // The first to complete wins.
    select! {
        result = future => Ok(result),
        _ = sleep(duration) => Err(Elapsed),
    }
}
```

---

## 7. Multi-Core Coordination

### 7.1 Design Philosophy

The Aether runtime is distributed by default. Each core operates independently.
Cross-core coordination is the exception, not the rule. This section describes
the two narrow channels where cores interact: work-stealing and remote spawn.

Both mechanisms exist to prevent pathological imbalance -- one core overloaded
while others are idle. They are not part of the normal execution path. A
well-configured Aether application pins tasks and interrupts to cores such that
load is balanced statically at setup time.

### 7.2 Work-Stealing Deque (Chase-Lev)

Each executor owns a work-stealing deque alongside its local `VecDeque`. When a
task is pushed onto the local queue, it is also visible in the steal deque.
Remote cores can steal from the tail of the deque while the local core pops from
the head.

```rust
/// Lock-free work-stealing deque (Chase-Lev algorithm).
///
/// The local core pushes and pops from the "bottom" (LIFO for locality).
/// Remote cores steal from the "top" (FIFO, so they get older tasks).
///
/// This asymmetry is intentional: the local core keeps the most recently
/// enqueued tasks (best cache locality), while stealers get the oldest
/// tasks (least likely to have hot cache lines on the local core).
pub struct WorkStealQueue<T> {
    /// Circular buffer of task references.
    buffer: AtomicPtr<CircularBuffer<T>>,

    /// Bottom index (local push/pop). Only modified by the owner core.
    bottom: AtomicIsize,

    /// Top index (steal point). Modified by stealers via CAS.
    top: AtomicIsize,
}

impl<T> WorkStealQueue<T> {
    /// Push a task (local core only). O(1).
    pub fn push(&self, task: T) { /* ... */ }

    /// Pop a task (local core only). O(1).
    pub fn pop(&self) -> Option<T> { /* ... */ }

    /// Steal a task (remote cores). O(1) amortized.
    /// Returns None if the deque is empty or contended.
    pub fn steal(&self) -> Steal<T> { /* ... */ }
}

pub enum Steal<T> {
    /// Successfully stole a task.
    Success(T),
    /// The deque was empty.
    Empty,
    /// Lost a CAS race with another stealer. Retry.
    Retry,
}
```

**When to steal:** A core only attempts stealing when:

1. Its local queue is empty.
2. The reactor poll found no new completions.
3. There are no expired timers.

This ensures stealing is a last resort, not a regular occurrence.

**Steal order:** The stealer iterates cores in a deterministic but rotated order
(starting from `(my_core_id + 1) % num_cores`) to distribute steal pressure
evenly. It stops at the first successful steal to avoid draining another core.

**What can be stolen:** Only tasks in the `Ready` state that are not I/O-bound.
A task's "stealable" flag is set at spawn time. Tasks created by I/O futures
(which hold references to core-local reactor state) are marked non-stealable.

### 7.3 Remote Spawn

Spawning a task on a different core's executor:

```rust
/// Spawn a future on a specific core's executor.
///
/// The future is boxed, sent to the target core's MPSC inbox, and will
/// be picked up on the target core's next poll loop iteration.
pub fn spawn_on<F>(core: CoreId, future: F) -> JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let task = Task::allocate(future, core);
    let join_handle = task.join_handle();
    EXECUTORS[core.0 as usize].remote_inbox.push(task.as_ref());
    join_handle
}
```

The `remote_inbox` is a lock-free MPSC (multiple-producer, single-consumer)
queue. Multiple cores can push tasks into another core's inbox concurrently.
The owning core drains the inbox at the start of each poll loop iteration.

```rust
/// Lock-free MPSC queue for cross-core task submission.
///
/// Based on Vyukov's intrusive MPSC queue. Producers CAS onto the tail.
/// The consumer (owning core) processes from the head.
pub struct RemoteInbox {
    head: AtomicPtr<TaskNode>,
    tail: AtomicPtr<TaskNode>,
    stub: TaskNode,
}
```

The `Send` bound on `spawn_on` is intentional and important. Tasks that hold
references to core-local state (reactor WakerMaps, hardware queue pointers)
cannot implement `Send` and therefore cannot be spawned remotely. This is
enforced at compile time.

### 7.4 Tradeoffs

| Mechanism     | Benefit                           | Cost                                  |
|:--------------|:----------------------------------|:--------------------------------------|
| Work-stealing | Prevents core starvation          | CAS contention on steal, loss of cache locality |
| Remote spawn  | Dynamic task placement            | MPSC queue overhead, `Send` bound requirement  |
| No migration  | Maximal locality, zero sync cost  | Potential load imbalance               |

The default configuration disables work-stealing (`steal_enabled: false`). It
should only be enabled for workloads with unpredictable compute distribution
across cores. Pure I/O workloads (the common case for Aether) should keep it
disabled.

---

## 8. Backpressure and Overload

### 8.1 Run Queue Depth Limits

Each executor has a configurable `task_limit`. When the task count reaches this
limit, `spawn` returns an error instead of allocating a new task:

```rust
#[derive(Debug)]
pub enum SpawnError {
    /// The executor's task limit has been reached.
    QueueFull,
    /// The task allocator is out of memory.
    OutOfMemory,
}
```

This prevents unbounded memory growth from runaway task spawning. The limit
should be set based on the available slab allocator capacity and the expected
working set of concurrent operations.

### 8.2 CompletionSource Queue Depth

Each hardware completion source has a fixed queue depth (determined by the
hardware queue configuration, e.g., NVMe I/O queue depth). The WakerMap size
matches this depth exactly. When all WakerMap slots are occupied, the
application cannot submit more commands to that hardware queue until some
complete.

This is natural backpressure: hardware queue full means the application
must wait (`.await`) for completions before submitting more work. No explicit
flow control logic is needed -- the `Future` for command submission simply
returns `Poll::Pending` when the queue is full and registers a waker that fires
when a slot opens.

```rust
/// Submit an NVMe read command. Returns Pending if the submission queue is full.
impl Future for NvmeReadFuture {
    type Output = Result<(), NvmeError>;

    fn poll(self: Pin<&mut Self>, cx: &mut core::task::Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        // Check if the command has already completed.
        if let Some(status) = this.check_completion() {
            return Poll::Ready(status);
        }

        // Try to submit the command if not yet submitted.
        if !this.submitted {
            match this.queue.try_submit(&this.command) {
                Ok(cmd_id) => {
                    this.cmd_id = cmd_id;
                    this.submitted = true;
                    // Register waker for completion notification.
                    let reactor = current_executor().reactor_mut();
                    reactor.waker_map(this.source_id)
                        .insert(cmd_id as u64, cx.waker().clone());
                }
                Err(QueueFull) => {
                    // SQ is full. Register for notification when a slot opens.
                    this.queue.register_sq_space_waker(cx.waker().clone());
                    return Poll::Pending;
                }
            }
        }

        Poll::Pending
    }
}
```

### 8.3 Cooperative Yielding

Rust futures are cooperatively scheduled. A future that runs for a long time
without returning `Poll::Pending` blocks the entire executor -- no other tasks
on that core can make progress.

Aether provides a yield point for compute-heavy futures:

```rust
/// Yield the current task, allowing other tasks on this core to run.
/// The task is immediately re-enqueued in the Ready state.
///
/// Use this in compute-heavy loops to prevent starvation of I/O tasks.
pub async fn yield_now() {
    struct YieldFuture {
        yielded: bool,
    }

    impl Future for YieldFuture {
        type Output = ();

        fn poll(self: Pin<&mut Self>, cx: &mut core::task::Context<'_>) -> Poll<()> {
            let this = self.get_mut();
            if this.yielded {
                Poll::Ready(())
            } else {
                this.yielded = true;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }
    }

    YieldFuture { yielded: false }.await
}
```

The executor can also enforce a per-poll budget: if a task's `poll` call exceeds
a configurable number of TSC ticks, the executor inserts a synthetic yield on
the next poll. This is a safety net, not a primary mechanism -- well-written
futures should yield voluntarily.

---

## 9. Key Data Structures

This section consolidates all core data structures with field-level
documentation.

### 9.1 Executor

```rust
/// Per-core async executor.
///
/// One instance per physical core. Owns all tasks spawned on this core,
/// the reactor for this core's hardware completion sources, and the
/// timer wheel.
pub struct Executor {
    /// The physical core this executor is pinned to.
    pub core_id: CoreId,

    /// Local run queue. Tasks in the Ready state, waiting to be polled.
    /// Single-producer single-consumer: only this executor pushes (via
    /// waker callbacks) and pops (via the poll loop).
    local_queue: VecDeque<TaskRef>,

    /// Remote task inbox. Other cores push tasks here via spawn_on().
    /// Lock-free MPSC queue. Drained at the start of each poll iteration.
    remote_inbox: RemoteInbox,

    /// Work-stealing deque. The local core pushes to bottom and pops
    /// from bottom. Remote cores steal from top.
    steal_queue: WorkStealQueue<TaskRef>,

    /// Whether work-stealing is enabled for this executor.
    steal_enabled: bool,

    /// The reactor. Polls hardware completion sources and wakes futures.
    reactor: Reactor,

    /// Hierarchical timer wheel for delayed futures.
    timer_wheel: TimerWheel,

    /// TSC calibration data for converting ticks to microseconds.
    tsc_cal: TscCalibration,

    /// Current time in microseconds (updated at the top of each poll iteration).
    current_tick: u64,

    /// Per-core slab allocator for task storage.
    allocator: TaskAllocator,

    /// Total number of live tasks (all states).
    task_count: usize,

    /// Maximum allowed tasks before spawn returns QueueFull.
    task_limit: usize,

    /// Number of spin iterations before parking. 0 = park immediately.
    spin_budget: u32,
}
```

### 9.2 Reactor

```rust
/// Hardware completion event reactor.
///
/// Polls EventSlots and hardware completion queues, translating hardware
/// events into Waker invocations.
pub struct Reactor {
    /// Registered completion sources with their EventSlots and WakerMaps.
    sources: ArrayVec<CompletionSourceEntry, MAX_SOURCES>,

    /// Snapshot of each source's EventSlot counter from the last poll.
    /// Compared against the current counter to detect new completions
    /// without touching the hardware CQ.
    last_seen: [u64; MAX_SOURCES],
}

/// Maximum number of completion sources per core.
/// Typical: 1 NVMe CQ + 1 RDMA CQ + 1 network = 3.
/// Set conservatively high for flexibility.
const MAX_SOURCES: usize = 16;

/// A registered completion source with its associated state.
struct CompletionSourceEntry {
    /// The hardware completion source implementing the reap interface.
    source: &'static dyn CompletionSource,

    /// The EventSlot that the kernel increments on MSI-X interrupt.
    event_slot: &'static EventSlot,

    /// Maps hardware completion IDs (command ID, WR ID, etc.) to Wakers.
    waker_map: WakerMap,
}

/// Identifier for a registered completion source.
#[derive(Clone, Copy)]
pub struct SourceId(pub u32);
```

### 9.3 Task

```rust
/// Common task header. Shared across all Task<F> instantiations.
/// This is the type-erased portion that the executor interacts with.
#[repr(C)]
pub struct TaskHeader {
    /// Current task state (Ready, Polling, Parked, Complete).
    /// Atomic because wakers may be invoked from interrupt context
    /// or from a remote core.
    pub state: AtomicU8,

    /// The core this task was spawned on.
    pub core_affinity: CoreId,

    /// Whether this task can be stolen by another core.
    pub stealable: bool,

    /// Waker data for constructing a core::task::Waker.
    pub waker_data: WakerData,

    /// Type-erased poll function pointer.
    pub poll_fn: unsafe fn(*const TaskHeader, &mut core::task::Context<'_>) -> Poll<()>,
}

/// Task state machine.
#[repr(u8)]
pub enum TaskState {
    /// In the run queue, waiting to be polled.
    Ready    = 0,
    /// Currently being polled by the executor.
    Polling  = 1,
    /// Returned Pending. Waiting for a waker invocation.
    Parked   = 2,
    /// The future returned Ready. Output is available.
    Complete = 3,
}
```

### 9.4 WakerMap

```rust
/// Fixed-size map from hardware completion IDs to Wakers.
///
/// Indexed by completion ID (u64 cast to usize). Size equals the
/// hardware queue depth. No hashing, no collisions -- direct indexing.
pub struct WakerMap {
    /// Slot array. None = free, Some(waker) = occupied.
    slots: Box<[Option<core::task::Waker>]>,

    /// Number of occupied slots. Used for has_pending() checks.
    active: usize,

    /// Total slot count (matches hardware queue depth).
    capacity: usize,
}
```

### 9.5 TimerWheel

```rust
/// Four-level hierarchical timing wheel.
///
/// Provides O(1) insert and O(1) amortized expiry processing.
/// Microsecond resolution backed by TSC.
pub struct TimerWheel {
    /// The four wheel levels, each with 256 slots.
    levels: [WheelLevel; 4],

    /// Overflow bucket for timers beyond level 3's range (~71 minutes).
    overflow: LinkedList<TimerEntry>,

    /// Current tick in microseconds.
    current_tick: u64,

    /// Earliest pending expiry (cached for fast timeout computation).
    /// Updated on insert and expiry processing.
    earliest_expiry: Option<u64>,
}

/// One level of the timing wheel.
struct WheelLevel {
    /// 256 slots, each a linked list of timer entries.
    slots: [LinkedList<TimerEntry>; 256],

    /// Current cursor position (0-255).
    cursor: u8,

    /// Granularity of this level in microseconds.
    /// Level 0: 1, Level 1: 256, Level 2: 65536, Level 3: 16777216
    granularity: u64,
}
```

### 9.6 WorkStealQueue

```rust
/// Chase-Lev work-stealing deque.
///
/// The owning core operates on `bottom` (push and pop).
/// Remote cores operate on `top` (steal only).
///
/// Memory ordering:
/// - bottom: Relaxed loads/stores by owner, not accessed by stealers.
/// - top: Acquire/Release for steal CAS, Relaxed loads by owner.
/// - buffer: SeqCst fence on grow to ensure new buffer is visible.
pub struct WorkStealQueue<T> {
    /// Pointer to the circular buffer. Replaced atomically on grow.
    buffer: AtomicPtr<CircularBuffer<T>>,

    /// Bottom index. Modified only by the owner. Increment on push,
    /// decrement on pop.
    bottom: AtomicIsize,

    /// Top index. Modified by stealers via CAS. Increment on successful steal.
    top: AtomicIsize,
}

/// Growable circular buffer backing the work-steal deque.
struct CircularBuffer<T> {
    /// Log2 of the buffer capacity.
    log_size: usize,

    /// The actual storage. Length is 1 << log_size.
    storage: Box<[UnsafeCell<T>]>,
}
```

---

## 10. The Complete Poll Loop

### 10.1 Pseudocode

```rust
impl Executor {
    /// The main executor loop. Called once per core. Never returns under
    /// normal operation.
    pub fn run(&mut self) -> ! {
        loop {
            // -------------------------------------------------------
            // Phase 1: Drain the remote inbox.
            // Other cores may have spawned tasks on us via spawn_on().
            // -------------------------------------------------------
            while let Some(task) = self.remote_inbox.pop() {
                self.local_queue.push_back(task);
                if self.steal_enabled {
                    self.steal_queue.push(task);
                }
            }

            // -------------------------------------------------------
            // Phase 2: Advance timers.
            // Read TSC, convert to microseconds, advance the timer wheel.
            // Any expired timers have their wakers invoked, which pushes
            // the associated tasks onto local_queue.
            // -------------------------------------------------------
            self.current_tick = tsc_to_micros(rdtsc(), &self.tsc_cal);
            self.timer_wheel.advance_to(self.current_tick);

            // -------------------------------------------------------
            // Phase 3: Poll all ready tasks.
            // Drain the local queue. Each task is polled exactly once.
            // New tasks that become ready during polling (via synchronous
            // waker invocations) are appended to the back of the queue
            // and will be polled in the same iteration.
            // -------------------------------------------------------
            let mut polls_this_iteration = 0;
            while let Some(task_ref) = self.local_queue.pop_front() {
                // Transition: Ready -> Polling
                let header = unsafe { &*task_ref.ptr };
                header.state.store(TaskState::Polling as u8, Ordering::Release);

                // Build the Waker and Context.
                let raw_waker = core::task::RawWaker::new(
                    task_ref.ptr as *const (),
                    &WAKER_VTABLE,
                );
                let waker = unsafe { core::task::Waker::from_raw(raw_waker) };
                let mut cx = core::task::Context::from_waker(&waker);

                // Poll the future.
                let result = unsafe { (task_ref.poll_fn)(task_ref.ptr, &mut cx) };

                match result {
                    Poll::Ready(()) => {
                        // Task is done. Transition: Polling -> Complete
                        header.state.store(
                            TaskState::Complete as u8,
                            Ordering::Release,
                        );
                        self.task_count -= 1;
                        // The task's output is now in task.output.
                        // JoinHandle will read it.
                        // Deallocation happens when JoinHandle is dropped.
                    }
                    Poll::Pending => {
                        // The future is not ready. Transition: Polling -> Parked
                        // (unless a waker already fired during poll, in which
                        // case the state is already Ready and the task is already
                        // back in local_queue).
                        let _ = header.state.compare_exchange(
                            TaskState::Polling as u8,
                            TaskState::Parked as u8,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        );
                        // If CAS failed, state is Ready (waker fired during poll).
                        // The task is already in the queue. Nothing to do.
                    }
                }

                polls_this_iteration += 1;
            }

            // -------------------------------------------------------
            // Phase 4: Poll the reactor.
            // Check EventSlot counters. If any changed, reap completions
            // from the associated hardware CQ. Reaped completions invoke
            // wakers, which push tasks onto local_queue.
            // -------------------------------------------------------
            let completions = self.reactor.poll();

            // If the reactor found completions, tasks are now in local_queue.
            // Jump back to Phase 3 to poll them immediately.
            if completions > 0 || !self.local_queue.is_empty() {
                continue;
            }

            // -------------------------------------------------------
            // Phase 5: Work stealing (optional).
            // Our queue is empty and the reactor found nothing. Try to
            // steal work from other cores.
            // -------------------------------------------------------
            if self.steal_enabled {
                let num_cores = get_num_cores();
                let mut stolen = false;

                for offset in 1..num_cores {
                    let target = ((self.core_id.0 as usize + offset) % num_cores) as u32;
                    let target_queue = get_executor(CoreId(target)).steal_queue();

                    match target_queue.steal() {
                        Steal::Success(task) => {
                            self.local_queue.push_back(task);
                            stolen = true;
                            break; // Steal one at a time.
                        }
                        Steal::Empty => continue,
                        Steal::Retry => continue, // Lost race, try next core.
                    }
                }

                if stolen {
                    continue; // Go back to Phase 3 to poll the stolen task.
                }
            }

            // -------------------------------------------------------
            // Phase 6: Spin (optional).
            // Before committing to a full thread_park (which involves a
            // syscall and potential core halt), spin briefly checking
            // EventSlots. This absorbs bursty completions that arrive
            // just after we finished polling.
            // -------------------------------------------------------
            let mut parked = false;
            for _ in 0..self.spin_budget {
                core::hint::spin_loop();

                // Quick check: did any EventSlot counter change?
                if self.reactor.any_new_events() {
                    break;
                }

                // Quick check: did any remote spawns arrive?
                if !self.remote_inbox.is_empty() {
                    break;
                }
            }

            // If spinning found something, go back to the top.
            if self.reactor.any_new_events() || !self.remote_inbox.is_empty() {
                continue;
            }

            // -------------------------------------------------------
            // Phase 7: Park.
            // Nothing to do. Compute the timeout from the timer wheel
            // and park the thread. The kernel will wake us when an
            // EventSlot is incremented or the timeout expires.
            // -------------------------------------------------------
            let timeout = self.compute_park_timeout();
            let event_slots = self.reactor.event_slots();

            // Convert ArrayVec<&EventSlot> to slice for the syscall.
            let slot_refs: &[&EventSlot] = event_slots.as_slice();

            let park_result = asi::thread::thread_park(slot_refs, timeout);

            // -------------------------------------------------------
            // Phase 8: Woke up. Update the tick and loop back.
            // The next iteration will advance timers (Phase 2) and
            // poll the reactor (Phase 4), which will find the new
            // completions that triggered the wake.
            // -------------------------------------------------------
            match park_result {
                Ok(ParkResult::Woken { slot_index }) => {
                    // An EventSlot fired. The reactor will pick it up
                    // in Phase 4 on the next iteration.
                }
                Ok(ParkResult::TimedOut) => {
                    // Timer expired. Phase 2 will advance the timer wheel.
                }
                Err(_) => {
                    // Unexpected error from thread_park. Log and continue.
                    // The executor must not crash -- it owns all tasks on
                    // this core.
                }
            }
        }
    }
}
```

### 10.2 Execution Flow Diagram

```
                          +---> [Phase 1: Drain remote inbox]
                          |               |
                          |               v
                          |     [Phase 2: Advance timers]
                          |               |
                          |               v
                          |     [Phase 3: Poll ready tasks] <--+
                          |               |                    |
                          |               v                    |
                          |     [Phase 4: Reactor poll]        |
                          |               |                    |
                          |          found completions? -------+
                          |          (yes -> Phase 3)    yes
                          |               | no
                          |               v
                          |     [Phase 5: Work stealing]
                          |               |
                          |          stole task? --------------+
                          |          (yes -> Phase 3)    yes
                          |               | no
                          |               v
                          |     [Phase 6: Spin]
                          |               |
                          |          found events? ------------+
                          |          (yes -> top)         yes
                          |               | no
                          |               v
                          |     [Phase 7: thread_park]
                          |               |
                          |          (kernel halts core)
                          |               |
                          |          (interrupt or timeout)
                          |               |
                          |               v
                          +---- [Phase 8: Woke up, loop]
```

### 10.3 Latency Characteristics

| Path                                      | Latency            | Syscalls |
|:------------------------------------------|:-------------------|:---------|
| Task already ready in local queue         | ~100 ns            | 0        |
| Completion arrives during reactor poll    | ~200-500 ns        | 0        |
| Completion arrives during spin phase      | ~1-10 us           | 0        |
| Thread parked, woken by interrupt         | ~5-20 us           | 1 (park) |
| Thread parked, woken by timer             | ~5-20 us           | 1 (park) |
| Work stolen from remote core              | ~200-500 ns        | 0        |

The zero-syscall paths are the common case for loaded systems. The `thread_park`
path is only taken when the system is genuinely idle.

---

## Appendix A: Design Rationale Summary

### Why a per-core model instead of a thread pool?

Thread pools (Tokio's model) require a global scheduler, cross-thread
synchronization for the shared run queue, and incur cache invalidation when
tasks migrate between threads. In an exokernel where hardware queues are pinned
to cores via MSI-X affinity, task migration invalidates the locality contract
between the task, its completion queue, and its interrupt delivery. A per-core
model eliminates this class of problems entirely.

### Why fixed-size WakerMaps instead of a HashMap?

Hardware completion IDs are dense integers in the range `[0, queue_depth)`.
Direct indexing into a fixed-size array gives O(1) insert, lookup, and removal
with zero hashing overhead and no dynamic allocation. A `HashMap` would
introduce hashing cost, potential allocation on insert, and non-deterministic
performance due to hash collisions and table resizing.

### Why a hierarchical timer wheel instead of a BinaryHeap?

A binary heap gives O(log n) insert and O(log n) pop-min. A hierarchical timer
wheel gives O(1) insert and O(1) amortized tick advance. For an executor that
advances time on every loop iteration (potentially millions of times per second),
the constant-factor advantage of the wheel matters. The wheel also avoids dynamic
allocation -- all timer entries are pre-allocated from the slab.

### Why not busy-poll exclusively?

Busy-polling gives the lowest possible latency (~100 ns from CQE DMA to
user-space processing) but wastes power and prevents other threads from using
the core. The hybrid approach (spin briefly, then park) gives near-busy-poll
latency under load while allowing the core to halt when idle. The spin budget
is the tuning knob between latency and efficiency.

### Why allow work-stealing at all if tasks are core-pinned?

Without work-stealing, a core running a single long-compute task will leave
other cores idle even if there are ready tasks in its queue. This is acceptable
for pure I/O workloads where task runtimes are short (a few microseconds per
poll), but problematic for mixed workloads. Work-stealing is disabled by default
and exists as an opt-in escape hatch for workloads that need it.

### Why no global scheduler?

A global scheduler is a serialization point. Every task spawn, every wake, every
steal must go through it. At high task throughput (millions of wakes per second
across many cores), this becomes a bottleneck. The distributed model eliminates
the bottleneck at the cost of potentially suboptimal load distribution -- a
tradeoff that favors throughput over fairness, which is the right choice for
I/O-intensive exokernel workloads.
