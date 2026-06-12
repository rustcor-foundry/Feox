//! Minimal write-only 16550/DW-APB UART console (milestone 25).
//!
//! The console starts on the SBI legacy `console_putchar` (always available
//! under OpenSBI on QEMU; *probably* available on vendor firmwares). Once the
//! device tree is parsed, the kernel upgrades to this native driver — the
//! insurance policy for boards whose OpenSBI lacks the legacy console.
//!
//! Deliberately tiny: the UART is used exactly as U-Boot left it. No baud,
//! line-control, or FIFO programming — just poll LSR.THRE and write THR.
//! Register addressing honors the device tree's `reg-shift` (register `i` at
//! `base + (i << shift)`) and `reg-io-width` (1- or 4-byte accesses), which
//! covers QEMU virt (shift 0, width 1), the JH7110, and the Ky X1 (both
//! shift 2, width 4).

use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

/// Transmit holding register index.
const THR: usize = 0;
/// Line status register index.
const LSR: usize = 5;
/// LSR: transmit holding register empty.
const LSR_THRE: u8 = 0x20;

/// Bound on the THRE poll. If the UART never drains (wrong base, dead
/// clock), the driver marks itself dead so output falls back to SBI instead
/// of hanging the kernel.
const SPIN_LIMIT: u32 = 1_000_000;

static BASE: AtomicUsize = AtomicUsize::new(0);
static SHIFT: AtomicU32 = AtomicU32::new(0);
static WIDTH: AtomicU32 = AtomicU32::new(1);

/// Activates the native UART at `base` (must already be mapped R+W).
pub fn init(base: usize, reg_shift: u32, reg_io_width: u32) {
    SHIFT.store(reg_shift, Ordering::Relaxed);
    WIDTH.store(reg_io_width, Ordering::Relaxed);
    BASE.store(base, Ordering::Release);
}

/// Whether the native UART is active.
#[must_use]
pub fn ready() -> bool {
    BASE.load(Ordering::Acquire) != 0
}

fn reg_read(base: usize, index: usize) -> u8 {
    let addr = base + (index << SHIFT.load(Ordering::Relaxed));
    if WIDTH.load(Ordering::Relaxed) == 4 {
        // SAFETY: mapped UART MMIO; 4-byte access per the device tree.
        (unsafe { core::ptr::read_volatile(addr as *const u32) } & 0xFF) as u8
    } else {
        // SAFETY: mapped UART MMIO.
        unsafe { core::ptr::read_volatile(addr as *const u8) }
    }
}

fn reg_write(base: usize, index: usize, value: u8) {
    let addr = base + (index << SHIFT.load(Ordering::Relaxed));
    if WIDTH.load(Ordering::Relaxed) == 4 {
        // SAFETY: mapped UART MMIO; 4-byte access per the device tree.
        unsafe { core::ptr::write_volatile(addr as *mut u32, u32::from(value)) };
    } else {
        // SAFETY: mapped UART MMIO.
        unsafe { core::ptr::write_volatile(addr as *mut u8, value) };
    }
}

/// Emits one byte, returning false (and deactivating the driver) if the
/// transmitter never drained — the caller then falls back to SBI.
pub fn putb(byte: u8) -> bool {
    let base = BASE.load(Ordering::Acquire);
    if base == 0 {
        return false;
    }
    let mut spins = 0u32;
    while reg_read(base, LSR) & LSR_THRE == 0 {
        spins += 1;
        if spins >= SPIN_LIMIT {
            BASE.store(0, Ordering::Release);
            return false;
        }
        core::hint::spin_loop();
    }
    reg_write(base, THR, byte);
    true
}
