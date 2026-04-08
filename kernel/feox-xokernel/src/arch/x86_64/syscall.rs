//! Minimal `SYSCALL` / `SYSRET` transport for the x86_64 ASI lane.

#[cfg(target_os = "none")]
use core::arch::global_asm;
use core::mem::size_of;
use core::ptr::{addr_of, slice_from_raw_parts_mut};
use core::sync::atomic::{AtomicBool, Ordering};

use feox_asi::{AsiOp, BatchOp, CapDelegateArgs, CapHandle, CapInfo};

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

#[unsafe(no_mangle)]
extern "C" fn feox_syscall_dispatch(
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

    match opcode {
        AsiOp::CapList => dispatch_cap_list(args_ptr.cast_mut(), args_len, out_value),
        AsiOp::CapDelegate => dispatch_cap_delegate(args_ptr, args_len, out_value),
        AsiOp::CapRelease => dispatch_cap_release(args_ptr, args_len, out_value),
        AsiOp::ProcYield => {
            push_event_if_ready("asi-proc-yield");
            write_out(out_value, 0);
            SYSCALL_OK
        }
        AsiOp::AsiBatch => dispatch_batch(args_ptr, args_len, out_value),
        _ => {
            push_event_if_ready("asi-op-unsupported");
            write_out(out_value, 0);
            SYSCALL_ERR_UNSUPPORTED
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

fn syscall_cap_error(error: feox_asi::CapError) -> u64 {
    SYSCALL_CAP_ERROR_BASE + error as u64
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
        AsiOp, BatchOp, CapDelegateArgs, CapError, CapHandle, CapInfo, PhysicalAddress,
        ProcessId, SyscallResult,
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
}
