#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]

//! `NVMe` queue and inflight tracking primitives for Feox.

extern crate alloc;
#[cfg(test)]
extern crate std;

use alloc::vec::Vec;
use core::cell::Cell;
use core::future::Future;
use core::marker::PhantomData;
use core::pin::Pin;
use core::ptr::NonNull;
use core::task::{Context, Poll, Waker};

/// Decoded `NVMe` status tuple.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NvmeStatus {
    /// Status code type.
    pub sct: u8,
    /// Status code.
    pub sc: u8,
    /// Do not retry.
    pub dnr: bool,
}

/// Minimal completion representation used by the prototype queue model.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NvmeCompletion {
    /// Command identifier.
    pub cid: u16,
    /// Completion status.
    pub status: NvmeStatus,
}

impl NvmeCompletion {
    /// Creates a successful completion.
    #[must_use]
    pub const fn success(cid: u16) -> Self {
        Self {
            cid,
            status: NvmeStatus {
                sct: 0,
                sc: 0,
                dnr: false,
            },
        }
    }

    /// Returns whether the completion succeeded.
    #[must_use]
    pub const fn succeeded(self) -> bool {
        self.status.sct == 0 && self.status.sc == 0
    }
}

/// Driver-visible `NVMe` errors.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NvmeError {
    /// No command slots are currently available.
    QueueFull,
    /// The device disappeared from the system.
    DeviceRemoved,
    /// The capability or queue ownership was revoked.
    CapabilityRevoked,
    /// The command finished with a non-success status.
    CommandFailed(NvmeStatus),
    /// A stale future observed a recycled slot.
    StaleSlot,
}

type CompletionOutcome = Result<NvmeCompletion, NvmeError>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SlotState {
    Free,
    Submitted,
    Completed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CommandLease {
    cid: u16,
    generation: u64,
}

#[derive(Debug)]
struct InflightEntry {
    generation: u64,
    state: SlotState,
    result: Option<CompletionOutcome>,
    waker: Option<Waker>,
    future_attached: bool,
}

impl InflightEntry {
    fn new() -> Self {
        Self {
            generation: 0,
            state: SlotState::Free,
            result: None,
            waker: None,
            future_attached: false,
        }
    }
}

#[derive(Debug)]
struct LocalOnly(Cell<()>);

/// Core-local mapping from command IDs to pending futures.
#[derive(Debug)]
pub struct InflightMap {
    entries: Vec<InflightEntry>,
    free: Vec<u16>,
}

impl InflightMap {
    /// Creates an inflight map with one reusable slot per possible CID.
    #[must_use]
    pub fn new(depth: u16) -> Self {
        let mut entries = Vec::with_capacity(depth as usize);
        let mut free = Vec::with_capacity(depth as usize);

        for cid in 0..depth {
            entries.push(InflightEntry::new());
            free.push(depth - cid - 1);
        }

        Self { entries, free }
    }

    /// Returns the number of currently free CIDs.
    #[must_use]
    pub fn available(&self) -> usize {
        self.free.len()
    }

    /// Registers a new in-flight command and returns the core-local future that
    /// will observe its completion.
    ///
    /// # Errors
    ///
    /// Returns [`NvmeError::QueueFull`] when no command slots are currently
    /// available.
    pub fn register(&mut self) -> Result<(u16, NvmeIoFuture), NvmeError> {
        let cid = self.free.pop().ok_or(NvmeError::QueueFull)?;
        let entry = &mut self.entries[cid as usize];
        let next_generation = entry.generation.wrapping_add(1).max(1);

        entry.generation = next_generation;
        entry.state = SlotState::Submitted;
        entry.result = None;
        entry.waker = None;
        entry.future_attached = true;

        Ok((
            cid,
            NvmeIoFuture {
                map: NonNull::from(self),
                lease: CommandLease {
                    cid,
                    generation: next_generation,
                },
                _not_send: PhantomData,
            },
        ))
    }

    /// Completes a single command and wakes the waiter if it already registered
    /// interest.
    pub fn complete(&mut self, completion: NvmeCompletion) {
        let outcome = if completion.succeeded() {
            Ok(completion)
        } else {
            Err(NvmeError::CommandFailed(completion.status))
        };

        self.finish(completion.cid, outcome);
    }

    /// Fails every live command, including commands whose futures have not been
    /// polled yet.
    ///
    /// # Panics
    ///
    /// Panics if the inflight entry count no longer fits in `u16`, which would
    /// violate the queue model's command-ID contract.
    pub fn fail_all(&mut self, error: NvmeError) {
        for cid in 0..u16::try_from(self.entries.len()).expect("entry count fits in u16") {
            if self.entries[usize::from(cid)].state != SlotState::Free {
                self.finish(cid, Err(error));
            }
        }
    }

    fn finish(&mut self, cid: u16, outcome: CompletionOutcome) {
        let entry = &mut self.entries[cid as usize];
        if entry.state == SlotState::Free {
            return;
        }

        entry.state = SlotState::Completed;
        entry.result = Some(outcome);

        if entry.future_attached {
            if let Some(waker) = entry.waker.take() {
                waker.wake();
            }
        } else {
            self.recycle(cid);
        }
    }

    fn recycle(&mut self, cid: u16) {
        let entry = &mut self.entries[cid as usize];
        if entry.state == SlotState::Free {
            return;
        }

        entry.state = SlotState::Free;
        entry.result = None;
        entry.waker = None;
        entry.future_attached = false;
        self.free.push(cid);
    }
}

/// A submitted command whose completion will be delivered to the owning core.
#[derive(Debug)]
pub struct NvmeIoFuture {
    map: NonNull<InflightMap>,
    lease: CommandLease,
    _not_send: PhantomData<&'static LocalOnly>,
}

impl Future for NvmeIoFuture {
    type Output = Result<NvmeCompletion, NvmeError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        // SAFETY: the future is intentionally `!Send` and is only polled on the
        // owning core that also owns the inflight map.
        let map = unsafe { this.map.as_mut() };
        let cid = this.lease.cid as usize;

        if map.entries[cid].generation != this.lease.generation {
            return Poll::Ready(Err(NvmeError::StaleSlot));
        }

        if let Some(result) = map.entries[cid].result.take() {
            map.entries[cid].future_attached = false;
            map.entries[cid].waker = None;
            map.recycle(this.lease.cid);
            Poll::Ready(result)
        } else {
            map.entries[cid].waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}

impl Drop for NvmeIoFuture {
    fn drop(&mut self) {
        // SAFETY: the future is `!Send` and cannot outlive the executor-local
        // inflight map that created it.
        let map = unsafe { self.map.as_mut() };
        let cid = self.lease.cid as usize;

        if cid >= map.entries.len() {
            return;
        }

        let entry = &mut map.entries[cid];
        if entry.generation != self.lease.generation || entry.state == SlotState::Free {
            return;
        }

        entry.future_attached = false;
        entry.waker = None;

        if entry.state == SlotState::Completed {
            map.recycle(self.lease.cid);
        }
    }
}

/// Small prototype queue pair that exposes safe CID allocation semantics.
#[derive(Debug)]
pub struct NvmeQueuePair {
    inflight: InflightMap,
    failed: Option<NvmeError>,
}

impl NvmeQueuePair {
    /// Creates a new queue pair with a fixed command depth.
    #[must_use]
    pub fn new(depth: u16) -> Self {
        Self {
            inflight: InflightMap::new(depth),
            failed: None,
        }
    }

    /// Returns the number of currently free command slots.
    #[must_use]
    pub fn available(&self) -> usize {
        self.inflight.available()
    }

    /// Reserves a command ID and returns the future that will complete it.
    ///
    /// # Errors
    ///
    /// Returns the queue failure if the queue has already failed, or
    /// [`NvmeError::QueueFull`] when no command slots remain.
    pub fn submit(&mut self) -> Result<(u16, NvmeIoFuture), NvmeError> {
        if let Some(error) = self.failed {
            return Err(error);
        }

        self.inflight.register()
    }

    /// Delivers a completion to the corresponding in-flight command.
    pub fn complete(&mut self, completion: NvmeCompletion) {
        self.inflight.complete(completion);
    }

    /// Fails the queue and all pending commands.
    pub fn fail(&mut self, error: NvmeError) {
        self.failed = Some(error);
        self.inflight.fail_all(error);
    }
}

#[cfg(test)]
mod tests {
    use super::{InflightMap, NvmeCompletion, NvmeError, NvmeIoFuture, NvmeQueuePair};
    use core::future::Future;
    use core::pin::pin;
    use core::task::{Context, Poll, Waker};
    use static_assertions::assert_not_impl_any;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::Wake;

    assert_not_impl_any!(NvmeIoFuture: Send, Sync);

    #[derive(Debug)]
    struct WakeCounter {
        hits: AtomicUsize,
    }

    impl Wake for WakeCounter {
        fn wake(self: Arc<Self>) {
            self.hits.fetch_add(1, Ordering::SeqCst);
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.hits.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn counting_waker() -> (Arc<WakeCounter>, Waker) {
        let counter = Arc::new(WakeCounter {
            hits: AtomicUsize::new(0),
        });
        let waker = Waker::from(counter.clone());
        (counter, waker)
    }

    #[test]
    fn completed_but_unpolled_commands_do_not_release_their_cid() {
        let mut queue = NvmeQueuePair::new(1);
        let (cid, future) = queue.submit().expect("first submission should fit");
        queue.complete(NvmeCompletion::success(cid));

        assert!(matches!(queue.submit(), Err(NvmeError::QueueFull)));

        drop(future);
        let (reused_cid, _) = queue.submit().expect("cid should recycle after drop");
        assert_eq!(reused_cid, cid);
    }

    #[test]
    fn fail_all_marks_even_unpolled_futures_as_ready_with_an_error() {
        let mut inflight = InflightMap::new(2);
        let (_, mut future) = inflight.register().expect("slot available");
        inflight.fail_all(NvmeError::DeviceRemoved);

        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);
        assert_eq!(
            Future::poll(pin!(&mut future).as_mut(), &mut cx),
            Poll::Ready(Err(NvmeError::DeviceRemoved))
        );
    }

    #[test]
    fn fail_all_wakes_registered_waiters() {
        let mut inflight = InflightMap::new(2);
        let (_, mut future) = inflight.register().expect("slot available");
        let (counter, waker) = counting_waker();
        let mut cx = Context::from_waker(&waker);

        assert_eq!(
            Future::poll(pin!(&mut future).as_mut(), &mut cx),
            Poll::Pending
        );

        inflight.fail_all(NvmeError::CapabilityRevoked);

        assert_eq!(counter.hits.load(Ordering::SeqCst), 1);
        assert_eq!(
            Future::poll(pin!(&mut future).as_mut(), &mut cx),
            Poll::Ready(Err(NvmeError::CapabilityRevoked))
        );
    }
}
