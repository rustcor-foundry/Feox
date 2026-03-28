//! Architecture selection and early architecture support entrypoints.

use core::fmt;
use core::panic::PanicInfo;

#[cfg(target_arch = "x86_64")]
pub mod x86_64;

#[cfg(target_arch = "x86_64")]
mod selected {
    pub use super::x86_64::{
        cpu::{
            disable_interrupts, hlt_loop, invalidate_page, read_cr3, read_rsp,
            switch_page_table_root_and_jump, switch_stack_and_jump, trigger_breakpoint,
        },
        debugcon, gdt, idt, panic, serial,
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
    selected::debugcon::init();
}

/// Returns the active top-level page-table root physical address.
#[must_use]
pub fn active_page_table_root() -> u64 {
    selected::read_cr3()
}

/// Returns the current bootstrap stack pointer.
#[must_use]
pub fn current_stack_pointer() -> u64 {
    selected::read_rsp()
}

/// Switches to a supplied page-table root and jumps to a prepared entrypoint.
///
/// Safety requirements are architecture-specific and enforced by the caller.
pub unsafe fn switch_page_table_root_and_jump(root: u64, stack: u64, entry: u64) -> ! {
    unsafe { selected::switch_page_table_root_and_jump(root, stack, entry) }
}

/// Switches to a supplied stack and jumps to a prepared entrypoint.
///
/// Safety requirements are architecture-specific and enforced by the caller.
pub unsafe fn switch_stack_and_jump(stack: u64, entry: u64) -> ! {
    unsafe { selected::switch_stack_and_jump(stack, entry) }
}

/// Reloads the bootstrap descriptor tables from supplied higher-half aliases.
///
/// Safety requirements are architecture-specific and enforced by the caller.
pub unsafe fn reload_descriptor_tables(gdt_base: u64, idt_base: u64, idt_handler_delta: u64) {
    unsafe {
        selected::gdt::reload_with_base(gdt_base);
        selected::idt::relocate_and_reload(idt_base, idt_handler_delta);
    }
}

/// Invalidates the TLB entry for a single virtual address on the current core.
pub fn invalidate_page(virt: u64) {
    selected::invalidate_page(virt);
}

/// Raises a controlled software breakpoint through the active IDT.
pub fn trigger_breakpoint() {
    selected::trigger_breakpoint();
}

/// Writes preformatted early-boot output through the selected architecture console.
pub fn write_fmt(args: fmt::Arguments<'_>) {
    selected::serial::write_fmt(args);
    selected::debugcon::write_fmt(args);
}

/// Prints an early panic and halts forever.
pub fn panic_handle(info: &PanicInfo<'_>) -> ! {
    selected::panic::handle(info)
}

/// Enters the architecture's known-good halt loop.
pub fn halt_loop() -> ! {
    selected::hlt_loop()
}
