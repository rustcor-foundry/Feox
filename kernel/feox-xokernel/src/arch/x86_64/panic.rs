//! Panic handling for the early x86_64 bootstrap path.

use core::panic::PanicInfo;

use super::cpu;

/// Prints a panic banner to the serial console and halts forever.
pub fn handle(info: &PanicInfo<'_>) -> ! {
    cpu::disable_interrupts();
    crate::console::init();
    crate::kprintln!("\n[feox panic] {}", info);
    if let Some(runtime) = crate::runtime_context::snapshot() {
        crate::kprintln!(
            "runtime: stage={} root={:#018x} window={:#018x}-{:#018x}",
            runtime.stage,
            runtime.active_root,
            runtime.kernel_window_base,
            runtime.kernel_window_end
        );
        crate::kprintln!(
            "runtime: entry={:#018x} stack={:#018x} pages={} data_page={:#018x}",
            runtime.alias_entry,
            runtime.alias_stack,
            runtime.kernel_pages_mapped,
            runtime.data_page
        );
    }
    if let Some(core) = crate::runtime_context::core() {
        crate::kprintln!(
            "runtime: core={} stage={} root={:#018x} stack={:#018x} entry={:#018x}",
            core.core_id.0,
            core.stage,
            core.active_root,
            core.stack_pointer,
            core.alias_entry
        );
    }

    cpu::hlt_loop()
}
