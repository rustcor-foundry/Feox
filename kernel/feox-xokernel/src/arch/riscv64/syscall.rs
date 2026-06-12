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

use core::sync::atomic::AtomicU64;

use feox_asi::{
    AsiOp, CapDelegateArgs, CapHandle, CapInfo, CapPermissions, CapRequest, CapType, EventSlot,
    IRQ_SOURCE_NET_RX, IrqAttachArgs, IrqDetachArgs, MapFlags, MappedRegion, MemError,
    MemMapArgs, MemVtoPArgs, NetDeviceInfo, NetError, NetGetInfoArgs, NetRxArgs, NetTxArgs,
    PhysicalAddress, SYSCALL_CAP_ERROR_BASE, SYSCALL_ERR_INVALID_ARGS,
    SYSCALL_ERR_INVALID_OPCODE, SYSCALL_ERR_NOT_READY, SYSCALL_ERR_UNSUPPORTED,
    SYSCALL_MEM_ERROR_BASE, SYSCALL_NET_ERROR_BASE, SYSCALL_OK, SYSCALL_STORAGE_ERROR_BASE,
    StorageCompletion, StorageError, StoragePollArgs, StoragePollResult, StorageSubmitReadArgs,
    ThreadParkArgs,
};

use super::trap::{REG_A0, REG_A1, TrapFrame};
use super::{frame, net, paging, sched, umode};
use crate::capability;

/// Maximum Ethernet frame the net lane accepts (no jumbo frames).
const NET_MAX_FRAME: usize = 1514;

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

    // The storage lane's device (if bring-up created one) becomes a
    // CapType::StorageDevice capability — apps discover it via CapList.
    if let Some(lane) = storage_lane() {
        if let Ok(resource) = capability::register_bootstrap_storage_device_resource(
            PhysicalAddress(0),
            lane.disk_bytes,
        ) {
            if capability::mint_bootstrap_root_capability(
                resource,
                CapPermissions::READ | CapPermissions::WRITE,
            )
            .is_ok()
            {
                minted += 1;
            }
        }
    }

    // The live net device (if the QEMU bring-up found one) becomes a
    // CapType::NetDevice capability — apps discover it via CapList and drive
    // the net lane with it.
    if let Some(mmio) = net::mmio_base() {
        if let Ok(resource) = capability::register_bootstrap_net_device_resource(
            feox_asi::PhysicalAddress(mmio as u64),
            0x1000,
        ) {
            if capability::mint_bootstrap_root_capability(
                resource,
                CapPermissions::READ | CapPermissions::WRITE,
            )
            .is_ok()
            {
                minted += 1;
            }
        }
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
        AsiOp::IrqAttach => dispatch_irq_attach(args_ptr, args_len, out_value),
        AsiOp::IrqDetach => dispatch_irq_detach(args_ptr, args_len, out_value),
        AsiOp::NetSubmitTx => dispatch_net_tx(args_ptr, args_len, out_value),
        AsiOp::NetPollRx => dispatch_net_rx(args_ptr, args_len, out_value),
        AsiOp::NetGetInfo => dispatch_net_get_info(args_ptr, args_len, out_value),
        AsiOp::StorageSubmitRead => dispatch_storage_submit_read(args_ptr, args_len, out_value),
        AsiOp::StoragePoll => dispatch_storage_poll(args_ptr, args_len, out_value),
        AsiOp::ProcYield => SYSCALL_OK,
        // ProcExit is consumed at the trap boundary; reaching here means a
        // kernel-side caller used it, which the transport does not support.
        _ => SYSCALL_ERR_UNSUPPORTED,
    }
}

fn mem_error(error: MemError) -> u64 {
    SYSCALL_MEM_ERROR_BASE + error as u64
}

// ---- net lane (milestone 20) ----------------------------------------------

fn net_error(error: NetError) -> u64 {
    SYSCALL_NET_ERROR_BASE + error as u64
}

/// Verifies a `CapType::NetDevice` capability with READ + WRITE.
fn verify_net_device(handle: CapHandle) -> Result<(), u64> {
    match capability::verify_bootstrap_handle(
        handle,
        CapPermissions::READ | CapPermissions::WRITE,
    ) {
        Ok(view) if view.cap_type == CapType::NetDevice => Ok(()),
        _ => Err(net_error(NetError::InvalidCapability)),
    }
}

/// Resolves a buffer capability to an identity-mapped byte range with at
/// least `needed` bytes past `offset`.
fn net_buffer(handle: CapHandle, offset: u64, needed: usize, write: bool) -> Result<usize, u64> {
    let mut required = CapPermissions::READ;
    if write {
        required |= CapPermissions::WRITE;
    }
    let Ok((base, size)) = capability::cap_to_phys_base(handle, required) else {
        return Err(net_error(NetError::InvalidCapability));
    };
    if offset.saturating_add(needed as u64) > size {
        return Err(net_error(NetError::InvalidLength));
    }
    Ok(base.0 as usize + offset as usize)
}

/// `NetSubmitTx`: transmit one frame from capability-backed memory.
fn dispatch_net_tx(args_ptr: *const u8, args_len: u64, _out_value: &mut u64) -> u64 {
    if args_len != size_of::<NetTxArgs>() as u64 || args_ptr.is_null() {
        return SYSCALL_ERR_INVALID_ARGS;
    }
    let args = unsafe {
        // SAFETY: length checked, pointer non-null, read under sstatus.SUM.
        *(args_ptr.cast::<NetTxArgs>())
    };
    if let Err(code) = verify_net_device(args.device) {
        return code;
    }
    let length = args.length as usize;
    if !(14..=NET_MAX_FRAME).contains(&length) {
        return net_error(NetError::InvalidLength);
    }
    let pa = match net_buffer(args.buffer, args.offset, length, false) {
        Ok(pa) => pa,
        Err(code) => return code,
    };
    // SAFETY: the range was bounds-checked against the capability resource
    // and is identity-mapped RAM.
    let frame_bytes = unsafe { core::slice::from_raw_parts(pa as *const u8, length) };
    if net::tx_frame(frame_bytes) {
        SYSCALL_OK
    } else {
        net_error(NetError::SubmitFailed)
    }
}

/// `NetPollRx`: receive one pending frame into capability-backed memory.
/// Value register = frame length (0 = nothing pending).
fn dispatch_net_rx(args_ptr: *const u8, args_len: u64, out_value: &mut u64) -> u64 {
    if args_len != size_of::<NetRxArgs>() as u64 || args_ptr.is_null() {
        return SYSCALL_ERR_INVALID_ARGS;
    }
    let args = unsafe {
        // SAFETY: length checked, pointer non-null, read under sstatus.SUM.
        *(args_ptr.cast::<NetRxArgs>())
    };
    if let Err(code) = verify_net_device(args.device) {
        return code;
    }
    let pa = match net_buffer(args.buffer, args.offset, NET_MAX_FRAME, true) {
        Ok(pa) => pa,
        Err(code) => return code,
    };
    // SAFETY: bounds-checked capability memory, identity-mapped.
    let out = unsafe { core::slice::from_raw_parts_mut(pa as *mut u8, NET_MAX_FRAME) };
    *out_value = net::rx_frame(out).unwrap_or(0) as u64;
    SYSCALL_OK
}

/// `NetGetInfo`: report the device MAC and MTU through a caller pointer.
fn dispatch_net_get_info(args_ptr: *const u8, args_len: u64, _out_value: &mut u64) -> u64 {
    if args_len != size_of::<NetGetInfoArgs>() as u64 || args_ptr.is_null() {
        return SYSCALL_ERR_INVALID_ARGS;
    }
    let args = unsafe {
        // SAFETY: length checked, pointer non-null, read under sstatus.SUM.
        *(args_ptr.cast::<NetGetInfoArgs>())
    };
    if let Err(code) = verify_net_device(args.device) {
        return code;
    }
    let Some(mac) = net::mac() else {
        return net_error(NetError::NotInitialized);
    };
    let info = NetDeviceInfo {
        mac,
        _reserved: [0; 2],
        mtu: NET_MAX_FRAME as u32,
        _reserved2: 0,
    };
    if !args.out_info.is_null() {
        // SAFETY: caller-owned output slot (written under SUM for U-mode).
        unsafe { *args.out_info = info };
    }
    SYSCALL_OK
}

// ---- storage lane (milestone 27) ------------------------------------------

/// The storage lane's dedicated I/O queue ring + namespace geometry, plus the
/// single in-flight submission. Set once by [`init_storage_lane`] and never
/// moved afterwards (the `NvmeIoFuture` points into the ring's inflight map).
struct StorageLane {
    ring: feox_nvme::QueueRing<8>,
    block_size: usize,
    disk_bytes: u64,
    pending: Option<(u64, feox_nvme::NvmeIoFuture<8>)>,
    next_token: u64,
}

/// Boot-hart-only, same invariant as `frame.rs`.
static mut STORAGE_LANE: Option<StorageLane> = None;

#[allow(static_mut_refs)]
fn storage_lane() -> Option<&'static mut StorageLane> {
    // SAFETY: only the boot hart touches the storage lane (init at bring-up,
    // dispatch from the trap path with interrupts masked).
    unsafe { STORAGE_LANE.as_mut() }
}

/// Binds the storage lane to a dedicated NVMe I/O ring. The
/// `CapType::StorageDevice` capability itself is minted later by [`init`]
/// (which resets the bootstrap capability table — minting here would be
/// wiped).
pub fn init_storage_lane(ring: feox_nvme::QueueRing<8>, geometry: feox_nvme::NamespaceGeometry) {
    let block_size = geometry.block_size;
    // SAFETY: boot-hart-only static (see `storage_lane`).
    unsafe {
        STORAGE_LANE = Some(StorageLane {
            ring,
            block_size,
            disk_bytes: geometry.block_count * block_size as u64,
            pending: None,
            next_token: 1,
        });
    }
    crate::kprintln!(
        "[feox] asi: storage lane ready (queue 3, {}-byte blocks)",
        block_size
    );
}

fn storage_error(error: StorageError) -> u64 {
    SYSCALL_STORAGE_ERROR_BASE + error as u64
}

/// `StorageSubmitRead`: one-block read into capability-backed memory; value
/// register returns the poll token.
fn dispatch_storage_submit_read(args_ptr: *const u8, args_len: u64, out_value: &mut u64) -> u64 {
    if args_len != size_of::<StorageSubmitReadArgs>() as u64 || args_ptr.is_null() {
        return SYSCALL_ERR_INVALID_ARGS;
    }
    let args = unsafe {
        // SAFETY: length checked, pointer non-null, read under sstatus.SUM.
        *(args_ptr.cast::<StorageSubmitReadArgs>())
    };
    match capability::verify_bootstrap_handle(
        args.device,
        CapPermissions::READ | CapPermissions::WRITE,
    ) {
        Ok(view) if view.cap_type == CapType::StorageDevice => {}
        _ => return storage_error(StorageError::InvalidCapability),
    }
    if args.block_count != 1 {
        return storage_error(StorageError::UnsupportedBlockCount);
    }
    let Some(lane) = storage_lane() else {
        return storage_error(StorageError::NotInitialized);
    };
    if lane.pending.is_some() {
        return storage_error(StorageError::InflightTableFull);
    }
    // The buffer must hold one block past the offset.
    let needed = lane.block_size.max(1);
    let Ok((base, size)) = capability::cap_to_phys_base(
        args.buffer,
        CapPermissions::READ | CapPermissions::WRITE,
    ) else {
        return storage_error(StorageError::InvalidCapability);
    };
    if args.buffer_offset.saturating_add(needed as u64) > size {
        return storage_error(StorageError::InvalidCapability);
    }
    let buffer_pa = base.0 + args.buffer_offset;

    let command =
        feox_nvme::SubmissionQueueEntry::nvm_read(args.nsid, args.lba, 0, buffer_pa, 0);
    match lane.ring.submit(command) {
        Ok((_cid, future)) => {
            let token = lane.next_token;
            lane.next_token += 1;
            lane.pending = Some((token, future));
            *out_value = token;
            SYSCALL_OK
        }
        Err(_) => storage_error(StorageError::SubmitFailed),
    }
}

/// `StoragePoll`: drains completions and reports whether the token's
/// submission resolved (value = `StoragePollResult`; the completion record is
/// written through the caller pointer when Ready).
fn dispatch_storage_poll(args_ptr: *const u8, args_len: u64, out_value: &mut u64) -> u64 {
    if args_len != size_of::<StoragePollArgs>() as u64 || args_ptr.is_null() {
        return SYSCALL_ERR_INVALID_ARGS;
    }
    let args = unsafe {
        // SAFETY: length checked, pointer non-null, read under sstatus.SUM.
        *(args_ptr.cast::<StoragePollArgs>())
    };
    let Some(lane) = storage_lane() else {
        return storage_error(StorageError::NotInitialized);
    };
    let Some((token, future)) = lane.pending.as_mut() else {
        return storage_error(StorageError::InvalidToken);
    };
    if *token != args.token.0 {
        return storage_error(StorageError::InvalidToken);
    }
    lane.ring.process_completions();
    let waker = core::task::Waker::noop();
    let mut cx = core::task::Context::from_waker(waker);
    match core::future::Future::poll(core::pin::Pin::new(future), &mut cx) {
        core::task::Poll::Pending => {
            *out_value = StoragePollResult::NotReady as u64;
            SYSCALL_OK
        }
        core::task::Poll::Ready(outcome) => {
            lane.pending = None;
            let completion = match outcome {
                Ok(completion) => StorageCompletion {
                    nvme_sct: completion.status.sct,
                    nvme_sc: completion.status.sc,
                    dnr: u8::from(completion.status.dnr),
                    _reserved: 0,
                },
                Err(feox_nvme::NvmeError::CommandFailed(status)) => StorageCompletion {
                    nvme_sct: status.sct,
                    nvme_sc: status.sc,
                    dnr: u8::from(status.dnr),
                    _reserved: 0,
                },
                Err(_) => return storage_error(StorageError::SubmitFailed),
            };
            if !args.out_completion.is_null() {
                // SAFETY: caller-owned output slot (written under SUM).
                unsafe { *args.out_completion = completion };
            }
            *out_value = StoragePollResult::Ready as u64;
            SYSCALL_OK
        }
    }
}

// ---- IRQ lane (milestone 19) ----------------------------------------------

/// EventSlot PA attached to the net-RX source (0 = none). Translated at
/// attach time, so the interrupt path signals it via the identity map
/// regardless of which space is live.
static NET_RX_SLOT: AtomicUsize = AtomicUsize::new(0);
/// RX events seen since the last [`reset_net_rx`], attached or not — used to
/// flush pre-attach events so a wakeup can't be lost to the attach race.
static NET_RX_EVENTS: AtomicU64 = AtomicU64::new(0);

/// Interrupt-path hook (from `plic::handle_external`): count the RX event
/// and signal the attached slot, if any.
pub fn on_net_rx_event() {
    NET_RX_EVENTS.fetch_add(1, Ordering::AcqRel);
    signal_slot(NET_RX_SLOT.load(Ordering::Acquire));
}

fn signal_slot(pa: usize) {
    if pa != 0 {
        // SAFETY: pa was translated from a live R+W mapping at attach time
        // and points at an EventSlot (one AtomicU64) in identity-mapped RAM.
        unsafe { &*(pa as *const EventSlot) }.signal();
    }
}

/// RX events since the last reset (demo bookkeeping).
#[must_use]
pub fn net_rx_events() -> u64 {
    NET_RX_EVENTS.load(Ordering::Acquire)
}

/// Clears the IRQ lane's net-RX state (event count + attached slot).
pub fn reset_net_rx() {
    NET_RX_EVENTS.store(0, Ordering::Release);
    NET_RX_SLOT.store(0, Ordering::Release);
}

/// Attaches an event slot to an IRQ source. Only `IRQ_SOURCE_NET_RX` exists
/// so far. Events that fired before the attach are flushed as one signal.
fn dispatch_irq_attach(args_ptr: *const u8, args_len: u64, _out_value: &mut u64) -> u64 {
    if args_len != size_of::<IrqAttachArgs>() as u64 || args_ptr.is_null() {
        return SYSCALL_ERR_INVALID_ARGS;
    }
    let args = unsafe {
        // SAFETY: length checked, pointer non-null, read under sstatus.SUM.
        *(args_ptr.cast::<IrqAttachArgs>())
    };
    if args.source != IRQ_SOURCE_NET_RX {
        return SYSCALL_ERR_UNSUPPORTED;
    }
    // The slot must be a mapped, writable, u64-aligned VA in the caller's
    // space (the kernel increments it from the interrupt path).
    let slot_va = args.slot as usize;
    let space = paging::AddressSpace::from_active();
    let translated = if slot_va % 8 == 0 { space.translate(slot_va) } else { None };
    let Some((pa, flags)) = translated else {
        return mem_error(MemError::AddressNotMapped);
    };
    if flags & paging::PTE_W == 0 {
        return mem_error(MemError::AddressNotMapped);
    }
    NET_RX_SLOT.store(pa, Ordering::Release);
    if NET_RX_EVENTS.load(Ordering::Acquire) > 0 {
        signal_slot(pa);
    }
    SYSCALL_OK
}

/// Detaches the event slot from an IRQ source.
fn dispatch_irq_detach(args_ptr: *const u8, args_len: u64, _out_value: &mut u64) -> u64 {
    if args_len != size_of::<IrqDetachArgs>() as u64 || args_ptr.is_null() {
        return SYSCALL_ERR_INVALID_ARGS;
    }
    let args = unsafe {
        // SAFETY: length checked, pointer non-null, read under sstatus.SUM.
        *(args_ptr.cast::<IrqDetachArgs>())
    };
    if args.source != IRQ_SOURCE_NET_RX {
        return SYSCALL_ERR_UNSUPPORTED;
    }
    NET_RX_SLOT.store(0, Ordering::Release);
    SYSCALL_OK
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
