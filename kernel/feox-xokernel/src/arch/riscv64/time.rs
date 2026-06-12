//! Supervisor timer interrupts via the SBI timer (milestone 9).
//!
//! Enables `sstatus.SIE` + `sie.STIE`, arms the next deadline through the SBI
//! `set_timer` call (which also clears the pending timer interrupt), and counts
//! ticks from the trap dispatcher. This is the first interrupt source the
//! kernel handles — the foundation for preemption and interrupt-driven I/O.

use core::arch::asm;
use core::sync::atomic::{AtomicU64, Ordering};

/// Legacy SBI `set_timer` (EID 0x00).
const SBI_SET_TIMER: usize = 0x00;
/// `sie.STIE` — supervisor timer interrupt enable (bit 5).
const SIE_STIE: u64 = 1 << 5;
/// `sstatus.SIE` — supervisor global interrupt enable (bit 1).
const SSTATUS_SIE: u64 = 1 << 1;

static TICKS: AtomicU64 = AtomicU64::new(0);
/// Timebase ticks between interrupts (timebase_hz / TICK_HZ).
static INTERVAL: AtomicU64 = AtomicU64::new(0);

/// Reads the `time` CSR (current timebase count).
fn read_time() -> u64 {
    let value: u64;
    // SAFETY: `rdtime` reads the time CSR and has no side effects.
    unsafe { asm!("rdtime {0}", out(reg) value, options(nomem, nostack)) };
    value
}

/// Arms the next supervisor timer interrupt via SBI (also clears any pending).
fn arm_next() {
    let deadline = read_time() + INTERVAL.load(Ordering::Relaxed);
    // SAFETY: legacy SBI set_timer — EID in a7, absolute time in a0.
    unsafe {
        asm!(
            "ecall",
            in("a7") SBI_SET_TIMER,
            inout("a0") deadline => _,
            lateout("a1") _,
            options(nostack),
        );
    }
}

/// Arms the periodic timer at `tick_hz` and unmasks supervisor timer
/// interrupts.
pub fn enable(timebase_hz: u64, tick_hz: u64) {
    INTERVAL.store(timebase_hz / tick_hz, Ordering::Relaxed);
    arm_next();
    // SAFETY: setting sie.STIE then sstatus.SIE permits timer-interrupt
    // delivery; stvec already points at the trap vector (trap::init).
    unsafe {
        asm!("csrrs zero, sie, {0}", in(reg) SIE_STIE, options(nomem, nostack));
        asm!("csrrs zero, sstatus, {0}", in(reg) SSTATUS_SIE, options(nomem, nostack));
    }
}

/// Masks supervisor timer interrupts (clears `sie.STIE`).
pub fn disable() {
    // SAFETY: clearing sie.STIE only masks the timer interrupt source.
    unsafe { asm!("csrrc zero, sie, {0}", in(reg) SIE_STIE, options(nomem, nostack)) };
}

/// Trap-dispatcher hook for a supervisor timer interrupt: count it and re-arm.
pub fn on_timer_interrupt() {
    TICKS.fetch_add(1, Ordering::Relaxed);
    arm_next();
}

/// Number of timer ticks taken so far.
#[must_use]
pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}
