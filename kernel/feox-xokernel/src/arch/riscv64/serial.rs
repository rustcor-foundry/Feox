//! Serial console: SBI `console_putchar` early, native UART once discovered.
//!
//! In S-mode the kernel has no direct UART ownership during early bring-up,
//! so the firmware (OpenSBI) provides the console through an `ecall`. After
//! the device tree is parsed, `riscv_main` upgrades to the native 16550
//! driver (`uart.rs`) — every byte then goes straight to the hardware, with
//! SBI as the automatic fallback if the UART is absent or unresponsive.

use core::fmt::{self, Write};

/// Legacy SBI extension id for `console_putchar`.
const SBI_CONSOLE_PUTCHAR: usize = 0x01;

/// Emits a single byte to the SBI console.
fn sbi_putchar(byte: u8) {
    // SAFETY: an `ecall` with the legacy `console_putchar` EID in `a7` and the
    // byte in `a0` is the architectural SBI interface. The call returns in
    // a0/a1 (which we discard); no memory is touched.
    unsafe {
        core::arch::asm!(
            "ecall",
            in("a7") SBI_CONSOLE_PUTCHAR,
            inout("a0") byte as usize => _,
            lateout("a1") _,
            options(nostack),
        );
    }
}

/// No-op: the SBI console needs no device initialization.
pub fn init() {}

/// Returns a formatter-backed serial writer for early boot logging.
#[must_use]
pub fn writer() -> SerialWriter {
    SerialWriter
}

/// Writes preformatted arguments to the SBI console.
pub fn write_fmt(args: fmt::Arguments<'_>) {
    let _ = writer().write_fmt(args);
}

/// Stateless early-boot serial writer over SBI.
#[derive(Clone, Copy, Debug, Default)]
pub struct SerialWriter;

/// Emits one byte: native UART when active, SBI otherwise.
fn putchar(byte: u8) {
    if !super::uart::putb(byte) {
        sbi_putchar(byte);
    }
}

impl Write for SerialWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for byte in s.bytes() {
            if byte == b'\n' {
                putchar(b'\r');
            }
            putchar(byte);
        }
        Ok(())
    }
}
