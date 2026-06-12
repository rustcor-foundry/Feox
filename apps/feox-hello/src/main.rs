#![no_std]
#![no_main]

//! feox-hello: the first toolchain-built Feox U-mode app.
//!
//! Delivered into the kernel by `feox-xokernel`'s build.rs and run through
//! the M15 ELF loader in its own address space. It exercises every segment
//! the loader maps — text (this code), rodata ([`TAG`]), and bss
//! ([`COUNTER`], zero-initialized by the loader's `memsz > filesz` fill) —
//! plus the libOS syscall surface, then exits with a value the kernel can
//! predict: `fib(10) + COUNTER + cap_count` = 55 + 1 + the kernel's
//! capability total.

use feox_libos as libos;

/// rodata proof: mapped R-only by the loader; must read back intact.
static TAG: [u8; 4] = *b"feox";

/// bss proof: starts zero (loader zero-fill), incremented at runtime, so the
/// data segment must be mapped writable.
static mut COUNTER: usize = 0;

#[unsafe(no_mangle)]
extern "C" fn _start() -> ! {
    libos::yield_now();
    let caps = libos::cap_count().unwrap_or(0) as usize;
    // SAFETY: single-threaded process; no aliasing access to COUNTER.
    let count = unsafe {
        COUNTER += 1;
        COUNTER
    };
    if TAG != *b"feox" {
        libos::exit(0xbad);
    }
    libos::exit(fib(10) + count + caps)
}

/// Iterative Fibonacci — real computed Rust, fib(10) = 55.
fn fib(n: u32) -> usize {
    let (mut a, mut b) = (0usize, 1usize);
    for _ in 0..n {
        (a, b) = (b, a + b);
    }
    a
}
