#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![allow(clippy::similar_names, clippy::used_underscore_items)]

//! Top-level facade for the Feox xokernel prototype.

pub mod arch;
pub mod boot;
pub mod capability;
pub mod console;
pub mod memory;
pub mod paging;
pub mod runtime_context;
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
