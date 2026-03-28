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
        Self::from_address(handler as usize as u64)
    }

    fn from_address(address: u64) -> Self {
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

    /// Returns a copy of this entry with the IST index set to `ist`.
    ///
    /// The IST field is 3 bits; valid values are 1–7 (0 = no IST).
    fn with_ist(mut self, ist: u8) -> Self {
        self.ist = ist & 0x07;
        self
    }
}

static mut IDT: [IdtEntry; IDT_ENTRIES] = [IdtEntry::missing(); IDT_ENTRIES];

fn descriptor_for_base(base: u64) -> DescriptorTablePointer {
    DescriptorTablePointer {
        limit: (size_of::<[IdtEntry; IDT_ENTRIES]>() - 1) as u16,
        base,
    }
}

unsafe fn load_descriptor_table(base: u64) {
    let descriptor = descriptor_for_base(base);

    // Safety: the IDT points at a static table whose initialized entries all
    // target assembly stubs in this kernel image.
    unsafe {
        asm!("lidt [{descriptor}]", descriptor = in(reg) &descriptor);
    }
}

/// Returns the active bootstrap IDT base address.
#[must_use]
pub fn table_base() -> u64 {
    (&raw const IDT) as *const _ as u64
}

/// Rebuilds vectors 0-31 with a supplied address delta and reloads the IDT.
///
/// Safety: `base` must point at a valid copy of the Feox bootstrap IDT, and
/// `handler_delta` must translate the existing exception stubs to executable
/// addresses in the active kernel mapping.
pub unsafe fn relocate_and_reload(base: u64, handler_delta: u64) {
    // Safety: early bootstrap remains single-core and interrupts stay disabled
    // while we rewrite the static IDT entries.
    unsafe {
        let mut vector = 0usize;
        while vector < exceptions::HANDLERS.len() {
            let relocated = exceptions::HANDLERS[vector] as usize as u64 + handler_delta;
            let entry = IdtEntry::from_address(relocated);
            IDT[vector] = match vector {
                2 => entry.with_ist(1), // NMI → IST1
                8 => entry.with_ist(2), // #DF → IST2
                _ => entry,
            };
            vector += 1;
        }
        load_descriptor_table(base);
    }
}

/// Installs the bootstrap IDT with fatal exception handlers for vectors 0-31.
pub fn init() {
    unsafe {
        // Safety: early bootstrap is single-core and interrupts remain disabled
        // while we populate the static descriptor table.
        let mut vector = 0usize;
        while vector < exceptions::HANDLERS.len() {
            let entry = IdtEntry::new(exceptions::HANDLERS[vector]);
            IDT[vector] = match vector {
                2 => entry.with_ist(1), // NMI → IST1
                8 => entry.with_ist(2), // #DF → IST2
                _ => entry,
            };
            vector += 1;
        }
    }

    unsafe { load_descriptor_table(table_base()) }
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
