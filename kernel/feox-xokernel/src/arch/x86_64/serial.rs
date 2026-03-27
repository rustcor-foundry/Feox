//! Early serial console support via the legacy 16550-compatible COM1 port.

use core::arch::asm;
use core::fmt::{self, Write};
use core::sync::atomic::{AtomicBool, Ordering};

const COM1_BASE: u16 = 0x3F8;
const REG_DATA: u16 = 0;
const REG_INTERRUPT_ENABLE: u16 = 1;
const REG_FIFO_CONTROL: u16 = 2;
const REG_LINE_CONTROL: u16 = 3;
const REG_MODEM_CONTROL: u16 = 4;
const REG_LINE_STATUS: u16 = 5;

const LINE_STATUS_TRANSMIT_HOLDING_EMPTY: u8 = 1 << 5;

static INITIALIZED: AtomicBool = AtomicBool::new(false);

/// Initializes COM1 in 38400 8-N-1 mode.
pub fn init() {
    if INITIALIZED.swap(true, Ordering::AcqRel) {
        return;
    }

    write_port(COM1_BASE + REG_INTERRUPT_ENABLE, 0x00);
    write_port(COM1_BASE + REG_LINE_CONTROL, 0x80);
    write_port(COM1_BASE + REG_DATA, 0x03);
    write_port(COM1_BASE + REG_INTERRUPT_ENABLE, 0x00);
    write_port(COM1_BASE + REG_LINE_CONTROL, 0x03);
    write_port(COM1_BASE + REG_FIFO_CONTROL, 0xC7);
    write_port(COM1_BASE + REG_MODEM_CONTROL, 0x0B);
}

/// Returns a formatter-backed serial writer for early boot logging.
#[must_use]
pub fn writer() -> SerialWriter {
    init();
    SerialWriter
}

/// Writes preformatted arguments to the serial console.
pub fn write_fmt(args: fmt::Arguments<'_>) {
    let _ = writer().write_fmt(args);
}

/// Stateless early-boot serial writer.
#[derive(Clone, Copy, Debug, Default)]
pub struct SerialWriter;

impl Write for SerialWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for byte in s.bytes() {
            if byte == b'\n' {
                write_byte(b'\r');
            }
            write_byte(byte);
        }

        Ok(())
    }
}

fn write_byte(byte: u8) {
    init();

    while read_port(COM1_BASE + REG_LINE_STATUS) & LINE_STATUS_TRANSMIT_HOLDING_EMPTY == 0 {}

    write_port(COM1_BASE + REG_DATA, byte);
}

fn write_port(port: u16, value: u8) {
    unsafe {
        // Safety: port I/O is the architectural interface to the UART.
        asm!(
            "out dx, al",
            in("dx") port,
            in("al") value,
            options(nomem, nostack, preserves_flags)
        );
    }
}

fn read_port(port: u16) -> u8 {
    let value: u8;

    unsafe {
        // Safety: port I/O is the architectural interface to the UART.
        asm!(
            "in al, dx",
            in("dx") port,
            out("al") value,
            options(nomem, nostack, preserves_flags)
        );
    }

    value
}
