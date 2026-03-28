#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]

//! Executor primitives for the Feox async runtime.

use core::ptr::NonNull;
use core::sync::atomic::{AtomicU8, Ordering};
use core::task::{RawWaker, RawWakerVTable, Waker};
use feox_asi::CoreId;

/// Task scheduler state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum TaskState {
    /// The task is runnable and present in a ready queue.
    Ready = 0,
    /// The task is actively being polled.
    Polling = 1,
    /// The task is waiting for a wakeup.
    Parked = 2,
    /// A wake arrived while the task was being polled.
    WakePending = 3,
    /// The task has completed and should not be scheduled again.
    Complete = 4,
}

impl TaskState {
    fn from_raw(raw: u8) -> Self {
        match raw {
            0 => Self::Ready,
            1 => Self::Polling,
            2 => Self::Parked,
            3 => Self::WakePending,
            4 => Self::Complete,
            _ => panic!("invalid task state: {raw}"),
        }
    }
}

/// What the executor should do after invoking a waker.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WakeDisposition {
    /// Push the task onto the owning core's ready queue now.
    Enqueue(CoreId),
    /// The wake was recorded and the current poll epilogue will requeue it.
    Deferred(CoreId),
    /// The task was already runnable.
    AlreadyReady,
    /// The task is already complete and can ignore the wake.
    Completed,
}

/// What the executor should do when a poll returns `Pending`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PendingDisposition {
    /// The task can sleep until another wake arrives.
    Parked,
    /// A wake arrived during poll, so the task must be requeued immediately.
    Requeue(CoreId),
}

/// Minimal task header used by the executor and waker paths.
#[derive(Debug)]
pub struct TaskHeader {
    state: AtomicU8,
    core_affinity: CoreId,
}

impl TaskHeader {
    /// Creates a task header in the ready state.
    #[must_use]
    pub const fn new(core_affinity: CoreId) -> Self {
        Self {
            state: AtomicU8::new(TaskState::Ready as u8),
            core_affinity,
        }
    }

    /// Returns the owning core.
    #[must_use]
    pub const fn core_affinity(&self) -> CoreId {
        self.core_affinity
    }

    /// Reads the current scheduler state.
    #[must_use]
    pub fn state(&self) -> TaskState {
        TaskState::from_raw(self.state.load(Ordering::Acquire))
    }

    /// Marks the task as actively being polled.
    pub fn begin_poll(&self) {
        let previous = self.state.swap(TaskState::Polling as u8, Ordering::AcqRel);
        debug_assert_eq!(TaskState::from_raw(previous), TaskState::Ready);
    }

    /// Marks the task as complete.
    pub fn complete(&self) {
        self.state
            .store(TaskState::Complete as u8, Ordering::Release);
    }

    /// Records a wakeup in a way that preserves wake-during-poll.
    #[must_use]
    pub fn wake(&self) -> WakeDisposition {
        loop {
            match self.state() {
                TaskState::Ready | TaskState::WakePending => {
                    return WakeDisposition::AlreadyReady;
                }
                TaskState::Complete => return WakeDisposition::Completed,
                TaskState::Parked => {
                    if self
                        .state
                        .compare_exchange(
                            TaskState::Parked as u8,
                            TaskState::Ready as u8,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        return WakeDisposition::Enqueue(self.core_affinity);
                    }
                }
                TaskState::Polling => {
                    if self
                        .state
                        .compare_exchange(
                            TaskState::Polling as u8,
                            TaskState::WakePending as u8,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        return WakeDisposition::Deferred(self.core_affinity);
                    }
                }
            }
        }
    }

    /// Applies the poll epilogue when a future returns `Pending`.
    #[must_use]
    pub fn finish_pending_poll(&self) -> PendingDisposition {
        loop {
            match self.state() {
                TaskState::Polling => {
                    if self
                        .state
                        .compare_exchange(
                            TaskState::Polling as u8,
                            TaskState::Parked as u8,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        return PendingDisposition::Parked;
                    }
                }
                TaskState::WakePending => {
                    if self
                        .state
                        .compare_exchange(
                            TaskState::WakePending as u8,
                            TaskState::Ready as u8,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        return PendingDisposition::Requeue(self.core_affinity);
                    }
                }
                other => panic!("pending poll epilogue entered from {other:?}"),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// RunQueue
// ---------------------------------------------------------------------------

/// Fixed-capacity FIFO of runnable task-header pointers for a single core.
///
/// `CAP` is the maximum number of simultaneously runnable tasks. Pushing to a
/// full queue fails and returns `false` — this is a design-time constraint, not
/// a runtime allocation failure. The caller should size `CAP` to be larger than
/// the maximum expected runnable set.
///
/// `RunQueue` is intentionally `!Send` and `!Sync`: it must only be used from
/// the core that owns it. Cross-core wakeups must go through a separate
/// inter-core notification channel, not by pushing directly to another core's
/// queue.
pub struct RunQueue<const CAP: usize> {
    slots: [Option<NonNull<TaskHeader>>; CAP],
    /// Index of the oldest entry.
    head: usize,
    /// Number of valid entries.
    len: usize,
}

// `Option<NonNull<TaskHeader>>` is `Copy`, so the const initializer works.
// Safety: RunQueue must only be used from its owning core; it is not Send.
impl<const CAP: usize> RunQueue<CAP> {
    /// Creates an empty run queue.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            slots: [None; CAP],
            head: 0,
            len: 0,
        }
    }

    /// Returns the number of tasks currently in the queue.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Returns `true` if the queue holds no tasks.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Returns `true` if the queue is at capacity.
    #[must_use]
    pub const fn is_full(&self) -> bool {
        self.len == CAP
    }

    /// Enqueues a task-header pointer.
    ///
    /// Returns `false` and leaves the queue unchanged if capacity is exhausted.
    /// The caller must not push the same task twice while it is already in the
    /// queue (the queue does not deduplicate).
    pub fn push(&mut self, task: NonNull<TaskHeader>) -> bool {
        if self.len == CAP {
            return false;
        }
        let tail = (self.head + self.len) % CAP;
        self.slots[tail] = Some(task);
        self.len += 1;
        true
    }

    /// Dequeues the oldest task-header pointer.
    ///
    /// Returns `None` when the queue is empty.
    pub fn pop(&mut self) -> Option<NonNull<TaskHeader>> {
        if self.len == 0 {
            return None;
        }
        let task = self.slots[self.head].take();
        self.head = (self.head + 1) % CAP;
        self.len -= 1;
        task
    }
}

impl<const CAP: usize> Default for RunQueue<CAP> {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Waker infrastructure
// ---------------------------------------------------------------------------

/// Static vtable for wakers backed by a pinned `TaskHeader`.
///
/// The raw waker data pointer is a `*const TaskHeader` pointing into the
/// executor's pinned task storage. The task header lives for the duration of
/// the task, so the pointer is valid for as long as any `Waker` derived from
/// it is alive.
///
/// Clone is a copy of the raw pointer (no reference counting). Drop is a
/// no-op. `wake` and `wake_by_ref` call `TaskHeader::wake()`.
///
/// **Enqueue gap.** This vtable calls `TaskHeader::wake()` to record the
/// wakeup in the state machine but does not push the task onto a run queue.
/// When a task is woken from `Parked` → `Ready`, the caller that holds the
/// `WakeDisposition::Enqueue` result is responsible for pushing the task
/// pointer onto the owning core's `RunQueue`. For the single-core bootstrap
/// runtime this is fine because all wakes arrive synchronously. A full
/// reactor will extend this by carrying a `RunQueue` pointer in the waker
/// data once the queue layout is stable.
static TASK_HEADER_WAKER_VTABLE: RawWakerVTable = RawWakerVTable::new(
    // clone
    |ptr| RawWaker::new(ptr, &TASK_HEADER_WAKER_VTABLE),
    // wake (consumes self; no ref count to drop)
    |ptr| {
        // Safety: ptr is a valid *const TaskHeader for the life of the waker.
        let header = unsafe { &*(ptr as *const TaskHeader) };
        let _ = header.wake();
    },
    // wake_by_ref
    |ptr| {
        let header = unsafe { &*(ptr as *const TaskHeader) };
        let _ = header.wake();
    },
    // drop: no-op (no allocation to free)
    |_ptr| {},
);

/// Creates a `Waker` that calls `TaskHeader::wake()` when triggered.
///
/// # Safety
///
/// `header` must point to a `TaskHeader` that outlives the returned `Waker`.
/// In the executor model, task headers live inside pinned task storage for
/// the full duration of the task's lifetime, satisfying this requirement.
///
/// The returned waker records state transitions via the `TaskHeader` CAS
/// machine but does not itself enqueue the task. See the vtable documentation
/// for the current enqueue gap.
#[must_use]
pub unsafe fn make_task_waker(header: NonNull<TaskHeader>) -> Waker {
    let raw = RawWaker::new(header.as_ptr() as *const (), &TASK_HEADER_WAKER_VTABLE);
    // Safety: the vtable functions satisfy the contract documented on
    // RawWakerVTable: clone returns a valid waker, wake/wake_by_ref are safe
    // to call, drop is a no-op matching the no-allocation clone semantics.
    unsafe { Waker::from_raw(raw) }
}

#[cfg(test)]
mod tests {
    use super::{PendingDisposition, RunQueue, TaskHeader, TaskState, WakeDisposition};
    use feox_asi::CoreId;

    #[test]
    fn parked_wake_enqueues_the_task() {
        let task = TaskHeader::new(CoreId(3));
        task.begin_poll();
        assert_eq!(task.finish_pending_poll(), PendingDisposition::Parked);

        assert_eq!(task.wake(), WakeDisposition::Enqueue(CoreId(3)));
        assert_eq!(task.state(), TaskState::Ready);
    }

    #[test]
    fn wake_during_poll_is_requeued_after_the_poll_returns_pending() {
        let task = TaskHeader::new(CoreId(1));
        task.begin_poll();

        assert_eq!(task.wake(), WakeDisposition::Deferred(CoreId(1)));
        assert_eq!(task.state(), TaskState::WakePending);
        assert_eq!(
            task.finish_pending_poll(),
            PendingDisposition::Requeue(CoreId(1))
        );
        assert_eq!(task.state(), TaskState::Ready);
    }

    #[test]
    fn completed_tasks_ignore_late_wakes() {
        let task = TaskHeader::new(CoreId(0));
        task.complete();

        assert_eq!(task.wake(), WakeDisposition::Completed);
        assert_eq!(task.state(), TaskState::Complete);
    }

    // --- RunQueue tests ---

    #[test]
    fn run_queue_push_and_pop_are_fifo() {
        let mut a = TaskHeader::new(CoreId(0));
        let mut b = TaskHeader::new(CoreId(0));
        let ptr_a = unsafe { core::ptr::NonNull::new_unchecked(&mut a as *mut TaskHeader) };
        let ptr_b = unsafe { core::ptr::NonNull::new_unchecked(&mut b as *mut TaskHeader) };

        let mut queue = RunQueue::<4>::new();
        assert!(queue.push(ptr_a));
        assert!(queue.push(ptr_b));
        assert_eq!(queue.len(), 2);

        assert_eq!(queue.pop(), Some(ptr_a));
        assert_eq!(queue.pop(), Some(ptr_b));
        assert_eq!(queue.pop(), None);
        assert!(queue.is_empty());
    }

    #[test]
    fn run_queue_rejects_push_when_full() {
        let mut task = TaskHeader::new(CoreId(0));
        let ptr = unsafe { core::ptr::NonNull::new_unchecked(&mut task as *mut TaskHeader) };

        let mut queue = RunQueue::<2>::new();
        assert!(queue.push(ptr));
        assert!(queue.push(ptr));
        assert!(queue.is_full());
        assert!(!queue.push(ptr));
    }

    #[test]
    fn run_queue_wraps_around_correctly() {
        let mut a = TaskHeader::new(CoreId(0));
        let mut b = TaskHeader::new(CoreId(0));
        let mut c = TaskHeader::new(CoreId(0));
        let ptr_a = unsafe { core::ptr::NonNull::new_unchecked(&mut a as *mut TaskHeader) };
        let ptr_b = unsafe { core::ptr::NonNull::new_unchecked(&mut b as *mut TaskHeader) };
        let ptr_c = unsafe { core::ptr::NonNull::new_unchecked(&mut c as *mut TaskHeader) };

        let mut queue = RunQueue::<2>::new();
        assert!(queue.push(ptr_a));
        assert!(queue.push(ptr_b));
        // Pop one to make room, then push a third — exercises the wrap-around.
        assert_eq!(queue.pop(), Some(ptr_a));
        assert!(queue.push(ptr_c));
        assert_eq!(queue.pop(), Some(ptr_b));
        assert_eq!(queue.pop(), Some(ptr_c));
        assert!(queue.is_empty());
    }
}
