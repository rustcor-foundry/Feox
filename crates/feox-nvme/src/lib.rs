#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]

//! `NVMe` queue and inflight tracking primitives for Feox.

#[cfg(test)]
extern crate std;

use core::cell::Cell;
use core::future::Future;
use core::marker::PhantomData;
use core::pin::Pin;
use core::ptr::NonNull;
use core::task::{Context, Poll, Waker};

/// Decoded `NVMe` status tuple.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NvmeStatus {
    /// Status code type.
    pub sct: u8,
    /// Status code.
    pub sc: u8,
    /// Do not retry.
    pub dnr: bool,
}

/// Minimal completion representation used by the prototype queue model.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NvmeCompletion {
    /// Command identifier.
    pub cid: u16,
    /// Completion status.
    pub status: NvmeStatus,
}

impl NvmeCompletion {
    /// Creates a successful completion.
    #[must_use]
    pub const fn success(cid: u16) -> Self {
        Self {
            cid,
            status: NvmeStatus {
                sct: 0,
                sc: 0,
                dnr: false,
            },
        }
    }

    /// Returns whether the completion succeeded.
    #[must_use]
    pub const fn succeeded(self) -> bool {
        self.status.sct == 0 && self.status.sc == 0
    }
}

/// Driver-visible `NVMe` errors.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NvmeError {
    /// No command slots are currently available.
    QueueFull,
    /// The device disappeared from the system.
    DeviceRemoved,
    /// The capability or queue ownership was revoked.
    CapabilityRevoked,
    /// The command finished with a non-success status.
    CommandFailed(NvmeStatus),
    /// A stale future observed a recycled slot.
    StaleSlot,
}

type CompletionOutcome = Result<NvmeCompletion, NvmeError>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SlotState {
    Free,
    Submitted,
    Completed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CommandLease {
    cid: u16,
    generation: u64,
}

struct InflightEntry {
    generation: u64,
    state: SlotState,
    result: Option<CompletionOutcome>,
    waker: Option<Waker>,
    future_attached: bool,
}

impl InflightEntry {
    const fn new() -> Self {
        Self {
            generation: 0,
            state: SlotState::Free,
            result: None,
            waker: None,
            future_attached: false,
        }
    }
}

#[derive(Debug)]
struct LocalOnly(Cell<()>);

/// Core-local mapping from command IDs to pending futures.
///
/// `N` is the queue depth and must be in the range `1..=65535`. Each slot
/// corresponds to one CID value. The free list is a fixed-capacity stack; no
/// heap allocation is required.
pub struct InflightMap<const N: usize> {
    entries: [InflightEntry; N],
    /// Fixed-capacity free-CID stack. Valid entries are `free[0..free_head]`.
    free: [u16; N],
    /// Number of items currently on the free stack.
    free_head: usize,
}

impl<const N: usize> InflightMap<N> {
    /// Creates an inflight map with `N` reusable slots.
    ///
    /// CIDs are assigned from `0` to `N - 1`. They are pushed onto the free
    /// stack in reverse order so the first `register` call returns CID 0.
    #[must_use]
    pub fn new() -> Self {
        // Entries default to the Free/zero state; no unsafe required.
        let entries = core::array::from_fn(|_| InflightEntry::new());
        // Free stack: slot N-1 is the top so the first pop returns CID 0.
        let free = core::array::from_fn(|i| (N - i - 1) as u16);
        Self {
            entries,
            free,
            free_head: N,
        }
    }

    /// Returns the number of currently free CIDs.
    #[must_use]
    pub fn available(&self) -> usize {
        self.free_head
    }

    /// Registers a new in-flight command and returns the core-local future that
    /// will observe its completion.
    pub fn register(&mut self) -> Result<(u16, NvmeIoFuture<N>), NvmeError> {
        let cid = self.pop_free().ok_or(NvmeError::QueueFull)?;
        let entry = &mut self.entries[cid as usize];
        let next_generation = entry.generation.wrapping_add(1).max(1);

        entry.generation = next_generation;
        entry.state = SlotState::Submitted;
        entry.result = None;
        entry.waker = None;
        entry.future_attached = true;

        Ok((
            cid,
            NvmeIoFuture {
                map: NonNull::from(self),
                lease: CommandLease {
                    cid,
                    generation: next_generation,
                },
                _not_send: PhantomData,
            },
        ))
    }

    /// Completes a single command and wakes the waiter if it already registered
    /// interest.
    pub fn complete(&mut self, completion: NvmeCompletion) {
        let outcome = if completion.succeeded() {
            Ok(completion)
        } else {
            Err(NvmeError::CommandFailed(completion.status))
        };

        self.finish(completion.cid, outcome);
    }

    /// Fails every live command, including commands whose futures have not been
    /// polled yet.
    ///
    /// # Panics
    ///
    /// Panics if the inflight entry count no longer fits in `u16`, which would
    /// violate the queue model's command-ID contract.
    pub fn fail_all(&mut self, error: NvmeError) {
        for cid in 0..N {
            if self.entries[cid].state != SlotState::Free {
                self.finish(cid as u16, Err(error));
            }
        }
    }

    fn finish(&mut self, cid: u16, outcome: CompletionOutcome) {
        let entry = &mut self.entries[cid as usize];
        if entry.state == SlotState::Free {
            return;
        }

        entry.state = SlotState::Completed;
        entry.result = Some(outcome);

        if entry.future_attached {
            if let Some(waker) = entry.waker.take() {
                waker.wake();
            }
        } else {
            self.recycle(cid);
        }
    }

    fn recycle(&mut self, cid: u16) {
        let entry = &mut self.entries[cid as usize];
        if entry.state == SlotState::Free {
            return;
        }

        entry.state = SlotState::Free;
        entry.result = None;
        entry.waker = None;
        entry.future_attached = false;
        self.push_free(cid);
    }

    fn pop_free(&mut self) -> Option<u16> {
        if self.free_head == 0 {
            return None;
        }
        self.free_head -= 1;
        Some(self.free[self.free_head])
    }

    fn push_free(&mut self, cid: u16) {
        // Invariant: free_head < N because we only push CIDs that came from
        // pop_free, so the count can never exceed N.
        debug_assert!(self.free_head < N);
        self.free[self.free_head] = cid;
        self.free_head += 1;
    }
}

impl<const N: usize> Default for InflightMap<N> {
    fn default() -> Self {
        Self::new()
    }
}

/// A submitted command whose completion will be delivered to the owning core.
pub struct NvmeIoFuture<const N: usize> {
    map: NonNull<InflightMap<N>>,
    lease: CommandLease,
    _not_send: PhantomData<&'static LocalOnly>,
}

impl<const N: usize> Future for NvmeIoFuture<N> {
    type Output = Result<NvmeCompletion, NvmeError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        // SAFETY: the future is intentionally `!Send` and is only polled on the
        // owning core that also owns the inflight map.
        let map = unsafe { this.map.as_mut() };
        let cid = this.lease.cid as usize;

        if map.entries[cid].generation != this.lease.generation {
            return Poll::Ready(Err(NvmeError::StaleSlot));
        }

        if let Some(result) = map.entries[cid].result.take() {
            map.entries[cid].future_attached = false;
            map.entries[cid].waker = None;
            map.recycle(this.lease.cid);
            Poll::Ready(result)
        } else {
            map.entries[cid].waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}

impl<const N: usize> Drop for NvmeIoFuture<N> {
    fn drop(&mut self) {
        // SAFETY: the future is `!Send` and cannot outlive the executor-local
        // inflight map that created it.
        let map = unsafe { self.map.as_mut() };
        let cid = self.lease.cid as usize;

        if cid >= N {
            return;
        }

        let entry = &mut map.entries[cid];
        if entry.generation != self.lease.generation || entry.state == SlotState::Free {
            return;
        }

        entry.future_attached = false;
        entry.waker = None;

        if entry.state == SlotState::Completed {
            map.recycle(self.lease.cid);
        }
    }
}

/// Small prototype queue pair that exposes safe CID allocation semantics.
pub struct NvmeQueuePair<const N: usize> {
    inflight: InflightMap<N>,
    failed: Option<NvmeError>,
}

impl<const N: usize> NvmeQueuePair<N> {
    /// Creates a new queue pair with `N` command slots.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inflight: InflightMap::new(),
            failed: None,
        }
    }

    /// Returns the number of currently free command slots.
    #[must_use]
    pub fn available(&self) -> usize {
        self.inflight.available()
    }

    /// Reserves a command ID and returns the future that will complete it.
    pub fn submit(&mut self) -> Result<(u16, NvmeIoFuture<N>), NvmeError> {
        if let Some(error) = self.failed {
            return Err(error);
        }

        self.inflight.register()
    }

    /// Delivers a completion to the corresponding in-flight command.
    pub fn complete(&mut self, completion: NvmeCompletion) {
        self.inflight.complete(completion);
    }

    /// Fails the queue and all pending commands.
    pub fn fail(&mut self, error: NvmeError) {
        self.failed = Some(error);
        self.inflight.fail_all(error);
    }
}

impl<const N: usize> Default for NvmeQueuePair<N> {
    fn default() -> Self {
        Self::new()
    }
}

/// Decoded view of the NVMe `CAP` (Controller Capabilities) register.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Cap(pub u64);

impl Cap {
    /// Maximum Queue Entries Supported (zero-based, +1 to get the real
    /// queue depth limit).
    #[must_use]
    pub const fn mqes(self) -> u16 {
        (self.0 & 0xFFFF) as u16
    }
    /// Doorbell stride: bytes between adjacent doorbells = `4 << dstrd()`.
    #[must_use]
    pub const fn dstrd(self) -> u8 {
        ((self.0 >> 32) & 0xF) as u8
    }
    /// Minimum host memory page size: `2 ^ (12 + mpsmin())` bytes.
    #[must_use]
    pub const fn mpsmin(self) -> u8 {
        ((self.0 >> 48) & 0xF) as u8
    }
    /// Maximum host memory page size: `2 ^ (12 + mpsmax())` bytes.
    #[must_use]
    pub const fn mpsmax(self) -> u8 {
        ((self.0 >> 52) & 0xF) as u8
    }
}

/// Decoded view of the NVMe `VS` (Version) register.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Vs(pub u32);

impl Vs {
    /// Major version number (MJR).
    #[must_use]
    pub const fn major(self) -> u16 {
        ((self.0 >> 16) & 0xFFFF) as u16
    }
    /// Minor version number (MNR).
    #[must_use]
    pub const fn minor(self) -> u8 {
        ((self.0 >> 8) & 0xFF) as u8
    }
    /// Tertiary version number (TER).
    #[must_use]
    pub const fn tertiary(self) -> u8 {
        (self.0 & 0xFF) as u8
    }
}

/// Decoded view of the NVMe `CSTS` (Controller Status) register.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Csts(pub u32);

impl Csts {
    /// Controller Ready bit (CSTS.RDY, bit 0).
    #[must_use]
    pub const fn ready(self) -> bool {
        (self.0 & 0x1) != 0
    }
    /// Controller Fatal Status bit (CSTS.CFS, bit 1).
    #[must_use]
    pub const fn fatal(self) -> bool {
        (self.0 & 0x2) != 0
    }
}

/// NVMe admin / I/O submission queue entry (64 bytes, little-endian on
/// the wire matching x86_64 native order).
#[repr(C, align(64))]
#[derive(Clone, Copy, Debug, Default)]
pub struct SubmissionQueueEntry {
    /// Command Dword 0: opcode (low byte), fused (bits 9..8), reserved
    /// (bits 13..10), PSDT (bits 15..14), command identifier (bits 31..16).
    pub cdw0: u32,
    /// Namespace identifier (0 for admin commands that do not target
    /// a namespace).
    pub nsid: u32,
    /// Reserved bytes 8..16.
    pub reserved: u64,
    /// Metadata pointer.
    pub mptr: u64,
    /// PRP entry 1 — physical address of the data buffer.
    pub prp1: u64,
    /// PRP entry 2 — second PRP for buffers spanning more than one page.
    pub prp2: u64,
    /// Command Dword 10.
    pub cdw10: u32,
    /// Command Dword 11.
    pub cdw11: u32,
    /// Command Dword 12.
    pub cdw12: u32,
    /// Command Dword 13.
    pub cdw13: u32,
    /// Command Dword 14.
    pub cdw14: u32,
    /// Command Dword 15.
    pub cdw15: u32,
}

impl SubmissionQueueEntry {
    /// Builds an `Identify Controller` admin command (opcode 0x06,
    /// CNS = 0x01) targeting the supplied PRP1 buffer with the given
    /// command identifier.
    #[must_use]
    pub fn identify_controller(prp1: u64, cid: u16) -> Self {
        let mut entry = Self::default();
        entry.cdw0 = 0x06 | (u32::from(cid) << 16);
        entry.prp1 = prp1;
        // CNS = 0x01 in CDW10[7:0] selects the Identify Controller data
        // structure.
        entry.cdw10 = 0x0000_0001;
        entry
    }

    /// Builds a `Create I/O Completion Queue` admin command
    /// (opcode 0x05). `entries` is the desired queue size in entries
    /// (encoded zero-based on the wire). PC=1, IEN=0 (polled), IV=0.
    #[must_use]
    pub fn create_io_completion_queue(qid: u16, entries: u16, prp1: u64, cid: u16) -> Self {
        let mut entry = Self::default();
        entry.cdw0 = 0x05 | (u32::from(cid) << 16);
        entry.prp1 = prp1;
        entry.cdw10 = u32::from(qid) | (u32::from(entries - 1) << 16);
        entry.cdw11 = 0x0000_0001; // PC=1, IEN=0
        entry
    }

    /// Builds a `Create I/O Submission Queue` admin command
    /// (opcode 0x01). `cqid` is the paired I/O CQ. PC=1, priority=0.
    #[must_use]
    pub fn create_io_submission_queue(
        qid: u16,
        entries: u16,
        cqid: u16,
        prp1: u64,
        cid: u16,
    ) -> Self {
        let mut entry = Self::default();
        entry.cdw0 = 0x01 | (u32::from(cid) << 16);
        entry.prp1 = prp1;
        entry.cdw10 = u32::from(qid) | (u32::from(entries - 1) << 16);
        entry.cdw11 = 0x0000_0001 | (u32::from(cqid) << 16); // PC=1, CQID
        entry
    }

    /// Builds a `Delete I/O Completion Queue` admin command (opcode 0x04).
    #[must_use]
    pub fn delete_io_completion_queue(qid: u16, cid: u16) -> Self {
        let mut entry = Self::default();
        entry.cdw0 = 0x04 | (u32::from(cid) << 16);
        entry.cdw10 = u32::from(qid);
        entry
    }

    /// Builds a `Delete I/O Submission Queue` admin command (opcode 0x00).
    #[must_use]
    pub fn delete_io_submission_queue(qid: u16, cid: u16) -> Self {
        let mut entry = Self::default();
        entry.cdw0 = 0x00 | (u32::from(cid) << 16);
        entry.cdw10 = u32::from(qid);
        entry
    }

    /// Builds an `NVM Read` I/O command (opcode 0x02). `nlb` is the
    /// number of logical blocks **encoded as zero-based**: pass `0` to
    /// read one block, `1` for two, etc. `prp1` is the data buffer
    /// physical base.
    #[must_use]
    pub fn nvm_read(nsid: u32, slba: u64, nlb_zero_based: u16, prp1: u64, cid: u16) -> Self {
        let mut entry = Self::default();
        entry.cdw0 = 0x02 | (u32::from(cid) << 16);
        entry.nsid = nsid;
        entry.prp1 = prp1;
        entry.cdw10 = slba as u32;
        entry.cdw11 = (slba >> 32) as u32;
        entry.cdw12 = u32::from(nlb_zero_based);
        entry
    }

    /// Builds an `NVM Write` I/O command (opcode 0x01). Field encoding
    /// matches [`Self::nvm_read`].
    #[must_use]
    pub fn nvm_write(nsid: u32, slba: u64, nlb_zero_based: u16, prp1: u64, cid: u16) -> Self {
        let mut entry = Self::default();
        entry.cdw0 = 0x01 | (u32::from(cid) << 16);
        entry.nsid = nsid;
        entry.prp1 = prp1;
        entry.cdw10 = slba as u32;
        entry.cdw11 = (slba >> 32) as u32;
        entry.cdw12 = u32::from(nlb_zero_based);
        entry
    }

    /// Builds an `NVM Flush` I/O command (opcode 0x00): a durability
    /// barrier for all previously completed writes on the namespace.
    #[must_use]
    pub fn nvm_flush(nsid: u32, cid: u16) -> Self {
        let mut entry = Self::default();
        entry.cdw0 = 0x00 | (u32::from(cid) << 16);
        entry.nsid = nsid;
        entry
    }

    /// Builds an `Identify Namespace` admin command (opcode 0x06,
    /// CNS = 0x00) for `nsid`, DMA'd into the PRP1 buffer.
    #[must_use]
    pub fn identify_namespace(nsid: u32, prp1: u64, cid: u16) -> Self {
        let mut entry = Self::default();
        entry.cdw0 = 0x06 | (u32::from(cid) << 16);
        entry.nsid = nsid;
        entry.prp1 = prp1;
        entry.cdw10 = 0x0000_0000; // CNS = 0x00: Identify Namespace
        entry
    }

    /// Returns this entry with the command identifier replaced (CDW0
    /// bits 31..16). Queue rings use this to assign ring-owned CIDs at
    /// submit time, so builders may pass a placeholder.
    #[must_use]
    pub const fn with_cid(mut self, cid: u16) -> Self {
        self.cdw0 = (self.cdw0 & 0xFFFF) | ((cid as u32) << 16);
        self
    }
}

/// NVMe admin / I/O completion queue entry (16 bytes).
#[repr(C, align(16))]
#[derive(Clone, Copy, Debug, Default)]
pub struct CompletionQueueEntry {
    /// Dword 0 — command-specific.
    pub dw0: u32,
    /// Dword 1 — command-specific.
    pub dw1: u32,
    /// Dword 2 — submission queue head pointer (low 16), submission
    /// queue identifier (high 16).
    pub dw2: u32,
    /// Dword 3 — command identifier (low 16), phase bit (bit 16),
    /// status field (bits 31..17).
    pub dw3: u32,
}

impl CompletionQueueEntry {
    /// Returns the command identifier the queue completion corresponds to.
    #[must_use]
    pub const fn command_id(self) -> u16 {
        (self.dw3 & 0xFFFF) as u16
    }
    /// Returns the phase bit observed in this completion (bit 16).
    #[must_use]
    pub const fn phase(self) -> u8 {
        ((self.dw3 >> 16) & 1) as u8
    }
    /// Returns the 15-bit status field (status code + status code type +
    /// "do not retry"). Zero means success.
    #[must_use]
    pub const fn status_field(self) -> u16 {
        ((self.dw3 >> 17) & 0x7FFF) as u16
    }
    /// Returns the submission queue head pointer reported by the controller.
    #[must_use]
    pub const fn sq_head(self) -> u16 {
        (self.dw2 & 0xFFFF) as u16
    }

    /// Decodes the status field into an [`NvmeStatus`] (SC in bits 7..0,
    /// SCT in bits 10..8, DNR in bit 14 of the 15-bit field).
    #[must_use]
    pub const fn status(self) -> NvmeStatus {
        let field = self.status_field();
        NvmeStatus {
            sct: ((field >> 8) & 0x7) as u8,
            sc: (field & 0xFF) as u8,
            dnr: (field >> 14) & 1 != 0,
        }
    }
}

/// Namespace geometry decoded from the `Identify Namespace` data structure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NamespaceGeometry {
    /// Namespace size in logical blocks (NSZE).
    pub block_count: u64,
    /// Bytes per logical block, from the formatted LBA format's LBADS.
    pub block_size: usize,
}

/// Parses the fields the block layer needs out of an `Identify Namespace`
/// (CNS 0x00) data buffer. `data` must hold at least the first 192 bytes
/// (NSZE through the 16-entry LBA format table). Returns `None` when the
/// buffer is too short or the formatted LBA size is implausible.
#[must_use]
pub fn parse_identify_namespace(data: &[u8]) -> Option<NamespaceGeometry> {
    if data.len() < 192 {
        return None;
    }
    let block_count = u64::from_le_bytes(data[0..8].try_into().ok()?);
    let flbas_index = (data[26] & 0x0F) as usize;
    let lbaf_offset = 128 + flbas_index * 4;
    let lbaf = u32::from_le_bytes(data[lbaf_offset..lbaf_offset + 4].try_into().ok()?);
    let lbads = (lbaf >> 16) & 0xFF;
    if !(9..=16).contains(&lbads) {
        return None;
    }
    Some(NamespaceGeometry {
        block_count,
        block_size: 1usize << lbads,
    })
}

/// Thin reader over an NVMe controller's MMIO register bank.
///
/// The caller supplies a raw pointer to the start of the controller's
/// `BAR0` (already mapped into the kernel address space with UC caching).
/// Methods read registers via volatile accesses; no decode of submission
/// queue / completion queue state is performed here.
#[derive(Clone, Copy, Debug)]
pub struct ControllerRegisters {
    base: *mut u8,
}

// SAFETY: `ControllerRegisters` is a raw pointer to MMIO; the caller
// guarantees the pointer is valid kernel virtual memory for the lifetime
// of the controller. The pointer itself does not have `Send`/`Sync`
// hazards beyond what the device exposes.
unsafe impl Send for ControllerRegisters {}
unsafe impl Sync for ControllerRegisters {}

impl ControllerRegisters {
    /// Wraps a pointer to the NVMe controller's mapped MMIO base.
    ///
    /// # Safety
    ///
    /// `base` must point at the first byte of a kernel virtual range that
    /// covers at least the first 0x1008 bytes of the NVMe BAR0 mapping
    /// (controller registers + admin doorbells) and that mapping must
    /// remain installed for the lifetime of this value.
    #[must_use]
    pub const unsafe fn new(base: *mut u8) -> Self {
        Self { base }
    }

    fn read_u32(self, offset: usize) -> u32 {
        unsafe {
            // SAFETY: caller guarantees `base` covers the register bank.
            core::ptr::read_volatile(self.base.add(offset).cast::<u32>())
        }
    }

    fn write_u32(self, offset: usize, value: u32) {
        unsafe {
            // SAFETY: same.
            core::ptr::write_volatile(self.base.add(offset).cast::<u32>(), value);
        }
    }

    /// Reads the controller's `CAP` register (offset 0x00).
    #[must_use]
    pub fn cap(self) -> Cap {
        let low = self.read_u32(0x00);
        let high = self.read_u32(0x04);
        Cap((u64::from(high) << 32) | u64::from(low))
    }

    /// Reads the controller's `VS` register (offset 0x08).
    #[must_use]
    pub fn vs(self) -> Vs {
        Vs(self.read_u32(0x08))
    }

    /// Reads the controller's `CC` register (offset 0x14).
    #[must_use]
    pub fn cc(self) -> u32 {
        self.read_u32(0x14)
    }

    /// Writes the controller's `CC` register (offset 0x14).
    pub fn set_cc(self, value: u32) {
        self.write_u32(0x14, value);
    }

    /// Reads the controller's `CSTS` register (offset 0x1C).
    #[must_use]
    pub fn csts(self) -> Csts {
        Csts(self.read_u32(0x1C))
    }

    /// Writes the admin queue attributes register (offset 0x24).
    ///
    /// `sq_entries` and `cq_entries` are the true entry counts. Each
    /// must be in the range 2..=4096; this method encodes them as the
    /// zero-based field values the controller expects.
    pub fn set_aqa(self, sq_entries: u16, cq_entries: u16) {
        let aqa = (u32::from(cq_entries - 1) << 16) | u32::from(sq_entries - 1);
        self.write_u32(0x24, aqa);
    }

    /// Writes the admin submission queue base address register
    /// (offsets 0x28 low / 0x2C high). `phys` must be 4 KiB aligned.
    pub fn set_asq(self, phys: u64) {
        self.write_u32(0x28, phys as u32);
        self.write_u32(0x2C, (phys >> 32) as u32);
    }

    /// Writes the admin completion queue base address register
    /// (offsets 0x30 low / 0x34 high). `phys` must be 4 KiB aligned.
    pub fn set_acq(self, phys: u64) {
        self.write_u32(0x30, phys as u32);
        self.write_u32(0x34, (phys >> 32) as u32);
    }

    /// Rings the submission queue tail doorbell for `qid` (admin = 0).
    ///
    /// Assumes the controller advertises `CAP.DSTRD = 0` (4-byte stride).
    /// Callers using a controller with a wider stride must compute the
    /// doorbell offset themselves (or use the `_strided` variants).
    pub fn ring_sq_tail_doorbell(self, qid: u16, tail: u16) {
        self.ring_sq_tail_doorbell_strided(qid, tail, 0);
    }

    /// Rings the completion queue head doorbell for `qid` (admin = 0).
    pub fn ring_cq_head_doorbell(self, qid: u16, head: u16) {
        self.ring_cq_head_doorbell_strided(qid, head, 0);
    }

    /// Rings the SQ tail doorbell with an explicit `CAP.DSTRD` value
    /// (doorbell stride = `4 << dstrd` bytes).
    pub fn ring_sq_tail_doorbell_strided(self, qid: u16, tail: u16, dstrd: u8) {
        let stride = 4usize << dstrd;
        let offset = 0x1000 + (2 * usize::from(qid)) * stride;
        self.write_u32(offset, u32::from(tail));
    }

    /// Rings the CQ head doorbell with an explicit `CAP.DSTRD` value.
    pub fn ring_cq_head_doorbell_strided(self, qid: u16, head: u16, dstrd: u8) {
        let stride = 4usize << dstrd;
        let offset = 0x1000 + (2 * usize::from(qid) + 1) * stride;
        self.write_u32(offset, u32::from(head));
    }
}

/// A live submission/completion ring pair: the NVMe data path.
///
/// This is the piece the inflight model alone could not provide: `submit`
/// takes a real command descriptor, assigns it a ring-owned CID, writes the
/// SQE into the submission ring, and rings the tail doorbell;
/// `process_completions` consumes phase-valid CQEs and resolves the matching
/// [`NvmeIoFuture`]s. `N` is both the ring depth and the inflight capacity,
/// so a free CID always implies a free ring slot (the controller cannot have
/// more than `N - 1` commands outstanding because `submit` refuses when no
/// CID is free).
///
/// The ring memory is caller-owned: two zeroed, physically contiguous,
/// device-visible buffers (one page suffices for `N <= 64` SQEs / `N <= 256`
/// CQEs). Like [`NvmeIoFuture`], a ring is core-local (`!Send`).
pub struct QueueRing<const N: usize> {
    regs: ControllerRegisters,
    qid: u16,
    dstrd: u8,
    sq: NonNull<SubmissionQueueEntry>,
    cq: NonNull<CompletionQueueEntry>,
    sq_tail: u16,
    cq_head: u16,
    cq_phase: u8,
    inflight: InflightMap<N>,
    failed: Option<NvmeError>,
}

impl<const N: usize> QueueRing<N> {
    /// Binds a ring pair to its queue id and doorbells.
    ///
    /// # Safety
    ///
    /// `sq` and `cq` must point at zeroed, device-visible buffers holding at
    /// least `N` submission / completion entries respectively, registered
    /// with the controller for `qid` (via AQA/ASQ/ACQ for the admin queue or
    /// Create I/O Queue commands otherwise), and must outlive the ring.
    #[must_use]
    pub unsafe fn new(
        regs: ControllerRegisters,
        qid: u16,
        dstrd: u8,
        sq: NonNull<SubmissionQueueEntry>,
        cq: NonNull<CompletionQueueEntry>,
    ) -> Self {
        Self {
            regs,
            qid,
            dstrd,
            sq,
            cq,
            sq_tail: 0,
            cq_head: 0,
            // Zeroed CQ memory means the first valid completion carries
            // phase 1.
            cq_phase: 1,
            inflight: InflightMap::new(),
            failed: None,
        }
    }

    /// Number of free command slots.
    #[must_use]
    pub fn available(&self) -> usize {
        self.inflight.available()
    }

    /// Submits `command` to the ring: assigns a CID (overriding CDW0 bits
    /// 31..16), writes the SQE, and rings the tail doorbell. The returned
    /// future resolves when [`Self::process_completions`] sees the CQE.
    pub fn submit(
        &mut self,
        command: SubmissionQueueEntry,
    ) -> Result<(u16, NvmeIoFuture<N>), NvmeError> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let (cid, future) = self.inflight.register()?;
        let sqe = command.with_cid(cid);
        // SAFETY: `new` guarantees `sq` holds N entries; sq_tail < N.
        unsafe {
            self.sq
                .as_ptr()
                .add(usize::from(self.sq_tail))
                .write_volatile(sqe);
        }
        // Publish the SQE before the doorbell makes it visible.
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        self.sq_tail = (self.sq_tail + 1) % (N as u16);
        self.regs
            .ring_sq_tail_doorbell_strided(self.qid, self.sq_tail, self.dstrd);
        Ok((cid, future))
    }

    /// Drains every phase-valid completion from the CQ, resolving the
    /// matching futures, and rings the CQ head doorbell once. Returns the
    /// number of completions processed.
    pub fn process_completions(&mut self) -> usize {
        let mut processed = 0usize;
        loop {
            // SAFETY: `new` guarantees `cq` holds N entries; cq_head < N.
            let cqe = unsafe {
                self.cq
                    .as_ptr()
                    .add(usize::from(self.cq_head))
                    .read_volatile()
            };
            if cqe.phase() != self.cq_phase {
                break;
            }
            core::sync::atomic::fence(core::sync::atomic::Ordering::Acquire);
            self.inflight.complete(NvmeCompletion {
                cid: cqe.command_id(),
                status: cqe.status(),
            });
            self.cq_head += 1;
            if usize::from(self.cq_head) == N {
                self.cq_head = 0;
                self.cq_phase ^= 1;
            }
            processed += 1;
        }
        if processed > 0 {
            self.regs
                .ring_cq_head_doorbell_strided(self.qid, self.cq_head, self.dstrd);
        }
        processed
    }

    /// Fails the ring and every pending command (device removed, capability
    /// revoked, ...).
    pub fn fail(&mut self, error: NvmeError) {
        self.failed = Some(error);
        self.inflight.fail_all(error);
    }
}

#[cfg(test)]
mod tests {
    use super::{Cap, InflightMap, NvmeCompletion, NvmeError, NvmeIoFuture, NvmeQueuePair, Vs};
    use core::future::Future;
    use core::pin::pin;
    use core::task::{Context, Poll, Waker};
    use static_assertions::assert_not_impl_any;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::Wake;

    assert_not_impl_any!(NvmeIoFuture<64>: Send, Sync);

    #[derive(Debug)]
    struct WakeCounter {
        hits: AtomicUsize,
    }

    impl Wake for WakeCounter {
        fn wake(self: Arc<Self>) {
            self.hits.fetch_add(1, Ordering::SeqCst);
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.hits.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn counting_waker() -> (Arc<WakeCounter>, Waker) {
        let counter = Arc::new(WakeCounter {
            hits: AtomicUsize::new(0),
        });
        let waker = Waker::from(counter.clone());
        (counter, waker)
    }

    #[test]
    fn completed_but_unpolled_commands_do_not_release_their_cid() {
        let mut queue = NvmeQueuePair::<1>::new();
        let (cid, future) = queue.submit().expect("first submission should fit");
        queue.complete(NvmeCompletion::success(cid));

        assert!(matches!(queue.submit(), Err(NvmeError::QueueFull)));

        drop(future);
        let (reused_cid, _) = queue.submit().expect("cid should recycle after drop");
        assert_eq!(reused_cid, cid);
    }

    #[test]
    fn fail_all_marks_even_unpolled_futures_as_ready_with_an_error() {
        let mut inflight = InflightMap::<2>::new();
        let (_, mut future) = inflight.register().expect("slot available");
        inflight.fail_all(NvmeError::DeviceRemoved);

        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);
        assert_eq!(
            Future::poll(pin!(&mut future).as_mut(), &mut cx),
            Poll::Ready(Err(NvmeError::DeviceRemoved))
        );
    }

    #[test]
    fn fail_all_wakes_registered_waiters() {
        let mut inflight = InflightMap::<2>::new();
        let (_, mut future) = inflight.register().expect("slot available");
        let (counter, waker) = counting_waker();
        let mut cx = Context::from_waker(&waker);

        assert_eq!(
            Future::poll(pin!(&mut future).as_mut(), &mut cx),
            Poll::Pending
        );

        inflight.fail_all(NvmeError::CapabilityRevoked);

        assert_eq!(counter.hits.load(Ordering::SeqCst), 1);
        assert_eq!(
            Future::poll(pin!(&mut future).as_mut(), &mut cx),
            Poll::Ready(Err(NvmeError::CapabilityRevoked))
        );
    }

    #[test]
    fn cap_decodes_known_fields() {
        // MQES = 0x00FF (0-based; queue depth = 256)
        // DSTRD = 0 (4-byte doorbells)
        // MPSMIN = 0 (4 KiB), MPSMAX = 0 (4 KiB)
        let cap = Cap(0x0000_0000_0000_00FF);
        assert_eq!(cap.mqes(), 0x00FF);
        assert_eq!(cap.dstrd(), 0);
        assert_eq!(cap.mpsmin(), 0);
        assert_eq!(cap.mpsmax(), 0);

        // DSTRD nibble at bits 35..32: 0x3 → byte 4 high nibble.
        let cap = Cap(0x0003_0003_0000_03FF);
        assert_eq!(cap.dstrd(), 3);
    }

    #[test]
    fn queue_ring_moves_a_command_end_to_end() {
        use super::{
            CompletionQueueEntry, ControllerRegisters, QueueRing, SubmissionQueueEntry,
        };
        use core::ptr::NonNull;

        // Fake register bank standing in for BAR0 (doorbell writes land in
        // plain memory) + ring memory.
        let mut bar = std::vec![0u8; 0x1100];
        let mut sq = [SubmissionQueueEntry::default(); 4];
        let mut cq = [CompletionQueueEntry::default(); 4];
        // SAFETY (test): bar covers the register bank; rings hold 4 entries.
        let regs = unsafe { ControllerRegisters::new(bar.as_mut_ptr()) };
        let mut ring: QueueRing<4> = unsafe {
            QueueRing::new(
                regs,
                1,
                0,
                NonNull::new(sq.as_mut_ptr()).unwrap(),
                NonNull::new(cq.as_mut_ptr()).unwrap(),
            )
        };

        // Submit: SQE written with the ring-assigned CID, doorbell rung.
        let cmd = SubmissionQueueEntry::nvm_read(1, 7, 0, 0xD000, 0xFFFF);
        let (cid, mut future) = ring.submit(cmd).expect("ring has room");
        assert_eq!((sq[0].cdw0 >> 16) as u16, cid);
        assert_eq!(sq[0].cdw0 & 0xFF, 0x02); // opcode survives the CID patch
        assert_eq!(sq[0].cdw10, 7); // SLBA low
        let sq_doorbell = u32::from_le_bytes(bar[0x1008..0x100C].try_into().unwrap());
        assert_eq!(sq_doorbell, 1); // qid 1 SQ tail doorbell

        // No completion yet.
        assert_eq!(ring.process_completions(), 0);

        // Hand-craft the controller's CQE: phase 1, success, our CID.
        cq[0].dw3 = u32::from(cid) | (1 << 16);
        assert_eq!(ring.process_completions(), 1);
        let cq_doorbell = u32::from_le_bytes(bar[0x100C..0x1010].try_into().unwrap());
        assert_eq!(cq_doorbell, 1); // qid 1 CQ head doorbell

        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);
        match Future::poll(pin!(&mut future).as_mut(), &mut cx) {
            Poll::Ready(Ok(completion)) => assert!(completion.succeeded()),
            other => panic!("expected success, got {other:?}"),
        }
    }

    #[test]
    fn identify_namespace_parses_geometry() {
        let mut data = std::vec![0u8; 4096];
        data[0..8].copy_from_slice(&32768u64.to_le_bytes()); // NSZE
        data[26] = 0x01; // FLBAS: format index 1
        // LBA format table entry 1: LBADS = 12 (4096-byte blocks).
        data[132..136].copy_from_slice(&(12u32 << 16).to_le_bytes());
        let geometry = super::parse_identify_namespace(&data).expect("parses");
        assert_eq!(geometry.block_count, 32768);
        assert_eq!(geometry.block_size, 4096);

        // Implausible LBADS is rejected.
        data[132..136].copy_from_slice(&(3u32 << 16).to_le_bytes());
        assert!(super::parse_identify_namespace(&data).is_none());
        // Truncated buffers are rejected.
        assert!(super::parse_identify_namespace(&data[..100]).is_none());
    }

    #[test]
    fn cqe_status_decodes_fields() {
        use super::CompletionQueueEntry;
        // Status field: DNR=1, SCT=2, SC=0x81 -> bits (14,9..8,7..0) of the
        // 15-bit field at dw3[31:17]; phase 1 at bit 16.
        let field: u32 = (1 << 14) | (2 << 8) | 0x81;
        let cqe = CompletionQueueEntry {
            dw0: 0,
            dw1: 0,
            dw2: 0,
            dw3: (field << 17) | (1 << 16) | 0x002A,
        };
        let status = cqe.status();
        assert_eq!(status.sct, 2);
        assert_eq!(status.sc, 0x81);
        assert!(status.dnr);
        assert_eq!(cqe.command_id(), 0x2A);
    }

    #[test]
    fn vs_decodes_major_minor_tertiary() {
        // NVMe 1.4.0 reports VS = 0x0001_0400.
        let vs = Vs(0x0001_0400);
        assert_eq!(vs.major(), 1);
        assert_eq!(vs.minor(), 4);
        assert_eq!(vs.tertiary(), 0);
        // NVMe 2.0.0 reports VS = 0x0002_0000.
        let vs = Vs(0x0002_0000);
        assert_eq!(vs.major(), 2);
        assert_eq!(vs.minor(), 0);
    }
}
