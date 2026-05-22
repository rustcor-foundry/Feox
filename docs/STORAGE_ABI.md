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
// 0x0500 StorageSubmitRead
#[repr(C)]
pub struct StorageSubmitReadArgs {
    /// Capability identifying the storage device. In bootstrap this
    /// is currently a sentinel handle (the kernel only knows about
    /// one NVMe device); a real device-capability table will replace
    /// this in v1.
    pub device: CapHandle,
    /// NVMe namespace identifier (1-based).
    pub nsid: u32,
    /// Logical block address to read.
    pub lba: u64,
    /// Number of logical blocks. v0 must be 1.
    pub block_count: u16,
    pub _reserved: u16,
    /// Physical address of the DMA buffer (4 KiB aligned, single page
    /// for v0). v1 will replace this with a buffer CapHandle + offset
    /// once `CapRequest::DmaPool` flows end-to-end to user space.
    pub buffer_phys: PhysicalAddress,
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

## Capability story (bootstrap vs. v1)

For v0 the kernel performs only a shape check on the `device`
capability — it accepts any `CapHandle` because the bootstrap process
owns the entire NVMe device. The buffer is named by raw physical
address because the bootstrap probe already owns kernel-direct-map
addresses for the buffer page.

The v1 evolution path is:

1. Add `CapType::StorageDevice` and have PCI enumeration mint one such
   capability per NVMe controller discovered.
2. Add `CapType::DmaPool` flow end-to-end (already in `feox-asi` as
   `CapRequest::DmaPool`) so the user-space buffer is owned by a
   capability the kernel can translate without trusting a raw `u64`.
3. Replace `buffer_phys` with `{ buffer: CapHandle, offset_bytes: u64 }`
   and have the kernel translate via `cap_to_dma_phys`.

The wire layout of `StorageSubmitReadArgs` is **not** stable across
this transition; v0 is bootstrap-only and the field set will change
once the capability story tightens.

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
