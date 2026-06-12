#![no_std]

//! Minimal libOS for Feox U-mode apps: the ASI ecall ABI as Rust functions.
//!
//! Transport (see `arch/riscv64/syscall.rs` in the kernel): the opcode goes in
//! `a7`, the argument pointer/length in `a0`/`a1`; the kernel returns the
//! transport result code in `a0` and the call-specific value in `a1`.
//! `ProcExit` never returns. The opcodes and result codes come from
//! `feox-asi`, the shared ABI crate, so app and kernel cannot drift.

use core::mem::size_of;

use feox_asi::{
    AsiOp, CapHandle, CapRequest, Duration, EventSlot, MapFlags, MappedRegion, MemMapArgs,
    MemVtoPArgs, PageFlags, PhysicalAddress, SYSCALL_OK, ThreadParkArgs,
};

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

/// Requests a capability over `num_pages` fresh physical pages.
pub fn cap_request_pages(num_pages: usize, contiguous: bool) -> Option<CapHandle> {
    let request = CapRequest::PhysicalPages {
        num_pages,
        flags: if contiguous {
            PageFlags::CONTIGUOUS
        } else {
            PageFlags::empty()
        },
    };
    let (code, packed) = syscall(
        AsiOp::CapRequest,
        (&raw const request).cast(),
        size_of::<CapRequest>(),
    );
    (code == SYSCALL_OK).then(|| CapHandle {
        id: packed as u32,
        generation: (packed >> 32) as u32,
    })
}

/// Releases a capability (cascading through any delegated children).
pub fn cap_release(handle: CapHandle) -> bool {
    let (code, _) = syscall(
        AsiOp::CapRelease,
        (&raw const handle).cast(),
        size_of::<CapHandle>(),
    );
    code == SYSCALL_OK
}

/// Maps `length_bytes` of the capability's backing memory (from
/// `offset_bytes`) into this process's address space at a kernel-chosen VA.
pub fn mem_map(
    handle: CapHandle,
    offset_bytes: u64,
    length_bytes: u64,
    flags: MapFlags,
) -> Option<MappedRegion> {
    let mut region = MappedRegion::default();
    let args = MemMapArgs {
        handle,
        offset_bytes,
        length_bytes,
        flags,
        out_region: &mut region,
    };
    let (code, _) = syscall(AsiOp::MemMap, (&raw const args).cast(), size_of::<MemMapArgs>());
    (code == SYSCALL_OK).then_some(region)
}

/// Unmaps a region previously returned by [`mem_map`].
pub fn mem_unmap(region: MappedRegion) -> bool {
    let (code, _) = syscall(
        AsiOp::MemUnmap,
        (&raw const region).cast(),
        size_of::<MappedRegion>(),
    );
    code == SYSCALL_OK
}

/// Translates a mapped VA back to the physical address inside `handle`'s
/// resource.
pub fn mem_vtop(handle: CapHandle, virtual_address: u64) -> Option<u64> {
    let mut physical = PhysicalAddress(0);
    let args = MemVtoPArgs {
        handle,
        virtual_address,
        out_physical_address: &mut physical,
    };
    let (code, value) = syscall(AsiOp::MemVtoP, (&raw const args).cast(), size_of::<MemVtoPArgs>());
    (code == SYSCALL_OK).then_some(value)
}

/// Parks this thread on `slot` until its counter differs from `observed` or
/// the timeout lapses (`Duration::from_nanos(0)` = no timeout). Returns
/// `Some(true)` when woken by a counter change, `Some(false)` on timeout,
/// `None` on error. Futex-shaped: returns immediately if the counter already
/// moved, so wakeups cannot be lost between the load and the park.
pub fn park(slot: &EventSlot, observed: u64, timeout: Duration) -> Option<bool> {
    let args = ThreadParkArgs {
        slot,
        observed,
        timeout,
    };
    let (code, value) = syscall(
        AsiOp::ThreadPark,
        (&raw const args).cast(),
        size_of::<ThreadParkArgs>(),
    );
    (code == SYSCALL_OK).then_some(value == 1)
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
