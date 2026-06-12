//! NVMe bring-up, admin commands, and block I/O on riscv64, driven by the
//! shared `feox-nvme` crate (milestones 6b-6d; re-based in M22).
//!
//! - 6b: assign the BAR from the PCIe MMIO window, enable decoding/bus-master,
//!   and run the reset/enable handshake to `CSTS.RDY=1`.
//! - 6c: Identify Controller over the admin queue (full doorbell -> DMA ->
//!   completion-phase poll cycle).
//! - 6d: Identify Namespace, create an I/O queue pair, and write a known
//!   pattern to LBA 0 (with an NVM Flush barrier) then read it back and
//!   verify — proving real block I/O.
//!
//! The data path is `feox_nvme::QueueRing`: command descriptors in, SQE write
//! + tail doorbell, phase-bit CQE processing resolving `NvmeIoFuture`s — the
//! same seam `rfs-feox` builds its `BlockDevice` on. This boot self-test is
//! the crate's hardware validation: every command here goes through ring
//! submit + future poll, not bespoke queue logic. Boot-time commands have one
//! outstanding entry, so completions are spun for synchronously; the async
//! shape is what RFS consumes.

use core::future::Future;
use core::pin::Pin;
use core::ptr::{NonNull, read_volatile, write_volatile};
use core::task::{Context, Poll, Waker};

use feox_nvme::{
    ControllerRegisters, NamespaceGeometry, NvmeError, QueueRing, SubmissionQueueEntry,
    parse_identify_namespace,
};

use super::{frame, pci};

const CC_ENABLE: u32 = 1 << 0;
const CC_IOSQES_SHIFT: u32 = 16;
const CC_IOCQES_SHIFT: u32 = 20;
const NVME_SQE_LOG2: u32 = 6;
const NVME_CQE_LOG2: u32 = 4;

const ADMIN_QUEUE_DEPTH: usize = 64;
const IO_QUEUE_DEPTH: usize = 8;
const IO_QUEUE_ID: u16 = 1;
const NSID: u32 = 1;

const SPIN_LIMIT: u32 = 5_000_000;

/// Default LBA size assumed until Identify Namespace reports otherwise.
const DEFAULT_LBA_SIZE: usize = 512;

fn zero_frame(frame: usize) {
    for i in 0..512 {
        // SAFETY: fresh identity-mapped allocator frame.
        unsafe { write_volatile((frame as *mut u64).add(i), 0) };
    }
}

/// Allocates and zeroes one frame for ring/DMA use.
fn dma_frame(what: &str) -> Option<usize> {
    let Some(addr) = frame::alloc() else {
        crate::kprintln!("[feox] nvme: out of frames for {}", what);
        return None;
    };
    zero_frame(addr);
    Some(addr)
}

/// Submits `command` on `ring` and spins (process completions + poll the
/// future) until it resolves, logging failures/timeouts.
fn execute<const N: usize>(
    ring: &mut QueueRing<N>,
    command: SubmissionQueueEntry,
    what: &str,
) -> bool {
    let (_cid, mut future) = match ring.submit(command) {
        Ok(pair) => pair,
        Err(error) => {
            crate::kprintln!("[feox] nvme: {} submit failed ({:?})", what, error);
            return false;
        }
    };
    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    let mut spins = 0u32;
    loop {
        ring.process_completions();
        match Future::poll(Pin::new(&mut future), &mut cx) {
            Poll::Ready(Ok(_)) => return true,
            Poll::Ready(Err(NvmeError::CommandFailed(status))) => {
                crate::kprintln!(
                    "[feox] nvme: {} failed (sct={:#x} sc={:#x} dnr={})",
                    what,
                    status.sct,
                    status.sc,
                    status.dnr
                );
                return false;
            }
            Poll::Ready(Err(error)) => {
                crate::kprintln!("[feox] nvme: {} failed ({:?})", what, error);
                return false;
            }
            Poll::Pending => {}
        }
        spins += 1;
        if spins >= SPIN_LIMIT {
            crate::kprintln!("[feox] nvme: {} timed out", what);
            return false;
        }
        core::hint::spin_loop();
    }
}

/// An I/O queue ring of this driver's depth (what RFS's adapter consumes).
pub type IoRing = QueueRing<IO_QUEUE_DEPTH>;

/// A brought-up NVMe controller with its admin ring (and, after setup, an
/// I/O ring pair), all `feox-nvme` machinery.
pub struct Controller {
    regs: ControllerRegisters,
    dstrd: u8,
    admin: QueueRing<ADMIN_QUEUE_DEPTH>,
    io: Option<IoRing>,
    geometry: NamespaceGeometry,
}

impl Controller {
    /// Identify Controller (6c): prints the model/serial DMA'd back.
    #[must_use]
    pub fn identify_controller(&mut self) -> bool {
        let Some(buffer) = dma_frame("identify buffer") else {
            return false;
        };
        let command = SubmissionQueueEntry::identify_controller(buffer as u64, 0);
        if !execute(&mut self.admin, command, "identify-controller") {
            return false;
        }
        let serial = ascii_field(buffer + 4, 20);
        let model = ascii_field(buffer + 24, 40);
        crate::kprintln!("[feox] nvme: identify ok — model='{}' serial='{}'", model, serial);
        crate::kprintln!("[feox] milestone 6c: NVMe admin command round-trip complete.");
        true
    }

    /// Identify Namespace (6d): reads the namespace size and LBA data size,
    /// decoded by the crate's geometry parser.
    #[must_use]
    pub fn identify_namespace(&mut self) -> bool {
        let Some(buffer) = dma_frame("namespace identify") else {
            return false;
        };
        let command = SubmissionQueueEntry::identify_namespace(NSID, buffer as u64, 0);
        if !execute(&mut self.admin, command, "identify-namespace") {
            return false;
        }
        // SAFETY: `buffer` is the identity-mapped 4 KiB frame the controller
        // just DMA'd the Identify Namespace structure into.
        let data = unsafe { core::slice::from_raw_parts(buffer as *const u8, 4096) };
        let Some(geometry) = parse_identify_namespace(data) else {
            crate::kprintln!("[feox] nvme: implausible Identify Namespace data");
            return false;
        };
        self.geometry = geometry;
        crate::kprintln!(
            "[feox] nvme: namespace {} — {} blocks, {}-byte LBA",
            NSID,
            geometry.block_count,
            geometry.block_size
        );
        true
    }

    /// Creates one I/O completion + submission queue pair with id `qid` and
    /// returns its ring.
    fn create_io_ring(&mut self, qid: u16) -> Option<IoRing> {
        let (cq, sq) = (dma_frame("I/O CQ")?, dma_frame("I/O SQ")?);

        let cq_cmd =
            SubmissionQueueEntry::create_io_completion_queue(qid, IO_QUEUE_DEPTH as u16, cq as u64, 0);
        if !execute(&mut self.admin, cq_cmd, "create-io-cq") {
            return None;
        }
        let sq_cmd = SubmissionQueueEntry::create_io_submission_queue(
            qid,
            IO_QUEUE_DEPTH as u16,
            qid,
            sq as u64,
            0,
        );
        if !execute(&mut self.admin, sq_cmd, "create-io-sq") {
            return None;
        }

        // SAFETY: both rings are zeroed, device-registered (the two admin
        // commands above), identity-mapped frames that live for the kernel's
        // lifetime.
        let ring = unsafe {
            QueueRing::new(
                self.regs,
                qid,
                self.dstrd,
                NonNull::new_unchecked(sq as *mut SubmissionQueueEntry),
                NonNull::new_unchecked(cq as *mut feox_nvme::CompletionQueueEntry),
            )
        };
        crate::kprintln!(
            "[feox] nvme: I/O queue {} ready (SQ={:#x} CQ={:#x}, depth {})",
            qid,
            sq,
            cq,
            IO_QUEUE_DEPTH
        );
        Some(ring)
    }

    /// Creates the primary I/O queue pair (queue id 1).
    #[must_use]
    pub fn create_io_queues(&mut self) -> bool {
        match self.create_io_ring(IO_QUEUE_ID) {
            Some(ring) => {
                self.io = Some(ring);
                true
            }
            None => false,
        }
    }

    /// Creates an additional I/O queue pair (e.g. for a second block-device
    /// handle, like the RFS remount proof).
    pub fn create_extra_io_ring(&mut self, qid: u16) -> Option<IoRing> {
        self.create_io_ring(qid)
    }

    /// The namespace geometry from Identify Namespace.
    #[must_use]
    pub fn geometry(&self) -> NamespaceGeometry {
        self.geometry
    }

    /// Consumes the controller, yielding the primary I/O ring + geometry —
    /// the parts an `rfs-feox` block device is built from. (The admin ring
    /// and registers are dropped; queues stay live at the controller until
    /// reset, which never happens during bring-up.)
    #[must_use]
    pub fn into_io_ring(self) -> Option<(IoRing, NamespaceGeometry)> {
        Some((self.io?, self.geometry))
    }

    /// Writes a known pattern to LBA 0, issues an NVM Flush barrier, then
    /// reads it back into a separate buffer and verifies they match — proof
    /// of real block I/O through the crate's ring path.
    #[must_use]
    pub fn block_io_selftest(&mut self) -> bool {
        let Some(io) = self.io.as_mut() else {
            crate::kprintln!("[feox] nvme: no I/O queue for block test");
            return false;
        };
        // The DMA buffers are one 4 KiB frame each with PRP2 = 0, so a single
        // command can move at most one page. A namespace formatted with an
        // LBA larger than 4 KiB (parse_identify_namespace accepts up to 64
        // KiB) would have the controller DMA past the frame — skip the test
        // rather than corrupt memory. (QEMU's default 512 never trips this.)
        if self.geometry.block_size > 4096 {
            crate::kprintln!(
                "[feox] nvme: block test skipped ({}-byte LBA exceeds one DMA page)",
                self.geometry.block_size
            );
            return true;
        }
        let (Some(write_buf), Some(read_buf)) =
            (dma_frame("write buffer"), dma_frame("read buffer"))
        else {
            return false;
        };
        let len = self.geometry.block_size.min(4096);
        fill_pattern(write_buf, len);

        let write = SubmissionQueueEntry::nvm_write(NSID, 0, 0, write_buf as u64, 0);
        if !execute(io, write, "write") {
            return false;
        }
        let flush = SubmissionQueueEntry::nvm_flush(NSID, 0);
        if !execute(io, flush, "flush") {
            return false;
        }
        let read = SubmissionQueueEntry::nvm_read(NSID, 0, 0, read_buf as u64, 0);
        if !execute(io, read, "read") {
            return false;
        }

        let matched = buffers_equal(write_buf, read_buf, len);
        // SAFETY: identity-mapped read buffer.
        let head = unsafe { read_volatile(read_buf as *const u32) };
        crate::kprintln!(
            "[feox] nvme: block 0 write+flush+read back {} bytes, first4={:#010x}, match={}",
            len,
            head,
            matched
        );
        if matched {
            crate::kprintln!("[feox] milestone 6d: NVMe block I/O verified.");
        } else {
            crate::kprintln!("[feox] nvme: block I/O MISMATCH");
        }
        matched
    }
}

/// Brings the controller to ready (6b) and returns a handle with its admin
/// ring. Logs and returns `None` on fault.
#[must_use]
pub fn init(device: &pci::PciDevice) -> Option<Controller> {
    let size = pci::size_bar64(device, 0);
    let base = pci::MMIO_BASE;
    pci::set_bar64(device, 0, base as u64);
    pci::enable_memory_and_bus_master(device);

    // SAFETY: BAR0 was just assigned to the mapped QEMU PCIe MMIO window;
    // the mapping covers registers + doorbells and lives forever.
    let regs = unsafe { ControllerRegisters::new(base as *mut u8) };
    let cap = regs.cap();
    let vs = regs.vs();
    let dstrd = cap.dstrd();
    crate::kprintln!(
        "[feox] nvme: BAR0={:#x} (size={:#x}) CAP={:#018x} version={}.{}.{} dstrd={}",
        base,
        size,
        cap.0,
        vs.major(),
        vs.minor(),
        vs.tertiary(),
        dstrd
    );

    regs.set_cc(regs.cc() & !CC_ENABLE);
    if !spin_until(regs, |csts| !csts.ready()) {
        crate::kprintln!("[feox] nvme: timeout waiting for reset (CSTS.RDY=0)");
        return None;
    }

    let admin_sq = dma_frame("admin SQ")?;
    let admin_cq = dma_frame("admin CQ")?;
    regs.set_aqa(ADMIN_QUEUE_DEPTH as u16, ADMIN_QUEUE_DEPTH as u16);
    regs.set_asq(admin_sq as u64);
    regs.set_acq(admin_cq as u64);

    let cc_value =
        (NVME_SQE_LOG2 << CC_IOSQES_SHIFT) | (NVME_CQE_LOG2 << CC_IOCQES_SHIFT) | CC_ENABLE;
    regs.set_cc(cc_value);
    if !spin_until(regs, feox_nvme::Csts::ready) {
        crate::kprintln!(
            "[feox] nvme: timeout waiting for enable (CSTS={:#x})",
            regs.csts().0
        );
        return None;
    }

    crate::kprintln!(
        "[feox] nvme: admin queues SQ={:#x} CQ={:#x} (depth {}); controller ready (CSTS.RDY=1)",
        admin_sq,
        admin_cq,
        ADMIN_QUEUE_DEPTH
    );
    crate::kprintln!("[feox] milestone 6b: NVMe controller enabled.");

    // SAFETY: zeroed, identity-mapped admin rings registered via AQA/ASQ/ACQ
    // above; they live for the kernel's lifetime.
    let admin = unsafe {
        QueueRing::new(
            regs,
            0,
            dstrd,
            NonNull::new_unchecked(admin_sq as *mut SubmissionQueueEntry),
            NonNull::new_unchecked(admin_cq as *mut feox_nvme::CompletionQueueEntry),
        )
    };
    Some(Controller {
        regs,
        dstrd,
        admin,
        io: None,
        geometry: NamespaceGeometry {
            block_count: 0,
            block_size: DEFAULT_LBA_SIZE,
        },
    })
}

fn spin_until(regs: ControllerRegisters, cond: impl Fn(feox_nvme::Csts) -> bool) -> bool {
    let mut spins = 0u32;
    while !cond(regs.csts()) {
        spins += 1;
        if spins >= SPIN_LIMIT {
            return false;
        }
        core::hint::spin_loop();
    }
    true
}

/// Fills the first `len` bytes of `frame` with a recognizable, position-
/// dependent pattern (so a mis-DMA shows up as a mismatch).
fn fill_pattern(frame: usize, len: usize) {
    const SIG: &[u8] = b"FEOX-riscv64-NVMe-6d-block-io;";
    for i in 0..len {
        let byte = if i < SIG.len() {
            SIG[i]
        } else {
            // Position-dependent, non-trivial, never all-zero.
            ((i * 7 + 0x31) & 0xFF) as u8
        };
        // SAFETY: `frame` is an identity-mapped allocator frame; `i < len <= 4096`.
        unsafe { write_volatile((frame as *mut u8).add(i), byte) };
    }
}

/// Returns true if the first `len` bytes of the two frames are equal.
fn buffers_equal(a: usize, b: usize, len: usize) -> bool {
    for i in 0..len {
        // SAFETY: both are identity-mapped frames; `i < len <= 4096`.
        let (x, y) = unsafe {
            (
                read_volatile((a as *const u8).add(i)),
                read_volatile((b as *const u8).add(i)),
            )
        };
        if x != y {
            return false;
        }
    }
    true
}

/// Renders a fixed-width space-padded ASCII identify field as a trimmed string.
fn ascii_field(addr: usize, len: usize) -> &'static str {
    // SAFETY: within the identity-mapped 4 KiB identify buffer.
    let bytes = unsafe { core::slice::from_raw_parts(addr as *const u8, len) };
    let end = bytes
        .iter()
        .rposition(|&b| b != b' ' && b != 0)
        .map_or(0, |i| i + 1);
    core::str::from_utf8(&bytes[..end]).unwrap_or("<non-utf8>")
}
