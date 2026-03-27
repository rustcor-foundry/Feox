#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]

//! Shared types for the Feox Aether System Interface boundary.

use core::sync::atomic::{AtomicU64, Ordering};

/// Identifier for a physical or logical CPU core.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Hash)]
#[repr(transparent)]
pub struct CoreId(pub u16);

/// ASI duration value with nanosecond precision.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Hash)]
#[repr(transparent)]
pub struct Duration {
    nanos: u64,
}

impl Duration {
    /// Creates a duration from nanoseconds.
    #[must_use]
    pub const fn from_nanos(nanos: u64) -> Self {
        Self { nanos }
    }

    /// Returns the duration as nanoseconds.
    #[must_use]
    pub const fn as_nanos(self) -> u64 {
        self.nanos
    }
}

/// Shared interrupt/event counter observed by user space and the kernel.
#[derive(Debug)]
#[repr(C)]
pub struct EventSlot {
    counter: AtomicU64,
}

impl EventSlot {
    /// Creates a zeroed event slot.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            counter: AtomicU64::new(0),
        }
    }

    /// Loads the current event counter.
    #[must_use]
    pub fn load(&self) -> u64 {
        self.counter.load(Ordering::Acquire)
    }

    /// Signals the slot and returns the post-increment value.
    pub fn signal(&self) -> u64 {
        self.counter.fetch_add(1, Ordering::AcqRel) + 1
    }
}

impl Default for EventSlot {
    fn default() -> Self {
        Self::new()
    }
}

/// Result of parking a thread on one or more event slots.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub enum ParkResult {
    /// A watched slot fired.
    Woken {
        /// Index into the watched slot slice.
        slot_index: usize,
    },
    /// The supplied timeout elapsed.
    TimedOut,
}

/// Opaque capability handle shared across kernel and user space.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
#[repr(C)]
pub struct CapHandle {
    /// Capability table slot identifier.
    pub id: u32,
    /// Monotonic generation used for stale-handle detection.
    pub generation: u32,
}
