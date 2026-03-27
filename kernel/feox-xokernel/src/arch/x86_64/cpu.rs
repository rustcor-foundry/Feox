//! Minimal CPU helpers for early boot.

use core::arch::asm;

/// Disables maskable interrupts on the current core.
pub fn disable_interrupts() {
    unsafe {
        // Safety: this is the architectural instruction for clearing IF.
        asm!("cli", options(nomem, nostack, preserves_flags));
    }
}

/// Halts the current core until the next interrupt.
pub fn halt() {
    unsafe {
        // Safety: this executes the architectural halt instruction.
        asm!("hlt", options(nomem, nostack));
    }
}

/// Enters an infinite halt loop after disabling interrupts.
pub fn hlt_loop() -> ! {
    disable_interrupts();

    loop {
        halt();
    }
}

/// Reads the current CR2 register value.
#[must_use]
pub fn read_cr2() -> u64 {
    let value: u64;

    unsafe {
        // Safety: reading CR2 is a side-effect-free architectural register read.
        asm!("mov {}, cr2", out(reg) value, options(nomem, nostack, preserves_flags));
    }

    value
}

/// Reads the current CR3 register value.
#[must_use]
pub fn read_cr3() -> u64 {
    let value: u64;

    unsafe {
        // Safety: reading CR3 is a side-effect-free architectural register read.
        asm!("mov {}, cr3", out(reg) value, options(nomem, nostack, preserves_flags));
    }

    value
}
