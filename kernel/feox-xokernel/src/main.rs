#![cfg_attr(target_os = "none", no_std)]
#![cfg_attr(target_os = "none", no_main)]
#![forbid(unsafe_op_in_unsafe_fn)]

//! Bare-metal entrypoint for the Feox x86_64 bootstrap image.

#[cfg(target_os = "none")]
use core::arch::global_asm;
#[cfg(target_os = "none")]
use core::panic::PanicInfo;

#[cfg(target_os = "none")]
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

#[cfg(target_os = "none")]
#[unsafe(link_section = ".bss.boot_stack")]
static mut BOOT_STACK: [u8; BOOT_STACK_SIZE] = [0; BOOT_STACK_SIZE];

#[cfg(target_os = "none")]
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

#[cfg(target_os = "none")]
#[unsafe(no_mangle)]
extern "C" fn feox_entry(boot_info: *const feox_xokernel::bootabi::BootInfo) -> ! {
    let handoff = unsafe { feox_xokernel::bootabi::BootHandoff::from_ptr(boot_info) };
    feox_xokernel::boot::bootstrap(feox_xokernel::KernelConfig::default(), handoff)
}

#[cfg(target_os = "none")]
#[panic_handler]
fn panic(info: &PanicInfo<'_>) -> ! {
    feox_xokernel::arch::panic_handle(info)
}

#[cfg(not(target_os = "none"))]
fn main() {}
