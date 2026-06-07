//! Supervisor trap handling for riscv64.
//!
//! Installs a direct-mode `stvec` pointing at `trap_entry`, a hand-written
//! save/restore stub that builds a [`TrapFrame`] on the kernel stack and calls
//! [`trap_dispatch`]. This is the riscv64 analogue of the x86_64 IDT path and
//! the foundation for timer interrupts, page-fault handling, and the syscall
//! (`ecall`) entry that land in later passes.
//!
//! Milestone 2 scope: catch a synchronous exception (a deliberate `ebreak`),
//! decode `scause`, advance `sepc` past the trapping instruction, and `sret`
//! back so execution resumes cleanly. Interrupts stay masked; this exercises
//! the full save -> dispatch -> resume cycle on real firmware.

use core::arch::asm;

/// Saved supervisor trap context.
///
/// Field order and offsets are shared with the `trap_entry` assembly below;
/// `#[repr(C)]` keeps them stable. `regs` holds the general-purpose registers
/// `x1..=x31` (x0 is hardwired zero and not saved), so `regs[n]` is `x(n + 1)`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct TrapFrame {
    /// General-purpose registers x1..=x31 (`regs[n]` == `x(n + 1)`).
    pub regs: [usize; 31],
    /// Exception program counter (address of the trapping instruction).
    pub sepc: usize,
    /// Supervisor status snapshot at trap time.
    pub sstatus: usize,
    /// Trap cause: bit 63 = interrupt flag, low bits = cause code.
    pub scause: usize,
    /// Trap value (faulting address / bad instruction, cause-dependent).
    pub stval: usize,
}

// Indices into `TrapFrame::regs` for the registers we name in diagnostics.
// (regs[n] == x(n + 1); ra is x1 -> index 0.)
const REG_RA: usize = 0;

/// `scause` interrupt flag (bit 63 on rv64).
const SCAUSE_INTERRUPT: usize = 1 << 63;
/// Synchronous-exception cause code for `ebreak` / breakpoint.
const EXC_BREAKPOINT: usize = 3;

unsafe extern "C" {
    /// Assembly trap vector; installed into `stvec`. Never called directly.
    fn trap_entry();
}

core::arch::global_asm!(
    ".section .text.trap,\"ax\"",
    // stvec direct mode requires the base address to be 4-byte aligned (the
    // low two bits encode the mode), so force alignment of the vector.
    ".balign 4",
    ".global trap_entry",
    "trap_entry:",
    // Carve a 288-byte frame (280 bytes of state + 8 padding) so the stack
    // stays 16-byte aligned for the C-ABI call into trap_dispatch.
    "addi sp, sp, -288",
    "sd x1, 0(sp)",
    // x2 (sp): store the pre-trap value (current sp + frame size).
    "addi t0, sp, 288",
    "sd t0, 8(sp)",
    "sd x3, 16(sp)",
    "sd x4, 24(sp)",
    "sd x5, 32(sp)",
    "sd x6, 40(sp)",
    "sd x7, 48(sp)",
    "sd x8, 56(sp)",
    "sd x9, 64(sp)",
    "sd x10, 72(sp)",
    "sd x11, 80(sp)",
    "sd x12, 88(sp)",
    "sd x13, 96(sp)",
    "sd x14, 104(sp)",
    "sd x15, 112(sp)",
    "sd x16, 120(sp)",
    "sd x17, 128(sp)",
    "sd x18, 136(sp)",
    "sd x19, 144(sp)",
    "sd x20, 152(sp)",
    "sd x21, 160(sp)",
    "sd x22, 168(sp)",
    "sd x23, 176(sp)",
    "sd x24, 184(sp)",
    "sd x25, 192(sp)",
    "sd x26, 200(sp)",
    "sd x27, 208(sp)",
    "sd x28, 216(sp)",
    "sd x29, 224(sp)",
    "sd x30, 232(sp)",
    "sd x31, 240(sp)",
    // Control/status registers.
    "csrr t0, sepc",
    "sd t0, 248(sp)",
    "csrr t0, sstatus",
    "sd t0, 256(sp)",
    "csrr t0, scause",
    "sd t0, 264(sp)",
    "csrr t0, stval",
    "sd t0, 272(sp)",
    // trap_dispatch(&mut TrapFrame)
    "mv a0, sp",
    "call {dispatch}",
    // Restore CSRs the handler may have updated (sepc advance, sstatus).
    "ld t0, 248(sp)",
    "csrw sepc, t0",
    "ld t0, 256(sp)",
    "csrw sstatus, t0",
    // Restore GPRs (x2/sp is rebuilt via the frame-size add below, so skip it).
    "ld x1, 0(sp)",
    "ld x3, 16(sp)",
    "ld x4, 24(sp)",
    "ld x5, 32(sp)",
    "ld x6, 40(sp)",
    "ld x7, 48(sp)",
    "ld x8, 56(sp)",
    "ld x9, 64(sp)",
    "ld x10, 72(sp)",
    "ld x11, 80(sp)",
    "ld x12, 88(sp)",
    "ld x13, 96(sp)",
    "ld x14, 104(sp)",
    "ld x15, 112(sp)",
    "ld x16, 120(sp)",
    "ld x17, 128(sp)",
    "ld x18, 136(sp)",
    "ld x19, 144(sp)",
    "ld x20, 152(sp)",
    "ld x21, 160(sp)",
    "ld x22, 168(sp)",
    "ld x23, 176(sp)",
    "ld x24, 184(sp)",
    "ld x25, 192(sp)",
    "ld x26, 200(sp)",
    "ld x27, 208(sp)",
    "ld x28, 216(sp)",
    "ld x29, 224(sp)",
    "ld x30, 232(sp)",
    "ld x31, 240(sp)",
    "addi sp, sp, 288",
    "sret",
    dispatch = sym trap_dispatch,
);

/// Installs the supervisor trap vector (`stvec`) in direct mode.
///
/// After this, any synchronous exception or (once unmasked) interrupt taken in
/// S-mode vectors through `trap_entry`. Safe to call once during early boot.
pub fn init() {
    let base = trap_entry as *const () as usize;
    debug_assert_eq!(base & 0b11, 0, "trap_entry must be 4-byte aligned for stvec");
    // SAFETY: writing stvec with a 4-byte-aligned base and mode bits = 0
    // (direct mode) is the architectural way to install the trap vector.
    unsafe {
        asm!("csrw stvec, {base}", base = in(reg) base, options(nomem, nostack));
    }
}

/// Rust trap dispatcher, invoked from `trap_entry` with the saved frame.
///
/// Milestone 2 handles the breakpoint exception (advancing `sepc` past the
/// `ebreak` so execution resumes after it) and treats everything else as fatal.
#[unsafe(no_mangle)]
extern "C" fn trap_dispatch(frame: *mut TrapFrame) {
    // SAFETY: `trap_entry` passes a pointer to the frame it just built on the
    // current stack; it is valid and uniquely borrowed for this call.
    let frame = unsafe { &mut *frame };

    let is_interrupt = frame.scause & SCAUSE_INTERRUPT != 0;
    let code = frame.scause & !SCAUSE_INTERRUPT;

    if !is_interrupt && code == EXC_BREAKPOINT {
        crate::kprintln!(
            "[trap] breakpoint: sepc={:#x} scause={:#x} ra={:#x}",
            frame.sepc,
            frame.scause,
            frame.regs[REG_RA]
        );
        // Resume after the trapping instruction. `ebreak` is 4 bytes, but its
        // compressed form `c.ebreak` is 2; decode the length from the low bits
        // of the opcode (0b11 in bits[1:0] => 32-bit instruction) so we advance
        // correctly either way.
        frame.sepc += instruction_len_at(frame.sepc);
        return;
    }

    // Anything else is unexpected at this stage; surface it loudly.
    panic!(
        "unhandled riscv64 trap: interrupt={} code={} scause={:#x} sepc={:#x} stval={:#x}",
        is_interrupt, code, frame.scause, frame.sepc, frame.stval
    );
}

/// Returns the byte length of the instruction at `pc` (2 for compressed RVC,
/// 4 otherwise) by inspecting the low bits of its first halfword.
fn instruction_len_at(pc: usize) -> usize {
    // SAFETY: `pc` is the trapping instruction's address inside the kernel's
    // own executable image; reading its first halfword is safe.
    let half = unsafe { (pc as *const u16).read_volatile() };
    if half & 0b11 == 0b11 { 4 } else { 2 }
}
