//! NVMe controller bring-up and admin commands on riscv64 (milestones 6b/6c).
//!
//! 6b assigns the controller's 64-bit BAR from the PCIe MMIO window, enables
//! memory decoding and bus mastering, and runs the reset/enable handshake
//! (disable -> `CSTS.RDY=0` -> program admin queues -> enable -> `CSTS.RDY=1`).
//!
//! 6c submits the first command: Identify Controller on the admin queue. This
//! exercises the full round-trip — write a 64-byte submission entry, ring the
//! submission doorbell, the controller DMAs the 4 KiB identify structure into a
//! kernel frame, then poll the completion queue's phase bit and read back the
//! controller's model/serial strings.
//!
//! Still self-contained MMIO; I/O queues + a block read (and `feox-nvme`
//! integration) land in 6d.

use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{Ordering, fence};

use super::{frame, pci};

// Controller register offsets within BAR0 (NVMe base spec).
const REG_CAP: usize = 0x00; // Controller Capabilities (64-bit)
const REG_VS: usize = 0x08; // Version (32-bit)
const REG_CC: usize = 0x14; // Controller Configuration (32-bit)
const REG_CSTS: usize = 0x1C; // Controller Status (32-bit)
const REG_AQA: usize = 0x24; // Admin Queue Attributes (32-bit)
const REG_ASQ: usize = 0x28; // Admin Submission Queue base (64-bit)
const REG_ACQ: usize = 0x30; // Admin Completion Queue base (64-bit)
/// Doorbell registers begin here; stride is `4 << CAP.DSTRD`.
const REG_DOORBELL_BASE: usize = 0x1000;

const CC_ENABLE: u32 = 1 << 0;
const CSTS_READY: u32 = 1 << 0;

const CC_IOSQES_SHIFT: u32 = 16;
const CC_IOCQES_SHIFT: u32 = 20;
const NVME_SQE_LOG2: u32 = 6; // 64-byte submission entries
const NVME_CQE_LOG2: u32 = 4; // 16-byte completion entries
const SQE_BYTES: usize = 1 << NVME_SQE_LOG2;
const CQE_BYTES: usize = 1 << NVME_CQE_LOG2;

/// Admin queue depth (entries); fits one 4 KiB frame for the SQ.
const ADMIN_QUEUE_DEPTH: u16 = 64;

/// Admin opcode: Identify.
const ADMIN_OP_IDENTIFY: u32 = 0x06;
/// Identify CNS: Identify Controller data structure.
const CNS_IDENTIFY_CONTROLLER: u32 = 0x01;

const SPIN_LIMIT: u32 = 5_000_000;

fn read32(addr: usize) -> u32 {
    // SAFETY: `addr` is a mapped device-MMIO or identity-mapped frame address.
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

/// Zeroes a 4 KiB frame the controller will DMA.
fn zero_frame(frame: usize) {
    for i in 0..512 {
        // SAFETY: `frame` is a fresh identity-mapped allocator frame.
        unsafe { write_volatile((frame as *mut u64).add(i), 0) };
    }
}

/// A brought-up NVMe controller with its admin queue retained for commands.
pub struct Controller {
    base: usize,
    admin_sq: usize,
    admin_cq: usize,
    sq_tail: u16,
    cq_head: u16,
    /// Phase bit the next completion will carry (starts 1; flips each wrap).
    cq_phase: u32,
    dstrd: u32,
}

impl Controller {
    /// Admin submission-queue tail doorbell address (queue 0).
    fn sq_doorbell(&self) -> usize {
        self.base + REG_DOORBELL_BASE
    }
    /// Admin completion-queue head doorbell address (queue 0).
    fn cq_doorbell(&self) -> usize {
        self.base + REG_DOORBELL_BASE + (4 << self.dstrd)
    }

    /// Issues Identify Controller and prints the model/serial read back from the
    /// DMA'd structure. Returns `true` on a successful completion.
    #[must_use]
    pub fn identify_controller(&mut self) -> bool {
        let Some(buffer) = frame::alloc() else {
            crate::kprintln!("[feox] nvme: out of frames for identify buffer");
            return false;
        };
        zero_frame(buffer);

        // Build the 64-byte submission entry in the admin SQ slot.
        let sqe = self.admin_sq + usize::from(self.sq_tail) * SQE_BYTES;
        for i in 0..(SQE_BYTES / 4) {
            write32(sqe + i * 4, 0);
        }
        write32(sqe, ADMIN_OP_IDENTIFY); // CDW0: opcode, CID 0
        write64(sqe + 24, buffer as u64); // PRP1: data buffer (one 4 KiB page)
        write32(sqe + 40, CNS_IDENTIFY_CONTROLLER); // CDW10: CNS

        // Publish the entry before ringing the doorbell (device DMAs the SQ).
        fence(Ordering::SeqCst);
        self.sq_tail = (self.sq_tail + 1) % ADMIN_QUEUE_DEPTH;
        write32(self.sq_doorbell(), u32::from(self.sq_tail));

        // Poll the completion-queue entry for our phase bit.
        let cqe = self.admin_cq + usize::from(self.cq_head) * CQE_BYTES;
        let mut spins = 0u32;
        loop {
            let status_dword = read32(cqe + 12);
            if (status_dword >> 16) & 1 == self.cq_phase {
                let status_code = (status_dword >> 17) & 0x7FFF;
                fence(Ordering::SeqCst);
                self.advance_cq();
                if status_code != 0 {
                    crate::kprintln!("[feox] nvme: identify failed (status={:#x})", status_code);
                    return false;
                }
                break;
            }
            spins += 1;
            if spins >= SPIN_LIMIT {
                crate::kprintln!("[feox] nvme: identify completion timeout");
                return false;
            }
            core::hint::spin_loop();
        }

        // Identify Controller layout: serial @ 4 (20 B), model @ 24 (40 B).
        let serial = ascii_field(buffer + 4, 20);
        let model = ascii_field(buffer + 24, 40);
        crate::kprintln!("[feox] nvme: identify ok — model='{}' serial='{}'", model, serial);
        crate::kprintln!("[feox] milestone 6c: NVMe admin command round-trip complete.");
        true
    }

    /// Advances the completion-queue head, flipping the phase on wrap, and rings
    /// the completion doorbell.
    fn advance_cq(&mut self) {
        self.cq_head += 1;
        if self.cq_head == ADMIN_QUEUE_DEPTH {
            self.cq_head = 0;
            self.cq_phase ^= 1;
        }
        write32(self.cq_doorbell(), u32::from(self.cq_head));
    }
}

/// Brings the NVMe controller to the enabled/ready state and returns a handle
/// with its admin queue. Never panics on controller faults — logs and returns
/// `None` so boot continues.
#[must_use]
pub fn init(device: &pci::PciDevice) -> Option<Controller> {
    // Assign BAR0 (a 64-bit memory BAR) and enable decoding + bus mastering.
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

    // Reset: clear CC.EN and wait for CSTS.RDY to clear.
    write32(base + REG_CC, read32(base + REG_CC) & !CC_ENABLE);
    if !spin_until(base, |csts| csts & CSTS_READY == 0) {
        crate::kprintln!("[feox] nvme: timeout waiting for reset (CSTS.RDY=0)");
        return None;
    }

    // Admin queues — frames are not zeroed by the allocator; zero them.
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

    Some(Controller {
        base,
        admin_sq,
        admin_cq,
        sq_tail: 0,
        cq_head: 0,
        cq_phase: 1, // CQ memory starts zeroed; first completion carries phase 1
        dstrd,
    })
}

/// Spins on `CSTS` until `cond` holds or the spin bound is exceeded.
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

/// Renders a fixed-width space-padded ASCII identify field as a trimmed string.
fn ascii_field(addr: usize, len: usize) -> &'static str {
    // SAFETY: `addr`/`len` lie within the identity-mapped 4 KiB identify buffer.
    let bytes = unsafe { core::slice::from_raw_parts(addr as *const u8, len) };
    let end = bytes
        .iter()
        .rposition(|&b| b != b' ' && b != 0)
        .map_or(0, |i| i + 1);
    core::str::from_utf8(&bytes[..end]).unwrap_or("<non-utf8>")
}
