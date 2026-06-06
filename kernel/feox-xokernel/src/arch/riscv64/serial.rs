//! Early serial console via the SBI legacy `console_putchar` call.
//!
//! In S-mode the kernel has no direct UART ownership during early bring-up, so
//! the firmware (OpenSBI) provides the console through an `ecall`. The legacy
//! `console_putchar` extension (EID `0x01`) is universally available on the
//! SBI implementations Feox targets (QEMU `virt` / U-Boot OpenSBI). A native
//! 16550 driver replaces this once MMIO + the device tree are wired up.

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

impl Write for SerialWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for byte in s.bytes() {
            if byte == b'\n' {
                sbi_putchar(b'\r');
            }
            sbi_putchar(byte);
        }
        Ok(())
    }
}
