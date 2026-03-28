#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]

//! `NVMe` queue and inflight tracking primitives for Feox.

#[cfg(test)]
extern crate std;

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

struct InflightEntry {
    generation: u64,
    state: SlotState,
    result: Option<CompletionOutcome>,
    waker: Option<Waker>,
    future_attached: bool,
}

impl InflightEntry {
    const fn new() -> Self {
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
///
/// `N` is the queue depth and must be in the range `1..=65535`. Each slot
/// corresponds to one CID value. The free list is a fixed-capacity stack; no
/// heap allocation is required.
pub struct InflightMap<const N: usize> {
    entries: [InflightEntry; N],
    /// Fixed-capacity free-CID stack. Valid entries are `free[0..free_head]`.
    free: [u16; N],
    /// Number of items currently on the free stack.
    free_head: usize,
}

impl<const N: usize> InflightMap<N> {
    /// Creates an inflight map with `N` reusable slots.
    ///
    /// CIDs are assigned from `0` to `N - 1`. They are pushed onto the free
    /// stack in reverse order so the first `register` call returns CID 0.
    #[must_use]
    pub fn new() -> Self {
        // Entries default to the Free/zero state; no unsafe required.
        let entries = core::array::from_fn(|_| InflightEntry::new());
        // Free stack: slot N-1 is the top so the first pop returns CID 0.
        let free = core::array::from_fn(|i| (N - i - 1) as u16);
        Self {
            entries,
            free,
            free_head: N,
        }
    }

    /// Returns the number of currently free CIDs.
    #[must_use]
    pub fn available(&self) -> usize {
        self.free_head
    }

    /// Registers a new in-flight command and returns the core-local future that
    /// will observe its completion.
    pub fn register(&mut self) -> Result<(u16, NvmeIoFuture<N>), NvmeError> {
        let cid = self.pop_free().ok_or(NvmeError::QueueFull)?;
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
        for cid in 0..N {
            if self.entries[cid].state != SlotState::Free {
                self.finish(cid as u16, Err(error));
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
        self.push_free(cid);
    }

    fn pop_free(&mut self) -> Option<u16> {
        if self.free_head == 0 {
            return None;
        }
        self.free_head -= 1;
        Some(self.free[self.free_head])
    }

    fn push_free(&mut self, cid: u16) {
        // Invariant: free_head < N because we only push CIDs that came from
        // pop_free, so the count can never exceed N.
        debug_assert!(self.free_head < N);
        self.free[self.free_head] = cid;
        self.free_head += 1;
    }
}

impl<const N: usize> Default for InflightMap<N> {
    fn default() -> Self {
        Self::new()
    }
}

/// A submitted command whose completion will be delivered to the owning core.
pub struct NvmeIoFuture<const N: usize> {
    map: NonNull<InflightMap<N>>,
    lease: CommandLease,
    _not_send: PhantomData<&'static LocalOnly>,
}

impl<const N: usize> Future for NvmeIoFuture<N> {
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

impl<const N: usize> Drop for NvmeIoFuture<N> {
    fn drop(&mut self) {
        // SAFETY: the future is `!Send` and cannot outlive the executor-local
        // inflight map that created it.
        let map = unsafe { self.map.as_mut() };
        let cid = self.lease.cid as usize;

        if cid >= N {
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
pub struct NvmeQueuePair<const N: usize> {
    inflight: InflightMap<N>,
    failed: Option<NvmeError>,
}

impl<const N: usize> NvmeQueuePair<N> {
    /// Creates a new queue pair with `N` command slots.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inflight: InflightMap::new(),
            failed: None,
        }
    }

    /// Returns the number of currently free command slots.
    #[must_use]
    pub fn available(&self) -> usize {
        self.inflight.available()
    }

    /// Reserves a command ID and returns the future that will complete it.
    pub fn submit(&mut self) -> Result<(u16, NvmeIoFuture<N>), NvmeError> {
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

impl<const N: usize> Default for NvmeQueuePair<N> {
    fn default() -> Self {
        Self::new()
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

    assert_not_impl_any!(NvmeIoFuture<64>: Send, Sync);

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
        let mut queue = NvmeQueuePair::<1>::new();
        let (cid, future) = queue.submit().expect("first submission should fit");
        queue.complete(NvmeCompletion::success(cid));

        assert!(matches!(queue.submit(), Err(NvmeError::QueueFull)));

        drop(future);
        let (reused_cid, _) = queue.submit().expect("cid should recycle after drop");
        assert_eq!(reused_cid, cid);
    }

    #[test]
    fn fail_all_marks_even_unpolled_futures_as_ready_with_an_error() {
        let mut inflight = InflightMap::<2>::new();
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
        let mut inflight = InflightMap::<2>::new();
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
