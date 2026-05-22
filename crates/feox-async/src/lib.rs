#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]

//! Executor primitives for the Feox async runtime.

#[cfg(test)]
extern crate std;

use core::cell::UnsafeCell;
use core::future::Future;
use core::mem::MaybeUninit;
use core::pin::Pin;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU8, Ordering};
use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
use feox_asi::CoreId;

/// Type-erased poll function installed in a [`TaskHeader`] at spawn time.
///
/// Called by the executor with a pointer to the task's header. The callee
/// locates the surrounding [`TaskCell`] storage via the `#[repr(C)]` layout
/// guarantee that the header is always at offset zero.
pub type PollFn = for<'cx> unsafe fn(NonNull<TaskHeader>, &mut Context<'cx>) -> Poll<()>;

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

/// Type-erased function pointer that knows how to push a task into a
/// specific run queue. Bound to a [`TaskHeader`] via
/// [`TaskHeader::bind_wake_target`]; invoked by the waker vtable when a
/// `wake()` call transitions a parked task back to `Ready`.
pub type WakeDispatcher = unsafe fn(target: *mut u8, header: NonNull<TaskHeader>);

/// Minimal task header used by the executor and waker paths.
///
/// Always the first field of [`TaskCell`] (enforced by `#[repr(C)]`), so a
/// `*mut TaskHeader` can be safely cast to a `*mut TaskCell<F>` by the
/// type-erased poll path.
#[derive(Debug)]
pub struct TaskHeader {
    state: AtomicU8,
    core_affinity: CoreId,
    /// Type-erased poll function. `None` until [`TaskCell::spawn`] installs it.
    poll_fn: UnsafeCell<Option<PollFn>>,
    /// Opaque pointer passed to [`Self::wake_dispatcher`] when an external
    /// wake transitions the task from `Parked` back to `Ready`. Typically
    /// points at the owning run queue. `null` means no external dispatcher
    /// is bound.
    wake_target: AtomicPtr<u8>,
    /// Function pointer (typed as [`WakeDispatcher`]) stored as an opaque
    /// data pointer because stable Rust has no `AtomicFnPtr`. Read with
    /// [`Self::take_wake_dispatcher`].
    wake_dispatcher: AtomicPtr<core::ffi::c_void>,
}

// Safety: `TaskHeader` uses `AtomicU8` for state transitions and a
// `UnsafeCell<Option<PollFn>>` for the poll function. The poll function is
// written exactly once (before any polling begins) and then only read. All
// concurrent state access goes through the atomic state machine. The wake
// target/dispatcher pair is set via atomics and is single-writer (the
// executor that owns the task) during the task's lifetime.
unsafe impl Sync for TaskHeader {}

impl TaskHeader {
    /// Creates a task header in the ready state with no poll function installed.
    #[must_use]
    pub const fn new(core_affinity: CoreId) -> Self {
        Self {
            state: AtomicU8::new(TaskState::Ready as u8),
            core_affinity,
            poll_fn: UnsafeCell::new(None),
            wake_target: AtomicPtr::new(core::ptr::null_mut()),
            wake_dispatcher: AtomicPtr::new(core::ptr::null_mut()),
        }
    }

    /// Binds an external wake hook. When the waker vtable observes a wake
    /// that transitions the task from `Parked` back to `Ready`, it calls
    /// `dispatcher(target, header)` to push the task back into its owning
    /// run queue.
    ///
    /// Closing the "enqueue gap" from [`TASK_HEADER_WAKER_VTABLE`]: until
    /// a target is bound the vtable's `wake` is a no-op past the atomic
    /// state transition, so wakes that arrive between polls are lost.
    ///
    /// # Safety
    ///
    /// - `target` must remain valid for as long as any clone of the
    ///   task's waker can still fire (typically the lifetime of the
    ///   owning executor).
    /// - `dispatcher` must be safe to call from any wake context (e.g.
    ///   from an external drainer between executor polls). On a
    ///   single-core polled runtime that means no concurrent access to
    ///   the run queue elsewhere.
    pub unsafe fn bind_wake_target(&self, target: *mut u8, dispatcher: WakeDispatcher) {
        let fn_ptr = dispatcher as *mut core::ffi::c_void;
        self.wake_target.store(target, Ordering::Release);
        self.wake_dispatcher.store(fn_ptr, Ordering::Release);
    }

    /// Removes any bound wake hook. After this call, the waker reverts to
    /// the no-op behavior of the prototype vtable.
    pub fn clear_wake_target(&self) {
        self.wake_target.store(core::ptr::null_mut(), Ordering::Release);
        self.wake_dispatcher
            .store(core::ptr::null_mut(), Ordering::Release);
    }

    /// Invokes the bound wake dispatcher, if any. Called by the vtable
    /// after `wake()` returns [`WakeDisposition::Enqueue`].
    fn invoke_wake_dispatcher(&self) {
        let dispatcher_ptr = self.wake_dispatcher.load(Ordering::Acquire);
        if dispatcher_ptr.is_null() {
            return;
        }
        let target = self.wake_target.load(Ordering::Acquire);
        // SAFETY: the dispatcher pointer was installed via
        // `bind_wake_target` whose unsafe contract requires it to be a
        // valid `WakeDispatcher` callable with `target`.
        let dispatcher: WakeDispatcher = unsafe {
            core::mem::transmute::<*mut core::ffi::c_void, WakeDispatcher>(dispatcher_ptr)
        };
        unsafe {
            dispatcher(target, NonNull::from(self));
        }
    }

    /// Installs the type-erased poll function.
    ///
    /// # Safety
    ///
    /// Must be called exactly once, before the task is enqueued for polling.
    pub unsafe fn install_poll_fn(&self, f: PollFn) {
        // SAFETY: upheld by the caller — no concurrent write is possible
        // because this is called exactly once before the task is visible to
        // the executor.
        unsafe { *self.poll_fn.get() = Some(f) };
    }

    /// Calls the installed poll function.
    ///
    /// # Safety
    ///
    /// `this` must be in the `Polling` state and have a poll function
    /// installed via [`TaskHeader::install_poll_fn`].
    pub unsafe fn poll(this: NonNull<TaskHeader>, cx: &mut Context<'_>) -> Poll<()> {
        // SAFETY: upheld by the caller — Polling state guarantees exclusive
        // access to the future, and the poll_fn is installed before first poll.
        let f = unsafe { (*this.as_ref().poll_fn.get()).expect("poll called before install_poll_fn") };
        unsafe { f(this, cx) }
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
// TaskCell — pinned per-task future storage
// ---------------------------------------------------------------------------

/// Pinned storage for a single async task.
///
/// Intended to be placed in a `static` binding so the future has a stable
/// address for the duration of its lifetime. `spawn` writes the future and
/// returns a header pointer ready to enqueue into a [`SingleCoreExecutor`].
///
/// # Layout
///
/// `#[repr(C)]` guarantees that `header` is at offset zero, so the executor's
/// type-erased poll path can safely cast a `*mut TaskHeader` back to a
/// `*mut TaskCell<F>`.
#[repr(C)]
pub struct TaskCell<F: Future<Output = ()>> {
    /// Task state machine — must remain the first field.
    header: TaskHeader,
    /// Future storage, written once by `spawn` and polled exclusively while
    /// in the `Polling` state.
    future: UnsafeCell<MaybeUninit<F>>,
    /// Guards against double-spawn.
    spawned: AtomicBool,
}

// Safety: `TaskCell` mediates all future access through the atomic state
// machine in `TaskHeader`. The future is written once (protected by the
// `spawned` CAS) and polled exclusively while in `Polling` state. Requiring
// `F: Send` allows the waker to move a wake signal across cores.
unsafe impl<F: Future<Output = ()> + Send> Sync for TaskCell<F> {}
unsafe impl<F: Future<Output = ()> + Send> Send for TaskCell<F> {}

impl<F: Future<Output = ()>> TaskCell<F> {
    /// Creates an empty task cell for the given core affinity.
    #[must_use]
    pub const fn new(core_affinity: CoreId) -> Self {
        Self {
            header: TaskHeader::new(core_affinity),
            future: UnsafeCell::new(MaybeUninit::uninit()),
            spawned: AtomicBool::new(false),
        }
    }

    /// Installs `future` and returns a header pointer ready for enqueueing.
    ///
    /// Returns `None` if the cell is already occupied (double-spawn guard).
    pub fn spawn(&self, future: F) -> Option<NonNull<TaskHeader>> {
        self.spawned
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()?;

        // SAFETY: the CAS above gave us exclusive write access to the future
        // slot; no concurrent spawn or poll is possible at this point.
        unsafe {
            (*self.future.get()).write(future);
            self.header.install_poll_fn(Self::poll_erased);
        }
        Some(NonNull::from(&self.header))
    }

    /// Type-erased poll trampoline stored in `TaskHeader::poll_fn`.
    ///
    /// # Safety
    ///
    /// `task` must point to the `header` field of a live `TaskCell<F>`.
    /// Because `TaskCell<F>` is `#[repr(C)]` with `header` first, the header
    /// address equals the cell address, making the pointer cast valid.
    unsafe fn poll_erased(task: NonNull<TaskHeader>, cx: &mut Context<'_>) -> Poll<()> {
        // SAFETY: #[repr(C)] puts `header` at offset 0, so the TaskHeader
        // pointer equals the TaskCell pointer.
        let cell_ptr = task.as_ptr() as *mut TaskCell<F>;
        // SAFETY: the executor holds the Polling state, guaranteeing that no
        // other thread can concurrently access the future.
        unsafe {
            let future = Pin::new_unchecked((*(*cell_ptr).future.get()).assume_init_mut());
            future.poll(cx)
        }
    }
}

// ---------------------------------------------------------------------------
// SingleCoreExecutor — drives a run queue to completion
// ---------------------------------------------------------------------------

/// Monomorphized [`WakeDispatcher`] used by [`SingleCoreExecutor::enqueue`]
/// to push a woken task back into a `RunQueue<CAP>`.
///
/// # Safety
///
/// `target` must point at a live `RunQueue<CAP>` owned by the executor
/// that originally bound this dispatcher into the task's header. The
/// caller (the waker vtable) only invokes this when the task is in
/// `Ready` state, which on a single-core polled runtime means the
/// owning executor is not currently inside `poll_one`/`run_until_idle`.
unsafe fn run_queue_push_dispatcher<const CAP: usize>(
    target: *mut u8,
    header: NonNull<TaskHeader>,
) {
    let queue = unsafe { &mut *target.cast::<RunQueue<CAP>>() };
    let _ = queue.push(header);
}

/// Minimal single-core async executor.
///
/// Drives a fixed-capacity [`RunQueue`] of type-erased tasks. Each call to
/// [`poll_one`] dequeues one task, invokes its poll function, and re-enqueues
/// it if a wake arrived during the poll. [`run_until_idle`] loops until the
/// queue drains.
///
/// The executor is `!Send` and `!Sync` through its `RunQueue` and must only
/// be used from the core that owns it.
///
/// [`poll_one`]: SingleCoreExecutor::poll_one
/// [`run_until_idle`]: SingleCoreExecutor::run_until_idle
pub struct SingleCoreExecutor<const CAP: usize> {
    run_queue: RunQueue<CAP>,
}

impl<const CAP: usize> SingleCoreExecutor<CAP> {
    /// Creates an idle executor with an empty run queue.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            run_queue: RunQueue::new(),
        }
    }

    /// Returns `true` if the run queue holds no tasks.
    #[must_use]
    pub fn is_idle(&self) -> bool {
        self.run_queue.is_empty()
    }

    /// Enqueues a task header returned by [`TaskCell::spawn`].
    ///
    /// Binds the task's waker to this executor's run queue so that
    /// external wakes (delivered from outside a poll) re-enqueue the
    /// task automatically. Returns `false` if the run queue is full.
    pub fn enqueue(&mut self, task: NonNull<TaskHeader>) -> bool {
        unsafe {
            // SAFETY: this executor owns its run queue for as long as it
            // exists; the dispatcher we register is the monomorphized
            // pusher for `RunQueue<CAP>`. The caller is responsible for
            // keeping the executor alive at least as long as any waker
            // clone of `task`.
            task.as_ref().bind_wake_target(
                core::ptr::addr_of_mut!(self.run_queue).cast::<u8>(),
                run_queue_push_dispatcher::<CAP>,
            );
        }
        self.run_queue.push(task)
    }

    /// Dequeues and polls one task.
    ///
    /// Returns `true` if a task was polled, `false` if the queue was empty.
    ///
    /// # Safety
    ///
    /// Every header in the run queue must have been produced by a live
    /// [`TaskCell::spawn`] call whose [`TaskCell`] has not been dropped or
    /// moved since spawning.
    pub unsafe fn poll_one(&mut self) -> bool {
        let task_ptr = match self.run_queue.pop() {
            Some(ptr) => ptr,
            None => return false,
        };

        // SAFETY: upheld by the caller.
        let header = unsafe { task_ptr.as_ref() };
        header.begin_poll();

        let waker = unsafe { make_task_waker(task_ptr) };
        let cx = &mut Context::from_waker(&waker);

        // SAFETY: the task is in Polling state and has a poll_fn installed.
        let result = unsafe { TaskHeader::poll(task_ptr, cx) };

        match result {
            Poll::Ready(()) => {
                header.complete();
            }
            Poll::Pending => match header.finish_pending_poll() {
                PendingDisposition::Parked => {}
                PendingDisposition::Requeue(_) => {
                    // A wake arrived during poll — re-enqueue immediately.
                    let _ = self.run_queue.push(task_ptr);
                }
            },
        }
        true
    }

    /// Polls tasks until the run queue is empty.
    ///
    /// # Safety
    ///
    /// Same as [`poll_one`](SingleCoreExecutor::poll_one).
    pub unsafe fn run_until_idle(&mut self) {
        // SAFETY: upheld by the caller.
        while unsafe { self.poll_one() } {}
    }
}

impl<const CAP: usize> Default for SingleCoreExecutor<CAP> {
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
/// no-op. `wake` and `wake_by_ref` call `TaskHeader::wake()` and, when
/// that returns [`WakeDisposition::Enqueue`], invoke the task's bound
/// wake dispatcher (see [`TaskHeader::bind_wake_target`]) so an
/// external wake (e.g. an I/O drainer between executor polls) lands
/// the task back in the owning [`RunQueue`].
///
/// Tasks that have not been bound to an executor fall through to a
/// no-op past the atomic state transition; that mirrors the original
/// prototype behavior and is appropriate for wakes that arrive during
/// a poll (the executor handles those via `finish_pending_poll`).
static TASK_HEADER_WAKER_VTABLE: RawWakerVTable = RawWakerVTable::new(
    // clone
    |ptr| RawWaker::new(ptr, &TASK_HEADER_WAKER_VTABLE),
    // wake (consumes self; no ref count to drop)
    |ptr| {
        // Safety: ptr is a valid *const TaskHeader for the life of the waker.
        let header = unsafe { &*(ptr as *const TaskHeader) };
        if let WakeDisposition::Enqueue(_) = header.wake() {
            header.invoke_wake_dispatcher();
        }
    },
    // wake_by_ref
    |ptr| {
        let header = unsafe { &*(ptr as *const TaskHeader) };
        if let WakeDisposition::Enqueue(_) = header.wake() {
            header.invoke_wake_dispatcher();
        }
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
    use super::{
        PendingDisposition, RunQueue, SingleCoreExecutor, TaskCell, TaskHeader, TaskState,
        WakeDisposition,
    };
    use core::future::Future;
    use core::pin::Pin;
    use core::task::{Context, Poll};
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

    // --- SingleCoreExecutor + TaskCell tests ---

    /// A future that completes immediately on its first poll.
    struct ReadyFuture;

    impl Future for ReadyFuture {
        type Output = ();
        fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
            Poll::Ready(())
        }
    }

    /// A future that returns `Pending` for `n` polls, self-waking each time,
    /// then returns `Ready`.
    struct CountdownFuture {
        remaining: u32,
    }

    impl Future for CountdownFuture {
        type Output = ();
        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            if self.remaining == 0 {
                Poll::Ready(())
            } else {
                self.remaining -= 1;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }
    }

    #[test]
    fn executor_runs_immediately_ready_task_to_completion() {
        let cell = TaskCell::<ReadyFuture>::new(CoreId(0));
        let header = cell.spawn(ReadyFuture).expect("spawn succeeds on fresh cell");

        let mut executor = SingleCoreExecutor::<4>::new();
        assert!(executor.enqueue(header));

        // SAFETY: cell is live for the duration of this test.
        unsafe { executor.run_until_idle() };

        assert_eq!(unsafe { header.as_ref() }.state(), TaskState::Complete);
        assert!(executor.is_idle());
    }

    #[test]
    fn executor_double_spawn_returns_none() {
        let cell = TaskCell::<ReadyFuture>::new(CoreId(0));
        assert!(cell.spawn(ReadyFuture).is_some());
        assert!(cell.spawn(ReadyFuture).is_none());
    }

    #[test]
    fn executor_runs_self_waking_task_through_multiple_polls() {
        let cell = TaskCell::<CountdownFuture>::new(CoreId(0));
        let header = cell
            .spawn(CountdownFuture { remaining: 3 })
            .expect("spawn succeeds");

        let mut executor = SingleCoreExecutor::<4>::new();
        assert!(executor.enqueue(header));

        // SAFETY: cell is live for the duration of this test.
        unsafe { executor.run_until_idle() };

        assert_eq!(unsafe { header.as_ref() }.state(), TaskState::Complete);
    }

    /// A future that captures its waker on the first poll and parks
    /// (without self-waking). The test code fires the stashed waker
    /// from outside any poll to simulate an external drainer.
    #[derive(Default)]
    struct WakerHolder {
        waker: std::sync::Mutex<Option<core::task::Waker>>,
    }

    struct ExternalWakeFuture {
        holder: std::sync::Arc<WakerHolder>,
        polled: bool,
    }

    impl Future for ExternalWakeFuture {
        type Output = ();
        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            if !self.polled {
                *self.holder.waker.lock().unwrap() = Some(cx.waker().clone());
                self.polled = true;
                Poll::Pending
            } else {
                Poll::Ready(())
            }
        }
    }

    #[test]
    fn waker_re_enqueues_parked_task_after_external_wake() {
        let holder = std::sync::Arc::new(WakerHolder::default());
        let cell = TaskCell::<ExternalWakeFuture>::new(CoreId(0));
        let header = cell
            .spawn(ExternalWakeFuture {
                holder: holder.clone(),
                polled: false,
            })
            .expect("spawn succeeds");

        let mut executor = SingleCoreExecutor::<4>::new();
        assert!(executor.enqueue(header));

        // First pass: the task polls once, stashes its waker, and parks.
        // SAFETY: `cell` outlives the executor.
        unsafe { executor.run_until_idle() };
        assert_eq!(unsafe { header.as_ref() }.state(), TaskState::Parked);
        assert!(executor.is_idle());

        // Fire the stashed waker from outside any poll. This is the
        // path that used to silently lose the wake in the prototype
        // vtable.
        let waker = holder
            .waker
            .lock()
            .unwrap()
            .take()
            .expect("waker was stashed");
        waker.wake();

        // The bound dispatcher should have re-enqueued the task.
        assert!(!executor.is_idle());

        // SAFETY: same as above.
        unsafe { executor.run_until_idle() };
        assert_eq!(unsafe { header.as_ref() }.state(), TaskState::Complete);
    }
}
