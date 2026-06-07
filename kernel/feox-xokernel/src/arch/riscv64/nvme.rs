//! NVMe controller bring-up on riscv64 (milestone 6b).
//!
//! Assigns the controller's 64-bit BAR from the PCIe MMIO window (no firmware
//! ran PCI enumeration, so it is unassigned), enables memory decoding and bus
//! mastering, then performs the controller reset/enable handshake: disable,
//! wait `CSTS.RDY=0`, program the admin submission/completion queues, enable,
//! and wait `CSTS.RDY=1`. Reaching ready proves real device-register MMIO and a
//! valid controller configuration on riscv64.
//!
//! Command submission (Identify) and I/O queues land in 6c, where `feox-nvme`
//! supplies the queue/inflight logic. This pass is deliberately self-contained.

use core::ptr::{read_volatile, write_volatile};

use super::{frame, pci};

// Controller register offsets within BAR0 (NVMe base spec).
const REG_CAP: usize = 0x00; // Controller Capabilities (64-bit)
const REG_VS: usize = 0x08; // Version (32-bit)
const REG_CC: usize = 0x14; // Controller Configuration (32-bit)
const REG_CSTS: usize = 0x1C; // Controller Status (32-bit)
const REG_AQA: usize = 0x24; // Admin Queue Attributes (32-bit)
const REG_ASQ: usize = 0x28; // Admin Submission Queue base (64-bit)
const REG_ACQ: usize = 0x30; // Admin Completion Queue base (64-bit)

const CC_ENABLE: u32 = 1 << 0;
const CSTS_READY: u32 = 1 << 0;

// CC field shifts.
const CC_IOSQES_SHIFT: u32 = 16; // I/O Submission Queue Entry Size (log2)
const CC_IOCQES_SHIFT: u32 = 20; // I/O Completion Queue Entry Size (log2)
const NVME_SQE_LOG2: u32 = 6; // 64-byte submission entries
const NVME_CQE_LOG2: u32 = 4; // 16-byte completion entries

/// Admin queue depth (entries). 64 x 64 B submission entries = one 4 KiB frame;
/// 64 x 16 B completion entries fit in one frame. Well under QEMU's MQES.
const ADMIN_QUEUE_DEPTH: u32 = 64;

/// Spin bound for the readiness handshakes (~plenty for QEMU).
const SPIN_LIMIT: u32 = 5_000_000;

fn read32(base: usize, off: usize) -> u32 {
    // SAFETY: `base` is the BAR0 region mapped R/W as device memory; `off` is a
    // valid register offset. MMIO register reads are side-effect-aware here.
    unsafe { read_volatile((base + off) as *const u32) }
}

fn write32(base: usize, off: usize, value: u32) {
    // SAFETY: as above; writing controller registers is the intended effect.
    unsafe { write_volatile((base + off) as *mut u32, value) }
}

fn read64(base: usize, off: usize) -> u64 {
    // SAFETY: as `read32`, for the 64-bit registers (CAP/ASQ/ACQ).
    unsafe { read_volatile((base + off) as *const u64) }
}

fn write64(base: usize, off: usize, value: u64) {
    // SAFETY: as `write32`, for the 64-bit registers.
    unsafe { write_volatile((base + off) as *mut u64, value) }
}

/// Zeroes a 4 KiB frame (used for admin queue memory the controller DMAs).
fn zero_frame(frame: usize) {
    let words = frame as *mut u64;
    for i in 0..512 {
        // SAFETY: `frame` is a fresh identity-mapped allocator frame.
        unsafe { write_volatile(words.add(i), 0) };
    }
}

/// Brings the NVMe controller to the enabled/ready state. Returns `true` on
/// success. Never panics on controller faults — it logs and returns `false` so
/// boot continues.
#[must_use]
pub fn init(device: &pci::PciDevice) -> bool {
    // Assign BAR0 (a 64-bit memory BAR) at the MMIO window base and turn on
    // memory decoding + bus mastering.
    let size = pci::size_bar64(device, 0);
    let base = pci::MMIO_BASE as u64;
    pci::set_bar64(device, 0, base);
    pci::enable_memory_and_bus_master(device);
    let base = base as usize;

    let cap = read64(base, REG_CAP);
    let vs = read32(base, REG_VS);
    crate::kprintln!(
        "[feox] nvme: BAR0={:#x} (size={:#x}) CAP={:#018x} version={}.{}.{}",
        base,
        size,
        cap,
        vs >> 16,
        (vs >> 8) & 0xFF,
        vs & 0xFF
    );

    // Reset: clear CC.EN and wait for CSTS.RDY to clear.
    let cc = read32(base, REG_CC);
    write32(base, REG_CC, cc & !CC_ENABLE);
    if !spin_until(base, |csts| csts & CSTS_READY == 0) {
        crate::kprintln!("[feox] nvme: timeout waiting for reset (CSTS.RDY=0)");
        return false;
    }

    // Program the admin queues. Frames are not zeroed by the allocator, so zero
    // them — the controller DMAs these regions.
    let Some(asq) = frame::alloc() else {
        crate::kprintln!("[feox] nvme: out of frames for admin SQ");
        return false;
    };
    let Some(acq) = frame::alloc() else {
        crate::kprintln!("[feox] nvme: out of frames for admin CQ");
        return false;
    };
    zero_frame(asq);
    zero_frame(acq);

    // AQA: ACQS in 27:16, ASQS in 11:0, both zero-based (depth - 1).
    let aqa = ((ADMIN_QUEUE_DEPTH - 1) << 16) | (ADMIN_QUEUE_DEPTH - 1);
    write32(base, REG_AQA, aqa);
    write64(base, REG_ASQ, asq as u64);
    write64(base, REG_ACQ, acq as u64);

    // Enable: NVM command set (CSS=0), 4 KiB pages (MPS=0), standard entry
    // sizes, EN=1.
    let cc_value =
        (NVME_SQE_LOG2 << CC_IOSQES_SHIFT) | (NVME_CQE_LOG2 << CC_IOCQES_SHIFT) | CC_ENABLE;
    write32(base, REG_CC, cc_value);
    if !spin_until(base, |csts| csts & CSTS_READY != 0) {
        crate::kprintln!(
            "[feox] nvme: timeout waiting for enable (CSTS={:#x})",
            read32(base, REG_CSTS)
        );
        return false;
    }

    crate::kprintln!(
        "[feox] nvme: admin queues SQ={:#x} CQ={:#x} (depth {}); controller ready (CSTS.RDY=1)",
        asq,
        acq,
        ADMIN_QUEUE_DEPTH
    );
    crate::kprintln!("[feox] milestone 6b: NVMe controller enabled.");
    true
}

/// Spins on `CSTS` until `cond` holds or the spin bound is exceeded.
fn spin_until(base: usize, cond: impl Fn(u32) -> bool) -> bool {
    let mut spins = 0u32;
    while !cond(read32(base, REG_CSTS)) {
        spins += 1;
        if spins >= SPIN_LIMIT {
            return false;
        }
        core::hint::spin_loop();
    }
    true
}
