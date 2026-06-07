//! feox-async runtime bring-up on riscv64 (milestone 5).
//!
//! Proves the portable async executor (`feox-async`) drives futures on
//! riscv64: two self-waking tasks are spawned onto a single-core executor and
//! run to completion. The executor is static-storage (no heap), so the tasks
//! live in `static` [`TaskCell`]s with a named future type (`Yield`) rather
//! than an unnameable async block.

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};

use crate::asi::CoreId;
use crate::runtime::{SingleCoreExecutor, TaskCell};

/// A future that yields `remaining` times — re-waking itself each poll — before
/// completing. Printing on every poll makes the executor's scheduling visible
/// (the two tasks interleave through the run queue).
struct Yield {
    id: u32,
    remaining: u32,
}

impl Future for Yield {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.remaining == 0 {
            crate::kprintln!("[feox]   task {} complete", self.id);
            return Poll::Ready(());
        }
        self.remaining -= 1;
        crate::kprintln!(
            "[feox]   task {} tick ({} remaining)",
            self.id,
            self.remaining
        );
        // Wake during poll: the task header records the wake and the executor
        // re-enqueues us after this poll returns Pending.
        cx.waker().wake_by_ref();
        Poll::Pending
    }
}

/// Boot core affinity for the demo tasks.
const CORE0: CoreId = CoreId(0);

/// Static task storage (the executor holds no heap; headers must outlive it).
static TASK_A: TaskCell<Yield> = TaskCell::new(CORE0);
static TASK_B: TaskCell<Yield> = TaskCell::new(CORE0);

/// Spawns two self-waking tasks on a single-core executor and runs them to
/// completion, demonstrating that feox-async schedules futures on riscv64.
pub fn demo() {
    let mut executor = SingleCoreExecutor::<8>::new();

    if let Some(task) = TASK_A.spawn(Yield { id: 0, remaining: 3 }) {
        executor.enqueue(task);
    }
    if let Some(task) = TASK_B.spawn(Yield { id: 1, remaining: 2 }) {
        executor.enqueue(task);
    }

    // SAFETY: both headers come from the live `static` TaskCells above, which
    // outlive `executor`, satisfying `run_until_idle`'s requirement that every
    // queued header belong to a spawned, un-dropped TaskCell.
    unsafe {
        executor.run_until_idle();
    }

    crate::kprintln!(
        "[feox] milestone 5: feox-async executor drained (idle={})",
        executor.is_idle()
    );
}
