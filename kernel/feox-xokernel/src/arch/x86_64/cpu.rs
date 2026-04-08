//! Minimal CPU helpers for early boot.

use core::arch::asm;

/// IA32_EFER MSR address.
const IA32_EFER: u32 = 0xC000_0080;
/// Execute-disable enable bit in IA32_EFER (bit 11).
const EFER_NXE: u64 = 1 << 11;

/// Reads a 64-bit MSR.
///
/// # Safety
///
/// The caller must ensure `msr` is a valid MSR address for the current
/// processor and that reading it does not produce side effects that violate
/// kernel invariants.
pub(crate) unsafe fn rdmsr(msr: u32) -> u64 {
    let lo: u32;
    let hi: u32;
    unsafe {
        asm!(
            "rdmsr",
            in("ecx") msr,
            out("eax") lo,
            out("edx") hi,
            options(nomem, nostack, preserves_flags),
        );
    }
    ((hi as u64) << 32) | (lo as u64)
}

/// Writes a 64-bit MSR.
///
/// # Safety
///
/// The caller must ensure `msr` is a valid writable MSR address and that the
/// supplied value is within the legal range for that register.
pub(crate) unsafe fn wrmsr(msr: u32, value: u64) {
    let lo = value as u32;
    let hi = (value >> 32) as u32;
    unsafe {
        asm!(
            "wrmsr",
            in("ecx") msr,
            in("eax") lo,
            in("edx") hi,
            options(nomem, nostack, preserves_flags),
        );
    }
}

/// Enables the Execute-Disable (NX) bit in IA32_EFER.
///
/// After this call, page-table entries with bit 63 set will prevent
/// instruction fetches from those pages. Must be called before any
/// `FLAG_NO_EXECUTE` mappings are installed.
pub fn enable_nxe() {
    unsafe {
        // Safety: IA32_EFER is a valid MSR on all x86_64 processors. Setting
        // NXE only extends the page-table encoding; it does not change the
        // meaning of existing entries that have bit 63 clear.
        let efer = rdmsr(IA32_EFER);
        wrmsr(IA32_EFER, efer | EFER_NXE);
    }
}

/// Reads the current CR4 register value.
fn read_cr4() -> u64 {
    let value: u64;
    unsafe {
        // Safety: reading CR4 is a side-effect-free architectural register read.
        asm!("mov {}, cr4", out(reg) value, options(nomem, nostack, preserves_flags));
    }
    value
}

/// Writes a value to the CR4 register.
///
/// # Safety
///
/// The caller must ensure `value` is a legal CR4 state for the current
/// processor. Enabling a feature that the CPU does not support will cause a
/// general-protection fault.
unsafe fn write_cr4(value: u64) {
    unsafe {
        // Safety: upheld by the caller.
        asm!("mov cr4, {}", in(reg) value, options(nomem, nostack, preserves_flags));
    }
}

/// Enables supervisor-mode security bits in CR4 if the CPU supports them.
///
/// Sets the following bits when reported as available by CPUID leaf 7:
///
/// - **SMEP** (CR4.20): prevents the kernel from executing user-mode pages.
/// - **SMAP** (CR4.21): prevents the kernel from reading user-mode pages
///   without an explicit `stac`/`clac` bracket.
/// - **UMIP** (CR4.11): prevents user-mode from reading descriptor-table
///   registers (`SGDT`, `SIDT`, `SMSW`, `SLDT`, `STR`), which would
///   otherwise leak kernel addresses.
///
/// Must be called after the GDT and IDT are loaded. SMEP and SMAP are not
/// relevant until user-mode mappings are live, but establishing them during
/// `early_init` closes the window before any page-table work.
pub fn enable_cr4_security_bits() {
    // CPUID leaf 7, sub-leaf 0 reports structured extended feature flags.
    // rbx is reserved by LLVM and cannot be named as an asm operand; save and
    // restore it around the CPUID instruction, moving EBX output to a scratch
    // register before restoring.
    let ebx: u32;
    let ecx: u32;
    unsafe {
        // Safety: CPUID is always available on x86_64. We preserve rbx by
        // saving it on the stack and restoring after copying EBX's output.
        core::arch::asm!(
            "push rbx",
            "cpuid",
            "mov {ebx_out:e}, ebx",
            "pop rbx",
            inout("eax") 7u32 => _,
            inout("ecx") 0u32 => ecx,
            ebx_out = out(reg) ebx,
            lateout("edx") _,
            options(nomem, preserves_flags),
        );
    }

    const CR4_UMIP: u64 = 1 << 11;
    const CR4_SMEP: u64 = 1 << 20;
    const CR4_SMAP: u64 = 1 << 21;

    let smep_supported = (ebx >> 7) & 1 != 0;   // EBX bit 7
    let smap_supported = (ebx >> 20) & 1 != 0;  // EBX bit 20
    let umip_supported = (ecx >> 2) & 1 != 0;   // ECX bit 2

    let mut cr4 = read_cr4();
    if smep_supported {
        cr4 |= CR4_SMEP;
    }
    if smap_supported {
        cr4 |= CR4_SMAP;
    }
    if umip_supported {
        cr4 |= CR4_UMIP;
    }
    unsafe { write_cr4(cr4) };
}

/// Disables maskable interrupts on the current core.
pub fn disable_interrupts() {
    // SAFETY: this emits the architectural instruction for clearing IF.
    unsafe {
        asm!("cli", options(nomem, nostack, preserves_flags));
    }
}

/// Halts the current core until the next interrupt.
pub fn halt() {
    // SAFETY: this emits the architectural halt instruction.
    unsafe {
        asm!("hlt", options(nomem, nostack));
    }
}

/// Enters an infinite halt loop after disabling maskable interrupts.
///
/// `cli` clears IF, preventing maskable interrupts from waking the core.
/// NMIs and machine-check exceptions are not masked by `cli` and will still
/// be delivered with a valid RSP. This is intentional: the halt loop is used
/// for panic and fatal-error paths where the core must not be rescheduled
/// but must remain reachable by NMI-driven debuggers or watchdog signals.
///
/// Note: because maskable IRQs are disabled, the panic path that lands here
/// will not accept any maskable soft-reboot or QEMU exit signal. This is a
/// deliberate choice for the bootstrap phase.
pub fn hlt_loop() -> ! {
    disable_interrupts();

    loop {
        halt();
    }
}

/// Reads the current CR2 register value.
#[must_use]
pub fn read_cr2() -> u64 {
    let value: u64;

    // SAFETY: reading CR2 is a side-effect-free architectural register read.
    unsafe {
        asm!("mov {}, cr2", out(reg) value, options(nomem, nostack, preserves_flags));
    }

    value
}

/// Reads the current CR3 register value.
#[must_use]
pub fn read_cr3() -> u64 {
    let value: u64;

    // SAFETY: reading CR3 is a side-effect-free architectural register read.
    unsafe {
        asm!("mov {}, cr3", out(reg) value, options(nomem, nostack, preserves_flags));
    }

    value
}

/// Reads the current RSP register value.
#[must_use]
pub fn read_rsp() -> u64 {
    let value: u64;

    // SAFETY: reading RSP is a side-effect-free register move.
    unsafe {
        asm!("mov {}, rsp", out(reg) value, options(nomem, nostack, preserves_flags));
    }

    value
}

/// Invalidates the TLB entry for a single virtual address on the current core.
///
/// Must be called after every PTE write that affects an address that may
/// already be cached in the TLB. A CR3 write implicitly flushes all entries,
/// so this is not needed immediately after a full root switch.
pub fn invalidate_page(virt_addr: u64) {
    unsafe {
        // Safety: invlpg is a ring-0 privileged instruction. The caller is
        // responsible for only calling this from supervisor mode. The address
        // operand is the virtual address whose TLB entry should be flushed;
        // it does not need to be mapped.
        asm!("invlpg [{addr}]", addr = in(reg) virt_addr, options(nostack, preserves_flags));
    }
}

/// Raises a software breakpoint exception on the current core.
pub fn trigger_breakpoint() {
    // SAFETY: this intentionally raises vector 3 so the bootstrap exception
    // path can be validated under controlled conditions.
    unsafe {
        asm!("int3", options(nomem, nostack));
    }
}

/// Switches to a new page-table root, installs a new stack pointer, and jumps
/// to the supplied entrypoint.
///
/// # Safety
///
/// The caller must ensure the current execution path remains valid long enough
/// to execute the CR3 write and jump sequence, and that the target entry and
/// stack are mapped in the new address space.
#[cfg(debug_assertions)]
pub unsafe fn switch_page_table_root_and_jump(root: u64, stack: u64, entry: u64) -> ! {
    // SAFETY: upheld by the caller; this is the minimal architectural
    // sequence required to move onto a prepared address space.
    unsafe {
        // SAFETY: upheld by the caller; this is the minimal architectural
        // sequence required to move onto a prepared address space.
        // Debug builds emit debugcon markers before and after the CR3 write
        // and before the stack switch so the transition boundary is visible
        // in the QEMU trace.
        asm!(
            "mov dx, 0x402",
            "mov al, 0x3c",
            "out dx, al",
            "mov al, 0x30",
            "out dx, al",
            "mov al, 0x3e",
            "out dx, al",
            "mov cr3, rcx",
            "mov dx, 0x402",
            "mov al, 0x3c",
            "out dx, al",
            "mov al, 0x31",
            "out dx, al",
            "mov al, 0x3e",
            "out dx, al",
            "mov rsp, r8",
            "push 0",
            "mov dx, 0x402",
            "mov al, 0x3c",
            "out dx, al",
            "mov al, 0x32",
            "out dx, al",
            "mov al, 0x3e",
            "out dx, al",
            "xor rbp, rbp",
            "jmp r9",
            in("rcx") root,
            in("r8") stack,
            in("r9") entry,
            options(noreturn)
        );
    }
}

/// Release build: minimal CR3 switch, stack install, and jump with no
/// diagnostic I/O.
#[cfg(not(debug_assertions))]
pub unsafe fn switch_page_table_root_and_jump(root: u64, stack: u64, entry: u64) -> ! {
    unsafe {
        // SAFETY: upheld by the caller.
        asm!(
            "mov cr3, rcx",
            "mov rsp, r8",
            "push 0",
            "xor rbp, rbp",
            "jmp r9",
            in("rcx") root,
            in("r8") stack,
            in("r9") entry,
            options(noreturn)
        );
    }
}

/// Switches to a new stack pointer and jumps to the supplied entrypoint.
///
/// # Safety
///
/// The caller must ensure the target stack and entry are valid in the current
/// address space.
pub unsafe fn switch_stack_and_jump(stack: u64, entry: u64) -> ! {
    // SAFETY: upheld by the caller; this is the minimal architectural
    // sequence required to move onto a prepared stack and entrypoint.
    unsafe {
        asm!(
            "mov rsp, r8",
            "push 0",
            "xor rbp, rbp",
            "jmp r9",
            in("r8") stack,
            in("r9") entry,
            options(noreturn)
        );
    }
}
