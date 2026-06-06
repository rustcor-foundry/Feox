//! Minimal CPU helpers for early riscv64 boot.

use core::arch::asm;

/// Disables supervisor interrupts by clearing `sstatus.SIE` (bit 1).
///
/// Standard (non-NMI) interrupts will not be delivered to the hart after this.
pub fn disable_interrupts() {
    // SAFETY: `csrci sstatus, 0x2` atomically clears the SIE bit; it has no
    // effect beyond masking supervisor interrupts on the current hart.
    unsafe {
        asm!("csrci sstatus, 0x2", options(nomem, nostack));
    }
}

/// Halts the current hart until the next interrupt (`wfi`).
pub fn halt() {
    // SAFETY: `wfi` is a hint instruction; at worst it behaves as a no-op.
    unsafe {
        asm!("wfi", options(nomem, nostack));
    }
}

/// Disables interrupts and enters an infinite `wfi` loop.
///
/// Used by panic and fatal-error paths so the hart parks without being
/// rescheduled. `wfi` keeps power down between any non-masked wakeups.
pub fn hlt_loop() -> ! {
    disable_interrupts();
    loop {
        halt();
    }
}
