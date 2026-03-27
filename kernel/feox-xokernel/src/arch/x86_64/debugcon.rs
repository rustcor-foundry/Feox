//! QEMU debug console support via the legacy ISA debug port.

use core::arch::asm;
use core::fmt::{self, Write};

const DEBUGCON_PORT: u16 = 0x402;

/// Initializes the QEMU debug console path.
pub fn init() {}

/// Writes preformatted arguments to the debug console.
pub fn write_fmt(args: fmt::Arguments<'_>) {
    let _ = DebugconWriter.write_fmt(args);
}

#[derive(Clone, Copy, Debug, Default)]
struct DebugconWriter;

impl Write for DebugconWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for byte in s.bytes() {
            write_byte(byte);
        }
        Ok(())
    }
}

fn write_byte(byte: u8) {
    unsafe {
        asm!(
            "out dx, al",
            in("dx") DEBUGCON_PORT,
            in("al") byte,
            options(nomem, nostack, preserves_flags)
        );
    }
}
