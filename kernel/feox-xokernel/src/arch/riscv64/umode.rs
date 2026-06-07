//! U-mode execution (milestone 12): drop to user mode and come back.
//!
//! `enter_user` saves the kernel's callee-saved context (a setjmp-style buffer),
//! switches `sstatus` to return to U-mode (SPP=0) with `sstatus.SUM` set (so the
//! S-mode trap path may save the trap frame on the user stack), and `sret`s to
//! the user entry. When the user traps (here, via `ecall`), the trap dispatcher
//! calls `resume_kernel`, which restores the saved context and `ret`s — so
//! `enter_user` appears to return to its caller. This longjmp is the
//! context-switch primitive the scheduler (M14) will build on.
//!
//! Single-hart only: `KERNEL_CONTEXT` is one global buffer. A per-thread context
//! arrives with the process/scheduler milestone.

use core::arch::global_asm;

use super::{frame, paging};

/// setjmp-style buffer: ra, sp, s0..s11 (14 registers), saved by `enter_user`
/// and restored by `resume_kernel`.
static mut KERNEL_CONTEXT: [usize; 14] = [0; 14];

global_asm!(
    ".section .text,\"ax\"",
    ".global enter_user",
    // a0 = user entry PC, a1 = user stack pointer.
    "enter_user:",
    "la t0, {ctx}",
    "sd ra, 0(t0)",
    "sd sp, 8(t0)",
    "sd s0, 16(t0)",
    "sd s1, 24(t0)",
    "sd s2, 32(t0)",
    "sd s3, 40(t0)",
    "sd s4, 48(t0)",
    "sd s5, 56(t0)",
    "sd s6, 64(t0)",
    "sd s7, 72(t0)",
    "sd s8, 80(t0)",
    "sd s9, 88(t0)",
    "sd s10, 96(t0)",
    "sd s11, 104(t0)",
    // sstatus: clear SPP (bit 8) so sret returns to U-mode, clear SPIE (bit 5)
    // so interrupts stay masked in U-mode for this demo.
    "li t1, 0x120",
    "csrc sstatus, t1",
    // set SUM (bit 18) so S-mode trap code may access the user stack.
    "li t1, 0x40000",
    "csrs sstatus, t1",
    "csrw sepc, a0",
    "mv sp, a1",
    "sret",
    // resume_kernel(): clear SUM, restore the saved kernel context, and return
    // to enter_user's caller (longjmp). Never returns to its own caller.
    ".global resume_kernel",
    "resume_kernel:",
    "li t1, 0x40000",
    "csrc sstatus, t1",
    "la t0, {ctx}",
    "ld ra, 0(t0)",
    "ld sp, 8(t0)",
    "ld s0, 16(t0)",
    "ld s1, 24(t0)",
    "ld s2, 32(t0)",
    "ld s3, 40(t0)",
    "ld s4, 48(t0)",
    "ld s5, 56(t0)",
    "ld s6, 64(t0)",
    "ld s7, 72(t0)",
    "ld s8, 80(t0)",
    "ld s9, 88(t0)",
    "ld s10, 96(t0)",
    "ld s11, 104(t0)",
    "ret",
    ctx = sym KERNEL_CONTEXT,
);

unsafe extern "C" {
    /// Enters U-mode at `entry` with stack `user_sp`; returns (via the trap
    /// dispatcher's `resume_kernel`) once the user traps.
    fn enter_user(entry: usize, user_sp: usize);
    /// Longjmp back into the kernel at the point `enter_user` was called.
    fn resume_kernel() -> !;
}

/// Returns control to the kernel from a U-mode trap. Called by the dispatcher.
///
/// # Safety
/// Must only be called while a corresponding `enter_user` is on the stack
/// (i.e. from the trap taken during that U-mode excursion).
pub unsafe fn resume_to_kernel() -> ! {
    // SAFETY: upheld by the caller (the U-mode ecall trap path).
    unsafe { resume_kernel() }
}

/// User VAs for the demo, in an otherwise-unused sv39 gigapage (4 GiB) so they
/// never collide with the kernel's mappings.
const USER_CODE_VA: usize = 0x1_0000_0000;
const USER_STACK_VA: usize = 0x1_0000_4000;

/// Enters U-mode running a tiny program (`ecall; 1: j 1b`), handles the ecall
/// (in the trap dispatcher), and returns — proving the S↔U round trip.
pub fn demo() {
    let (Some(code), Some(stack)) = (frame::alloc(), frame::alloc()) else {
        crate::kprintln!("[feox] umode: out of frames for the U-mode demo");
        return;
    };

    // Write the user program into the code frame (via its identity mapping),
    // then make it visible to instruction fetch.
    // SAFETY: `code` is a fresh, identity-mapped, writable frame.
    unsafe {
        let p = code as *mut u32;
        p.write_volatile(0x0000_0073); // ecall
        p.add(1).write_volatile(0x0000_006f); // 1: j 1b
        core::arch::asm!("fence.i", options(nostack));
    }

    let mut space = paging::AddressSpace::from_active();
    space.map(USER_CODE_VA, code, 4096, paging::PTE_U | paging::PTE_R | paging::PTE_X);
    space.map(USER_STACK_VA, stack, 4096, paging::PTE_U | paging::PTE_R | paging::PTE_W);
    paging::flush_tlb_all();

    crate::kprintln!("[feox] umode: sret to U-mode at {:#x}...", USER_CODE_VA);
    // SAFETY: the user pages are mapped U-accessible; on the user's ecall the
    // dispatcher longjmps back here via resume_kernel.
    unsafe { enter_user(USER_CODE_VA, USER_STACK_VA + 4096) };

    space.unmap(USER_CODE_VA, 4096);
    space.unmap(USER_STACK_VA, 4096);
    paging::flush_tlb_all();
    frame::free(code);
    frame::free(stack);

    crate::kprintln!("[feox] milestone 12: U-mode execution (entered U-mode, ecall, returned).");
}
