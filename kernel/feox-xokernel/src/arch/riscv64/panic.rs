//! Panic handling for the early riscv64 bootstrap path.
//!
//! Mirrors the x86_64 panic flow but without the runtime-context snapshot,
//! which depends on subsystems not yet ported to riscv64.

use core::panic::PanicInfo;

use super::cpu;

/// Prints the panic message on the early console and parks the hart forever.
pub fn handle(info: &PanicInfo<'_>) -> ! {
    cpu::disable_interrupts();

    // Bring the console up if a panic struck before (or during) init; idempotent.
    crate::console::init();

    crate::kprintln!();
    crate::kprintln!("[feox panic] {}", info);

    cpu::hlt_loop()
}
