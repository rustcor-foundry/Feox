//! Early kernel console facade.

use core::fmt;

use crate::arch;

/// Initializes the early console for the selected architecture.
pub fn init() {
    arch::console_init();
}

/// Writes preformatted output to the early console.
pub fn write_fmt(args: fmt::Arguments<'_>) {
    arch::write_fmt(args);
}
