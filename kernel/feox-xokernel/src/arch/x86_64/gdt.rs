//! Minimal global descriptor table for early long-mode bootstrap.

use core::arch::asm;
use core::mem::size_of;

const GDT_NULL: u64 = 0;
const GDT_KERNEL_CODE: u64 = 0x00AF_9A00_0000_FFFF;
const GDT_KERNEL_DATA: u64 = 0x00CF_9200_0000_FFFF;

static GDT: [u64; 3] = [GDT_NULL, GDT_KERNEL_CODE, GDT_KERNEL_DATA];

const KERNEL_CODE_SELECTOR: u16 = 0x08;
const KERNEL_DATA_SELECTOR: u16 = 0x10;

#[repr(C, packed)]
struct DescriptorTablePointer {
    limit: u16,
    base: u64,
}

/// Returns the selector for the kernel code segment.
#[must_use]
pub const fn kernel_code_selector() -> u16 {
    KERNEL_CODE_SELECTOR
}

fn descriptor_for_base(base: u64) -> DescriptorTablePointer {
    DescriptorTablePointer {
        limit: (size_of::<[u64; 3]>() - 1) as u16,
        base,
    }
}

unsafe fn load_descriptor_table(base: u64) {
    let descriptor = descriptor_for_base(base);

    // Safety: the descriptor table points at a valid kernel GDT image and the
    // far return reloads CS from that table before execution continues.
    unsafe {
        asm!(
            "lgdt [{descriptor}]",
            "mov ax, {data_selector:x}",
            "mov ds, ax",
            "mov es, ax",
            "mov fs, ax",
            "mov gs, ax",
            "mov ss, ax",
            "push {code_selector}",
            "lea rax, [rip + 2f]",
            "push rax",
            "retfq",
            "2:",
            descriptor = in(reg) &descriptor,
            data_selector = in(reg) u64::from(KERNEL_DATA_SELECTOR),
            code_selector = in(reg) u64::from(KERNEL_CODE_SELECTOR),
            lateout("rax") _,
        );
    }
}

/// Returns the active bootstrap GDT base address.
#[must_use]
pub fn table_base() -> u64 {
    GDT.as_ptr() as u64
}

/// Reloads the bootstrap GDT from a supplied base address.
///
/// Safety: `base` must point at a valid copy of Feox's bootstrap GDT.
pub unsafe fn reload_with_base(base: u64) {
    unsafe { load_descriptor_table(base) }
}

/// Loads the bootstrap GDT and reloads the visible segment registers.
pub fn init() {
    unsafe { load_descriptor_table(table_base()) }
}
