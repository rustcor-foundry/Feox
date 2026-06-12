//! ASI syscall dispatch for the riscv64 ecall lane (milestone 13).
//!
//! Register ABI (mirrors the x86_64 SYSCALL lane's rdi/rsi/rdx contract):
//! the user places the raw [`AsiOp`] opcode in `a7`, an argument pointer in
//! `a0`, and the argument length in `a1`, then executes `ecall`. The kernel
//! returns the transport result code in `a0` and the call-specific value in
//! `a1`. `AsiOp::ProcExit` is intercepted at the trap boundary (it tears down
//! the U-mode excursion via `umode::exit_to_kernel`) and never reaches
//! [`dispatch`].
//!
//! This riscv64 lane serves the capability table (`CapRequest`, `CapRelease`,
//! `CapDelegate`, `CapList`), the memory lane (`MemMap`/`MemUnmap`/`MemVtoP`,
//! operating on the *calling process's* address space — the trap does not
//! switch `satp`, so `AddressSpace::from_active()` is the caller's space), and
//! `ProcYield`. The storage lane stays x86_64-only until its riscv64 backend
//! lands. Unifying both dispatchers over a portable core is a noted follow-up.

use core::mem::size_of;
use core::ptr::slice_from_raw_parts_mut;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use feox_asi::{
    AsiOp, CapDelegateArgs, CapHandle, CapInfo, CapPermissions, CapRequest, MapFlags,
    MappedRegion, MemError, MemMapArgs, MemVtoPArgs, PhysicalAddress, SYSCALL_CAP_ERROR_BASE,
    SYSCALL_ERR_INVALID_ARGS, SYSCALL_ERR_INVALID_OPCODE, SYSCALL_ERR_NOT_READY,
    SYSCALL_ERR_UNSUPPORTED, SYSCALL_MEM_ERROR_BASE, SYSCALL_OK, ThreadParkArgs,
};

use super::trap::{REG_A0, REG_A1, TrapFrame};
use super::{frame, paging, sched, umode};
use crate::capability;

/// User mmap window: kernel-chosen VAs for `MemMap`, inside sv39 root slot 8
/// (the process VA window, private per process space) and above any app
/// image/stack. A monotonic global bump — VAs are never reused, which keeps
/// distinct mappings (even across processes) at distinct addresses; each VA
/// only ever becomes live in the address space of the process that mapped it.
const MMAP_BASE: usize = 0x2_1000_0000;
/// One past the end of sv39 root slot 8.
const MMAP_END: usize = 0x2_4000_0000;
static MMAP_NEXT: AtomicUsize = AtomicUsize::new(MMAP_BASE);

static INITIALIZED: AtomicBool = AtomicBool::new(false);

/// Returns whether the ASI ecall lane is initialized.
#[must_use]
pub fn is_ready() -> bool {
    INITIALIZED.load(Ordering::Acquire)
}

/// Initializes the bootstrap capability system for the riscv64 lane: resets
/// the table, registers the frame allocator's RAM window as an introspection
/// resource (with a root capability), and carves a small frame-backed pool
/// that satisfies `CapRequest::PhysicalPages`.
pub fn init() {
    capability::init_bootstrap_process(feox_asi::ProcessId(0));

    // The whole managed RAM window, for introspection. Not allocatable: the
    // frame allocator owns placement inside it.
    let (ram_base, ram_end) = frame::window();
    let mut minted = 0usize;
    if let Ok(resource) = capability::register_bootstrap_memory_resource(
        feox_asi::PhysicalAddress(ram_base as u64),
        (ram_end - ram_base) as u64,
    ) {
        if capability::mint_bootstrap_root_capability(resource, feox_asi::CapPermissions::all())
            .is_ok()
        {
            minted += 1;
        }
    }

    // A dedicated pool for page-allocation requests, carved from the frame
    // allocator so capability-granted pages never collide with kernel frames.
    const POOL_FRAMES: usize = 16;
    if let Some(pool_base) = frame::alloc_contiguous(POOL_FRAMES) {
        let _ = capability::register_bootstrap_memory_resource_with_kind(
            feox_asi::PhysicalAddress(pool_base as u64),
            (POOL_FRAMES * frame::FRAME_SIZE) as u64,
            true,
        );
    }

    INITIALIZED.store(true, Ordering::Release);
    crate::kprintln!(
        "[feox] asi: ecall lane ready ({} resources, {} root cap{})",
        capability::resource_count_public(),
        minted,
        if minted == 1 { "" } else { "s" }
    );
}

/// Dispatches one ASI syscall from the riscv64 ecall lane. Called from the
/// trap dispatcher for U-mode ecalls and directly by the kernel-side
/// self-test, so both exercise the same validation + dispatch path.
pub fn dispatch(opcode_raw: u64, args_ptr: *const u8, args_len: u64, out_value: &mut u64) -> u64 {
    *out_value = 0;
    if !is_ready() {
        return SYSCALL_ERR_NOT_READY;
    }

    let Some(opcode) = AsiOp::from_raw(opcode_raw) else {
        return SYSCALL_ERR_INVALID_OPCODE;
    };

    match opcode {
        AsiOp::CapRequest => dispatch_cap_request(args_ptr, args_len, out_value),
        AsiOp::CapList => dispatch_cap_list(args_ptr.cast_mut(), args_len, out_value),
        AsiOp::CapRelease => dispatch_cap_release(args_ptr, args_len, out_value),
        AsiOp::CapDelegate => dispatch_cap_delegate(args_ptr, args_len, out_value),
        AsiOp::MemMap => dispatch_mem_map(args_ptr, args_len, out_value),
        AsiOp::MemUnmap => dispatch_mem_unmap(args_ptr, args_len, out_value),
        AsiOp::MemVtoP => dispatch_mem_vtop(args_ptr, args_len, out_value),
        AsiOp::ProcYield => SYSCALL_OK,
        // ProcExit is consumed at the trap boundary; reaching here means a
        // kernel-side caller used it, which the transport does not support.
        _ => SYSCALL_ERR_UNSUPPORTED,
    }
}

fn mem_error(error: MemError) -> u64 {
    SYSCALL_MEM_ERROR_BASE + error as u64
}

/// `ThreadPark` from a running thread (called from the trap dispatcher with
/// the live frame, since parking switches it). Futex-shaped: returns
/// immediately (value 1) if the slot's counter already differs from
/// `observed`; otherwise blocks the thread until the tick-driven wake scan
/// sees the counter change (value 1) or the timeout lapse (value 0).
pub fn park_from_user(frame: &mut TrapFrame) {
    // Resume past the ecall whenever this thread next runs.
    frame.sepc += 4;

    let args_ptr = frame.regs[REG_A0] as *const u8;
    let args_len = frame.regs[REG_A1] as u64;
    if args_len != size_of::<ThreadParkArgs>() as u64 || args_ptr.is_null() {
        frame.regs[REG_A0] = SYSCALL_ERR_INVALID_ARGS as usize;
        frame.regs[REG_A1] = 0;
        return;
    }
    let args = unsafe {
        // SAFETY: length checked, pointer non-null, read under sstatus.SUM.
        *(args_ptr.cast::<ThreadParkArgs>())
    };

    // The slot must be a mapped, readable, u64-aligned VA in the caller's
    // space. Translate it now so the wake scan can poll the counter through
    // the identity map regardless of which space is live later.
    let slot_va = args.slot as usize;
    let space = paging::AddressSpace::from_active();
    let translated = if slot_va % 8 == 0 { space.translate(slot_va) } else { None };
    let Some((slot_pa, flags)) = translated else {
        frame.regs[REG_A0] = mem_error(MemError::AddressNotMapped) as usize;
        frame.regs[REG_A1] = 0;
        return;
    };
    if flags & paging::PTE_R == 0 {
        frame.regs[REG_A0] = mem_error(MemError::AddressNotMapped) as usize;
        frame.regs[REG_A1] = 0;
        return;
    }

    // SAFETY: slot_pa is a readable mapped page, identity-visible to S-mode.
    let count = unsafe { (slot_pa as *const u64).read_volatile() };
    if count != args.observed {
        frame.regs[REG_A0] = SYSCALL_OK as usize;
        frame.regs[REG_A1] = 1;
        return;
    }

    let nanos = args.timeout.as_nanos();
    let deadline = if nanos == 0 {
        0
    } else {
        let ticks = nanos
            .saturating_mul(sched::SCHED_TICK_HZ)
            .div_ceil(1_000_000_000)
            .max(1);
        super::time::ticks().saturating_add(ticks)
    };
    sched::block_current(frame, slot_pa, args.observed, deadline);
}

/// Maps a capability-backed physical range into the calling process's address
/// space at a kernel-chosen VA from the mmap window. The capability must
/// grant READ (and WRITE when requested); offset/length must be page-aligned
/// and inside the capability's resource.
fn dispatch_mem_map(args_ptr: *const u8, args_len: u64, out_value: &mut u64) -> u64 {
    if args_len != size_of::<MemMapArgs>() as u64 || args_ptr.is_null() {
        return SYSCALL_ERR_INVALID_ARGS;
    }
    let args = unsafe {
        // SAFETY: length is checked against `MemMapArgs` and the pointer is
        // non-null (U-mode callers are read under sstatus.SUM).
        *(args_ptr.cast::<MemMapArgs>())
    };

    if !args.flags.contains(MapFlags::READ)
        || args.flags.contains(MapFlags::EXEC)
        || args.flags.contains(MapFlags::UNCACHEABLE)
        || args.flags.contains(MapFlags::WRITE_COMBINE)
    {
        return mem_error(MemError::InvalidFlags);
    }
    if args.length_bytes == 0
        || args.length_bytes % frame::FRAME_SIZE as u64 != 0
        || args.offset_bytes % frame::FRAME_SIZE as u64 != 0
    {
        return mem_error(MemError::AlignmentViolation);
    }

    let writable = args.flags.contains(MapFlags::WRITE);
    let mut required = CapPermissions::READ;
    if writable {
        required |= CapPermissions::WRITE;
    }
    let Ok((base, size)) = capability::cap_to_phys_base(args.handle, required) else {
        return mem_error(MemError::InvalidCapability);
    };
    if args.offset_bytes.saturating_add(args.length_bytes) > size {
        return mem_error(MemError::OffsetOutOfRange);
    }

    let length = args.length_bytes as usize;
    let va = MMAP_NEXT.fetch_add(length, Ordering::Relaxed);
    if va + length > MMAP_END {
        return mem_error(MemError::OutOfVirtualSpace);
    }

    let mut flags = paging::PTE_U | paging::PTE_R;
    if writable {
        flags |= paging::PTE_W;
    }
    // The trap left satp untouched, so the active space IS the caller's; for
    // a process this extends its private slot-8 subtree.
    let mut space = paging::AddressSpace::from_active();
    space.map(va, base.0 as usize + args.offset_bytes as usize, length, flags);
    paging::flush_tlb_all();

    let region = MappedRegion {
        base: va as u64,
        length_bytes: args.length_bytes,
        flags: args.flags,
    };
    if !args.out_region.is_null() {
        // SAFETY: caller-owned output slot (written under SUM for U-mode).
        unsafe { *args.out_region = region };
    }
    *out_value = va as u64;
    SYSCALL_OK
}

/// Unmaps a region previously returned by `MemMap` from the calling process's
/// address space. The backing frames stay owned by the capability.
fn dispatch_mem_unmap(args_ptr: *const u8, args_len: u64, _out_value: &mut u64) -> u64 {
    if args_len != size_of::<MappedRegion>() as u64 || args_ptr.is_null() {
        return SYSCALL_ERR_INVALID_ARGS;
    }
    let region = unsafe {
        // SAFETY: length is checked against `MappedRegion`, pointer non-null.
        *(args_ptr.cast::<MappedRegion>())
    };
    let base = region.base as usize;
    let length = region.length_bytes as usize;
    if base < MMAP_BASE
        || base % frame::FRAME_SIZE != 0
        || length == 0
        || length % frame::FRAME_SIZE != 0
        || base.saturating_add(length) > MMAP_END
    {
        return mem_error(MemError::AddressNotMapped);
    }
    let mut space = paging::AddressSpace::from_active();
    space.unmap(base, length);
    SYSCALL_OK
}

/// Translates a VA in the calling process's space; the result must fall
/// inside the supplied capability's physical resource.
fn dispatch_mem_vtop(args_ptr: *const u8, args_len: u64, out_value: &mut u64) -> u64 {
    if args_len != size_of::<MemVtoPArgs>() as u64 || args_ptr.is_null() {
        return SYSCALL_ERR_INVALID_ARGS;
    }
    let args = unsafe {
        // SAFETY: length is checked against `MemVtoPArgs`, pointer non-null.
        *(args_ptr.cast::<MemVtoPArgs>())
    };
    let Ok((base, size)) = capability::cap_to_phys_base(args.handle, CapPermissions::READ) else {
        return mem_error(MemError::InvalidCapability);
    };
    let space = paging::AddressSpace::from_active();
    let Some((pa, _)) = space.translate(args.virtual_address as usize) else {
        return mem_error(MemError::AddressNotMapped);
    };
    if (pa as u64) < base.0 || (pa as u64) >= base.0 + size {
        return mem_error(MemError::AddressNotMapped);
    }
    if !args.out_physical_address.is_null() {
        // SAFETY: caller-owned output slot (written under SUM for U-mode).
        unsafe { *args.out_physical_address = PhysicalAddress(pa as u64) };
    }
    *out_value = pa as u64;
    SYSCALL_OK
}

fn dispatch_cap_request(args_ptr: *const u8, args_len: u64, out_value: &mut u64) -> u64 {
    if args_len != size_of::<CapRequest>() as u64 || args_ptr.is_null() {
        return SYSCALL_ERR_INVALID_ARGS;
    }
    let request = unsafe {
        // SAFETY: length is checked against `CapRequest` and the pointer is
        // non-null. For U-mode callers the trap path runs with sstatus.SUM
        // set, so reading user memory is permitted.
        *(args_ptr.cast::<CapRequest>())
    };
    match capability::request_bootstrap_capability(&request) {
        Ok(handle) => {
            *out_value = (u64::from(handle.generation) << 32) | u64::from(handle.id);
            SYSCALL_OK
        }
        Err(error) => SYSCALL_CAP_ERROR_BASE + error as u64,
    }
}

fn dispatch_cap_list(args_ptr: *mut u8, args_len: u64, out_value: &mut u64) -> u64 {
    if args_len % size_of::<CapInfo>() as u64 != 0 {
        return SYSCALL_ERR_INVALID_ARGS;
    }
    let capacity = (args_len / size_of::<CapInfo>() as u64) as usize;
    let infos = if capacity == 0 || args_ptr.is_null() {
        &mut [][..]
    } else {
        unsafe {
            // SAFETY: the caller supplies a writable buffer and length (SUM is
            // set for U-mode callers, as above).
            &mut *slice_from_raw_parts_mut(args_ptr.cast::<CapInfo>(), capacity)
        }
    };
    let (_, total) = capability::list_bootstrap_capabilities(infos);
    *out_value = total as u64;
    SYSCALL_OK
}

// `dispatch` pre-zeroes the value register; release has no value to return.
fn dispatch_cap_release(args_ptr: *const u8, args_len: u64, _out_value: &mut u64) -> u64 {
    if args_len != size_of::<CapHandle>() as u64 || args_ptr.is_null() {
        return SYSCALL_ERR_INVALID_ARGS;
    }
    let handle = unsafe {
        // SAFETY: length is checked against `CapHandle` and the pointer is non-null.
        *(args_ptr.cast::<CapHandle>())
    };
    match capability::release_bootstrap_handle(handle) {
        Ok(()) => SYSCALL_OK,
        Err(error) => SYSCALL_CAP_ERROR_BASE + error as u64,
    }
}

fn dispatch_cap_delegate(args_ptr: *const u8, args_len: u64, out_value: &mut u64) -> u64 {
    if args_len != size_of::<CapDelegateArgs>() as u64 || args_ptr.is_null() {
        return SYSCALL_ERR_INVALID_ARGS;
    }
    let args = unsafe {
        // SAFETY: length is checked against `CapDelegateArgs` and the pointer is non-null.
        *(args_ptr.cast::<CapDelegateArgs>())
    };
    match capability::delegate_bootstrap_handle(args.handle, args.target_pid, args.mask) {
        Ok(handle) => {
            *out_value = (u64::from(handle.generation) << 32) | u64::from(handle.id);
            SYSCALL_OK
        }
        Err(error) => SYSCALL_CAP_ERROR_BASE + error as u64,
    }
}

/// Milestone 13 demo: bring up the capability system, exercise the dispatcher
/// from the kernel side (`CapRequest` for physical pages), then drop to U-mode
/// and let a user program query the capability table over the real ecall ABI.
pub fn demo() {
    init();

    // Kernel-side self-test through the same dispatch path U-mode hits:
    // request two contiguous physical pages and verify the returned handle.
    let request = CapRequest::PhysicalPages {
        num_pages: 2,
        flags: feox_asi::PageFlags::CONTIGUOUS,
    };
    let mut packed = 0u64;
    let code = dispatch(
        AsiOp::CapRequest as u64,
        (&raw const request).cast(),
        size_of::<CapRequest>() as u64,
        &mut packed,
    );
    let handle = CapHandle {
        id: packed as u32,
        generation: (packed >> 32) as u32,
    };
    let verified = code == SYSCALL_OK
        && capability::verify_bootstrap_handle(
            handle,
            feox_asi::CapPermissions::READ | feox_asi::CapPermissions::WRITE,
        )
        .is_ok();
    crate::kprintln!(
        "[feox] asi: kernel-side cap_request -> code={:#x} handle=({}, gen {}) verified={}",
        code,
        handle.id,
        handle.generation,
        verified
    );

    // U-mode round trip: the user program calls CapList (null buffer, so the
    // kernel returns just the total) and exits with that total as its exit
    // value — proving a real syscall crossed U->S->U and back out.
    let expected = capability::active_count();
    let program = [
        0x0000_0513, // li a0, 0       (args ptr = null)
        0x0000_0593, // li a1, 0       (args len = 0)
        0x0030_0893, // li a7, 0x003   (AsiOp::CapList)
        0x0000_0073, // ecall          -> a0 = code, a1 = total active caps
        0x0005_8513, // mv a0, a1      (exit value = the total)
        0x3010_0893, // li a7, 0x301   (AsiOp::ProcExit)
        0x0000_0073, // ecall          (never returns)
        0x0000_006f, // 1: j 1b        (unreached)
    ];
    crate::kprintln!("[feox] asi: entering U-mode for a CapList syscall...");
    match umode::run_user_program(&program) {
        Some(reported) => crate::kprintln!(
            "[feox] milestone 13: ASI syscall dispatch (U-mode CapList reported {} caps, kernel has {}, match={}).",
            reported,
            expected,
            reported == expected && verified
        ),
        None => crate::kprintln!("[feox] asi: out of frames for the U-mode syscall demo"),
    }
}
