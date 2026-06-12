#![cfg_attr(target_os = "none", no_std)]
#![cfg_attr(target_os = "none", no_main)]
#![forbid(unsafe_op_in_unsafe_fn)]

//! Bare-metal entrypoint for the Feox bootstrap image.
//!
//! Two architecture entries live here. x86_64 enters through the UEFI loader's
//! `BootInfo` handoff and runs the full `boot::bootstrap` flow. riscv64 enters
//! directly in S-mode (OpenSBI -> `-kernel` / U-Boot), receiving `a0=hartid`
//! and `a1=dtb`, and takes the minimal milestone-1 path that brings up the SBI
//! console and parks the boot hart.

#[cfg(target_os = "none")]
use core::arch::global_asm;
#[cfg(target_os = "none")]
use core::panic::PanicInfo;

#[cfg(all(target_os = "none", target_arch = "x86_64"))]
/// Size of the low-half boot stack used before the higher-half handoff.
///
/// The 16 KiB initial budget overflowed once the boot path started
/// pulling in feox-async + feox-nvme code (deeper stack frames in the
/// transition-root build, direct-map install, MMIO prebuild, PCI scan,
/// NVMe admin/IO queue setup, async-runtime spawn site). The overflow
/// silently corrupted the GDT static at the very bottom of .data
/// (a few hundred bytes below `BOOT_STACK`), which then made the
/// post-handoff `lgdt` reload load a zeroed GDT and lock up. 64 KiB
/// gives the boot path the headroom it needs.
const BOOT_STACK_SIZE: usize = 64 * 1024;

#[cfg(all(target_os = "none", target_arch = "x86_64"))]
#[unsafe(link_section = ".bss.boot_stack")]
static mut BOOT_STACK: [u8; BOOT_STACK_SIZE] = [0; BOOT_STACK_SIZE];

#[cfg(all(target_os = "none", target_arch = "x86_64"))]
global_asm!(
    ".section .text.boot,\"ax\"",
    ".global _start",
    "_start:",
    "lea rsp, [rip + {boot_stack}]",
    "add rsp, {boot_stack_size}",
    "xor rbp, rbp",
    "call {entry}",
    "2:",
    "hlt",
    "jmp 2b",
    boot_stack = sym BOOT_STACK,
    boot_stack_size = const BOOT_STACK_SIZE,
    entry = sym feox_entry,
);

#[cfg(all(target_os = "none", target_arch = "x86_64"))]
#[unsafe(no_mangle)]
extern "C" fn feox_entry(boot_info: *const feox_xokernel::bootabi::BootInfo) -> ! {
    let handoff = unsafe { feox_xokernel::bootabi::BootHandoff::from_ptr(boot_info) };
    feox_xokernel::boot::bootstrap(feox_xokernel::KernelConfig::default(), handoff)
}

#[cfg(all(target_os = "none", target_arch = "riscv64"))]
/// Size of the S-mode boot stack for the riscv64 milestone-1 path.
///
/// The minimal bring-up (console init + banner + park) needs only a shallow
/// stack; 64 KiB matches the x86_64 budget and leaves ample headroom as the
/// riscv64 bootstrap grows.
const BOOT_STACK_SIZE: usize = 64 * 1024;

#[cfg(all(target_os = "none", target_arch = "riscv64"))]
#[unsafe(link_section = ".bss.boot_stack")]
static mut BOOT_STACK: [u8; BOOT_STACK_SIZE] = [0; BOOT_STACK_SIZE];

#[cfg(all(target_os = "none", target_arch = "riscv64"))]
global_asm!(
    ".section .text.boot,\"ax\"",
    ".global _start",
    // RISC-V Linux boot image header (64 bytes): lets U-Boot's `booti` load
    // the objcopy'd flat Image on real hardware (Orange Pi RV / RV2) and
    // enter at code0 with a0=hartid, a1=dtb. QEMU's ELF loader jumps
    // straight to the `_start` entry symbol instead, skipping the header —
    // both paths work from one source.
    "feox_image_header:",
    "j _start",                            // code0: jump over the header
    ".word 0",                             // code1
    ".quad 0x200000",                      // text_offset: RAM base + 2 MiB
    ".quad __kernel_end - __kernel_start", // image_size (incl. bss footprint)
    ".quad 0",                             // flags (little-endian kernel)
    ".word 2",                             // header version 0.2
    ".word 0",                             // res1
    ".quad 0",                             // res2
    ".quad 0x5643534952",                  // magic: 'RISCV'
    ".word 0x05435352",                    // magic2
    ".word 0",                             // res3
    "_start:",
    // Zero bss FIRST (registers only; no stack yet): ELF loaders do this for
    // us, but `booti` copies a raw image and leaves bss as whatever was in
    // RAM. a0/a1 (hartid, dtb) are untouched throughout.
    "la t0, __sbss",
    "la t1, __ebss",
    "1:",
    "bgeu t0, t1, 2f",
    "sd zero, 0(t0)",
    "addi t0, t0, 8",
    "j 1b",
    "2:",
    // OpenSBI/U-Boot enter S-mode with a0=hartid, a1=dtb; preserve both
    // across the stack setup so they reach `feox_entry` as its two arguments.
    "la sp, {boot_stack}",
    "li t0, {boot_stack_size}",
    "add sp, sp, t0",
    "mv fp, zero",
    "call {entry}",
    "3:",
    "wfi",
    "j 3b",
    boot_stack = sym BOOT_STACK,
    boot_stack_size = const BOOT_STACK_SIZE,
    entry = sym feox_entry,
);

#[cfg(all(target_os = "none", target_arch = "riscv64"))]
#[unsafe(no_mangle)]
extern "C" fn feox_entry(hartid: usize, dtb: usize) -> ! {
    feox_xokernel::arch::riscv64::riscv_main(hartid, dtb)
}

#[cfg(target_os = "none")]
#[panic_handler]
fn panic(info: &PanicInfo<'_>) -> ! {
    feox_xokernel::arch::panic_handle(info)
}

#[cfg(not(target_os = "none"))]
fn main() {}
