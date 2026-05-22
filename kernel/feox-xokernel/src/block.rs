//! Bootstrap-scoped kernel block I/O API.
//!
//! Wraps the NVMe submit/drain primitives behind a small free-function
//! surface so other kernel subsystems can issue a read without touching
//! `feox_nvme::SubmissionQueueEntry` directly:
//!
//! ```ignore
//! let future = block::read(nsid, lba, buf_phys)?;
//! let completion = future.await;
//! ```
//!
//! The state lives in a single retained device (no multi-namespace or
//! multi-device support yet); the runtime is expected to call
//! [`block::drain`] between executor polls so completions land in the
//! retained [`feox_nvme::NvmeQueuePair`] and wake any pending futures.

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};

use crate::nvme::{CompletionQueueEntry, ControllerRegisters, SubmissionQueueEntry};
use feox_nvme::{NvmeCompletion, NvmeError, NvmeIoFuture, NvmeQueuePair, NvmeStatus};

/// Errors returned by the block API surface.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BlockError {
    /// [`initialize`] has not been called for the global block device.
    NotInitialized,
    /// [`initialize`] was called more than once without an intervening
    /// [`shutdown`].
    AlreadyInitialized,
    /// The underlying NVMe layer reported an error during submit.
    Nvme(NvmeError),
}

impl From<NvmeError> for BlockError {
    fn from(err: NvmeError) -> Self {
        Self::Nvme(err)
    }
}

/// Configuration handed to [`initialize`] after admin queue bring-up.
#[derive(Clone, Copy, Debug)]
pub struct BlockDeviceConfig {
    /// Mapped controller register bank.
    pub registers: ControllerRegisters,
    /// Kernel virtual base of the I/O submission queue buffer.
    pub sq_virt: *mut SubmissionQueueEntry,
    /// Kernel virtual base of the I/O completion queue buffer.
    pub cq_virt: *mut CompletionQueueEntry,
    /// Entry count of the I/O submission queue.
    pub sq_entries: u16,
    /// Entry count of the I/O completion queue.
    pub cq_entries: u16,
    /// NVMe I/O queue identifier (the `qid` argument to admin
    /// `Create I/O Submission Queue` / `Create I/O Completion Queue`).
    pub io_qid: u16,
}

/// Retained device state owned by the global block layer.
struct BlockDeviceState {
    registers: ControllerRegisters,
    sq_virt: *mut SubmissionQueueEntry,
    cq_virt: *mut CompletionQueueEntry,
    sq_tail: u16,
    cq_head: u16,
    cq_phase: u8,
    sq_entries: u16,
    cq_entries: u16,
    io_qid: u16,
    queue_pair: NvmeQueuePair<8>,
}

static mut BLOCK_DEVICE: Option<BlockDeviceState> = None;

unsafe fn device() -> Option<&'static mut BlockDeviceState> {
    unsafe {
        let ptr = &raw mut BLOCK_DEVICE;
        (*ptr).as_mut()
    }
}

/// Initializes the global block device. Returns
/// [`BlockError::AlreadyInitialized`] if a previous [`initialize`] was
/// not paired with [`shutdown`].
pub fn initialize(config: BlockDeviceConfig) -> Result<(), BlockError> {
    unsafe {
        let ptr = &raw mut BLOCK_DEVICE;
        if (*ptr).is_some() {
            return Err(BlockError::AlreadyInitialized);
        }
        *ptr = Some(BlockDeviceState {
            registers: config.registers,
            sq_virt: config.sq_virt,
            cq_virt: config.cq_virt,
            sq_tail: 0,
            cq_head: 0,
            cq_phase: 1,
            sq_entries: config.sq_entries,
            cq_entries: config.cq_entries,
            io_qid: config.io_qid,
            queue_pair: NvmeQueuePair::<8>::new(),
        });
    }
    Ok(())
}

/// Clears the global block device. Outstanding futures observe the
/// underlying queue pair as failed via [`feox_nvme::NvmeQueuePair::fail`]
/// before the state is dropped.
pub fn shutdown() {
    unsafe {
        let ptr = &raw mut BLOCK_DEVICE;
        if let Some(state) = (*ptr).as_mut() {
            state.queue_pair.fail(NvmeError::DeviceRemoved);
        }
        *ptr = None;
    }
}

/// Submits a single-LBA read against `nsid`. Returns a future that
/// resolves when the controller completes the command. The caller must
/// keep [`drain`] running while the future is pending so completions
/// land in the queue pair.
pub fn read(nsid: u32, lba: u64, buf_phys: u64) -> Result<NvmeIoFuture<8>, BlockError> {
    let state = unsafe { device() }.ok_or(BlockError::NotInitialized)?;
    let (cid, future) = state.queue_pair.submit()?;
    let sqe = SubmissionQueueEntry::nvm_read(nsid, lba, 0, buf_phys, cid);
    unsafe {
        // SAFETY: `sq_virt` is the freshly allocated I/O SQ page (kernel
        // direct map); we own slot `sq_tail` because we just reserved a
        // CID from the queue pair.
        core::ptr::write_volatile(state.sq_virt.add(state.sq_tail as usize), sqe);
    }
    state.sq_tail = (state.sq_tail + 1) % state.sq_entries;
    state
        .registers
        .ring_sq_tail_doorbell(state.io_qid, state.sq_tail);
    Ok(future)
}

/// Pumps completions from the I/O CQ into the retained queue pair.
/// Returns `true` if any completion was delivered. The queue pair's
/// `complete` call fires the waker registered by the corresponding
/// future, which (now that the executor enqueue gap is closed) lands
/// the awaiting task back in the run queue automatically.
pub fn drain() -> bool {
    let state = match unsafe { device() } {
        Some(state) => state,
        None => return false,
    };
    let mut drained = false;
    loop {
        let cqe = unsafe {
            // SAFETY: `cq_virt` is a kernel-only direct-map alias; the
            // controller writes CQEs into it via DMA.
            core::ptr::read_volatile(state.cq_virt.add(state.cq_head as usize))
        };
        if cqe.phase() != state.cq_phase {
            return drained;
        }
        let sf = cqe.status_field();
        let status = NvmeStatus {
            sct: ((sf >> 8) & 0x7) as u8,
            sc: (sf & 0xFF) as u8,
            dnr: ((sf >> 14) & 1) != 0,
        };
        state.queue_pair.complete(NvmeCompletion {
            cid: cqe.command_id(),
            status,
        });
        state.cq_head += 1;
        if state.cq_head >= state.cq_entries {
            state.cq_head = 0;
            state.cq_phase ^= 1;
        }
        state
            .registers
            .ring_cq_head_doorbell(state.io_qid, state.cq_head);
        drained = true;
    }
}

/// Returns `true` if [`initialize`] has been called and the device
/// hasn't been [`shutdown`].
#[must_use]
pub fn is_initialized() -> bool {
    unsafe { device() }.is_some()
}

/// Cooperative yield: returns `Pending` once (after self-waking), then
/// `Ready` on the next poll. Used by [`drainer_task`] to give other
/// tasks a chance to run between drain passes while still keeping the
/// drainer scheduled.
struct YieldNow {
    yielded: bool,
}

impl Future for YieldNow {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.yielded {
            Poll::Ready(())
        } else {
            self.yielded = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }
}

fn yield_now() -> YieldNow {
    YieldNow { yielded: false }
}

/// Background drainer task: pumps completions from the NVMe I/O CQ on
/// every executor pass.
///
/// Spawn this once into the same executor that drives the rest of the
/// async workload (typically before the first call to [`read`]). After
/// each drain attempt the task yields back via [`YieldNow`] (a
/// self-waking `Pending`), so the executor immediately re-queues it and
/// other ready tasks get to run between drain passes.
///
/// The future never returns. Callers drive the executor with
/// [`feox_async::SingleCoreExecutor::poll_one`] in a loop and break
/// once their real workload task reaches `TaskState::Complete`;
/// `run_until_idle` would never return while the drainer is enqueued.
pub async fn drainer_task() {
    loop {
        drain();
        yield_now().await;
    }
}
