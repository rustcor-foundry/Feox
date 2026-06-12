//! U-mode execution (milestone 12): drop to user mode and come back.
//!
//! `enter_user` saves the kernel's callee-saved context (a setjmp-style buffer),
//! switches `sstatus` to return to U-mode (SPP=0) with `sstatus.SUM` set (so the
//! S-mode trap path may access user memory), and `sret`s to the user entry.
//! When the user requests exit (an `ecall` with `AsiOp::ProcExit`), the trap
//! dispatcher calls [`exit_to_kernel`], which records the exit value, restores
//! the saved context, and `ret`s — so `enter_user` appears to return to its
//! caller. This longjmp is the context-switch primitive the scheduler (M14)
//! will build on.
//!
//! Single-hart only: `KERNEL_CONTEXT` is one global buffer. A per-thread context
//! arrives with the process/scheduler milestone.

use core::arch::global_asm;
use core::sync::atomic::{AtomicUsize, Ordering};

use super::{frame, paging};

/// setjmp-style buffer: ra, sp, s0..s11 (14 registers), saved by `enter_user`
/// and restored by `resume_kernel`.
static mut KERNEL_CONTEXT: [usize; 14] = [0; 14];

/// Exit value passed by the user program through `AsiOp::ProcExit` (its `a0`),
/// recorded by [`exit_to_kernel`] before the longjmp.
static EXIT_VALUE: AtomicUsize = AtomicUsize::new(0);

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
    // sstatus: clear SPP (bit 8) so sret returns to U-mode, and SIE (bit 1)
    // so no S-mode interrupt can land between arming sscratch below and the
    // sret (it would be misclassified as from-U). Set SPIE (bit 5) and SUM
    // (bit 18, so S-mode trap code may access user memory). S-level interrupt
    // sources unmasked in sie (the scheduler's timer) deliver in U-mode
    // regardless of SIE.
    "li t1, 0x102",
    "csrc sstatus, t1",
    "li t1, 0x40020",
    "csrs sstatus, t1",
    // Arm sscratch with the trap-stack top: traps taken from U-mode switch to
    // the dedicated kernel trap stack (see trap.rs).
    "la t1, {trap_stack_top}",
    "ld t1, 0(t1)",
    "csrw sscratch, t1",
    "csrw sepc, a0",
    "mv sp, a1",
    "sret",
    // resume_kernel(): clear SUM, zero sscratch (back in S-mode for good),
    // restore the saved kernel context, and return to enter_user's caller
    // (longjmp). Never returns to its own caller.
    ".global resume_kernel",
    "resume_kernel:",
    "li t1, 0x40000",
    "csrc sstatus, t1",
    "csrw sscratch, zero",
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
    trap_stack_top = sym super::trap::TRAP_STACK_TOP,
);

unsafe extern "C" {
    /// Enters U-mode at `entry` with stack `user_sp`; returns (via the trap
    /// dispatcher's `resume_kernel`) once the user exits.
    pub(super) fn enter_user(entry: usize, user_sp: usize);
    /// Longjmp back into the kernel at the point `enter_user` was called.
    fn resume_kernel() -> !;
}

/// Returns control to the kernel from a U-mode `ProcExit`, recording the exit
/// value the user passed in `a0`. Called by the trap dispatcher.
///
/// # Safety
/// Must only be called while a corresponding `enter_user` is on the stack
/// (i.e. from a trap taken during that U-mode excursion).
pub unsafe fn exit_to_kernel(value: usize) -> ! {
    EXIT_VALUE.store(value, Ordering::Release);
    // SAFETY: upheld by the caller (the U-mode trap path).
    unsafe { resume_kernel() }
}

/// User VAs for U-mode excursions, in an otherwise-unused sv39 gigapage
/// (4 GiB) so they never collide with the kernel's mappings.
const USER_CODE_VA: usize = 0x1_0000_0000;
const USER_STACK_VA: usize = 0x1_0000_4000;

/// Runs a tiny user program (raw instruction words copied into a fresh U-mode
/// code page, with a fresh U-mode stack page) until it exits via
/// `AsiOp::ProcExit`, and returns the exit value the user passed in `a0`.
/// Returns `None` if no frames are available.
pub fn run_user_program(words: &[u32]) -> Option<usize> {
    debug_assert!(words.len() * 4 <= frame::FRAME_SIZE);
    let code = frame::alloc()?;
    let Some(stack) = frame::alloc() else {
        frame::free(code);
        return None;
    };

    // Write the user program into the code frame (via its identity mapping),
    // then make it visible to instruction fetch.
    // SAFETY: `code` is a fresh, identity-mapped, writable frame and the
    // program fits in it (asserted above).
    unsafe {
        let p = code as *mut u32;
        for (i, word) in words.iter().enumerate() {
            p.add(i).write_volatile(*word);
        }
        core::arch::asm!("fence.i", options(nostack));
    }

    let mut space = paging::AddressSpace::from_active();
    space.map(USER_CODE_VA, code, 4096, paging::PTE_U | paging::PTE_R | paging::PTE_X);
    space.map(USER_STACK_VA, stack, 4096, paging::PTE_U | paging::PTE_R | paging::PTE_W);
    paging::flush_tlb_all();

    // SAFETY: the user pages are mapped U-accessible; on the user's ProcExit
    // the trap dispatcher longjmps back here via exit_to_kernel.
    unsafe { enter_user(USER_CODE_VA, USER_STACK_VA + 4096) };

    space.unmap(USER_CODE_VA, 4096);
    space.unmap(USER_STACK_VA, 4096);
    paging::flush_tlb_all();
    frame::free(code);
    frame::free(stack);

    Some(EXIT_VALUE.load(Ordering::Acquire))
}

/// Enters U-mode running a tiny program that immediately exits
/// (`li a0, 0; li a7, ProcExit; ecall`), proving the S↔U round trip.
pub fn demo() {
    let program = [
        0x0000_0513, // li a0, 0       (exit value)
        0x3010_0893, // li a7, 0x301   (AsiOp::ProcExit)
        0x0000_0073, // ecall          (traps to S; the dispatcher longjmps back)
        0x0000_006f, // 1: j 1b        (unreached)
    ];
    crate::kprintln!("[feox] umode: sret to U-mode at {:#x}...", USER_CODE_VA);
    match run_user_program(&program) {
        Some(value) => crate::kprintln!(
            "[feox] milestone 12: U-mode execution (entered U-mode, exit({}), returned).",
            value
        ),
        None => crate::kprintln!("[feox] umode: out of frames for the U-mode demo"),
    }
}
