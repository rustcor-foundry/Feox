# Aether System Interface (ASI) Specification
## Version 0.1.0 — Draft

---

## 1. Overview

The Aether System Interface (ASI) is the sole runtime boundary between user-space
applications and the Aether kernel. It defines the minimal set of privileged
operations required to grant applications direct, safe access to hardware.

### Design Principles

1. **Setup-time interface.** Applications call the ASI during initialization to
   acquire capabilities and map hardware resources. Once I/O is flowing, the ASI
   is not on the hot path.
2. **Minimal surface.** Every call must justify why it requires kernel privilege.
   If it can be done in user-space, it does not belong here.
3. **Typed capabilities.** Unlike POSIX file descriptors (opaque integers), ASI
   capabilities are typed. User-space code knows exactly what kind of resource
   it holds.
4. **Batch-friendly.** Setup operations can be submitted as a batch to minimize
   kernel transitions.
5. **Fail explicitly.** Every call returns a typed error. No errno globals, no
   ambiguous return codes.

### Syscall Count

17 individual operations + 1 batch wrapper = 18 total.
Linux has ~450. That is the exokernel difference.

---

## 2. Transport Mechanism

### Architecture Status Note

The ASI concepts in this document are intended to be architecture-neutral where
possible, but the current implementation framing is still x86-first.

Today:

- the implemented Feox kernel lane is `x86_64`
- syscall transport wording below reflects that current lane
- interrupt and IOMMU examples also use x86 server terminology

This should be read as:

- architecture-neutral API intent
- current x86-oriented implementation notes

As Feox grows an ARM64 lane, the architecture-specific transport and interrupt
notes should be split out more cleanly.

### 2.1 Instruction

On x86-64, all ASI calls use the `syscall` / `sysret` instruction pair.

For future non-x86 architectures, the ASI should keep the same typed call model
even if the trap instruction and register convention differ.

### 2.2 Register Convention

| Register | Purpose                                    |
|:---------|:-------------------------------------------|
| `rax`    | ASI opcode (`AsiOp`)                       |
| `rdi`    | Argument pointer (pointer to call struct)  |
| `rsi`    | Argument length (bytes)                    |
| `rax`    | Return: result code (0 = success, else error) |
| `rdi`    | Return: output value (call-specific)       |

Arguments are passed as a pointer to a `#[repr(C)]` struct specific to each
call. This keeps the register convention fixed regardless of call complexity.

### 2.3 Batch Interface

Multiple ASI operations can be submitted in a single kernel entry:

```rust
/// Submit multiple ASI operations in a single syscall transition.
/// Operations execute sequentially. Execution stops at the first failure.
/// Returns the index of the failed operation, or `count` if all succeeded.
pub fn asi_batch(ops: &mut [BatchOp]) -> Result<usize, BatchError>;

#[repr(C)]
pub struct BatchOp {
    pub opcode: AsiOp,
    pub args: *mut u8,         // pointer to the call-specific args struct
    pub args_len: usize,
    pub result: SyscallResult, // kernel writes result here
}

#[repr(C)]
pub enum BatchError {
    /// Operation at index `failed_at` returned the given error.
    OperationFailed { failed_at: usize },
    /// The batch itself was malformed.
    InvalidBatch,
}
```

Typical startup batches 5-8 operations into a single transition.

---

## 3. Shared Types

These types are shared between kernel and user-space via the `asi` crate.

```rust
// asi/src/types.rs

/// Physical address on the machine.
#[repr(transparent)]
pub struct PhysicalAddress(pub u64);

/// PCI Bus:Device:Function address.
#[repr(C)]
pub struct PciAddress {
    pub segment: u16,
    pub bus: u8,
    pub device: u8,
    pub function: u8,
}

/// Process identifier.
#[repr(transparent)]
pub struct ProcessId(pub u64);

/// Thread identifier.
#[repr(transparent)]
pub struct ThreadId(pub u64);

/// Core identifier.
#[repr(transparent)]
pub struct CoreId(pub u32);

/// A set of cores, represented as a bitmask.
/// Supports up to 256 cores. Extend if needed.
#[repr(C)]
pub struct CoreSet {
    pub bits: [u64; 4],
}

/// Fixed-length inline string for IPC endpoint names.
/// No heap allocation. 64 bytes max.
#[repr(C)]
pub struct AsiString {
    pub data: [u8; 64],
    pub len: u8,
}

/// Result type for all ASI calls.
#[repr(C)]
pub struct SyscallResult {
    pub code: u64,    // 0 = success, nonzero = error discriminant
    pub value: u64,   // call-specific return value
}
```

---

## 4. Group 0 — Capability Management

Every interaction with hardware begins with acquiring a capability. Capabilities
are unforgeable, typed, process-scoped tokens that grant access to a specific
hardware resource.

### 4.1 Capability Handle

```rust
/// Capability handle — typed, unforgeable, process-scoped.
/// Handles are validated on every use via the generation counter.
#[repr(C)]
pub struct CapHandle {
    /// Kernel-assigned, unique within the owning process.
    pub id: u64,
    /// What kind of resource this grants access to.
    pub cap_type: CapType,
    /// Revocation counter. Incremented when a capability is revoked.
    /// Stale handles (wrong generation) fail immediately.
    pub generation: u32,
}

#[repr(C)]
pub enum CapType {
    PhysicalMemory  = 0,
    DmaPool         = 1,
    DeviceMmio      = 2,
    MsixVector      = 3,
    IpcEndpoint     = 4,
}
```

### 4.2 Capability Permissions

```rust
bitflags! {
    #[repr(C)]
    pub struct CapPermissions: u32 {
        const READ     = 0b0000_0001;
        const WRITE    = 0b0000_0010;
        const DELEGATE = 0b0000_0100;  // can this cap be further delegated?
        const REVOKE   = 0b0000_1000;  // can the holder revoke child caps?
    }
}
```

### 4.3 Revocation Model: Cascading

When a capability is released or its owning process exits, all capabilities
delegated from it are **recursively revoked**. This is cascading revocation.

The kernel maintains a delegation tree per resource. Revocation walks the tree
and increments the `generation` counter on each descendant. Any subsequent use
of a stale handle returns `CapError::GenerationMismatch`.

In-flight DMA operations referencing revoked memory are drained before the
physical pages are reclaimed. The IOMMU mapping is torn down only after
hardware confirms no outstanding transactions.

### 4.4 cap_request

```rust
/// Request a new capability from the kernel.
///
/// Opcode: 0x0000
///
/// The kernel checks whether the calling process has the authority to access
/// the requested resource. On success, returns a typed capability handle.
pub fn cap_request(request: &CapRequest) -> Result<CapHandle, CapError>;

#[repr(C)]
pub enum CapRequest {
    /// Access to a PCI device's BAR region (for MMIO to device registers).
    DeviceBar {
        pci_addr: PciAddress,
        bar_index: u8,
    },

    /// A contiguous, DMA-safe physical memory region.
    /// The kernel programs the IOMMU to allow the associated device
    /// to access this region. The `device` field specifies which device.
    DmaPool {
        size_bytes: usize,
        alignment: usize,
        device: PciAddress,    // IOMMU will authorize this device for DMA
    },

    /// Ownership of a specific MSI-X interrupt vector on a device.
    MsixVector {
        pci_addr: PciAddress,
        vector: u16,
    },

    /// A range of physical memory pages (for shared memory, large buffers).
    PhysicalPages {
        num_pages: usize,
        flags: PageFlags,
    },

    /// An IPC endpoint for inter-process communication.
    IpcEndpoint {
        name: AsiString,
    },
}

bitflags! {
    #[repr(C)]
    pub struct PageFlags: u32 {
        const HUGE_2M    = 0b0001;
        const HUGE_1G    = 0b0010;
        const CONTIGUOUS = 0b0100;  // physically contiguous (for DMA)
    }
}
```

**IOMMU behavior:** When a `DmaPool` capability is granted, the kernel
automatically programs the active IOMMU implementation for the target
architecture to allow the specified device
to read/write the allocated physical pages. When the capability is released
(or cascade-revoked), the IOMMU mapping is torn down after draining in-flight
DMA. User-space never interacts with the IOMMU directly.

### 4.5 cap_release

```rust
/// Release a capability. Cascading revocation applies.
///
/// Opcode: 0x0001
///
/// All capabilities delegated from this handle are recursively revoked.
/// Associated memory mappings are unmapped. IOMMU entries are torn down
/// after in-flight DMA is drained.
pub fn cap_release(handle: CapHandle) -> Result<(), CapError>;
```

### 4.6 cap_delegate

```rust
/// Delegate a capability to another process with (optionally) reduced permissions.
///
/// Opcode: 0x0002
///
/// The new capability is a child of the source. If the source is revoked,
/// the child is cascade-revoked.
///
/// Permissions can only be narrowed, never widened. Attempting to grant
/// permissions the source does not hold returns PermissionDenied.
pub fn cap_delegate(
    handle: CapHandle,
    target_pid: ProcessId,
    mask: CapPermissions,
) -> Result<CapHandle, CapError>;
```

### 4.7 cap_list

```rust
/// List all capabilities held by the calling process.
///
/// Opcode: 0x0003
///
/// Writes capability metadata into the provided buffer. Returns the
/// number of entries written. If the buffer is too small, returns
/// as many as fit and sets the result value to the total count.
pub fn cap_list(buf: &mut [CapInfo]) -> Result<usize, CapError>;

#[repr(C)]
pub struct CapInfo {
    pub handle: CapHandle,
    pub permissions: CapPermissions,
    pub parent: Option<CapHandle>,   // None if this is a root capability
    pub child_count: u32,            // number of delegated children
}
```

### 4.8 Capability Errors

```rust
#[repr(C)]
pub enum CapError {
    /// The calling process lacks authority to access this resource.
    PermissionDenied      = 0,
    /// Another process holds exclusive access to this resource.
    ResourceBusy          = 1,
    /// The requested PCI device, BAR, or vector does not exist.
    ResourceNotFound      = 2,
    /// The handle was revoked or was never valid.
    InvalidHandle         = 3,
    /// The capability was revoked and re-issued; this handle is stale.
    GenerationMismatch    = 4,
    /// Insufficient physical memory to fulfill the request.
    OutOfMemory           = 5,
    /// Attempted to widen permissions during delegation.
    PermissionEscalation  = 6,
}
```

---

## 5. Group 1 — Memory Mapping

Once a capability is acquired, the resource must be mapped into the process's
virtual address space before it can be accessed.

### 5.1 mem_map

```rust
/// Map a capability's resource into this process's virtual address space.
///
/// Opcode: 0x0100
///
/// This bridges "I have permission" (CapHandle) and "I can access it" (pointer).
/// The kernel allocates virtual address space, configures page tables, and
/// returns the mapping.
pub fn mem_map(
    handle: CapHandle,
    offset: usize,
    length: usize,
    flags: MapFlags,
) -> Result<MappedRegion, MemError>;

#[repr(C)]
pub struct MappedRegion {
    /// Virtual address in the calling process's address space.
    pub base: *mut u8,
    /// Length of the mapping in bytes.
    pub length: usize,
    /// Flags applied to this mapping.
    pub flags: MapFlags,
}

bitflags! {
    #[repr(C)]
    pub struct MapFlags: u32 {
        /// Page is readable.
        const READ           = 0b0000_0001;
        /// Page is writable.
        const WRITE          = 0b0000_0010;
        /// Page is executable (rarely needed for drivers).
        const EXEC           = 0b0000_0100;
        /// Disable caching. Required for MMIO device registers.
        const UNCACHEABLE    = 0b0000_1000;
        /// Write-combining mode. Optimal for doorbell pages.
        const WRITE_COMBINE  = 0b0001_0000;
    }
}
```

### 5.2 mem_unmap

```rust
/// Unmap a previously mapped region.
///
/// Opcode: 0x0101
///
/// The virtual address range is released. The underlying capability
/// is NOT released — only the mapping is removed.
pub fn mem_unmap(region: MappedRegion) -> Result<(), MemError>;
```

### 5.3 mem_vtop

```rust
/// Translate a virtual address to its physical address.
///
/// Opcode: 0x0102
///
/// Required for programming DMA descriptors — hardware needs physical
/// addresses. Only works on addresses within a DmaPool capability.
pub fn mem_vtop(
    dma_handle: CapHandle,
    vaddr: *const u8,
) -> Result<PhysicalAddress, MemError>;
```

### 5.4 mem_vtop_batch

```rust
/// Batch virtual-to-physical translation. One syscall, many translations.
///
/// Opcode: 0x0103
///
/// Critical for NVMe and RDMA setup where scatter-gather lists require
/// many physical addresses at once.
pub fn mem_vtop_batch(
    dma_handle: CapHandle,
    vaddrs: &[*const u8],
    paddrs: &mut [PhysicalAddress],
) -> Result<(), MemError>;
```

### 5.5 Memory Errors

```rust
#[repr(C)]
pub enum MemError {
    /// The provided capability handle is invalid or revoked.
    InvalidCapability   = 0,
    /// No virtual address space available for the mapping.
    OutOfVirtualSpace   = 1,
    /// Insufficient physical memory.
    OutOfPhysicalMemory = 2,
    /// Requested alignment cannot be satisfied.
    AlignmentViolation  = 3,
    /// Offset + length exceeds the capability's resource bounds.
    OffsetOutOfRange    = 4,
    /// mem_vtop called on a non-DMA capability.
    NotDmaCapable       = 5,
    /// The virtual address is not within the specified DMA pool.
    AddressOutOfRange   = 6,
}
```

---

## 6. Group 2 — Interrupt Routing

User-space drivers must receive hardware interrupts. The kernel demultiplexes
interrupt sources to the correct process via a lock-free shared memory
mechanism.

In the current x86-oriented lane, this section is written in terms of MSI-X.

### 6.1 EventSlot

The shared memory structure for interrupt delivery. The kernel writes to it;
user-space reads from it. No signals. No stack unwinding. No context switch
into a handler.

```rust
/// Cache-line aligned, lock-free interrupt notification slot.
///
/// The kernel atomically increments `counter` on each interrupt.
/// The user-space async reactor checks this value and polls the
/// hardware completion queue when it changes.
#[repr(C, align(64))]
pub struct EventSlot {
    /// Monotonically increasing interrupt counter.
    pub counter: AtomicU64,
    /// TSC timestamp of the most recent interrupt (for latency measurement).
    pub timestamp: AtomicU64,
    /// Reserved padding to fill one cache line (64 bytes).
    _pad: [u8; 48],
}
```

**Interrupt delivery path:**

```
Hardware fires interrupt vector or equivalent routed source
  -> Kernel ISR: atomic_increment(event_slot.counter), optional IPI to wake parked thread
  -> User-space reactor: observes counter change, polls hardware CQ, wakes Futures
```

### 6.2 irq_attach

```rust
/// Bind a user-space EventSlot to an interrupt source.
///
/// Opcode: 0x0200
///
/// When the hardware fires the specified interrupt source, the kernel writes to
/// the EventSlot. If the owning thread is parked (via thread_park),
/// the kernel wakes it.
///
/// The `target_core` parameter pins the interrupt to a specific core's
/// local interrupt routing path, ensuring the completion event and the polling thread
/// are on the same core (no cache bouncing).
pub fn irq_attach(
    msix_handle: CapHandle,
    slot: &EventSlot,
    target_core: Option<CoreId>,
) -> Result<IrqBinding, IrqError>;

#[repr(C)]
pub struct IrqBinding {
    pub id: u64,
    pub vector: u16,
    pub core: CoreId,
}
```

### 6.3 irq_detach

```rust
/// Detach and release an interrupt binding.
///
/// Opcode: 0x0201
///
/// The interrupt source is detached and the EventSlot is no longer written to.
pub fn irq_detach(binding: IrqBinding) -> Result<(), IrqError>;
```

### 6.4 Interrupt Errors

```rust
#[repr(C)]
pub enum IrqError {
    /// The MSI-X capability handle is invalid or revoked.
    InvalidCapability   = 0,
    /// This vector is already bound to another EventSlot.
    VectorAlreadyBound  = 1,
    /// The specified core does not exist or is offline.
    InvalidCore         = 2,
}
```

---

## 7. Group 3 — Process & Thread Management

Minimal process management. The kernel manages address spaces, scheduling,
and core assignment. Application logic belongs in user-space.

### 7.1 proc_spawn

```rust
/// Spawn a new process from an ELF image.
///
/// Opcode: 0x0300
///
/// The new process starts with an empty capability set. The parent must
/// explicitly delegate capabilities via `initial_caps`. This enforces
/// the principle of least privilege.
pub fn proc_spawn(
    image: &[u8],
    args: &[AsiString],
    initial_caps: &[CapDelegation],
) -> Result<ProcessId, ProcError>;

#[repr(C)]
pub struct CapDelegation {
    pub source: CapHandle,        // parent's capability
    pub permissions: CapPermissions, // permissions to grant (narrowing only)
}
```

### 7.2 proc_exit

```rust
/// Terminate the calling process.
///
/// Opcode: 0x0301
///
/// All capabilities are released (cascading revocation applies).
/// All memory mappings are unmapped. All threads are terminated.
/// In-flight DMA is drained before physical pages are reclaimed.
pub fn proc_exit(code: i32) -> !;
```

### 7.3 proc_yield

```rust
/// Voluntarily yield the current timeslice.
///
/// Opcode: 0x0302
///
/// Hint to the scheduler. The calling thread is moved to the back
/// of its core's run queue.
pub fn proc_yield();
```

### 7.4 proc_set_affinity

```rust
/// Set the core affinity mask for the calling thread.
///
/// Opcode: 0x0303
///
/// The thread will only be scheduled on cores in the provided set.
/// Critical for pinning I/O threads to the same core as their
/// interrupt delivery (see irq_attach target_core).
pub fn proc_set_affinity(cores: &CoreSet) -> Result<(), ProcError>;
```

### 7.5 thread_spawn

```rust
/// Create a new thread within the calling process.
///
/// Opcode: 0x0310
///
/// The new thread shares the process's address space and capability table.
/// A pre-allocated stack (from a MappedRegion) must be provided.
/// Optionally pin to a specific core.
pub fn thread_spawn(
    entry: fn(*mut u8) -> !,
    stack: MappedRegion,
    arg: *mut u8,
    core: Option<CoreId>,
) -> Result<ThreadId, ProcError>;
```

### 7.6 thread_exit

```rust
/// Terminate the calling thread.
///
/// Opcode: 0x0311
///
/// The thread's stack is not automatically freed — the process is
/// responsible for managing stack memory.
pub fn thread_exit() -> !;
```

### 7.7 thread_park

```rust
/// Park the calling thread until an EventSlot fires or the timeout expires.
///
/// Opcode: 0x0312
///
/// This is the primary sleep mechanism for the async executor. When there
/// are no ready tasks, the executor parks on its set of EventSlots
/// (one per completion queue). The kernel wakes the thread when any
/// slot is incremented by an interrupt, or when the timeout expires.
///
/// Returns the index of the EventSlot that fired, or TimedOut.
pub fn thread_park(
    slots: &[&EventSlot],
    timeout: Option<Duration>,
) -> Result<ParkResult, ProcError>;

#[repr(C)]
pub enum ParkResult {
    /// The EventSlot at this index was incremented.
    Woken { slot_index: usize },
    /// The timeout expired before any slot fired.
    TimedOut,
}

/// Duration for thread_park timeout.
/// Nanosecond precision, backed by TSC or HPET.
#[repr(C)]
pub struct Duration {
    pub nanos: u64,
}
```

### 7.8 Process/Thread Errors

```rust
#[repr(C)]
pub enum ProcError {
    /// The ELF image is malformed or unsupported.
    InvalidImage     = 0,
    /// Insufficient memory for the new address space or stack.
    OutOfMemory      = 1,
    /// The specified core does not exist or is offline.
    InvalidCore      = 2,
    /// Process or system thread limit reached.
    TooManyThreads   = 3,
    /// The provided stack region is invalid or too small.
    InvalidStack     = 4,
}
```

---

## 8. Group 4 — Device Discovery

Applications must discover available hardware before requesting capabilities.
The kernel owns PCI/PCIe enumeration (requires config space access) and
exposes the results via a query interface.

### 8.1 dev_enumerate

```rust
/// Query available PCI/PCIe devices.
///
/// Opcode: 0x0400
///
/// Returns device metadata for all devices matching the filter.
/// If the result buffer is too small, returns as many as fit and
/// sets the result value to the total number of matching devices.
pub fn dev_enumerate(
    filter: &DeviceFilter,
    results: &mut [PciDeviceInfo],
) -> Result<usize, DevError>;

#[repr(C)]
pub struct DeviceFilter {
    /// Filter by vendor ID (e.g., 0x15b3 for Mellanox). None = any.
    pub vendor_id: Option<u16>,
    /// Filter by PCI class code (e.g., 0x0108 for NVMe). None = any.
    pub device_class: Option<u16>,
}

#[repr(C)]
pub struct PciDeviceInfo {
    /// PCI bus address of this device.
    pub address: PciAddress,
    /// Vendor and device identifiers.
    pub vendor_id: u16,
    pub device_id: u16,
    /// PCI class code (identifies device type).
    pub class_code: u16,
    /// Base Address Register information (up to 6 BARs).
    pub bars: [BarInfo; 6],
    /// Number of MSI-X vectors this device supports on x86-class PCIe systems.
    pub msix_count: u16,
    /// Whether this device is currently claimed by another process.
    pub claimed: bool,
}

#[repr(C)]
pub struct BarInfo {
    /// Physical base address of this BAR.
    pub base_physical: PhysicalAddress,
    /// Size of the BAR region in bytes.
    pub size: usize,
    /// True = memory-mapped I/O. False = I/O port (unsupported).
    pub is_mmio: bool,
    /// True = this BAR is 64-bit (consumes the next BAR slot).
    pub is_64bit: bool,
    /// True = prefetchable (can use write-combining).
    pub is_prefetchable: bool,
}
```

### 8.2 Device Errors

```rust
#[repr(C)]
pub enum DevError {
    /// The result buffer was too small. Check result value for total count.
    BufferTooSmall   = 0,
    /// PCI enumeration failed (hardware or firmware error).
    EnumerationFailed = 1,
}
```

---

## 9. Complete Opcode Table

```rust
// asi/src/opcodes.rs

#[repr(u64)]
pub enum AsiOp {
    // Group 0: Capability Management
    CapRequest       = 0x0000,
    CapRelease       = 0x0001,
    CapDelegate      = 0x0002,
    CapList          = 0x0003,

    // Group 1: Memory Mapping
    MemMap           = 0x0100,
    MemUnmap         = 0x0101,
    MemVtoP          = 0x0102,
    MemVtoPBatch     = 0x0103,

    // Group 2: Interrupt Routing
    IrqAttach        = 0x0200,
    IrqDetach        = 0x0201,

    // Group 3: Process & Thread Management
    ProcSpawn        = 0x0300,
    ProcExit         = 0x0301,
    ProcYield        = 0x0302,
    ProcSetAffinity  = 0x0303,
    ThreadSpawn      = 0x0310,
    ThreadExit       = 0x0311,
    ThreadPark       = 0x0312,

    // Group 4: Device Discovery
    DevEnumerate     = 0x0400,

    // Batch
    AsiBatch         = 0xFF00,
}
```

---

## 10. Typical Application Startup Sequence

The following demonstrates a complete NVMe application startup using batch
submission to minimize kernel transitions:

```rust
async fn main() -> Result<(), AsiError> {
    // 1. Discover hardware (1 syscall)
    let mut devices = [PciDeviceInfo::zeroed(); 4];
    let count = asi::device::dev_enumerate(
        &DeviceFilter { device_class: Some(0x0108), ..default() },
        &mut devices,
    )?;
    let nvme = &devices[0];

    // 2. Acquire all capabilities in one batch (1 syscall)
    let mut ops = [
        BatchOp::new(AsiOp::CapRequest, &CapRequest::DeviceBar {
            pci_addr: nvme.address,
            bar_index: 0,
        }),
        BatchOp::new(AsiOp::CapRequest, &CapRequest::DmaPool {
            size_bytes: 2 * 1024 * 1024,
            alignment: 4096,
            device: nvme.address,
        }),
        BatchOp::new(AsiOp::CapRequest, &CapRequest::MsixVector {
            pci_addr: nvme.address,
            vector: 0,
        }),
    ];
    let completed = asi::batch(&mut ops)?;

    let bar0_cap = ops[0].result.as_cap_handle()?;
    let dma_cap  = ops[1].result.as_cap_handle()?;
    let msix_cap = ops[2].result.as_cap_handle()?;

    // 3. Map resources + attach interrupt in one batch (1 syscall)
    let mut ops2 = [
        BatchOp::new(AsiOp::MemMap, &MemMapArgs {
            handle: bar0_cap,
            offset: 0,
            length: nvme.bars[0].size,
            flags: MapFlags::READ | MapFlags::WRITE | MapFlags::UNCACHEABLE,
        }),
        BatchOp::new(AsiOp::MemMap, &MemMapArgs {
            handle: dma_cap,
            offset: 0,
            length: 2 * 1024 * 1024,
            flags: MapFlags::READ | MapFlags::WRITE,
        }),
        BatchOp::new(AsiOp::IrqAttach, &IrqAttachArgs {
            msix_handle: msix_cap,
            slot: &event_slot,
            target_core: Some(CoreId(0)),
        }),
    ];
    asi::batch(&mut ops2)?;

    // 3 syscalls total for complete setup.
    // === No more syscalls on the I/O path. ===

    // 4. Initialize NVMe controller entirely in user-space
    let regs = ops2[0].result.as_mapped_region()?;
    let dma  = ops2[1].result.as_mapped_region()?;
    let controller = NvmeController::init(regs, dma)?;
    let qpair = controller.create_io_queue_pair(1)?;

    // 5. Do I/O — zero syscalls, direct hardware queue access
    let buf = DmaBuf::from_pool(&dma, 4096)?;
    qpair.read(lba: 0, buf).await?;

    Ok(())
}
```

**3 syscalls** for complete application startup. **0 syscalls** for I/O.

---

## 11. Security Considerations

### 11.1 Capability Authority

The initial process (init) receives root capabilities for all discovered
hardware. All other processes receive capabilities only via delegation.
There is no ambient authority — a process that holds no capabilities
cannot access any hardware or kernel resources.

### 11.2 IOMMU Enforcement

All DMA operations are constrained by IOMMU mappings that the
kernel programs automatically when DmaPool capabilities are granted. A device
can only DMA to/from pages explicitly authorized for it. This prevents DMA
attacks where a malicious driver programs a device to read/write arbitrary
physical memory.

### 11.3 Cascading Revocation

Capability delegation forms a tree. Revoking any node revokes all descendants.
This ensures that a parent process can always fully contain a child's hardware
access. There is no way to "launder" a capability to escape revocation.

### 11.4 Generation Counters

Each capability slot has a monotonic generation counter. When a capability
is revoked, the generation increments. Any subsequent use of a handle with
a stale generation fails with `GenerationMismatch` before any resource
access occurs. This is a constant-time check on every ASI call.

---

## 12. Future Extensions (Reserved Opcode Space)

These opcodes are reserved but not specified in v0.1.0:

| Range         | Purpose                                  |
|:--------------|:-----------------------------------------|
| `0x0500-05FF` | Power management (C-states, frequency)   |
| `0x0600-06FF` | Debug / tracing                          |
| `0x0700-07FF` | Hot-plug device events                   |
| `0xFE00-FEFF` | Vendor-specific extensions               |

---

## Appendix A: Design Rationale

### Why not integer syscall numbers with register arguments?

Register-based syscalls (Linux style) are fast but untyped. A wrong value in
`rsi` is a silent bug or a kernel panic. ASI passes a pointer to a typed
`#[repr(C)]` struct. The kernel validates the struct before acting. The
performance difference is negligible because ASI calls are setup-time only.

The exact trap instruction and argument registers may vary by architecture, but
the typed-call model should remain stable.

### Why cascading revocation instead of reference counting?

Reference-counted capabilities (where a capability lives as long as anyone
holds it) create a problem: a parent process cannot fully revoke a child's
access. In an exokernel where capabilities grant direct hardware access,
the parent must always be able to contain the child. Cascading revocation
is the only model that guarantees this.

### Why implicit IOMMU management?

Requiring user-space to explicitly program the IOMMU would create a security
gap: a buggy or malicious driver could skip the IOMMU setup and allow a
device to DMA anywhere. By making IOMMU programming automatic and mandatory,
the kernel ensures that DMA safety is not opt-in.

### Why EventSlots instead of signals or upcalls?

Signals (POSIX) require complex stack unwinding and are notoriously difficult
to use correctly. Upcalls (like Scheduler Activations) require the kernel to
inject code into user-space, which conflicts with memory safety guarantees.
EventSlots are a single atomic write from the kernel and a single atomic read
from user-space. They compose naturally with async/await polling loops.
