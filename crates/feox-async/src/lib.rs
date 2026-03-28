#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]

//! Executor primitives for the Feox async runtime.

use core::sync::atomic::{AtomicU8, Ordering};
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
    ///
    /// # Panics
    ///
    /// Panics if the task is not currently in either the `Polling` or
    /// `WakePending` state when the pending-poll epilogue runs.
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

#[cfg(test)]
mod tests {
    use super::{PendingDisposition, TaskHeader, TaskState, WakeDisposition};
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
}
