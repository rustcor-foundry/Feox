//! Supervisor trap handling for riscv64.
//!
//! Installs a direct-mode `stvec` pointing at `trap_entry`, a hand-written
//! save/restore stub that builds a [`TrapFrame`] and calls [`trap_dispatch`].
//! This is the riscv64 analogue of the x86_64 IDT path: it handles the boot
//! `ebreak` self-test, supervisor timer interrupts, and the U-mode `ecall`
//! syscall entry.
//!
//! Stack discipline (milestone 14): `sscratch` holds the kernel trap-stack top
//! while the hart runs in U-mode and 0 while it runs in S-mode. Traps from
//! U-mode therefore land on the dedicated [`TRAP_STACK`]; traps from S-mode
//! push the frame on the interrupted kernel stack, as before. The restore path
//! re-arms `sscratch` from the *destination* mode (the saved `sstatus.SPP`),
//! which is what makes trap-level thread switching work: the dispatcher may
//! replace the entire frame (including `sepc`, `sstatus`, and `x2`) and the
//! stub faithfully resumes whichever context the frame now describes.
//!
//! Reentrancy: while a U-mode trap is being handled, `sscratch` temporarily
//! holds the (non-zero) user sp, so a nested S-mode fault inside the handler
//! would misclassify its origin. Interrupts are masked during handling and the
//! dispatcher panics on unexpected exceptions, so a nested fault is already a
//! fatal kernel bug; the misclassification only affects that panic's stack.

use core::arch::asm;
use core::ptr::addr_of;
use core::sync::atomic::{AtomicUsize, Ordering};

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

// Indices into `TrapFrame::regs` for the registers we name in diagnostics and
// the syscall ABI. (regs[n] == x(n + 1); ra is x1 -> index 0.)
const REG_RA: usize = 0;
/// sp (x2): stack pointer.
pub(super) const REG_SP: usize = 1;
/// a0 (x10): syscall argument pointer in, result code out.
pub(super) const REG_A0: usize = 9;
/// a1 (x11): syscall argument length in, call-specific value out.
pub(super) const REG_A1: usize = 10;
/// a7 (x17): raw ASI opcode.
pub(super) const REG_A7: usize = 16;

/// `sstatus.SPP` (bit 8): previous privilege mode (0 = U, 1 = S).
pub(super) const SSTATUS_SPP: usize = 1 << 8;

const TRAP_STACK_SIZE: usize = 16 * 1024;

#[repr(C, align(16))]
struct TrapStack([u8; TRAP_STACK_SIZE]);

/// Dedicated stack for traps taken from U-mode. One static suffices: only the
/// boot hart runs U-mode code, one thread at a time, and interrupts stay
/// masked while a trap is handled.
static mut TRAP_STACK: TrapStack = TrapStack([0; TRAP_STACK_SIZE]);

/// Top of [`TRAP_STACK`], read by the restore stub (and `enter_user`) when
/// arming `sscratch` for a return to U-mode. Set once in [`init`].
pub(super) static TRAP_STACK_TOP: AtomicUsize = AtomicUsize::new(0);

/// `scause` interrupt flag (bit 63 on rv64).
const SCAUSE_INTERRUPT: usize = 1 << 63;
/// Synchronous-exception cause code for `ebreak` / breakpoint.
const EXC_BREAKPOINT: usize = 3;
/// Interrupt cause code for the supervisor timer.
const INT_SUPERVISOR_TIMER: usize = 5;
/// Synchronous-exception cause code for an environment call from U-mode.
const EXC_ECALL_FROM_U: usize = 8;

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
    // sscratch is the kernel trap-stack top while in U-mode, 0 while in
    // S-mode. Swap to find out where we came from.
    "csrrw sp, sscratch, sp",
    "bnez sp, 1f",
    // From S-mode: sscratch held 0. Swap back so sp is the interrupted kernel
    // stack and sscratch is 0 again. (From U-mode we keep the trap stack in
    // sp; the user sp stays parked in sscratch until saved below.)
    "csrrw sp, sscratch, sp",
    "1:",
    // Carve a 288-byte frame (280 bytes of state + 8 padding) so the stack
    // stays 16-byte aligned for the C-ABI call into trap_dispatch.
    "addi sp, sp, -288",
    "sd x1, 0(sp)",
    // x2 (sp) is derived after the other registers are saved, once scratch
    // registers are free (saving x5/t0 first also keeps it intact in the
    // frame, where the pre-M14 stub clobbered it).
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
    // x2 (pre-trap sp): from U-mode it is parked in sscratch; from S-mode it
    // is this frame's base + 288. Decided by sstatus.SPP.
    "csrr t0, sstatus",
    "andi t0, t0, 0x100",
    "beqz t0, 2f",
    "addi t0, sp, 288",
    "j 3f",
    "2:",
    "csrr t0, sscratch",
    "3:",
    "sd t0, 8(sp)",
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
    // Restore CSRs from the frame, which the handler may have rewritten (sepc
    // advance, or a whole-frame thread switch).
    "ld t0, 248(sp)",
    "csrw sepc, t0",
    "ld t0, 256(sp)",
    "csrw sstatus, t0",
    // Re-arm sscratch for the destination mode (saved sstatus.SPP): the trap
    // stack top when sret drops to U-mode, 0 when it stays in S-mode.
    "andi t0, t0, 0x100",
    "bnez t0, 4f",
    "la t0, {trap_stack_top}",
    "ld t0, 0(t0)",
    "csrw sscratch, t0",
    "j 5f",
    "4:",
    "csrw sscratch, zero",
    "5:",
    // Restore GPRs; x2/sp comes last, straight from the frame, so a switched
    // frame resumes on its own stack.
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
    "ld x2, 8(sp)",
    "sret",
    dispatch = sym trap_dispatch,
    trap_stack_top = sym TRAP_STACK_TOP,
);

/// Installs the supervisor trap vector (`stvec`) in direct mode.
///
/// After this, any synchronous exception or (once unmasked) interrupt taken in
/// S-mode vectors through `trap_entry`. Safe to call once during early boot.
pub fn init() {
    let base = trap_entry as *const () as usize;
    debug_assert_eq!(base & 0b11, 0, "trap_entry must be 4-byte aligned for stvec");
    TRAP_STACK_TOP.store(
        addr_of!(TRAP_STACK) as usize + TRAP_STACK_SIZE,
        Ordering::Release,
    );
    // SAFETY: writing stvec with a 4-byte-aligned base and mode bits = 0
    // (direct mode) is the architectural way to install the trap vector.
    // sscratch must be 0 while running in S-mode (the stub's from-S marker);
    // firmware may have left anything in it.
    unsafe {
        asm!(
            "csrw sscratch, zero",
            "csrw stvec, {base}",
            base = in(reg) base,
            options(nomem, nostack)
        );
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

    if is_interrupt && code == INT_SUPERVISOR_TIMER {
        // Interrupts resume the interrupted instruction, so sepc is left as-is.
        super::time::on_timer_interrupt();
        // Preemption point: when the scheduler is active and the tick landed
        // in U-mode, this may swap the whole frame for another thread's.
        super::sched::on_tick(frame);
        return;
    }

    if !is_interrupt && code == EXC_ECALL_FROM_U {
        // ASI syscall: a7 = opcode, a0 = args pointer, a1 = args length.
        let opcode = frame.regs[REG_A7];
        if super::sched::active() {
            // Under the scheduler, yield/exit/park are thread-lifecycle
            // events (frame switches), not table dispatches. Everything else
            // falls through to the normal ASI path and resumes the same
            // thread.
            if opcode == feox_asi::AsiOp::ProcYield as usize {
                super::sched::yield_current(frame);
                return;
            }
            if opcode == feox_asi::AsiOp::ProcExit as usize {
                super::sched::exit_current(frame, frame.regs[REG_A0]);
                return;
            }
            if opcode == feox_asi::AsiOp::ThreadPark as usize {
                super::syscall::park_from_user(frame);
                return;
            }
        } else if opcode == feox_asi::AsiOp::ProcExit as usize {
            // Single-excursion mode (umode::run_user_program): tear down the
            // U-mode excursion, recording the exit value (the user's a0).
            crate::kprintln!(
                "[feox] asi: proc_exit from U-mode (value={})",
                frame.regs[REG_A0]
            );
            // SAFETY: reached only from the U-mode excursion started by
            // enter_user.
            unsafe { super::umode::exit_to_kernel(frame.regs[REG_A0]) };
        }
        let mut value = 0u64;
        let result = super::syscall::dispatch(
            opcode as u64,
            frame.regs[REG_A0] as *const u8,
            frame.regs[REG_A1] as u64,
            &mut value,
        );
        crate::kprintln!(
            "[feox] asi: ecall op={:#x} -> code={:#x} value={}",
            opcode,
            result,
            value
        );
        frame.regs[REG_A0] = result as usize;
        frame.regs[REG_A1] = value as usize;
        // Resume past the trapping instruction; `ecall` has no compressed
        // form, so it is always 4 bytes.
        frame.sepc += 4;
        return;
    }

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
        "unhandled riscv64 trap: {} (interrupt={} code={}) scause={:#x} sepc={:#x} stval={:#x}",
        cause_name(is_interrupt, code),
        is_interrupt,
        code,
        frame.scause,
        frame.sepc,
        frame.stval
    );
}

/// Human-readable name for a trap cause, for diagnostics.
fn cause_name(is_interrupt: bool, code: usize) -> &'static str {
    if is_interrupt {
        match code {
            1 => "supervisor software interrupt",
            5 => "supervisor timer interrupt",
            9 => "supervisor external interrupt",
            _ => "interrupt",
        }
    } else {
        match code {
            0 => "instruction address misaligned",
            1 => "instruction access fault",
            2 => "illegal instruction",
            3 => "breakpoint",
            4 => "load address misaligned",
            5 => "load access fault",
            6 => "store/AMO address misaligned",
            7 => "store/AMO access fault",
            8 => "environment call from U-mode",
            9 => "environment call from S-mode",
            12 => "instruction page fault",
            13 => "load page fault",
            15 => "store/AMO page fault",
            _ => "exception",
        }
    }
}

/// Returns the byte length of the instruction at `pc` (2 for compressed RVC,
/// 4 otherwise) by inspecting the low bits of its first halfword.
fn instruction_len_at(pc: usize) -> usize {
    // SAFETY: `pc` is the trapping instruction's address inside the kernel's
    // own executable image; reading its first halfword is safe.
    let half = unsafe { (pc as *const u16).read_volatile() };
    if half & 0b11 == 0b11 { 4 } else { 2 }
}
