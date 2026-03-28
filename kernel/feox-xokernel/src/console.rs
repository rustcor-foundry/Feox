//! Early kernel console facade.

use core::fmt;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::arch;

/// Set to `true` once `init` has completed at least once.
///
/// Exception handlers use this to avoid re-entering `init` when the console
/// is already live, which would otherwise create a recursive-fault window if
/// the initialization path itself raised an exception.
static CONSOLE_READY: AtomicBool = AtomicBool::new(false);

/// Initializes the early console for the selected architecture.
pub fn init() {
    arch::console_init();
    CONSOLE_READY.store(true, Ordering::Release);
}

/// Returns `true` if `init` has completed at least once.
///
/// Safe to call from exception handlers before any synchronization is available.
#[must_use]
pub fn is_ready() -> bool {
    CONSOLE_READY.load(Ordering::Acquire)
}

/// Writes preformatted output to the early console.
pub fn write_fmt(args: fmt::Arguments<'_>) {
    arch::write_fmt(args);
}
