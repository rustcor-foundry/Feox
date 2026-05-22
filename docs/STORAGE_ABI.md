# Storage ABI

Last updated: 2026-05-21

This document captures the first-cut storage surface that the Feox ASI
exposes across the kernel / user boundary. It locks the v0 semantics so
the kernel-side dispatch and the future user-side wrappers agree on a
single shape; later revisions can evolve from this baseline.

## Decision summary

| Question | Decision | Rationale |
|----------|----------|-----------|
| Block-style or NVMe-specific? | Block-style first, with NVMe escape hatch deferred. | Keeps the public ABI protocol-agnostic. The kernel's `crate::block` already hides NVMe specifics behind a small read/drain surface. Future drivers (SATA, virtio-blk, ...) can plug into the same opcodes. |
| Sync, EventSlot, or Submit+Poll? | **Submit+Poll**. | Doesn't require a long-lived kernel executor + per-syscall task to drive event-slot signaling. Matches the kernel's current model where the executor + drainer pump completions, and lets the syscall return immediately. EventSlot variant is planned for v1 once thread parking is wired through to user space. |
| Capability shape | Two capabilities per submission: storage device cap + DMA buffer cap. | Aligns with `mem_map` / `cap_request` pattern. Bootstrap can use a sentinel device cap until a real device-capability table exists. |
| Wait semantics | Caller polls; kernel drains on every syscall entry. | Avoids blocking the syscall handler. Drain-on-entry guarantees that any pending completion lands before `StoragePoll` checks the result. |

## Opcodes

```
0x0500  StorageSubmitRead   submit a single-LBA read; returns a token
0x0501  StoragePoll         poll a token; NotReady | Ready { completion }
```

Opcodes `0x0502`-`0x05FF` are reserved for follow-up storage operations
(write, flush, multi-LBA reads, EventSlot variants, raw NVMe escape
hatch).

## Argument and result types

The exact field layout is owned by `crates/feox-asi/src/lib.rs`. The
shapes below summarize the contract that the kernel-side dispatcher
relies on.

```rust
// 0x0500 StorageSubmitRead (v1 shape; v0 took a raw buffer_phys)
#[repr(C)]
pub struct StorageSubmitReadArgs {
    /// Capability identifying the storage device. Still accepted as a
    /// sentinel today; `CapType::StorageDevice` enforcement lands once
    /// PCI enumeration mints one such cap per controller.
    pub device: CapHandle,
    /// NVMe namespace identifier (1-based).
    pub nsid: u32,
    /// Logical block address to read.
    pub lba: u64,
    /// Number of logical blocks. v1 must be 1.
    pub block_count: u16,
    pub _reserved: u16,
    /// Capability backing the DMA buffer. v1 accepts
    /// `CapType::PhysicalMemory`; once `CapRequest::DmaPool` flows
    /// end-to-end, `CapType::DmaPool` will be accepted alongside it.
    /// Requires READ + WRITE permissions.
    pub buffer: CapHandle,
    /// Byte offset into the buffer capability. The kernel rejects
    /// submissions when `buffer_offset + 4096` exceeds the
    /// capability's `size_bytes`.
    pub buffer_offset: u64,
}

// Token returned by StorageSubmitRead and consumed by StoragePoll.
// Opaque; the kernel uses it as an index into a small inflight table.
#[repr(transparent)]
pub struct StorageToken(pub u64);

// 0x0501 StoragePoll
#[repr(C)]
pub struct StoragePollArgs {
    pub token: StorageToken,
    /// Writable destination for the completion (only written when the
    /// result is `Ready`).
    pub out_completion: *mut StorageCompletion,
}

#[repr(C)]
pub struct StorageCompletion {
    /// NVMe SCT (status code type), 0 on success.
    pub nvme_sct: u8,
    /// NVMe SC (status code), 0 on success.
    pub nvme_sc: u8,
    /// 1 if the controller set DNR (do-not-retry).
    pub dnr: u8,
    pub _reserved: u8,
}

#[repr(u64)]
pub enum StoragePollResult {
    NotReady = 0,
    Ready    = 1,
}

#[repr(u64)]
pub enum StorageError {
    InvalidCapability     = 0,
    NotInitialized        = 1,
    UnsupportedBlockCount = 2,
    SubmitFailed          = 3,
    InvalidToken          = 4,
    InflightTableFull     = 5,
}
```

The syscall return-code convention follows the existing ASI pattern:

- `SYSCALL_OK (0)` on success
- `SYSCALL_ERR_INVALID_ARGS` on shape errors (wrong `args_len`, null
  pointer, etc.)
- A high-half `0xFFFF_05XX` code carrying the `StorageError` variant on
  storage-layer failure

`*out_value` carries the `StorageToken` (for submit) or the
`StoragePollResult` (for poll); the completion struct is written
through the user-supplied `out_completion` pointer.

## Capability story (v0 / v1 / v2)

**v0 (initial).** Buffer named by raw `buffer_phys: PhysicalAddress`,
device cap accepted as any `CapHandle`. Bootstrap-only.

**v1.** Buffer named by `{ buffer: CapHandle, buffer_offset: u64 }`;
kernel translates via `cap_to_phys_base`, which accepts
`CapType::PhysicalMemory`. Device capability still not enforced —
`CapType::StorageDevice` exists as an enum variant but is not minted.

**v2 (current).** Device capability is enforced.
`block::register_device_capability(bar_base, bar_size)` registers a
`CapType::StorageDevice` resource over the controller's BAR and mints
a root capability that the boot code retains on the live block device.
`dispatch_storage_submit_read` verifies `args.device` is a live
`StorageDevice` cap with `READ | WRITE`; anything else (including the
old sentinel zero handle) returns `0xFFFF_0500 + InvalidCapability`.
The block layer releases the device cap during `shutdown`. The
`StorageSubmitReadArgs` wire layout did not change from v1.

**v3 (planned).** `CapType::DmaPool` minting + delegation wired
through `cap_request` so user space can request DMA-safe memory
directly (today the buffer must be a `PhysicalMemory` cap, which works
in bootstrap but won't generalize once an IOMMU is in the mix). Add
an EventSlot/park variant so callers can sleep on completion instead
of spin-polling.

The wire layout of `StorageSubmitReadArgs` is **not** stable across
v0 → v1 (the `buffer_phys` field was replaced). It is intended to be
stable from v1 forward; v2 only tightened the kernel-side enforcement.

## Drain semantics

Every syscall entry (regardless of opcode) calls `crate::block::drain`
before dispatching. This guarantees that any completion delivered by
the controller between two syscalls lands in the inflight table before
the next `StoragePoll` checks it. The drain pass is cheap when no
completions are pending; the kernel already maintains the I/O CQ phase
bit so an empty CQ is a single read.

## Kernel-side inflight model

`crate::block` gains a small fixed-capacity `InflightSubmissions` table
that maps `StorageToken` → submission state:

```rust
enum SubmissionState {
    Pending { cid: u16, future: NvmeIoFuture<8> },
    Ready   { completion: StorageCompletion },
    Empty,
}
```

`storage_submit_read` reserves the next free slot, calls
`block::read`, and stores the resulting `NvmeIoFuture`. `storage_poll`
drains, then advances the future via a `noop_waker()` poll. If the
future resolves, the completion is stored as `Ready` and the slot's
state changes accordingly. The next `storage_poll` call returns the
stored completion and frees the slot.

The inflight table is sized for v0 single-caller use; SMP / multi-
process scaling will revisit the capacity.

## Out of scope (v0)

- writes, flushes, trim, NVMe vendor commands
- multi-LBA reads (`block_count != 1`)
- EventSlot / ThreadPark integration (planned for v1)
- multi-device / multi-namespace device enumeration
- raw NVMe SQE escape hatch
- read-only mounts, file systems, anything above the block layer
