#![cfg_attr(target_os = "none", no_std)]
#![cfg_attr(target_os = "none", no_main)]
#![forbid(unsafe_op_in_unsafe_fn)]

//! Bare-metal entrypoint for the Feox x86_64 bootstrap image.

#[cfg(target_os = "none")]
use core::arch::global_asm;
#[cfg(target_os = "none")]
use core::panic::PanicInfo;

#[cfg(target_os = "none")]
const BOOT_STACK_SIZE: usize = 16 * 1024;

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
