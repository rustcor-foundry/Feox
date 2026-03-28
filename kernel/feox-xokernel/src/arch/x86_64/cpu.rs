//! Minimal CPU helpers for early boot.

use core::arch::asm;

/// Disables maskable interrupts on the current core.
pub fn disable_interrupts() {
    unsafe {
        // Safety: this is the architectural instruction for clearing IF.
        asm!("cli", options(nomem, nostack, preserves_flags));
    }
}

/// Halts the current core until the next interrupt.
pub fn halt() {
    unsafe {
        // Safety: this executes the architectural halt instruction.
        asm!("hlt", options(nomem, nostack));
    }
}

/// Enters an infinite halt loop after disabling interrupts.
pub fn hlt_loop() -> ! {
    disable_interrupts();

    loop {
        halt();
    }
}

/// Reads the current CR2 register value.
#[must_use]
pub fn read_cr2() -> u64 {
    let value: u64;

    unsafe {
        // Safety: reading CR2 is a side-effect-free architectural register read.
        asm!("mov {}, cr2", out(reg) value, options(nomem, nostack, preserves_flags));
    }

    value
}

/// Reads the current CR3 register value.
#[must_use]
pub fn read_cr3() -> u64 {
    let value: u64;

    unsafe {
        // Safety: reading CR3 is a side-effect-free architectural register read.
        asm!("mov {}, cr3", out(reg) value, options(nomem, nostack, preserves_flags));
    }

    value
}

/// Reads the current RSP register value.
#[must_use]
pub fn read_rsp() -> u64 {
    let value: u64;

    unsafe {
        // Safety: reading RSP is a side-effect-free register move.
        asm!("mov {}, rsp", out(reg) value, options(nomem, nostack, preserves_flags));
    }

    value
}

/// Invalidates the TLB entry for a single virtual address on the current core.
///
/// Must be called after every PTE write that affects an address that may
/// already be cached in the TLB. A CR3 write implicitly flushes all entries,
/// so this is not needed immediately after a full root switch.
pub fn invalidate_page(virt_addr: u64) {
    unsafe {
        // Safety: invlpg is a ring-0 privileged instruction. The caller is
        // responsible for only calling this from supervisor mode. The address
        // operand is the virtual address whose TLB entry should be flushed;
        // it does not need to be mapped.
        asm!("invlpg [{addr}]", addr = in(reg) virt_addr, options(nostack, preserves_flags));
    }
}

/// Raises a software breakpoint exception on the current core.
pub fn trigger_breakpoint() {
    unsafe {
        // Safety: this intentionally raises vector 3 so the bootstrap exception
        // path can be validated under controlled conditions.
        asm!("int3", options(nomem, nostack));
    }
}

/// Switches to a new page-table root, installs a new stack pointer, and jumps
/// to the supplied entrypoint.
///
/// The caller must ensure the current execution path remains valid long enough
/// to execute the CR3 write and jump sequence, and that the target entry and
/// stack are mapped in the new address space.
#[cfg(debug_assertions)]
pub unsafe fn switch_page_table_root_and_jump(root: u64, stack: u64, entry: u64) -> ! {
    unsafe {
        // SAFETY: upheld by the caller; this is the minimal architectural
        // sequence required to move onto a prepared address space.
        // Debug builds emit debugcon markers before and after the CR3 write
        // and before the stack switch so the transition boundary is visible
        // in the QEMU trace.
        asm!(
            "mov dx, 0x402",
            "mov al, 0x3c",
            "out dx, al",
            "mov al, 0x30",
            "out dx, al",
            "mov al, 0x3e",
            "out dx, al",
            "mov cr3, rcx",
            "mov dx, 0x402",
            "mov al, 0x3c",
            "out dx, al",
            "mov al, 0x31",
            "out dx, al",
            "mov al, 0x3e",
            "out dx, al",
            "mov rsp, r8",
            "push 0",
            "mov dx, 0x402",
            "mov al, 0x3c",
            "out dx, al",
            "mov al, 0x32",
            "out dx, al",
            "mov al, 0x3e",
            "out dx, al",
            "xor rbp, rbp",
            "jmp r9",
            in("rcx") root,
            in("r8") stack,
            in("r9") entry,
            options(noreturn)
        );
    }
}

/// Release build: minimal CR3 switch, stack install, and jump with no
/// diagnostic I/O.
#[cfg(not(debug_assertions))]
pub unsafe fn switch_page_table_root_and_jump(root: u64, stack: u64, entry: u64) -> ! {
    unsafe {
        // SAFETY: upheld by the caller.
        asm!(
            "mov cr3, rcx",
            "mov rsp, r8",
            "push 0",
            "xor rbp, rbp",
            "jmp r9",
            in("rcx") root,
            in("r8") stack,
            in("r9") entry,
            options(noreturn)
        );
    }
}

/// Switches to a new stack pointer and jumps to the supplied entrypoint.
///
/// The caller must ensure the target stack and entry are valid in the current
/// address space.
pub unsafe fn switch_stack_and_jump(stack: u64, entry: u64) -> ! {
    unsafe {
        // SAFETY: upheld by the caller; this is the minimal architectural
        // sequence required to move onto a prepared stack and entrypoint.
        asm!(
            "mov rsp, r8",
            "push 0",
            "xor rbp, rbp",
            "jmp r9",
            in("r8") stack,
            in("r9") entry,
            options(noreturn)
        );
    }
}
