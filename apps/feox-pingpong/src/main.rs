#![no_std]
#![no_main]

//! feox-pingpong: two processes from one image, IPC over shared memory.
//!
//! The kernel maps one shared R+W page at [`SHARED_VA`] into both processes
//! and passes a role in `a0`: role 0 produces, role 1 consumes. The page
//! holds two [`EventSlot`]s (data-ready and ack) and a one-word mailbox.
//! Each round the producer writes a payload, signals `data`, and parks until
//! the matching ack; the consumer parks until `data` advances, reads the
//! payload, and signals `ack`. Four rounds of real block/wake cycles, then
//! both exit with payload-sum-derived values the kernel predicts. Distinct
//! `0xbNN` exits name a failed step.

use feox_asi::{Duration, EventSlot};
use feox_libos as libos;

/// Shared page VA (mapped into both processes by the kernel demo).
const SHARED_VA: usize = 0x2_2000_0000;
const ROUNDS: u64 = 4;
/// Park timeout: 2 s (200 ticks at the 100 Hz scheduler) — far above the
/// couple of ticks a wake actually takes, far below the run's tick budget…
/// which would have stopped a stuck run long before this fires.
const TIMEOUT: Duration = Duration::from_nanos(2_000_000_000);

// Shared page layout.
fn data_slot() -> &'static EventSlot {
    // SAFETY: the kernel maps a zeroed R+W page at SHARED_VA in both
    // processes; EventSlot is a single AtomicU64.
    unsafe { &*(SHARED_VA as *const EventSlot) }
}
fn ack_slot() -> &'static EventSlot {
    // SAFETY: as above, offset 8 within the page.
    unsafe { &*((SHARED_VA + 8) as *const EventSlot) }
}
const MAILBOX: *mut u64 = (SHARED_VA + 16) as *mut u64;

#[unsafe(no_mangle)]
extern "C" fn _start(role: usize) -> ! {
    match role {
        0 => producer(),
        1 => consumer(),
        _ => libos::exit(0xb10),
    }
}

/// Parks (repeatedly, in case of spurious wakes) until `slot`'s counter
/// reaches `target`. False on error or timeout.
fn park_until(slot: &EventSlot, target: u64) -> bool {
    loop {
        let current = slot.load();
        if current >= target {
            return true;
        }
        match libos::park(slot, current, TIMEOUT) {
            Some(true) => {}           // woken: re-check the counter
            Some(false) | None => return false, // timeout or error
        }
    }
}

fn producer() -> ! {
    let mut sum = 0u64;
    for i in 0..ROUNDS {
        let payload = 1000 + i * i;
        // SAFETY: the mailbox word lives in the shared R+W page.
        unsafe { MAILBOX.write_volatile(payload) };
        sum += payload;
        data_slot().signal();
        if !park_until(ack_slot(), i + 1) {
            libos::exit(0xb11);
        }
    }
    libos::exit((sum % 65521) as usize)
}

fn consumer() -> ! {
    let mut sum = 0u64;
    for i in 0..ROUNDS {
        if !park_until(data_slot(), i + 1) {
            libos::exit(0xb12);
        }
        // SAFETY: the mailbox word lives in the shared R+W page.
        sum += unsafe { MAILBOX.read_volatile() };
        ack_slot().signal();
    }
    libos::exit(((sum + ROUNDS) % 65521) as usize)
}
