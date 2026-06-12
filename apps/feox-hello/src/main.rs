#![no_std]
#![no_main]

//! feox-hello: the first real Feox U-mode app (M16 delivery, M17 workload).
//!
//! The exokernel workflow, end to end, from user space: request physical
//! pages as a capability, map them into this process's address space, do real
//! work in that memory across a reschedule, prove the translation, then unmap
//! and release — leaving the kernel's capability ledger exactly as it was
//! found. Exits with `sum(i^2 for i in 0..1024) % 65521`, which the kernel
//! predicts independently; any step failure exits with a distinct `0xbNN`
//! code instead, so a CI failure names the broken step.

use feox_asi::MapFlags;
use feox_libos as libos;

/// rodata proof: mapped R-only by the loader; must read back intact.
static TAG: [u8; 4] = *b"feox";

/// bss proof: starts zero (loader zero-fill), so the data segment must be
/// mapped writable for the increment below.
static mut COUNTER: usize = 0;

const PAGES: usize = 2;
const BYTES: u64 = (PAGES * 4096) as u64;

#[unsafe(no_mangle)]
extern "C" fn _start() -> ! {
    libos::yield_now();
    if TAG != *b"feox" {
        libos::exit(0xb01);
    }
    // SAFETY: single-threaded process; no aliasing access to COUNTER.
    unsafe {
        COUNTER += 1;
        if COUNTER != 1 {
            libos::exit(0xb02);
        }
    }

    let Some(caps_before) = libos::cap_count() else {
        libos::exit(0xb03);
    };
    let Some(handle) = libos::cap_request_pages(PAGES, true) else {
        libos::exit(0xb04);
    };
    let Some(region) = libos::mem_map(handle, 0, BYTES, MapFlags::READ | MapFlags::WRITE) else {
        libos::exit(0xb05);
    };

    // Real work in capability-backed memory: fill with i^2, hold the mapping
    // across a reschedule, then sum it back.
    let words = (region.length_bytes / 8) as usize;
    let base = region.base as *mut u64;
    for i in 0..words {
        // SAFETY: the kernel mapped [base, base+length) R+W for this process.
        unsafe { base.add(i).write_volatile((i as u64) * (i as u64)) };
    }
    libos::yield_now();
    let mut sum = 0u64;
    for i in 0..words {
        // SAFETY: as above; the mapping survives the reschedule.
        sum += unsafe { base.add(i).read_volatile() };
    }

    // The mapped VA must translate back into the capability's resource.
    if libos::mem_vtop(handle, region.base).is_none() {
        libos::exit(0xb06);
    }

    // Clean up and prove the ledger balances.
    if !libos::mem_unmap(region) {
        libos::exit(0xb07);
    }
    if !libos::cap_release(handle) {
        libos::exit(0xb08);
    }
    if libos::cap_count() != Some(caps_before) {
        libos::exit(0xb09);
    }

    libos::exit((sum % 65521) as usize)
}
