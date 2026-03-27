//! Minimal interrupt descriptor table support.

use core::arch::asm;
use core::mem::size_of;

use super::{exceptions, gdt};

const IDT_ENTRIES: usize = 256;
const INTERRUPT_GATE_PRESENT_RING0: u8 = 0x8E;

#[repr(C, packed)]
struct DescriptorTablePointer {
    limit: u16,
    base: u64,
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct IdtEntry {
    offset_low: u16,
    selector: u16,
    ist: u8,
    attributes: u8,
    offset_middle: u16,
    offset_high: u32,
    reserved: u32,
}

impl IdtEntry {
    const fn missing() -> Self {
        Self {
            offset_low: 0,
            selector: 0,
            ist: 0,
            attributes: 0,
            offset_middle: 0,
            offset_high: 0,
            reserved: 0,
        }
    }

    fn new(handler: unsafe extern "C" fn()) -> Self {
        let address = handler as usize as u64;

        Self {
            offset_low: address as u16,
            selector: gdt::kernel_code_selector(),
            ist: 0,
            attributes: INTERRUPT_GATE_PRESENT_RING0,
            offset_middle: (address >> 16) as u16,
            offset_high: (address >> 32) as u32,
            reserved: 0,
        }
    }
}

static mut IDT: [IdtEntry; IDT_ENTRIES] = [IdtEntry::missing(); IDT_ENTRIES];

/// Installs the bootstrap IDT with fatal exception handlers for vectors 0-31.
pub fn init() {
    unsafe {
        // Safety: early bootstrap is single-core and interrupts remain disabled
        // while we populate the static descriptor table.
        let mut vector = 0usize;
        while vector < exceptions::HANDLERS.len() {
            IDT[vector] = IdtEntry::new(exceptions::HANDLERS[vector]);
            vector += 1;
        }
    }

    let descriptor = DescriptorTablePointer {
        limit: (size_of::<[IdtEntry; IDT_ENTRIES]>() - 1) as u16,
        base: (&raw const IDT) as *const _ as u64,
    };

    unsafe {
        // Safety: the IDT points at a static table whose initialized entries all
        // target assembly stubs in this kernel image.
        asm!("lidt [{descriptor}]", descriptor = in(reg) &descriptor);
    }
}

#[cfg(test)]
mod tests {
    use super::IdtEntry;
    use core::mem::size_of;

    #[test]
    fn idt_entries_are_16_bytes() {
        assert_eq!(size_of::<IdtEntry>(), 16);
    }
}
