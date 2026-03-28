//! Minimal global descriptor table for early long-mode bootstrap.

use core::arch::asm;
use core::mem::size_of;
use core::ptr::addr_of;

const GDT_NULL: u64 = 0;
const GDT_KERNEL_CODE: u64 = 0x00AF_9A00_0000_FFFF;
const GDT_KERNEL_DATA: u64 = 0x00CF_9200_0000_FFFF;

/// GDT layout: null | kernel code | kernel data | TSS low | TSS high.
static mut GDT: [u64; 5] = [GDT_NULL, GDT_KERNEL_CODE, GDT_KERNEL_DATA, 0, 0];

const KERNEL_CODE_SELECTOR: u16 = 0x08;
const KERNEL_DATA_SELECTOR: u16 = 0x10;
/// Selector for the 64-bit TSS descriptor at GDT[3] (offset 0x18).
const TSS_SELECTOR: u16 = 0x18;

// ---------------------------------------------------------------------------
// IST stacks
// ---------------------------------------------------------------------------

const IST_STACK_SIZE: usize = 4096;

#[repr(C, align(16))]
struct AlignedStack([u8; IST_STACK_SIZE]);

/// Dedicated stack for NMI (IST1).
static mut NMI_IST_STACK: AlignedStack = AlignedStack([0; IST_STACK_SIZE]);
/// Dedicated stack for double-fault (IST2).
static mut DOUBLE_FAULT_IST_STACK: AlignedStack = AlignedStack([0; IST_STACK_SIZE]);

// ---------------------------------------------------------------------------
// Task State Segment
// ---------------------------------------------------------------------------

/// 64-bit Task State Segment (Intel SDM Vol. 3A §7.7).
///
/// Sized at exactly 104 bytes. `iopb_offset` is set to `size_of::<Tss>()`
/// to indicate that no I/O permission map is present.
#[repr(C, packed)]
struct Tss {
    reserved0: u32,
    /// RSP0–RSP2: privilege-level stack pointers (unused in bootstrap).
    rsp: [u64; 3],
    reserved1: u64,
    /// IST1–IST7: dedicated interrupt stack top pointers.
    ist: [u64; 7],
    reserved2: u64,
    reserved3: u16,
    iopb_offset: u16,
}

static mut TSS: Tss = Tss {
    reserved0: 0,
    rsp: [0; 3],
    reserved1: 0,
    ist: [0; 7],
    reserved2: 0,
    reserved3: 0,
    iopb_offset: size_of::<Tss>() as u16,
};

// ---------------------------------------------------------------------------
// Descriptor encoding helpers
// ---------------------------------------------------------------------------

/// Encodes the low and high 8-byte halves of a 64-bit system-segment (TSS)
/// descriptor for `base` with a byte-granularity limit of `size_of::<Tss>()-1`.
fn tss_descriptor(base: u64) -> (u64, u64) {
    let limit = (size_of::<Tss>() - 1) as u64;
    // type = 0x9 (64-bit TSS, Available), S=0, DPL=0, P=1 → attribute byte 0x89.
    let low = (limit & 0xFFFF)
        | ((base & 0x00FF_FFFF) << 16)
        | (0x89u64 << 40)
        | (((limit >> 16) & 0xF) << 48)
        | (((base >> 24) & 0xFF) << 56);
    let high = base >> 32;
    (low, high)
}

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
        limit: (size_of::<[u64; 5]>() - 1) as u16,
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
    addr_of!(GDT) as u64
}

/// Reloads the bootstrap GDT from a supplied base address.
///
/// Safety: `base` must point at a valid copy of Feox's bootstrap GDT.
pub unsafe fn reload_with_base(base: u64) {
    unsafe { load_descriptor_table(base) }
}

/// Loads the bootstrap GDT, installs IST stacks in the TSS, loads the TSS
/// descriptor, and initializes the Task Register.
pub fn init() {
    unsafe {
        // Install IST stack tops. x86 stacks grow down, so the IST value is
        // the address one past the last stack byte (i.e. the initial RSP the
        // CPU will use before its first push).
        TSS.ist[0] = addr_of!(NMI_IST_STACK) as u64 + IST_STACK_SIZE as u64;
        TSS.ist[1] = addr_of!(DOUBLE_FAULT_IST_STACK) as u64 + IST_STACK_SIZE as u64;

        // Build and install the 128-bit TSS descriptor into the GDT.
        let tss_base = addr_of!(TSS) as u64;
        let (low, high) = tss_descriptor(tss_base);
        GDT[3] = low;
        GDT[4] = high;

        load_descriptor_table(table_base());

        // Load the Task Register with the TSS selector so the CPU knows where
        // to find IST stacks when delivering NMI and double-fault exceptions.
        asm!(
            "ltr ax",
            in("ax") TSS_SELECTOR,
            options(nomem, nostack, preserves_flags)
        );
    }
}

#[cfg(test)]
mod tests {
    use super::Tss;
    use core::mem::size_of;

    #[test]
    fn tss_is_104_bytes() {
        assert_eq!(size_of::<Tss>(), 104);
    }
}
