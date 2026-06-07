//! NVMe bring-up, admin commands, and block I/O on riscv64 (milestones 6b-6d).
//!
//! - 6b: assign the BAR from the PCIe MMIO window, enable decoding/bus-master,
//!   and run the reset/enable handshake to `CSTS.RDY=1`.
//! - 6c: Identify Controller over the admin queue (full doorbell -> DMA ->
//!   completion-phase poll cycle).
//! - 6d: Identify Namespace, create an I/O queue pair, and write a known
//!   pattern to LBA 0 then read it back and verify — proving real block I/O.
//!
//! A [`Queue`] abstracts a submission/completion pair so admin and I/O commands
//! share one submit-and-poll path. Still hand-rolled MMIO/DMA (no `feox-nvme`
//! yet); commands have at most one outstanding entry, so a fixed CID of 0 is
//! fine and completions are polled synchronously.

use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{Ordering, fence};

use super::{frame, pci};

// Controller registers within BAR0.
const REG_CAP: usize = 0x00;
const REG_VS: usize = 0x08;
const REG_CC: usize = 0x14;
const REG_CSTS: usize = 0x1C;
const REG_AQA: usize = 0x24;
const REG_ASQ: usize = 0x28;
const REG_ACQ: usize = 0x30;
const REG_DOORBELL_BASE: usize = 0x1000;

const CC_ENABLE: u32 = 1 << 0;
const CSTS_READY: u32 = 1 << 0;
const CC_IOSQES_SHIFT: u32 = 16;
const CC_IOCQES_SHIFT: u32 = 20;
const NVME_SQE_LOG2: u32 = 6;
const NVME_CQE_LOG2: u32 = 4;
const SQE_BYTES: usize = 1 << NVME_SQE_LOG2;
const CQE_BYTES: usize = 1 << NVME_CQE_LOG2;

const ADMIN_QUEUE_DEPTH: u16 = 64;
const IO_QUEUE_DEPTH: u16 = 8;
const IO_QUEUE_ID: u16 = 1;
const NSID: u32 = 1;

// Opcodes.
const ADMIN_OP_CREATE_IO_SQ: u32 = 0x01;
const ADMIN_OP_CREATE_IO_CQ: u32 = 0x05;
const ADMIN_OP_IDENTIFY: u32 = 0x06;
const IO_OP_WRITE: u32 = 0x01;
const IO_OP_READ: u32 = 0x02;

const CNS_IDENTIFY_NAMESPACE: u32 = 0x00;
const CNS_IDENTIFY_CONTROLLER: u32 = 0x01;

const SPIN_LIMIT: u32 = 5_000_000;

/// Default LBA size assumed until Identify Namespace reports otherwise.
const DEFAULT_LBA_SIZE: usize = 512;

fn read32(addr: usize) -> u32 {
    // SAFETY: mapped device-MMIO or identity-mapped frame address.
    unsafe { read_volatile(addr as *const u32) }
}
fn write32(addr: usize, value: u32) {
    // SAFETY: as above.
    unsafe { write_volatile(addr as *mut u32, value) }
}
fn read64(addr: usize) -> u64 {
    // SAFETY: as above.
    unsafe { read_volatile(addr as *const u64) }
}
fn write64(addr: usize, value: u64) {
    // SAFETY: as above.
    unsafe { write_volatile(addr as *mut u64, value) }
}
fn read_u8(addr: usize) -> u8 {
    // SAFETY: as above.
    unsafe { read_volatile(addr as *const u8) }
}

fn zero_frame(frame: usize) {
    for i in 0..512 {
        // SAFETY: fresh identity-mapped allocator frame.
        unsafe { write_volatile((frame as *mut u64).add(i), 0) };
    }
}

/// A submission/completion queue pair with its doorbells and phase state.
struct Queue {
    sq: usize,
    cq: usize,
    depth: u16,
    sq_tail: u16,
    cq_head: u16,
    cq_phase: u32,
    sq_doorbell: usize,
    cq_doorbell: usize,
}

impl Queue {
    fn new(base: usize, qid: u16, dstrd: u32, sq: usize, cq: usize, depth: u16) -> Self {
        let stride = 4usize << dstrd;
        let qid = usize::from(qid);
        Self {
            sq,
            cq,
            depth,
            sq_tail: 0,
            cq_head: 0,
            cq_phase: 1, // zeroed CQ memory -> first completion carries phase 1
            sq_doorbell: base + REG_DOORBELL_BASE + (2 * qid) * stride,
            cq_doorbell: base + REG_DOORBELL_BASE + (2 * qid + 1) * stride,
        }
    }

    /// Submits a 16-dword command, rings the doorbell, polls for its completion,
    /// and returns the NVMe status code (0 = success), or `None` on timeout.
    fn submit(&mut self, cmd: &[u32; 16]) -> Option<u16> {
        let slot = self.sq + usize::from(self.sq_tail) * SQE_BYTES;
        for (i, &word) in cmd.iter().enumerate() {
            write32(slot + i * 4, word);
        }
        // Publish the SQ entry before ringing the doorbell.
        fence(Ordering::SeqCst);
        self.sq_tail = (self.sq_tail + 1) % self.depth;
        write32(self.sq_doorbell, u32::from(self.sq_tail));

        let cqe = self.cq + usize::from(self.cq_head) * CQE_BYTES;
        let mut spins = 0u32;
        loop {
            let status_dword = read32(cqe + 12);
            if (status_dword >> 16) & 1 == self.cq_phase {
                fence(Ordering::Acquire);
                let status = ((status_dword >> 17) & 0x7FFF) as u16;
                self.cq_head += 1;
                if self.cq_head == self.depth {
                    self.cq_head = 0;
                    self.cq_phase ^= 1;
                }
                write32(self.cq_doorbell, u32::from(self.cq_head));
                return Some(status);
            }
            spins += 1;
            if spins >= SPIN_LIMIT {
                return None;
            }
            core::hint::spin_loop();
        }
    }
}

/// A brought-up NVMe controller with its admin queue (and, after setup, an I/O
/// queue pair).
pub struct Controller {
    base: usize,
    dstrd: u32,
    admin: Queue,
    io: Option<Queue>,
    lba_size: usize,
}

/// Builds a zeroed command and applies `setup` to fill the fields it needs.
fn command(setup: impl FnOnce(&mut [u32; 16])) -> [u32; 16] {
    let mut cmd = [0u32; 16];
    setup(&mut cmd);
    cmd
}

impl Controller {
    /// Identify Controller (6c): prints the model/serial DMA'd back.
    #[must_use]
    pub fn identify_controller(&mut self) -> bool {
        let Some(buffer) = frame::alloc() else {
            crate::kprintln!("[feox] nvme: out of frames for identify buffer");
            return false;
        };
        zero_frame(buffer);
        let cmd = command(|c| {
            c[0] = ADMIN_OP_IDENTIFY;
            c[6] = buffer as u32;
            c[7] = (buffer as u64 >> 32) as u32;
            c[10] = CNS_IDENTIFY_CONTROLLER;
        });
        if !self.run(QueueKind::Admin, &cmd, "identify-controller") {
            return false;
        }
        let serial = ascii_field(buffer + 4, 20);
        let model = ascii_field(buffer + 24, 40);
        crate::kprintln!("[feox] nvme: identify ok — model='{}' serial='{}'", model, serial);
        crate::kprintln!("[feox] milestone 6c: NVMe admin command round-trip complete.");
        true
    }

    /// Identify Namespace (6d): reads the namespace size and LBA data size.
    #[must_use]
    pub fn identify_namespace(&mut self) -> bool {
        let Some(buffer) = frame::alloc() else {
            crate::kprintln!("[feox] nvme: out of frames for namespace identify");
            return false;
        };
        zero_frame(buffer);
        let cmd = command(|c| {
            c[0] = ADMIN_OP_IDENTIFY;
            c[1] = NSID;
            c[6] = buffer as u32;
            c[7] = (buffer as u64 >> 32) as u32;
            c[10] = CNS_IDENTIFY_NAMESPACE;
        });
        if !self.run(QueueKind::Admin, &cmd, "identify-namespace") {
            return false;
        }
        let nsze = read64(buffer); // namespace size in LBAs
        let flbas = (read_u8(buffer + 26) & 0x0F) as usize;
        let lbaf = read32(buffer + 128 + flbas * 4);
        let lbads = (lbaf >> 16) & 0xFF;
        self.lba_size = 1usize << lbads;
        crate::kprintln!(
            "[feox] nvme: namespace {} — {} blocks, {}-byte LBA",
            NSID,
            nsze,
            self.lba_size
        );
        true
    }

    /// Creates the I/O completion + submission queue pair (queue id 1).
    #[must_use]
    pub fn create_io_queues(&mut self) -> bool {
        let (Some(cq), Some(sq)) = (frame::alloc(), frame::alloc()) else {
            crate::kprintln!("[feox] nvme: out of frames for I/O queues");
            return false;
        };
        zero_frame(cq);
        zero_frame(sq);
        let qid = u32::from(IO_QUEUE_ID);
        let size_field = u32::from(IO_QUEUE_DEPTH - 1) << 16;

        // Create I/O CQ: PC=1, interrupts disabled (we poll).
        let cq_cmd = command(|c| {
            c[0] = ADMIN_OP_CREATE_IO_CQ;
            c[6] = cq as u32;
            c[7] = (cq as u64 >> 32) as u32;
            c[10] = size_field | qid;
            c[11] = 0x1; // PC
        });
        if !self.run(QueueKind::Admin, &cq_cmd, "create-io-cq") {
            return false;
        }

        // Create I/O SQ: PC=1, associated with CQ id 1.
        let sq_cmd = command(|c| {
            c[0] = ADMIN_OP_CREATE_IO_SQ;
            c[6] = sq as u32;
            c[7] = (sq as u64 >> 32) as u32;
            c[10] = size_field | qid;
            c[11] = (qid << 16) | 0x1; // CQID | PC
        });
        if !self.run(QueueKind::Admin, &sq_cmd, "create-io-sq") {
            return false;
        }

        self.io = Some(Queue::new(self.base, IO_QUEUE_ID, self.dstrd, sq, cq, IO_QUEUE_DEPTH));
        crate::kprintln!(
            "[feox] nvme: I/O queue {} ready (SQ={:#x} CQ={:#x}, depth {})",
            IO_QUEUE_ID,
            sq,
            cq,
            IO_QUEUE_DEPTH
        );
        true
    }

    /// Writes a known pattern to LBA 0 then reads it back into a separate buffer
    /// and verifies they match — proof of real block I/O.
    #[must_use]
    pub fn block_io_selftest(&mut self) -> bool {
        if self.io.is_none() {
            crate::kprintln!("[feox] nvme: no I/O queue for block test");
            return false;
        }
        let (Some(write_buf), Some(read_buf)) = (frame::alloc(), frame::alloc()) else {
            crate::kprintln!("[feox] nvme: out of frames for block test");
            return false;
        };
        let len = self.lba_size.min(4096);
        fill_pattern(write_buf, len);
        zero_frame(read_buf);

        if !self.io_rw(IO_OP_WRITE, 0, write_buf, "write") {
            return false;
        }
        if !self.io_rw(IO_OP_READ, 0, read_buf, "read") {
            return false;
        }

        let matched = buffers_equal(write_buf, read_buf, len);
        let head = read32(read_buf); // first 4 bytes read back, for the log
        crate::kprintln!(
            "[feox] nvme: block 0 write+read back {} bytes, first4={:#010x}, match={}",
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

    /// Issues a single-block I/O read/write of `buf` at `lba` on the I/O queue.
    fn io_rw(&mut self, opcode: u32, lba: u64, buf: usize, what: &str) -> bool {
        let cmd = command(|c| {
            c[0] = opcode;
            c[1] = NSID;
            c[6] = buf as u32;
            c[7] = (buf as u64 >> 32) as u32;
            c[10] = lba as u32;
            c[11] = (lba >> 32) as u32;
            c[12] = 0; // NLB is 0-based: 0 => one block
        });
        self.run(QueueKind::Io, &cmd, what)
    }

    /// Submits `cmd` on the chosen queue, logging timeouts/error status.
    fn run(&mut self, kind: QueueKind, cmd: &[u32; 16], what: &str) -> bool {
        let queue = match kind {
            QueueKind::Admin => &mut self.admin,
            QueueKind::Io => self.io.as_mut().expect("I/O queue not created"),
        };
        match queue.submit(cmd) {
            Some(0) => true,
            Some(status) => {
                crate::kprintln!("[feox] nvme: {} failed (status={:#x})", what, status);
                false
            }
            None => {
                crate::kprintln!("[feox] nvme: {} timed out", what);
                false
            }
        }
    }
}

/// Selects which queue a command runs on.
enum QueueKind {
    Admin,
    Io,
}

/// Brings the controller to ready (6b) and returns a handle with its admin
/// queue. Logs and returns `None` on fault.
#[must_use]
pub fn init(device: &pci::PciDevice) -> Option<Controller> {
    let size = pci::size_bar64(device, 0);
    let base = pci::MMIO_BASE;
    pci::set_bar64(device, 0, base as u64);
    pci::enable_memory_and_bus_master(device);

    let cap = read64(base + REG_CAP);
    let vs = read32(base + REG_VS);
    let dstrd = ((cap >> 32) & 0xF) as u32;
    crate::kprintln!(
        "[feox] nvme: BAR0={:#x} (size={:#x}) CAP={:#018x} version={}.{}.{} dstrd={}",
        base,
        size,
        cap,
        vs >> 16,
        (vs >> 8) & 0xFF,
        vs & 0xFF,
        dstrd
    );

    write32(base + REG_CC, read32(base + REG_CC) & !CC_ENABLE);
    if !spin_until(base, |csts| csts & CSTS_READY == 0) {
        crate::kprintln!("[feox] nvme: timeout waiting for reset (CSTS.RDY=0)");
        return None;
    }

    let admin_sq = frame::alloc()?;
    let admin_cq = frame::alloc()?;
    zero_frame(admin_sq);
    zero_frame(admin_cq);
    let aqa = (u32::from(ADMIN_QUEUE_DEPTH - 1) << 16) | u32::from(ADMIN_QUEUE_DEPTH - 1);
    write32(base + REG_AQA, aqa);
    write64(base + REG_ASQ, admin_sq as u64);
    write64(base + REG_ACQ, admin_cq as u64);

    let cc_value =
        (NVME_SQE_LOG2 << CC_IOSQES_SHIFT) | (NVME_CQE_LOG2 << CC_IOCQES_SHIFT) | CC_ENABLE;
    write32(base + REG_CC, cc_value);
    if !spin_until(base, |csts| csts & CSTS_READY != 0) {
        crate::kprintln!(
            "[feox] nvme: timeout waiting for enable (CSTS={:#x})",
            read32(base + REG_CSTS)
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

    let admin = Queue::new(base, 0, dstrd, admin_sq, admin_cq, ADMIN_QUEUE_DEPTH);
    Some(Controller {
        base,
        dstrd,
        admin,
        io: None,
        lba_size: DEFAULT_LBA_SIZE,
    })
}

fn spin_until(base: usize, cond: impl Fn(u32) -> bool) -> bool {
    let mut spins = 0u32;
    while !cond(read32(base + REG_CSTS)) {
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
