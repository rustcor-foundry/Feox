//! Early exception stubs and fatal exception dispatcher.

use core::arch::global_asm;

use super::cpu;

/// General-purpose register snapshot plus the CPU-pushed exception frame.
#[repr(C)]
pub struct ExceptionContext {
    /// General-purpose registers saved by the common trampoline.
    pub r15: u64,
    /// General-purpose registers saved by the common trampoline.
    pub r14: u64,
    /// General-purpose registers saved by the common trampoline.
    pub r13: u64,
    /// General-purpose registers saved by the common trampoline.
    pub r12: u64,
    /// General-purpose registers saved by the common trampoline.
    pub r11: u64,
    /// General-purpose registers saved by the common trampoline.
    pub r10: u64,
    /// General-purpose registers saved by the common trampoline.
    pub r9: u64,
    /// General-purpose registers saved by the common trampoline.
    pub r8: u64,
    /// General-purpose registers saved by the common trampoline.
    pub rbp: u64,
    /// General-purpose registers saved by the common trampoline.
    pub rdi: u64,
    /// General-purpose registers saved by the common trampoline.
    pub rsi: u64,
    /// General-purpose registers saved by the common trampoline.
    pub rdx: u64,
    /// General-purpose registers saved by the common trampoline.
    pub rcx: u64,
    /// General-purpose registers saved by the common trampoline.
    pub rbx: u64,
    /// General-purpose registers saved by the common trampoline.
    pub rax: u64,
    /// Interrupt/exception vector number.
    pub vector: u64,
    /// Hardware-supplied or synthesized error code.
    pub error_code: u64,
    /// Instruction pointer captured by the CPU.
    pub rip: u64,
    /// Code segment captured by the CPU.
    pub cs: u64,
    /// RFLAGS captured by the CPU.
    pub rflags: u64,
}

unsafe extern "C" {
    fn feox_exception_0();
    fn feox_exception_1();
    fn feox_exception_2();
    fn feox_exception_3();
    fn feox_exception_4();
    fn feox_exception_5();
    fn feox_exception_6();
    fn feox_exception_7();
    fn feox_exception_8();
    fn feox_exception_9();
    fn feox_exception_10();
    fn feox_exception_11();
    fn feox_exception_12();
    fn feox_exception_13();
    fn feox_exception_14();
    fn feox_exception_15();
    fn feox_exception_16();
    fn feox_exception_17();
    fn feox_exception_18();
    fn feox_exception_19();
    fn feox_exception_20();
    fn feox_exception_21();
    fn feox_exception_22();
    fn feox_exception_23();
    fn feox_exception_24();
    fn feox_exception_25();
    fn feox_exception_26();
    fn feox_exception_27();
    fn feox_exception_28();
    fn feox_exception_29();
    fn feox_exception_30();
    fn feox_exception_31();
}

/// Fatal exception entrypoints used for bootstrap vectors 0-31.
pub const HANDLERS: [unsafe extern "C" fn(); 32] = [
    feox_exception_0,
    feox_exception_1,
    feox_exception_2,
    feox_exception_3,
    feox_exception_4,
    feox_exception_5,
    feox_exception_6,
    feox_exception_7,
    feox_exception_8,
    feox_exception_9,
    feox_exception_10,
    feox_exception_11,
    feox_exception_12,
    feox_exception_13,
    feox_exception_14,
    feox_exception_15,
    feox_exception_16,
    feox_exception_17,
    feox_exception_18,
    feox_exception_19,
    feox_exception_20,
    feox_exception_21,
    feox_exception_22,
    feox_exception_23,
    feox_exception_24,
    feox_exception_25,
    feox_exception_26,
    feox_exception_27,
    feox_exception_28,
    feox_exception_29,
    feox_exception_30,
    feox_exception_31,
];

global_asm!(
    ".macro FEOX_PUSH_REGS",
    "push r15",
    "push r14",
    "push r13",
    "push r12",
    "push r11",
    "push r10",
    "push r9",
    "push r8",
    "push rbp",
    "push rdi",
    "push rsi",
    "push rdx",
    "push rcx",
    "push rbx",
    "push rax",
    ".endm",
    ".macro FEOX_EXCEPTION_NO_ERROR vector",
    ".global feox_exception_\\vector",
    "feox_exception_\\vector:",
    "push 0",
    "push \\vector",
    "jmp feox_exception_common",
    ".endm",
    ".macro FEOX_EXCEPTION_WITH_ERROR vector",
    ".global feox_exception_\\vector",
    "feox_exception_\\vector:",
    "push \\vector",
    "jmp feox_exception_common",
    ".endm",
    ".global feox_exception_common",
    "feox_exception_common:",
    "cld",
    "FEOX_PUSH_REGS",
    "mov rdi, rsp",
    "call {dispatch}",
    "ud2",
    "FEOX_EXCEPTION_NO_ERROR 0",
    "FEOX_EXCEPTION_NO_ERROR 1",
    "FEOX_EXCEPTION_NO_ERROR 2",
    "FEOX_EXCEPTION_NO_ERROR 3",
    "FEOX_EXCEPTION_NO_ERROR 4",
    "FEOX_EXCEPTION_NO_ERROR 5",
    "FEOX_EXCEPTION_NO_ERROR 6",
    "FEOX_EXCEPTION_NO_ERROR 7",
    "FEOX_EXCEPTION_WITH_ERROR 8",
    "FEOX_EXCEPTION_NO_ERROR 9",
    "FEOX_EXCEPTION_WITH_ERROR 10",
    "FEOX_EXCEPTION_WITH_ERROR 11",
    "FEOX_EXCEPTION_WITH_ERROR 12",
    "FEOX_EXCEPTION_WITH_ERROR 13",
    "FEOX_EXCEPTION_WITH_ERROR 14",
    "FEOX_EXCEPTION_NO_ERROR 15",
    "FEOX_EXCEPTION_NO_ERROR 16",
    "FEOX_EXCEPTION_WITH_ERROR 17",
    "FEOX_EXCEPTION_NO_ERROR 18",
    "FEOX_EXCEPTION_NO_ERROR 19",
    "FEOX_EXCEPTION_NO_ERROR 20",
    "FEOX_EXCEPTION_WITH_ERROR 21",
    "FEOX_EXCEPTION_NO_ERROR 22",
    "FEOX_EXCEPTION_NO_ERROR 23",
    "FEOX_EXCEPTION_NO_ERROR 24",
    "FEOX_EXCEPTION_NO_ERROR 25",
    "FEOX_EXCEPTION_NO_ERROR 26",
    "FEOX_EXCEPTION_NO_ERROR 27",
    "FEOX_EXCEPTION_NO_ERROR 28",
    "FEOX_EXCEPTION_WITH_ERROR 29",
    "FEOX_EXCEPTION_WITH_ERROR 30",
    "FEOX_EXCEPTION_NO_ERROR 31",
    dispatch = sym dispatch_exception,
);

extern "C" fn dispatch_exception(context: &ExceptionContext) -> ! {
    cpu::disable_interrupts();
    crate::console::init();

    crate::kprintln!();
    crate::kprintln!(
        "[feox exception] vector={} {}",
        context.vector,
        vector_name(context.vector as u8)
    );
    crate::kprintln!(
        "error={:#018x} rip={:#018x} cs={:#06x} rflags={:#018x}",
        context.error_code,
        context.rip,
        context.cs,
        context.rflags
    );
    crate::kprintln!(
        "rax={:#018x} rbx={:#018x} rcx={:#018x} rdx={:#018x}",
        context.rax,
        context.rbx,
        context.rcx,
        context.rdx
    );
    crate::kprintln!(
        "rsi={:#018x} rdi={:#018x} rbp={:#018x}",
        context.rsi,
        context.rdi,
        context.rbp
    );

    if context.vector == 14 {
        crate::kprintln!("cr2={:#018x}", cpu::read_cr2());
    }

    cpu::hlt_loop()
}

fn vector_name(vector: u8) -> &'static str {
    match vector {
        0 => "divide error",
        1 => "debug",
        2 => "non-maskable interrupt",
        3 => "breakpoint",
        4 => "overflow",
        5 => "bound range exceeded",
        6 => "invalid opcode",
        7 => "device not available",
        8 => "double fault",
        9 => "coprocessor segment overrun",
        10 => "invalid TSS",
        11 => "segment not present",
        12 => "stack-segment fault",
        13 => "general protection fault",
        14 => "page fault",
        15 => "reserved",
        16 => "x87 floating-point exception",
        17 => "alignment check",
        18 => "machine check",
        19 => "SIMD floating-point exception",
        20 => "virtualization exception",
        21 => "control protection exception",
        22 => "reserved",
        23 => "reserved",
        24 => "reserved",
        25 => "reserved",
        26 => "reserved",
        27 => "reserved",
        28 => "hypervisor injection exception",
        29 => "VMM communication exception",
        30 => "security exception",
        31 => "reserved",
        _ => "unknown",
    }
}
