//! Architecture selection and early architecture support entrypoints.

use core::fmt;
use core::panic::PanicInfo;

#[cfg(target_arch = "x86_64")]
pub mod x86_64;

#[cfg(target_arch = "x86_64")]
mod selected {
    pub use super::x86_64::{
        cpu::{disable_interrupts, hlt_loop, read_cr3},
        gdt, idt, panic, serial,
    };
}

#[cfg(not(target_arch = "x86_64"))]
compile_error!("Feox currently supports only x86_64 kernel builds. See docs/ARM64_PORT_PLAN.md.");

/// Returns the name of the currently selected kernel architecture.
#[cfg(target_arch = "x86_64")]
pub const CURRENT_ARCH: &str = "x86_64";

/// Performs the earliest architecture initialization required by the bootstrap path.
pub fn early_init() {
    selected::disable_interrupts();
    console_init();
    selected::gdt::init();
    selected::idt::init();
}

/// Initializes the selected architecture's early console.
pub fn console_init() {
    selected::serial::init();
}

/// Returns the active top-level page-table root physical address.
#[must_use]
pub fn active_page_table_root() -> u64 {
    selected::read_cr3()
}

/// Writes preformatted early-boot output through the selected architecture console.
pub fn write_fmt(args: fmt::Arguments<'_>) {
    selected::serial::write_fmt(args);
}

/// Prints an early panic and halts forever.
pub fn panic_handle(info: &PanicInfo<'_>) -> ! {
    selected::panic::handle(info)
}

/// Enters the architecture's known-good halt loop.
pub fn halt_loop() -> ! {
    selected::hlt_loop()
}
