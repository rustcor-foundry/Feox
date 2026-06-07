#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![allow(clippy::similar_names, clippy::used_underscore_items)]

//! Top-level facade for the Feox xokernel prototype.

// The host test harness needs std; the kernel itself stays no_std.
#[cfg(test)]
extern crate std;

// riscv64 has a global allocator (arch::riscv64::heap), so `alloc` is available
// there. The x86_64 path stays alloc-free for now.
#[cfg(target_arch = "riscv64")]
extern crate alloc;

pub mod arch;
pub mod console;

// The remaining subsystems are still x86_64-shaped (ACPI/RSDP intake, LAPIC,
// the GDT/IDT-aliased CR3 handoff in `boot`, the paging/memory/SMP stack). The
// riscv64 milestone-1 build takes a minimal `arch::riscv64::riscv_main` path
// that brings up the SBI console and parks, so these are gated to x86_64 until
// their riscv64 backends land (traps -> sv39 -> memory -> runtime -> SMP).
#[cfg(target_arch = "x86_64")]
pub mod acpi;
#[cfg(target_arch = "x86_64")]
pub mod block;
#[cfg(target_arch = "x86_64")]
pub mod boot;
#[cfg(target_arch = "x86_64")]
pub mod capability;
#[cfg(target_arch = "x86_64")]
pub mod lapic;
#[cfg(target_arch = "x86_64")]
pub mod memory;
#[cfg(target_arch = "x86_64")]
pub mod mmio;
#[cfg(target_arch = "x86_64")]
pub mod paging;
#[cfg(target_arch = "x86_64")]
pub mod pci;
#[cfg(target_arch = "x86_64")]
pub mod per_core;
#[cfg(target_arch = "x86_64")]
pub mod runtime_context;
#[cfg(target_arch = "x86_64")]
pub mod smp;
#[cfg(target_arch = "x86_64")]
pub mod vm;

pub use feox_asi as asi;
#[cfg(feature = "runtime")]
pub use feox_async as runtime;
pub use feox_boot as bootabi;
#[cfg(feature = "storage")]
pub use feox_nvme as nvme;

use core::fmt;
use feox_asi::CoreId;

/// Project name used by the prototype kernel.
pub const PROJECT_NAME: &str = "Feox";

/// Short descriptor of the implementation style.
pub const PROJECT_STYLE: &str = "no_std Rust exokernel";

/// Static kernel configuration used during bootstrap.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KernelConfig {
    /// Bootstrap processor that enters the kernel first.
    pub bootstrap_core: CoreId,
    /// Maximum number of scheduler-visible cores.
    pub max_cores: u16,
    /// Default `NVMe` queue depth for the early prototype.
    pub nvme_queue_depth: u16,
}

impl Default for KernelConfig {
    fn default() -> Self {
        Self {
            bootstrap_core: CoreId(0),
            max_cores: 1,
            nvme_queue_depth: 64,
        }
    }
}

#[doc(hidden)]
pub fn _print(args: fmt::Arguments<'_>) {
    console::write_fmt(args);
}

/// Prints formatted text to the early kernel console.
#[macro_export]
macro_rules! kprint {
    ($($arg:tt)*) => {{
        $crate::_print(core::format_args!($($arg)*));
    }};
}

/// Prints formatted text followed by a newline to the early kernel console.
#[macro_export]
macro_rules! kprintln {
    () => {{
        $crate::kprint!("\n");
    }};
    ($fmt:expr $(, $($arg:tt)*)?) => {{
        $crate::kprint!(core::concat!($fmt, "\n") $(, $($arg)*)?);
    }};
}
