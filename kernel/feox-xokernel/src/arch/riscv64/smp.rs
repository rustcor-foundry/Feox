//! SMP bring-up via the SBI HSM extension (milestone 7).
//!
//! Secondary harts (APs) are started with `sbi_hart_start`. Per the SBI spec a
//! started hart enters S-mode at the given address with paging disabled
//! (`satp=0`), `a0` = its hart id, and `a1` = the opaque value we passed. The
//! `_ap_start` stub then loads a per-hart stack and the shared kernel `satp`
//! from a boot block (handed over via the opaque pointer), turns paging on
//! (the kernel address space is identity-mapped, so addresses stay valid),
//! installs the trap vector, and calls [`ap_main`].
//!
//! This replaces the x86 AP trampoline. Bring-up is serialized (start one hart,
//! wait for it to come online, then the next) so the SBI console output stays
//! legible and we never run ahead of a hart that failed to start.

use core::arch::global_asm;
use core::sync::atomic::{AtomicUsize, Ordering};

use super::{cpu, frame, paging, trap};

/// SBI Hart State Management extension id ("HSM").
const SBI_EXT_HSM: usize = 0x48534D;
/// HSM function id: HART_START.
const SBI_FN_HART_START: usize = 0;
/// SBI success return code.
const SBI_SUCCESS: isize = 0;

/// Highest hart id we probe for (QEMU virt tops out well under this).
const MAX_HARTS: usize = 8;
/// Per-hart S-mode stack size in frames (16 KiB) — ample for the AP path.
const AP_STACK_FRAMES: usize = 4;
/// Spin bound while waiting for a hart to report online.
const ONLINE_SPIN_LIMIT: u32 = 50_000_000;

/// Number of secondary harts that have reached [`ap_main`].
static ONLINE: AtomicUsize = AtomicUsize::new(0);

/// Per-hart hand-off block read by `_ap_start` (offsets are fixed by the asm).
#[repr(C)]
struct HartBoot {
    /// Top of the AP's stack (stack grows down from here).
    stack_top: usize,
    /// `satp` value selecting the shared kernel address space.
    satp: usize,
}

// AP entry. SBI enters here in S-mode with paging off: a0 = hart id,
// a1 = &HartBoot (physical). Set up the stack, switch to the kernel page
// table, then call ap_main(hart id). a0 is preserved across the setup.
global_asm!(
    ".section .text,\"ax\"",
    ".global _ap_start",
    "_ap_start:",
    "ld sp, 0(a1)",     // stack_top
    "ld t0, 8(a1)",     // satp (shared kernel address space)
    "csrw satp, t0",
    "sfence.vma",
    "mv tp, a0",        // tp = hart id
    "mv fp, zero",
    "call {ap_main}",
    "2:",
    "wfi",
    "j 2b",
    ap_main = sym ap_main,
);

unsafe extern "C" {
    /// AP entry stub (above); only ever invoked by the SBI implementation.
    fn _ap_start();
}

/// Invokes `sbi_hart_start(hartid, start_addr, opaque)`; returns the SBI error
/// code (0 = success).
fn sbi_hart_start(hartid: usize, start_addr: usize, opaque: usize) -> isize {
    let error: isize;
    // SAFETY: the standard SBI ecall ABI — EID in a7, FID in a6, args in
    // a0..a2; the call returns the error code in a0 and a value in a1.
    unsafe {
        core::arch::asm!(
            "ecall",
            in("a7") SBI_EXT_HSM,
            in("a6") SBI_FN_HART_START,
            inout("a0") hartid => error,
            in("a1") start_addr,
            in("a2") opaque,
            lateout("a1") _,
            options(nostack),
        );
    }
    error
}

/// Starts the secondary harts, one at a time, and waits for each to come
/// online. `boot_hartid` is the hart already running this code.
pub fn bring_up_secondary_harts(boot_hartid: usize) {
    let satp = paging::read_satp();
    let start_addr = _ap_start as *const () as usize;
    let mut started = 0usize;

    for hartid in 0..MAX_HARTS {
        if hartid == boot_hartid {
            continue;
        }

        let Some(stack) = frame::alloc_contiguous(AP_STACK_FRAMES) else {
            crate::kprintln!("[feox] smp: out of frames for hart {} stack", hartid);
            break;
        };
        let Some(boot_block) = frame::alloc() else {
            crate::kprintln!("[feox] smp: out of frames for hart {} boot block", hartid);
            break;
        };

        // SAFETY: `boot_block` is a fresh identity-mapped frame; we write the
        // two fields `_ap_start` reads (matching `HartBoot`'s layout).
        unsafe {
            let block = boot_block as *mut HartBoot;
            (*block).stack_top = stack + AP_STACK_FRAMES * frame::FRAME_SIZE;
            (*block).satp = satp;
        }

        let expected = started + 1;
        if sbi_hart_start(hartid, start_addr, boot_block) != SBI_SUCCESS {
            // No such hart (or it can't start) — assume hart ids are contiguous.
            break;
        }
        started = expected;

        // Wait for this hart to reach ap_main before starting the next.
        let mut spins = 0u32;
        while ONLINE.load(Ordering::Acquire) < started {
            spins += 1;
            if spins >= ONLINE_SPIN_LIMIT {
                crate::kprintln!("[feox] smp: hart {} did not report online", hartid);
                break;
            }
            core::hint::spin_loop();
        }
    }

    crate::kprintln!(
        "[feox] smp: {} secondary hart(s) online (boot hart {})",
        ONLINE.load(Ordering::Acquire),
        boot_hartid
    );
    crate::kprintln!("[feox] milestone 7: SMP secondary harts up.");
}

/// Entry point for each secondary hart after `_ap_start` set up its stack and
/// switched to the kernel address space.
#[unsafe(no_mangle)]
extern "C" fn ap_main(hartid: usize) -> ! {
    cpu::disable_interrupts();
    trap::init();
    // Announce before signalling online so the BSP (which waits on the counter)
    // never races ahead of this hart's console output.
    crate::kprintln!("[feox]   hart {} alive (S-mode, paging on)", hartid);
    ONLINE.fetch_add(1, Ordering::Release);
    cpu::hlt_loop()
}
