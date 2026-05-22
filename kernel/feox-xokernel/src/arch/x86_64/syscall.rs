//! Minimal `SYSCALL` / `SYSRET` transport for the x86_64 ASI lane.

#[cfg(target_os = "none")]
use core::arch::global_asm;
use core::mem::size_of;
use core::ptr::{addr_of, slice_from_raw_parts_mut};
use core::sync::atomic::{AtomicBool, Ordering};

use feox_asi::{
    AsiOp, BatchOp, CapDelegateArgs, CapHandle, CapInfo, CapRequest, MemError, MemMapArgs,
    MemVtoPArgs, MemVtoPBatchArgs, MappedRegion, PhysicalAddress, StorageError, StoragePollArgs,
    StoragePollResult, StorageSubmitReadArgs,
};

#[cfg(target_os = "none")]
use super::cpu;
use super::gdt;

#[cfg(target_os = "none")]
const IA32_STAR: u32 = 0xC000_0081;
#[cfg(target_os = "none")]
const IA32_LSTAR: u32 = 0xC000_0082;
#[cfg(target_os = "none")]
const IA32_FMASK: u32 = 0xC000_0084;
#[cfg(target_os = "none")]
const IA32_EFER: u32 = 0xC000_0080;
#[cfg(target_os = "none")]
const EFER_SCE: u64 = 1 << 0;
#[cfg(target_os = "none")]
const SYSCALL_RFLAGS_MASK: u64 = (1 << 9) | (1 << 10);

const SYSCALL_OK: u64 = 0;
const SYSCALL_ERR_UNSUPPORTED: u64 = 0xFFFF_0001;
const SYSCALL_ERR_INVALID_OPCODE: u64 = 0xFFFF_0002;
const SYSCALL_ERR_INVALID_ARGS: u64 = 0xFFFF_0003;
const SYSCALL_ERR_BATCH_FAILED: u64 = 0xFFFF_0004;
const SYSCALL_ERR_NOT_READY: u64 = 0xFFFF_0005;
const SYSCALL_CAP_ERROR_BASE: u64 = 0x100;
const SYSCALL_MEM_ERROR_BASE: u64 = 0x200;
const SYSCALL_STORAGE_ERROR_BASE: u64 = 0xFFFF_0500;

const SYSCALL_STACK_SIZE: usize = 16 * 1024;

#[repr(C, align(16))]
struct AlignedSyscallStack([u8; SYSCALL_STACK_SIZE]);

static INITIALIZED: AtomicBool = AtomicBool::new(false);
static mut SYSCALL_STACK: AlignedSyscallStack = AlignedSyscallStack([0; SYSCALL_STACK_SIZE]);
static mut SYSCALL_KERNEL_STACK_TOP: u64 = 0;
#[cfg(target_os = "none")]
static mut SYSCALL_USER_RSP: u64 = 0;

#[cfg(target_os = "none")]
global_asm!(
    ".global feox_syscall_entry",
    "feox_syscall_entry:",
    "cld",
    "mov [rip + {user_rsp}], rsp",
    "mov rsp, [rip + {kernel_stack_top}]",
    "push r11",
    "push rcx",
    "push rdi",
    "push rsi",
    "push rax",
    "sub rsp, 8",
    "mov qword ptr [rsp], 0",
    "mov rdi, [rsp + 8]",
    "mov rsi, [rsp + 24]",
    "mov rdx, [rsp + 16]",
    "lea rcx, [rsp]",
    "call {dispatch}",
    "mov rdi, [rsp]",
    "add rsp, 32",
    "pop rcx",
    "pop r11",
    "mov rsp, [rip + {user_rsp}]",
    "sysretq",
    kernel_stack_top = sym SYSCALL_KERNEL_STACK_TOP,
    user_rsp = sym SYSCALL_USER_RSP,
    dispatch = sym feox_syscall_dispatch,
);

#[cfg(target_os = "none")]
unsafe extern "C" {
    fn feox_syscall_entry();
}

fn syscall_stack_top() -> u64 {
    addr_of!(SYSCALL_STACK) as u64 + SYSCALL_STACK_SIZE as u64
}

#[cfg(any(target_os = "none", test))]
fn star_value(kernel_cs: u16, sysret_base: u16) -> u64 {
    (u64::from(sysret_base) << 48) | (u64::from(kernel_cs) << 32)
}

/// Returns whether the ASI syscall transport is initialized on this core.
#[must_use]
pub fn is_ready() -> bool {
    INITIALIZED.load(Ordering::Acquire)
}

/// Installs the x86_64 `SYSCALL` / `SYSRET` transport for the current lane.
pub fn init() {
    let syscall_stack_top = syscall_stack_top();
    unsafe {
        SYSCALL_KERNEL_STACK_TOP = syscall_stack_top;
    }
    gdt::set_privilege_stack0(syscall_stack_top);

    #[cfg(target_os = "none")]
    unsafe {
        let efer = cpu::rdmsr(IA32_EFER);
        cpu::wrmsr(IA32_EFER, efer | EFER_SCE);
        cpu::wrmsr(
            IA32_STAR,
            star_value(
                gdt::kernel_code_selector(),
                gdt::syscall_sysret_base_selector(),
            ),
        );
        cpu::wrmsr(IA32_LSTAR, feox_syscall_entry as *const () as usize as u64);
        cpu::wrmsr(IA32_FMASK, SYSCALL_RFLAGS_MASK);
    }

    INITIALIZED.store(true, Ordering::Release);
}

/// Dispatches one ASI syscall. Exposed as a C symbol so the inline
/// SYSCALL/SYSRET trampoline can jump to it; kernel-side callers
/// (e.g. the boot self-test) invoke it directly to exercise the same
/// validation + dispatch path that ring 3 hits.
#[unsafe(no_mangle)]
pub extern "C" fn feox_syscall_dispatch(
    opcode_raw: u64,
    args_ptr: *const u8,
    args_len: u64,
    out_value: *mut u64,
) -> u64 {
    if !is_ready() {
        write_out(out_value, 0);
        return SYSCALL_ERR_NOT_READY;
    }

    let Some(opcode) = AsiOp::from_raw(opcode_raw) else {
        write_out(out_value, 0);
        return SYSCALL_ERR_INVALID_OPCODE;
    };

    // Pump any pending NVMe completions before dispatching. This is how
    // StoragePoll sees a freshly delivered completion without forcing
    // user space to drive a drainer task itself. Cheap when the I/O CQ
    // is empty (one volatile read of the CQE phase bit).
    if crate::block::is_initialized() {
        crate::block::drain();
    }

    match opcode {
        AsiOp::CapRequest => dispatch_cap_request(args_ptr, args_len, out_value),
        AsiOp::CapList => dispatch_cap_list(args_ptr.cast_mut(), args_len, out_value),
        AsiOp::CapDelegate => dispatch_cap_delegate(args_ptr, args_len, out_value),
        AsiOp::CapRelease => dispatch_cap_release(args_ptr, args_len, out_value),
        AsiOp::MemMap => dispatch_mem_map(args_ptr, args_len, out_value),
        AsiOp::MemUnmap => dispatch_mem_unmap(args_ptr, args_len, out_value),
        AsiOp::MemVtoP => dispatch_mem_vtop(args_ptr, args_len, out_value),
        AsiOp::MemVtoPBatch => dispatch_mem_vtop_batch(args_ptr, args_len, out_value),
        AsiOp::ProcYield => {
            push_event_if_ready("asi-proc-yield");
            write_out(out_value, 0);
            SYSCALL_OK
        }
        AsiOp::StorageSubmitRead => dispatch_storage_submit_read(args_ptr, args_len, out_value),
        AsiOp::StoragePoll => dispatch_storage_poll(args_ptr, args_len, out_value),
        AsiOp::AsiBatch => dispatch_batch(args_ptr, args_len, out_value),
        _ => {
            push_event_if_ready("asi-op-unsupported");
            write_out(out_value, 0);
            SYSCALL_ERR_UNSUPPORTED
        }
    }
}

fn dispatch_cap_request(args_ptr: *const u8, args_len: u64, out_value: *mut u64) -> u64 {
    if args_len != size_of::<CapRequest>() as u64 || args_ptr.is_null() {
        write_out(out_value, 0);
        return SYSCALL_ERR_INVALID_ARGS;
    }

    let request = unsafe {
        // SAFETY: length is checked against `CapRequest` and the pointer is non-null.
        *(args_ptr.cast::<CapRequest>())
    };

    match crate::capability::request_bootstrap_capability(&request) {
        Ok(handle) => {
            push_event_if_ready("asi-cap-request");
            write_out(
                out_value,
                ((handle.generation as u64) << 32) | u64::from(handle.id),
            );
            SYSCALL_OK
        }
        Err(error) => {
            write_out(out_value, 0);
            syscall_cap_error(error)
        }
    }
}

fn dispatch_cap_list(args_ptr: *mut u8, args_len: u64, out_value: *mut u64) -> u64 {
    if args_len % size_of::<CapInfo>() as u64 != 0 {
        write_out(out_value, 0);
        return SYSCALL_ERR_INVALID_ARGS;
    }

    let capacity = (args_len / size_of::<CapInfo>() as u64) as usize;
    let infos = if capacity == 0 || args_ptr.is_null() {
        &mut [][..]
    } else {
        unsafe {
            // SAFETY: the caller supplies a writable buffer and length.
            &mut *slice_from_raw_parts_mut(args_ptr.cast::<CapInfo>(), capacity)
        }
    };

    let (written, total) = crate::capability::list_bootstrap_capabilities(infos);
    push_event_if_ready("asi-cap-list");
    write_out(out_value, total as u64);
    if written < total && capacity < total {
        return SYSCALL_OK;
    }
    SYSCALL_OK
}

fn dispatch_cap_release(args_ptr: *const u8, args_len: u64, out_value: *mut u64) -> u64 {
    if args_len != size_of::<CapHandle>() as u64 || args_ptr.is_null() {
        write_out(out_value, 0);
        return SYSCALL_ERR_INVALID_ARGS;
    }

    let handle = unsafe {
        // SAFETY: length is checked against `CapHandle` and the pointer is non-null.
        *(args_ptr.cast::<CapHandle>())
    };

    match crate::capability::release_bootstrap_handle(handle) {
        Ok(()) => {
            push_event_if_ready("asi-cap-release");
            write_out(out_value, 0);
            SYSCALL_OK
        }
        Err(error) => {
            write_out(out_value, 0);
            syscall_cap_error(error)
        }
    }
}

fn dispatch_cap_delegate(args_ptr: *const u8, args_len: u64, out_value: *mut u64) -> u64 {
    if args_len != size_of::<CapDelegateArgs>() as u64 || args_ptr.is_null() {
        write_out(out_value, 0);
        return SYSCALL_ERR_INVALID_ARGS;
    }

    let args = unsafe {
        // SAFETY: length is checked against `CapDelegateArgs` and the pointer is non-null.
        *(args_ptr.cast::<CapDelegateArgs>())
    };

    match crate::capability::delegate_bootstrap_handle(args.handle, args.target_pid, args.mask) {
        Ok(handle) => {
            push_event_if_ready("asi-cap-delegate");
            write_out(
                out_value,
                ((handle.generation as u64) << 32) | u64::from(handle.id),
            );
            SYSCALL_OK
        }
        Err(error) => {
            write_out(out_value, 0);
            syscall_cap_error(error)
        }
    }
}

fn dispatch_mem_map(args_ptr: *const u8, args_len: u64, out_value: *mut u64) -> u64 {
    if args_len != size_of::<MemMapArgs>() as u64 || args_ptr.is_null() {
        write_out(out_value, 0);
        return SYSCALL_ERR_INVALID_ARGS;
    }

    let args = unsafe {
        // SAFETY: length is checked against `MemMapArgs` and the pointer is non-null.
        *(args_ptr.cast::<MemMapArgs>())
    };

    match crate::vm::mem_map_bootstrap(args) {
        Ok(region) => {
            write_mapped_region(args.out_region, region);
            push_event_if_ready("asi-mem-map");
            write_out(out_value, region.base);
            SYSCALL_OK
        }
        Err(error) => {
            write_out(out_value, 0);
            syscall_mem_error(error.as_mem_error())
        }
    }
}

fn dispatch_mem_unmap(args_ptr: *const u8, args_len: u64, out_value: *mut u64) -> u64 {
    if args_len != size_of::<MappedRegion>() as u64 || args_ptr.is_null() {
        write_out(out_value, 0);
        return SYSCALL_ERR_INVALID_ARGS;
    }

    let region = unsafe {
        // SAFETY: length is checked against `MappedRegion` and the pointer is non-null.
        *(args_ptr.cast::<MappedRegion>())
    };

    match crate::vm::mem_unmap_bootstrap(region) {
        Ok(()) => {
            push_event_if_ready("asi-mem-unmap");
            write_out(out_value, 0);
            SYSCALL_OK
        }
        Err(error) => {
            write_out(out_value, 0);
            syscall_mem_error(error.as_mem_error())
        }
    }
}

fn dispatch_mem_vtop(args_ptr: *const u8, args_len: u64, out_value: *mut u64) -> u64 {
    if args_len != size_of::<MemVtoPArgs>() as u64 || args_ptr.is_null() {
        write_out(out_value, 0);
        return SYSCALL_ERR_INVALID_ARGS;
    }

    let args = unsafe {
        // SAFETY: length is checked against `MemVtoPArgs` and the pointer is non-null.
        *(args_ptr.cast::<MemVtoPArgs>())
    };

    match crate::vm::mem_vtop_bootstrap(args) {
        Ok(physical_address) => {
            write_physical_address(args.out_physical_address, physical_address);
            push_event_if_ready("asi-mem-vtop");
            write_out(out_value, physical_address.0);
            SYSCALL_OK
        }
        Err(error) => {
            write_out(out_value, 0);
            syscall_mem_error(error.as_mem_error())
        }
    }
}

fn dispatch_mem_vtop_batch(args_ptr: *const u8, args_len: u64, out_value: *mut u64) -> u64 {
    if args_len != size_of::<MemVtoPBatchArgs>() as u64 || args_ptr.is_null() {
        write_out(out_value, 0);
        return SYSCALL_ERR_INVALID_ARGS;
    }

    let args = unsafe {
        // SAFETY: length is checked against `MemVtoPBatchArgs` and the pointer is non-null.
        *(args_ptr.cast::<MemVtoPBatchArgs>())
    };

    match crate::vm::mem_vtop_batch_bootstrap(args) {
        Ok(count) => {
            push_event_if_ready("asi-mem-vtop-batch");
            write_out(out_value, count as u64);
            SYSCALL_OK
        }
        Err(error) => {
            write_out(out_value, 0);
            syscall_mem_error(error.as_mem_error())
        }
    }
}

fn dispatch_batch(args_ptr: *const u8, args_len: u64, out_value: *mut u64) -> u64 {
    if args_len == 0 || args_ptr.is_null() {
        write_out(out_value, 0);
        return SYSCALL_ERR_INVALID_ARGS;
    }

    if args_len % size_of::<BatchOp>() as u64 != 0 {
        write_out(out_value, 0);
        return SYSCALL_ERR_INVALID_ARGS;
    }

    let count = (args_len / size_of::<BatchOp>() as u64) as usize;
    let ops = unsafe {
        // SAFETY: the batch wrapper length is validated above, so the slice
        // covers whole `BatchOp` records. Pointer validity remains the
        // caller's responsibility in this first bootstrap lane.
        &mut *slice_from_raw_parts_mut(args_ptr.cast_mut().cast::<BatchOp>(), count)
    };

    for (index, op) in ops.iter_mut().enumerate() {
        let mut value = 0;
        let code = dispatch_batch_op(*op, &mut value);
        op.result.code = code;
        op.result.value = value;
        if code != SYSCALL_OK {
            write_out(out_value, index as u64);
            return SYSCALL_ERR_BATCH_FAILED;
        }
    }

    push_event_if_ready("asi-batch-complete");
    write_out(out_value, count as u64);
    SYSCALL_OK
}

fn dispatch_batch_op(op: BatchOp, out_value: &mut u64) -> u64 {
    match op.opcode {
        AsiOp::AsiBatch => {
            *out_value = 0;
            SYSCALL_ERR_INVALID_ARGS
        }
        AsiOp::CapRequest => {
            if op.args.is_null() || op.args_len != size_of::<CapRequest>() {
                *out_value = 0;
                return SYSCALL_ERR_INVALID_ARGS;
            }
            let request = unsafe {
                // SAFETY: batch validation checked pointer non-null and exact size.
                *(op.args.cast::<CapRequest>())
            };
            match crate::capability::request_bootstrap_capability(&request) {
                Ok(handle) => {
                    push_event_if_ready("asi-cap-request");
                    *out_value = ((handle.generation as u64) << 32) | u64::from(handle.id);
                    SYSCALL_OK
                }
                Err(error) => {
                    *out_value = 0;
                    syscall_cap_error(error)
                }
            }
        }
        AsiOp::CapList => {
            *out_value = 0;
            SYSCALL_ERR_INVALID_ARGS
        }
        AsiOp::CapDelegate => {
            if op.args.is_null() || op.args_len != size_of::<CapDelegateArgs>() {
                *out_value = 0;
                return SYSCALL_ERR_INVALID_ARGS;
            }
            let args = unsafe {
                // SAFETY: batch validation checked pointer non-null and exact size.
                *(op.args.cast::<CapDelegateArgs>())
            };
            match crate::capability::delegate_bootstrap_handle(args.handle, args.target_pid, args.mask)
            {
                Ok(handle) => {
                    push_event_if_ready("asi-cap-delegate");
                    *out_value = ((handle.generation as u64) << 32) | u64::from(handle.id);
                    SYSCALL_OK
                }
                Err(error) => {
                    *out_value = 0;
                    syscall_cap_error(error)
                }
            }
        }
        AsiOp::MemMap => {
            if op.args.is_null() || op.args_len != size_of::<MemMapArgs>() {
                *out_value = 0;
                return SYSCALL_ERR_INVALID_ARGS;
            }
            let args = unsafe {
                // SAFETY: batch validation checked pointer non-null and exact size.
                *(op.args.cast::<MemMapArgs>())
            };
            match crate::vm::mem_map_bootstrap(args) {
                Ok(region) => {
                    write_mapped_region(args.out_region, region);
                    push_event_if_ready("asi-mem-map");
                    *out_value = region.base;
                    SYSCALL_OK
                }
                Err(error) => {
                    *out_value = 0;
                    syscall_mem_error(error.as_mem_error())
                }
            }
        }
        AsiOp::MemUnmap => {
            if op.args.is_null() || op.args_len != size_of::<MappedRegion>() {
                *out_value = 0;
                return SYSCALL_ERR_INVALID_ARGS;
            }
            let region = unsafe {
                // SAFETY: batch validation checked pointer non-null and exact size.
                *(op.args.cast::<MappedRegion>())
            };
            match crate::vm::mem_unmap_bootstrap(region) {
                Ok(()) => {
                    push_event_if_ready("asi-mem-unmap");
                    *out_value = 0;
                    SYSCALL_OK
                }
                Err(error) => {
                    *out_value = 0;
                    syscall_mem_error(error.as_mem_error())
                }
            }
        }
        AsiOp::MemVtoP => {
            if op.args.is_null() || op.args_len != size_of::<MemVtoPArgs>() {
                *out_value = 0;
                return SYSCALL_ERR_INVALID_ARGS;
            }
            let args = unsafe {
                // SAFETY: batch validation checked pointer non-null and exact size.
                *(op.args.cast::<MemVtoPArgs>())
            };
            match crate::vm::mem_vtop_bootstrap(args) {
                Ok(physical_address) => {
                    write_physical_address(args.out_physical_address, physical_address);
                    push_event_if_ready("asi-mem-vtop");
                    *out_value = physical_address.0;
                    SYSCALL_OK
                }
                Err(error) => {
                    *out_value = 0;
                    syscall_mem_error(error.as_mem_error())
                }
            }
        }
        AsiOp::MemVtoPBatch => {
            if op.args.is_null() || op.args_len != size_of::<MemVtoPBatchArgs>() {
                *out_value = 0;
                return SYSCALL_ERR_INVALID_ARGS;
            }
            let args = unsafe {
                // SAFETY: batch validation checked pointer non-null and exact size.
                *(op.args.cast::<MemVtoPBatchArgs>())
            };
            match crate::vm::mem_vtop_batch_bootstrap(args) {
                Ok(count) => {
                    push_event_if_ready("asi-mem-vtop-batch");
                    *out_value = count as u64;
                    SYSCALL_OK
                }
                Err(error) => {
                    *out_value = 0;
                    syscall_mem_error(error.as_mem_error())
                }
            }
        }
        AsiOp::CapRelease => {
            if op.args.is_null() || op.args_len != size_of::<CapHandle>() {
                *out_value = 0;
                return SYSCALL_ERR_INVALID_ARGS;
            }
            let handle = unsafe {
                // SAFETY: batch validation checked pointer non-null and exact size.
                *(op.args.cast::<CapHandle>())
            };
            match crate::capability::release_bootstrap_handle(handle) {
                Ok(()) => {
                    push_event_if_ready("asi-cap-release");
                    *out_value = 0;
                    SYSCALL_OK
                }
                Err(error) => {
                    *out_value = 0;
                    syscall_cap_error(error)
                }
            }
        }
        AsiOp::ProcYield => {
            push_event_if_ready("asi-proc-yield");
            *out_value = 0;
            SYSCALL_OK
        }
        _ => {
            push_event_if_ready("asi-op-unsupported");
            *out_value = 0;
            SYSCALL_ERR_UNSUPPORTED
        }
    }
}

fn push_event_if_ready(event: &'static str) {
    if crate::runtime_context::context_owner().is_some() {
        crate::runtime_context::push_event(event);
    }
}

fn write_out(out_value: *mut u64, value: u64) {
    if !out_value.is_null() {
        unsafe {
            // SAFETY: the caller controls the return-value storage. Null is
            // explicitly allowed so host-side tests can ignore it.
            *out_value = value;
        }
    }
}

fn write_mapped_region(out_region: *mut MappedRegion, region: MappedRegion) {
    if !out_region.is_null() {
        unsafe {
            // SAFETY: the caller owns the output slot, and null is explicitly allowed.
            *out_region = region;
        }
    }
}

fn write_physical_address(out_physical_address: *mut PhysicalAddress, physical_address: PhysicalAddress) {
    if !out_physical_address.is_null() {
        unsafe {
            // SAFETY: the caller owns the output slot, and null is explicitly allowed.
            *out_physical_address = physical_address;
        }
    }
}

fn syscall_cap_error(error: feox_asi::CapError) -> u64 {
    SYSCALL_CAP_ERROR_BASE + error as u64
}

fn syscall_mem_error(error: MemError) -> u64 {
    SYSCALL_MEM_ERROR_BASE + error as u64
}

fn syscall_storage_error(error: StorageError) -> u64 {
    SYSCALL_STORAGE_ERROR_BASE + error as u64
}

fn dispatch_storage_submit_read(args_ptr: *const u8, args_len: u64, out_value: *mut u64) -> u64 {
    if args_len != size_of::<StorageSubmitReadArgs>() as u64 || args_ptr.is_null() {
        write_out(out_value, 0);
        return SYSCALL_ERR_INVALID_ARGS;
    }
    let args = unsafe {
        // SAFETY: length is checked against `StorageSubmitReadArgs` and
        // the pointer is non-null.
        *(args_ptr.cast::<StorageSubmitReadArgs>())
    };
    // The `device` capability is not yet enforced; PCI enumeration
    // doesn't mint CapType::StorageDevice handles today. See
    // docs/STORAGE_ABI.md "Capability story (bootstrap vs. v1)".
    let _ = args.device;
    // Translate the buffer capability to a physical address. The
    // capability must be a PhysicalMemory (or future DmaPool) resource
    // with READ + WRITE permissions, and `buffer_offset + 4096` must
    // fit inside the capability's backing resource.
    let buffer_phys = match crate::capability::cap_to_phys_base(
        args.buffer,
        feox_asi::CapPermissions::READ | feox_asi::CapPermissions::WRITE,
    ) {
        Ok((base, size)) => {
            const READ_PAGE: u64 = 4096;
            if args.buffer_offset.saturating_add(READ_PAGE) > size {
                write_out(out_value, 0);
                return syscall_storage_error(feox_asi::StorageError::InvalidCapability);
            }
            base.0.saturating_add(args.buffer_offset)
        }
        Err(_) => {
            write_out(out_value, 0);
            return syscall_storage_error(feox_asi::StorageError::InvalidCapability);
        }
    };
    match crate::block::storage_submit_read(args.nsid, args.lba, args.block_count, buffer_phys) {
        Ok(token) => {
            push_event_if_ready("asi-storage-submit-read");
            write_out(out_value, token.0);
            SYSCALL_OK
        }
        Err(error) => {
            write_out(out_value, 0);
            syscall_storage_error(error)
        }
    }
}

fn dispatch_storage_poll(args_ptr: *const u8, args_len: u64, out_value: *mut u64) -> u64 {
    if args_len != size_of::<StoragePollArgs>() as u64 || args_ptr.is_null() {
        write_out(out_value, 0);
        return SYSCALL_ERR_INVALID_ARGS;
    }
    let args = unsafe {
        // SAFETY: length is checked against `StoragePollArgs` and the
        // pointer is non-null.
        *(args_ptr.cast::<StoragePollArgs>())
    };
    match crate::block::storage_poll(args.token) {
        Ok(None) => {
            write_out(out_value, StoragePollResult::NotReady as u64);
            SYSCALL_OK
        }
        Ok(Some(completion)) => {
            if !args.out_completion.is_null() {
                unsafe {
                    // SAFETY: caller-supplied writable pointer; we
                    // checked it's non-null.
                    *args.out_completion = completion;
                }
            }
            push_event_if_ready("asi-storage-poll-ready");
            write_out(out_value, StoragePollResult::Ready as u64);
            SYSCALL_OK
        }
        Err(error) => {
            write_out(out_value, 0);
            syscall_storage_error(error)
        }
    }
}


#[cfg(test)]
mod tests {
    use super::{
        SYSCALL_CAP_ERROR_BASE, SYSCALL_ERR_BATCH_FAILED, SYSCALL_ERR_INVALID_ARGS,
        SYSCALL_ERR_INVALID_OPCODE, SYSCALL_ERR_UNSUPPORTED, SYSCALL_OK, feox_syscall_dispatch,
        init, is_ready, star_value,
    };
    use core::mem::size_of;
    use feox_asi::{
        AsiOp, BatchOp, CapDelegateArgs, CapError, CapHandle, CapInfo, CapPermissions,
        CapRequest, CoreId, MapFlags, MemError, MemMapArgs, MemVtoPArgs, PageFlags,
        PhysicalAddress, ProcessId,
        SyscallResult,
    };

    #[test]
    fn syscall_init_marks_lane_ready() {
        let _guard = crate::capability::acquire_test_lock();
        init();
        assert!(is_ready());
    }

    #[test]
    fn star_value_packs_kernel_and_sysret_selectors() {
        let _guard = crate::capability::acquire_test_lock();
        assert_eq!(star_value(0x08, 0x13), 0x0013_0008_0000_0000);
    }

    #[test]
    fn invalid_opcode_is_rejected() {
        let _guard = crate::capability::acquire_test_lock();
        init();
        crate::capability::init_bootstrap_process(ProcessId(0));
        crate::capability::register_bootstrap_memory_resource(PhysicalAddress(0x1000), 0x1000)
            .expect("resource");
        let mut value = 99;
        let code = feox_syscall_dispatch(0xDEAD, core::ptr::null(), 0, &mut value);
        assert_eq!(code, SYSCALL_ERR_INVALID_OPCODE);
        assert_eq!(value, 0);
    }

    #[test]
    fn proc_yield_dispatches_successfully() {
        let _guard = crate::capability::acquire_test_lock();
        init();
        crate::capability::init_bootstrap_process(ProcessId(0));
        crate::capability::register_bootstrap_memory_resource(PhysicalAddress(0x1000), 0x1000)
            .expect("resource");
        let mut value = 99;
        let code = feox_syscall_dispatch(AsiOp::ProcYield as u64, core::ptr::null(), 0, &mut value);
        assert_eq!(code, SYSCALL_OK);
        assert_eq!(value, 0);
    }

    #[test]
    fn unsupported_leaf_opcode_returns_unsupported() {
        let _guard = crate::capability::acquire_test_lock();
        init();
        crate::capability::init_bootstrap_process(ProcessId(0));
        crate::capability::register_bootstrap_memory_resource(PhysicalAddress(0x1000), 0x1000)
            .expect("resource");
        let mut value = 99;
        let code =
            feox_syscall_dispatch(AsiOp::DevEnumerate as u64, core::ptr::null(), 0, &mut value);
        assert_eq!(code, SYSCALL_ERR_UNSUPPORTED);
        assert_eq!(value, 0);
    }

    #[test]
    fn batch_requires_whole_batch_op_records() {
        let _guard = crate::capability::acquire_test_lock();
        init();
        crate::capability::init_bootstrap_process(ProcessId(0));
        crate::capability::register_bootstrap_memory_resource(PhysicalAddress(0x1000), 0x1000)
            .expect("resource");
        let mut value = 99;
        let code = feox_syscall_dispatch(
            AsiOp::AsiBatch as u64,
            1usize as *const u8,
            7,
            &mut value,
        );
        assert_eq!(code, SYSCALL_ERR_INVALID_ARGS);
        assert_eq!(value, 0);
    }

    #[test]
    fn batch_reports_failed_operation_index() {
        let _guard = crate::capability::acquire_test_lock();
        init();
        crate::capability::init_bootstrap_process(ProcessId(0));
        crate::capability::register_bootstrap_memory_resource(PhysicalAddress(0x1000), 0x1000)
            .expect("resource");
        let mut ops = [
            BatchOp {
                opcode: AsiOp::ProcYield,
                args: core::ptr::null_mut(),
                args_len: 0,
                result: SyscallResult::default(),
            },
            BatchOp {
                opcode: AsiOp::DevEnumerate,
                args: core::ptr::null_mut(),
                args_len: 0,
                result: SyscallResult::default(),
            },
        ];
        let mut value = u64::MAX;
        let code = feox_syscall_dispatch(
            AsiOp::AsiBatch as u64,
            ops.as_mut_ptr().cast(),
            size_of::<[BatchOp; 2]>() as u64,
            &mut value,
        );
        assert_eq!(code, SYSCALL_ERR_BATCH_FAILED);
        assert_eq!(value, 1);
        assert_eq!(ops[0].result.code, SYSCALL_OK);
        assert_eq!(ops[1].result.code, SYSCALL_ERR_UNSUPPORTED);
    }

    #[test]
    fn cap_list_returns_total_capability_count() {
        let _guard = crate::capability::acquire_test_lock();
        init();
        crate::capability::init_bootstrap_process(ProcessId(0));
        let resource =
            crate::capability::register_bootstrap_memory_resource(PhysicalAddress(0x1000), 0x1000)
                .expect("resource");
        crate::capability::mint_bootstrap_root_capability(
            resource,
            feox_asi::CapPermissions::all(),
        )
        .expect("bootstrap cap should mint");

        let mut infos = [CapInfo::default(); 1];
        let mut total = 0;
        let code = feox_syscall_dispatch(
            AsiOp::CapList as u64,
            infos.as_mut_ptr().cast(),
            size_of::<[CapInfo; 1]>() as u64,
            &mut total,
        );
        assert_eq!(code, SYSCALL_OK);
        assert_eq!(total, 1);
        assert_eq!(infos[0].handle.id, 0);
    }

    #[test]
    fn cap_request_returns_new_physical_memory_handle() {
        let _guard = crate::capability::acquire_test_lock();
        init();
        crate::capability::init_bootstrap_process(ProcessId(0));
        crate::capability::register_bootstrap_memory_resource_with_kind(
            PhysicalAddress(0x8000),
            0x4000,
            true,
        )
        .expect("resource");
        let request = CapRequest::PhysicalPages {
            num_pages: 2,
            flags: PageFlags::CONTIGUOUS,
        };

        let mut packed = 0;
        let code = feox_syscall_dispatch(
            AsiOp::CapRequest as u64,
            (&raw const request).cast(),
            size_of::<CapRequest>() as u64,
            &mut packed,
        );
        assert_eq!(code, SYSCALL_OK);

        let handle = CapHandle {
            id: packed as u32,
            generation: (packed >> 32) as u32,
        };
        let view = crate::capability::verify_bootstrap_handle(handle, feox_asi::CapPermissions::READ)
            .expect("requested handle");
        assert_eq!(view.cap_type, feox_asi::CapType::PhysicalMemory);
    }

    #[test]
    fn cap_release_invalidates_generation() {
        let _guard = crate::capability::acquire_test_lock();
        init();
        crate::capability::init_bootstrap_process(ProcessId(0));
        let resource =
            crate::capability::register_bootstrap_memory_resource(PhysicalAddress(0x2000), 0x1000)
                .expect("resource");
        let handle = crate::capability::mint_bootstrap_root_capability(
            resource,
            feox_asi::CapPermissions::all(),
        )
        .expect("bootstrap cap should mint");

        let mut value = 123;
        let code = feox_syscall_dispatch(
            AsiOp::CapRelease as u64,
            (&raw const handle).cast(),
            size_of::<CapHandle>() as u64,
            &mut value,
        );
        assert_eq!(code, SYSCALL_OK);
        assert_eq!(value, 0);

        let code = feox_syscall_dispatch(
            AsiOp::CapRelease as u64,
            (&raw const handle).cast(),
            size_of::<CapHandle>() as u64,
            &mut value,
        );
        assert_eq!(code, SYSCALL_CAP_ERROR_BASE + CapError::GenerationMismatch as u64);
    }

    #[test]
    fn cap_delegate_creates_child_handle() {
        let _guard = crate::capability::acquire_test_lock();
        init();
        crate::capability::init_bootstrap_process(ProcessId(0));
        let resource =
            crate::capability::register_bootstrap_memory_resource(PhysicalAddress(0x3000), 0x1000)
                .expect("resource");
        let parent = crate::capability::mint_bootstrap_root_capability(
            resource,
            feox_asi::CapPermissions::all(),
        )
        .expect("parent");
        let args = CapDelegateArgs {
            handle: parent,
            target_pid: ProcessId(0),
            mask: feox_asi::CapPermissions::READ | feox_asi::CapPermissions::REVOKE,
        };
        let mut packed = 0;
        let code = feox_syscall_dispatch(
            AsiOp::CapDelegate as u64,
            (&raw const args).cast(),
            size_of::<CapDelegateArgs>() as u64,
            &mut packed,
        );
        assert_eq!(code, SYSCALL_OK);
        let child = CapHandle {
            id: packed as u32,
            generation: (packed >> 32) as u32,
        };
        let mut infos = [CapInfo::default(); 2];
        let (_, total) = crate::capability::list_bootstrap_capabilities(&mut infos);
        assert_eq!(total, 2);
        assert!(infos.iter().any(|info| info.handle == child && info.has_parent == 1));
    }

    #[test]
    fn mem_map_reports_invalid_flags() {
        let _guard = crate::capability::acquire_test_lock();
        init();
        crate::runtime_context::claim_bootstrap_context(CoreId(0));
        crate::capability::init_bootstrap_process(ProcessId(0));
        let args = MemMapArgs {
            handle: CapHandle::default(),
            offset_bytes: 0,
            length_bytes: 0x1000,
            flags: MapFlags::READ | MapFlags::UNCACHEABLE,
            out_region: core::ptr::null_mut(),
        };
        let mut out = 0;
        let code = feox_syscall_dispatch(
            AsiOp::MemMap as u64,
            (&raw const args).cast(),
            size_of::<MemMapArgs>() as u64,
            &mut out,
        );
        assert_eq!(code, super::SYSCALL_MEM_ERROR_BASE + MemError::InvalidFlags as u64);
    }

    #[test]
    fn mem_vtop_reports_unmapped_address() {
        let _guard = crate::capability::acquire_test_lock();
        init();
        crate::runtime_context::claim_bootstrap_context(CoreId(0));
        crate::capability::init_bootstrap_process(ProcessId(0));
        let resource_id =
            crate::capability::register_bootstrap_memory_resource(PhysicalAddress(0x2000), 0x2000)
                .expect("resource");
        let handle = crate::capability::mint_bootstrap_root_capability(
            resource_id,
            CapPermissions::READ,
        );
        let handle = handle.expect("handle");
        let mut physical = PhysicalAddress(0);
        let args = MemVtoPArgs {
            handle,
            virtual_address: crate::memory::BOOTSTRAP_VM_WINDOW_BASE,
            out_physical_address: &mut physical,
        };
        let mut out = 0;
        let code = feox_syscall_dispatch(
            AsiOp::MemVtoP as u64,
            (&raw const args).cast(),
            size_of::<MemVtoPArgs>() as u64,
            &mut out,
        );
        assert_eq!(
            code,
            super::SYSCALL_MEM_ERROR_BASE + MemError::AddressNotMapped as u64
        );
        assert_eq!(out, 0);
        assert_eq!(physical, PhysicalAddress(0));
    }
}
