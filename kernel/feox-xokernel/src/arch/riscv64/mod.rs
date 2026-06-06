//! riscv64 architecture support.
//!
//! First bring-up lane: S-mode entry, SBI serial console, and a `wfi` halt
//! loop. Trap handling, sv39 paging, SMP (SBI HSM), and the device-tree intake
//! land in subsequent passes; until then the kernel takes a minimal riscv64
//! path that bypasses the x86-shaped `boot::bootstrap` flow.

pub mod cpu;
pub mod panic;
pub mod serial;

use crate::{PROJECT_NAME, PROJECT_STYLE};

/// Minimal S-mode bring-up entry for the riscv64 milestone-1 path.
///
/// Called from `_start` (see `main.rs`) with the SBI-provided boot arguments:
/// `hartid` is the boot hart's id (`a0`) and `dtb` is the physical address of
/// the flattened device tree (`a1`). This deliberately bypasses
/// `boot::bootstrap`, which is still x86-shaped; it brings the console up,
/// prints the banner, and parks the hart. Subsequent passes grow this into the
/// real bootstrap (traps -> sv39 -> memory -> runtime).
pub fn riscv_main(hartid: usize, dtb: usize) -> ! {
    cpu::disable_interrupts();
    crate::console::init();

    crate::kprintln!();
    crate::kprintln!("{} - {}", PROJECT_NAME, PROJECT_STYLE);
    crate::kprintln!("arch: riscv64 (S-mode)");
    crate::kprintln!("boot hart: {}", hartid);
    crate::kprintln!("device tree: {:#x}", dtb);
    crate::kprintln!("[feox] riscv64 bring-up alive; parking boot hart.");

    cpu::hlt_loop()
}
