//! Minimal SiFive-compatible PLIC driver (milestone 19).
//!
//! Routes external device interrupts to the boot hart's S-mode context on
//! QEMU virt: set the source's priority, enable it in the context's enable
//! bitmap, zero the context's threshold, and unmask `sie.SEIE`. The trap
//! dispatcher calls [`handle_external`] for `scause` 9, which runs the
//! claim/complete protocol and forwards net-device events to the ASI IRQ
//! lane (`syscall::on_net_rx_event`) and the scheduler's wake path.
//!
//! Like the other QEMU-window devices, the base address is the QEMU virt
//! fixed layout; a device-tree-derived base lands with real-hardware bring-up.

use core::arch::asm;
use core::sync::atomic::{AtomicUsize, Ordering};

use super::trap::TrapFrame;

/// QEMU virt PLIC window (mapped by `build_kernel_address_space`).
pub const PLIC_BASE: usize = 0x0c00_0000;
/// PLIC window size.
pub const PLIC_SIZE: usize = 0x60_0000;

/// `sie.SEIE` — supervisor external interrupt enable (bit 9).
const SIE_SEIE: u64 = 1 << 9;

const PRIORITY_BASE: usize = 0x0;
const ENABLE_BASE: usize = 0x2000;
const ENABLE_STRIDE: usize = 0x80;
const CONTEXT_BASE: usize = 0x20_0000;
const CONTEXT_STRIDE: usize = 0x1000;

/// The boot hart's S-mode PLIC context (`2 * hart + 1`), set by [`init`].
static S_CONTEXT: AtomicUsize = AtomicUsize::new(0);

fn reg_w(offset: usize, value: u32) {
    // SAFETY: offset is within the mapped PLIC window.
    unsafe { core::ptr::write_volatile((PLIC_BASE + offset) as *mut u32, value) };
}

fn reg_r(offset: usize) -> u32 {
    // SAFETY: offset is within the mapped PLIC window.
    unsafe { core::ptr::read_volatile((PLIC_BASE + offset) as *const u32) }
}

/// Enables `irq` for the boot hart's S-mode context and drains anything
/// already pending (stale device state from the polled bring-up would
/// otherwise fire the moment `sie.SEIE` is set).
pub fn init(boot_hart: usize, irq: u32) {
    let context = 2 * boot_hart + 1;
    S_CONTEXT.store(context, Ordering::Release);

    reg_w(PRIORITY_BASE + 4 * irq as usize, 1);
    let enable = ENABLE_BASE + context * ENABLE_STRIDE + (irq as usize / 32) * 4;
    reg_w(enable, reg_r(enable) | (1 << (irq % 32)));
    reg_w(CONTEXT_BASE + context * CONTEXT_STRIDE, 0); // threshold: allow all

    // Drain stale pendings: claim, quiesce the device (ack its ISR), complete.
    while let Some(pending) = claim() {
        if Some(pending) == super::net::irq_number() {
            let _ = super::net::on_interrupt();
        }
        complete(pending);
    }
}

/// Unmasks supervisor external interrupts (`sie.SEIE`). Delivery still
/// requires `sstatus.SIE` in S-mode; U-mode always takes S-level interrupts.
pub fn enable_external() {
    // SAFETY: setting sie.SEIE only unmasks the external interrupt source.
    unsafe { asm!("csrrs zero, sie, {0}", in(reg) SIE_SEIE, options(nomem, nostack)) };
}

/// Claims the highest-priority pending interrupt for our context (0 = none).
fn claim() -> Option<u32> {
    let context = S_CONTEXT.load(Ordering::Acquire);
    let irq = reg_r(CONTEXT_BASE + context * CONTEXT_STRIDE + 4);
    (irq != 0).then_some(irq)
}

/// Signals completion of a claimed interrupt.
fn complete(irq: u32) {
    let context = S_CONTEXT.load(Ordering::Acquire);
    reg_w(CONTEXT_BASE + context * CONTEXT_STRIDE + 4, irq);
}

/// Supervisor-external-interrupt handler (trap dispatcher, `scause` 9):
/// claim/complete each pending source, forward net RX progress to the ASI
/// IRQ lane, then give the scheduler a chance to wake parked threads (and to
/// leave its idle loop immediately rather than on the next tick).
pub fn handle_external(frame: &mut TrapFrame) {
    while let Some(irq) = claim() {
        if Some(irq) == super::net::irq_number() && super::net::on_interrupt() {
            super::syscall::on_net_rx_event();
        }
        complete(irq);
    }
    super::sched::on_event(frame);
}
