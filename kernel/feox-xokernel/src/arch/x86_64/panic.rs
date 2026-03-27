//! Panic handling for the early x86_64 bootstrap path.

use core::panic::PanicInfo;

use super::cpu;

/// Prints a panic banner to the serial console and halts forever.
pub fn handle(info: &PanicInfo<'_>) -> ! {
    cpu::disable_interrupts();
    crate::console::init();
    crate::kprintln!("\n[feox panic] {}", info);

    cpu::hlt_loop()
}
