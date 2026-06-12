#![no_std]

//! Minimal libOS for Feox U-mode apps: the ASI ecall ABI as Rust functions.
//!
//! Transport (see `arch/riscv64/syscall.rs` in the kernel): the opcode goes in
//! `a7`, the argument pointer/length in `a0`/`a1`; the kernel returns the
//! transport result code in `a0` and the call-specific value in `a1`.
//! `ProcExit` never returns. The opcodes and result codes come from
//! `feox-asi`, the shared ABI crate, so app and kernel cannot drift.

use feox_asi::{AsiOp, SYSCALL_OK};

/// Raw ASI syscall. Returns `(result code, value)`.
#[inline]
pub fn syscall(op: AsiOp, args: *const u8, len: usize) -> (u64, u64) {
    let code: u64;
    let value: u64;
    // SAFETY: `ecall` traps to the kernel's ASI dispatcher, which only writes
    // the a0/a1 result registers and resumes past the instruction.
    unsafe {
        core::arch::asm!(
            "ecall",
            in("a7") op as u64,
            inlateout("a0") args as u64 => code,
            inlateout("a1") len as u64 => value,
            options(nostack),
        );
    }
    (code, value)
}

/// Yields the rest of the current timeslice to the next ready thread.
pub fn yield_now() {
    let _ = syscall(AsiOp::ProcYield, core::ptr::null(), 0);
}

/// Returns the number of capabilities visible to this process, or `None` if
/// the syscall failed.
pub fn cap_count() -> Option<u64> {
    let (code, total) = syscall(AsiOp::CapList, core::ptr::null(), 0);
    (code == SYSCALL_OK).then_some(total)
}

/// Exits the current process with `value`. Never returns.
pub fn exit(value: usize) -> ! {
    // SAFETY: ProcExit tears down this thread in the kernel; control never
    // comes back, matching the noreturn contract.
    unsafe {
        core::arch::asm!(
            "ecall",
            in("a7") AsiOp::ProcExit as u64,
            in("a0") value,
            options(noreturn),
        );
    }
}

/// Panic = abnormal exit with a recognizable value.
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    exit(0xdead)
}
