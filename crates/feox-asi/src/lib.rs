#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]

//! Shared types for the Feox Aether System Interface boundary.

use core::sync::atomic::{AtomicU64, Ordering};

/// Physical address on the machine.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Hash)]
#[repr(transparent)]
pub struct PhysicalAddress(pub u64);

/// PCI Bus:Device:Function address.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
#[repr(C)]
pub struct PciAddress {
    /// PCI segment group.
    pub segment: u16,
    /// PCI bus number.
    pub bus: u8,
    /// PCI device number.
    pub device: u8,
    /// PCI function number.
    pub function: u8,
}

/// Process identifier.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Hash)]
#[repr(transparent)]
pub struct ProcessId(pub u64);

/// Thread identifier.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Hash)]
#[repr(transparent)]
pub struct ThreadId(pub u64);

/// Identifier for a physical or logical CPU core.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Hash)]
#[repr(transparent)]
pub struct CoreId(pub u32);

/// A set of cores, represented as a fixed 256-bit mask.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(C)]
pub struct CoreSet {
    /// Four 64-bit words covering core IDs 0-255.
    pub bits: [u64; 4],
}

/// Complete ASI opcode table for the current transport boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u64)]
pub enum AsiOp {
    /// Request a new capability from the kernel.
    CapRequest = 0x0000,
    /// Release an existing capability.
    CapRelease = 0x0001,
    /// Delegate a capability to another protection domain.
    CapDelegate = 0x0002,
    /// List capabilities visible to the caller.
    CapList = 0x0003,
    /// Map a capability-backed region into the caller address space.
    MemMap = 0x0100,
    /// Unmap a previously mapped region.
    MemUnmap = 0x0101,
    /// Translate one virtual address to a physical address.
    MemVtoP = 0x0102,
    /// Translate many virtual addresses in one call.
    MemVtoPBatch = 0x0103,
    /// Attach an interrupt source to an event slot.
    IrqAttach = 0x0200,
    /// Detach an interrupt source.
    IrqDetach = 0x0201,
    /// Spawn a process.
    ProcSpawn = 0x0300,
    /// Exit the current process.
    ProcExit = 0x0301,
    /// Yield the current process.
    ProcYield = 0x0302,
    /// Update process affinity.
    ProcSetAffinity = 0x0303,
    /// Spawn a thread.
    ThreadSpawn = 0x0310,
    /// Exit the current thread.
    ThreadExit = 0x0311,
    /// Park the current thread on one or more event slots.
    ThreadPark = 0x0312,
    /// Enumerate visible devices.
    DevEnumerate = 0x0400,
    /// Submit many ASI operations in one syscall transition.
    AsiBatch = 0xFF00,
}

impl AsiOp {
    /// Converts a raw opcode into a typed ASI opcode.
    #[must_use]
    pub const fn from_raw(raw: u64) -> Option<Self> {
        match raw {
            0x0000 => Some(Self::CapRequest),
            0x0001 => Some(Self::CapRelease),
            0x0002 => Some(Self::CapDelegate),
            0x0003 => Some(Self::CapList),
            0x0100 => Some(Self::MemMap),
            0x0101 => Some(Self::MemUnmap),
            0x0102 => Some(Self::MemVtoP),
            0x0103 => Some(Self::MemVtoPBatch),
            0x0200 => Some(Self::IrqAttach),
            0x0201 => Some(Self::IrqDetach),
            0x0300 => Some(Self::ProcSpawn),
            0x0301 => Some(Self::ProcExit),
            0x0302 => Some(Self::ProcYield),
            0x0303 => Some(Self::ProcSetAffinity),
            0x0310 => Some(Self::ThreadSpawn),
            0x0311 => Some(Self::ThreadExit),
            0x0312 => Some(Self::ThreadPark),
            0x0400 => Some(Self::DevEnumerate),
            0xFF00 => Some(Self::AsiBatch),
            _ => None,
        }
    }
}

/// Result register pair returned by one ASI syscall.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(C)]
pub struct SyscallResult {
    /// Result code. Zero is success.
    pub code: u64,
    /// Call-specific value written by the kernel.
    pub value: u64,
}

impl SyscallResult {
    /// Successful result with the supplied return value.
    #[must_use]
    pub const fn success(value: u64) -> Self {
        Self { code: 0, value }
    }

    /// Failed result with the supplied error code.
    #[must_use]
    pub const fn failure(code: u64) -> Self {
        Self { code, value: 0 }
    }
}

/// One operation in an ASI batch submission.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct BatchOp {
    /// The operation to execute.
    pub opcode: AsiOp,
    /// Pointer to the call-specific argument struct.
    pub args: *mut u8,
    /// Size of the argument struct in bytes.
    pub args_len: usize,
    /// Per-operation result written by the kernel.
    pub result: SyscallResult,
}

/// Failure returned by the ASI batch wrapper.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub enum BatchError {
    /// One operation in the batch failed.
    OperationFailed {
        /// Index of the failed operation.
        failed_at: usize,
    },
    /// The batch wrapper itself was malformed.
    InvalidBatch,
}

/// Fixed-size inline string for bootstrap IPC names and similar identifiers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct AsiString {
    /// Number of bytes currently used in `bytes`.
    pub len: u8,
    /// Inline storage.
    pub bytes: [u8; 63],
}

impl AsiString {
    /// Empty inline string.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            len: 0,
            bytes: [0; 63],
        }
    }
}

impl Default for AsiString {
    fn default() -> Self {
        Self::empty()
    }
}

/// Physical-page allocation flags used by `cap_request`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(transparent)]
pub struct PageFlags(pub u32);

impl PageFlags {
    /// Request 2 MiB huge pages.
    pub const HUGE_2M: Self = Self(1 << 0);
    /// Request 1 GiB huge pages.
    pub const HUGE_1G: Self = Self(1 << 1);
    /// Request physically contiguous pages.
    pub const CONTIGUOUS: Self = Self(1 << 2);

    /// Empty page flags.
    #[must_use]
    pub const fn empty() -> Self {
        Self(0)
    }

    /// Returns whether `self` contains all bits in `other`.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }
}

/// Typed request payload for `cap_request`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub enum CapRequest {
    /// Access to a PCI BAR.
    DeviceBar {
        /// PCI location.
        pci_addr: PciAddress,
        /// BAR index.
        bar_index: u8,
    },
    /// A contiguous DMA-safe physical allocation.
    DmaPool {
        /// Requested size in bytes.
        size_bytes: usize,
        /// Required alignment.
        alignment: usize,
        /// Device allowed to access the pool.
        device: PciAddress,
    },
    /// Ownership of one MSI-X vector.
    MsixVector {
        /// PCI location.
        pci_addr: PciAddress,
        /// Vector number.
        vector: u16,
    },
    /// A range of physical pages.
    PhysicalPages {
        /// Number of 4 KiB pages requested.
        num_pages: usize,
        /// Allocation flags.
        flags: PageFlags,
    },
    /// An on-demand IPC endpoint.
    IpcEndpoint {
        /// Endpoint name.
        name: AsiString,
    },
}

/// Arguments for `cap_delegate`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(C)]
pub struct CapDelegateArgs {
    /// Source capability to delegate from.
    pub handle: CapHandle,
    /// Target process receiving the delegated capability.
    pub target_pid: ProcessId,
    /// Permission mask for the child capability.
    pub mask: CapPermissions,
}

/// Capability resource type granted by the kernel.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(u8)]
pub enum CapType {
    /// A range of physical memory.
    #[default]
    PhysicalMemory = 0,
    /// A PCI BAR mapping.
    DeviceBar = 1,
    /// A DMA-safe memory pool.
    DmaPool = 2,
    /// An MSI-X vector.
    MsixVector = 3,
    /// An IPC endpoint.
    IpcEndpoint = 4,
}

/// Capability permission bitset.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(transparent)]
pub struct CapPermissions(pub u32);

impl CapPermissions {
    /// Read authority.
    pub const READ: Self = Self(1 << 0);
    /// Write authority.
    pub const WRITE: Self = Self(1 << 1);
    /// Delegation authority.
    pub const DELEGATE: Self = Self(1 << 2);
    /// Revocation authority.
    pub const REVOKE: Self = Self(1 << 3);

    /// Empty permission set.
    #[must_use]
    pub const fn empty() -> Self {
        Self(0)
    }

    /// Full bootstrap permission set.
    #[must_use]
    pub const fn all() -> Self {
        Self(Self::READ.0 | Self::WRITE.0 | Self::DELEGATE.0 | Self::REVOKE.0)
    }

    /// Returns whether `self` contains all bits in `other`.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }
}

impl core::ops::BitOr for CapPermissions {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl core::ops::BitOrAssign for CapPermissions {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

/// Capability syscall error codes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u64)]
pub enum CapError {
    /// The caller lacks the requested authority.
    PermissionDenied = 0,
    /// Another owner holds exclusive access.
    ResourceBusy = 1,
    /// The requested resource does not exist.
    ResourceNotFound = 2,
    /// The handle is invalid or no longer active.
    InvalidHandle = 3,
    /// The handle generation is stale.
    GenerationMismatch = 4,
    /// The kernel cannot allocate additional capability state.
    OutOfMemory = 5,
    /// Delegation attempted to widen authority.
    PermissionEscalation = 6,
}

/// Metadata returned by `cap_list`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(C)]
pub struct CapInfo {
    /// Capability handle visible to user space.
    pub handle: CapHandle,
    /// Type of resource referenced by the handle.
    pub cap_type: CapType,
    /// Granted permission bits.
    pub permissions: CapPermissions,
    /// Parent capability handle, if present.
    pub parent: CapHandle,
    /// Whether `parent` is meaningful.
    pub has_parent: u8,
    /// Number of delegated children.
    pub child_count: u32,
    /// Reserved for future expansion while keeping a stable ABI footprint.
    pub reserved: [u8; 3],
}

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

#[cfg(test)]
mod tests {
    use super::{
        AsiOp, BatchOp, CapInfo, CapPermissions, CoreId, CoreSet, PhysicalAddress, SyscallResult,
    };
    use core::mem::size_of;

    #[test]
    fn syscall_result_is_two_u64_words() {
        assert_eq!(size_of::<SyscallResult>(), 16);
    }

    #[test]
    fn batch_op_has_expected_layout_footprint() {
        assert_eq!(size_of::<BatchOp>(), 40);
    }

    #[test]
    fn asi_op_round_trips_from_raw() {
        assert_eq!(AsiOp::from_raw(0xFF00), Some(AsiOp::AsiBatch));
        assert_eq!(AsiOp::from_raw(0x0312), Some(AsiOp::ThreadPark));
        assert_eq!(AsiOp::from_raw(0xDEAD), None);
    }

    #[test]
    fn shared_transport_types_keep_expected_sizes() {
        assert_eq!(size_of::<PhysicalAddress>(), 8);
        assert_eq!(size_of::<CoreId>(), 4);
        assert_eq!(size_of::<CoreSet>(), 32);
        assert_eq!(size_of::<CapInfo>(), 36);
    }

    #[test]
    fn capability_permissions_compose_as_a_bitset() {
        let mut permissions = CapPermissions::READ | CapPermissions::WRITE;
        permissions |= CapPermissions::REVOKE;
        assert!(permissions.contains(CapPermissions::READ));
        assert!(permissions.contains(CapPermissions::WRITE | CapPermissions::REVOKE));
    }
}
