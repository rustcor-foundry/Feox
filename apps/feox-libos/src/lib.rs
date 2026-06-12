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
    AsiOp, CapHandle, CapInfo, CapRequest, CapType, Duration, EventSlot, IrqAttachArgs,
    IrqDetachArgs, MapFlags, MappedRegion, MemMapArgs, MemVtoPArgs, NetDeviceInfo,
    NetGetInfoArgs, NetRxArgs, NetTxArgs, PageFlags, PhysicalAddress, SYSCALL_OK,
    ThreadParkArgs,
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

/// Fills `out` with capability metadata; returns `(written, total)`.
pub fn cap_list(out: &mut [CapInfo]) -> Option<(usize, u64)> {
    let (code, total) = syscall(
        AsiOp::CapList,
        out.as_mut_ptr().cast(),
        core::mem::size_of_val(out),
    );
    (code == SYSCALL_OK).then(|| (out.len().min(total as usize), total))
}

/// Finds the first capability of `cap_type` visible to this process.
pub fn find_capability(cap_type: CapType) -> Option<CapHandle> {
    let mut infos = [CapInfo::default(); 16];
    let (written, _) = cap_list(&mut infos)?;
    infos[..written]
        .iter()
        .find(|info| info.cap_type == cap_type)
        .map(|info| info.handle)
}

/// Queries a net-device capability for its MAC and MTU.
pub fn net_get_info(device: CapHandle) -> Option<NetDeviceInfo> {
    let mut info = NetDeviceInfo::default();
    let args = NetGetInfoArgs {
        device,
        out_info: &mut info,
    };
    let (code, _) = syscall(
        AsiOp::NetGetInfo,
        (&raw const args).cast(),
        size_of::<NetGetInfoArgs>(),
    );
    (code == SYSCALL_OK).then_some(info)
}

/// Transmits one Ethernet frame from capability-backed memory.
pub fn net_tx(device: CapHandle, buffer: CapHandle, offset: u64, length: u32) -> bool {
    let args = NetTxArgs {
        device,
        buffer,
        offset,
        length,
        _reserved: 0,
    };
    let (code, _) = syscall(AsiOp::NetSubmitTx, (&raw const args).cast(), size_of::<NetTxArgs>());
    code == SYSCALL_OK
}

/// Receives one pending Ethernet frame into capability-backed memory.
/// `Some(0)` means nothing was pending.
pub fn net_rx(device: CapHandle, buffer: CapHandle, offset: u64) -> Option<u32> {
    let args = NetRxArgs {
        device,
        buffer,
        offset,
    };
    let (code, length) = syscall(AsiOp::NetPollRx, (&raw const args).cast(), size_of::<NetRxArgs>());
    (code == SYSCALL_OK).then_some(length as u32)
}

/// Parks (repeatedly, tolerating spurious wakes) until `slot`'s counter
/// reaches `target`. False on error or timeout.
pub fn park_until(slot: &EventSlot, target: u64, timeout: Duration) -> bool {
    loop {
        let current = slot.load();
        if current >= target {
            return true;
        }
        match park(slot, current, timeout) {
            Some(true) => {}
            Some(false) | None => return false,
        }
    }
}

/// Attaches `slot` to an IRQ source (e.g. `feox_asi::IRQ_SOURCE_NET_RX`):
/// the kernel signals the slot once per interrupt event. Events that fired
/// before the attach are flushed as one signal.
pub fn irq_attach(source: u32, slot: &EventSlot) -> bool {
    let args = IrqAttachArgs {
        source,
        _reserved: 0,
        slot,
    };
    let (code, _) = syscall(
        AsiOp::IrqAttach,
        (&raw const args).cast(),
        size_of::<IrqAttachArgs>(),
    );
    code == SYSCALL_OK
}

/// Detaches the event slot from an IRQ source.
pub fn irq_detach(source: u32) -> bool {
    let args = IrqDetachArgs {
        source,
        _reserved: 0,
    };
    let (code, _) = syscall(
        AsiOp::IrqDetach,
        (&raw const args).cast(),
        size_of::<IrqDetachArgs>(),
    );
    code == SYSCALL_OK
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
