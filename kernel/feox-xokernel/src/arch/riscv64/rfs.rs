//! Milestone 23: an RFS volume on the NVMe disk — the RFS arc's payoff.
//!
//! The full cross-repo chain runs against real (QEMU) hardware at boot:
//! `rfs-core` (the CoW engine) -> `rfs-feox` (the `BlockDevice` adapter) ->
//! `feox-nvme` (`QueueRing` rings) -> the controller this kernel brought up
//! in milestone 6. The demo formats a filesystem on the disk, creates a
//! file, writes it through the CoW tree, commits, then proves durability by
//! re-opening the volume on a SECOND I/O queue pair — a genuinely fresh
//! device handle whose only connection to the first is the platters — and
//! reading the file back.
//!
//! The async engine is driven by `rfs_core::testkit::block_on` (a poll
//! loop); the adapter's pump future drains NVMe completions on every poll,
//! so the pair busy-polls each command to completion — the boot-time analog
//! of the executor integration a storage service will use.

use feox_nvme::NamespaceGeometry;
use rfs_core::fs::ROOT_INO;
use rfs_core::testkit::block_on;
use rfs_core::{BlockDevice, DigestMode, Filesystem, SegmentAllocator, SegmentGeom};
use rfs_feox::{DmaPage, NvmeBlockDevice};

use super::{frame, nvme};

type Device = NvmeBlockDevice<8>;
type Fs = Filesystem<SegmentAllocator, Device>;

/// What the demo writes through the CoW tree and reads back after remount.
const PAYLOAD: &[u8] = b"RFS on Feox: copy-on-write over capabilities.\n";
/// Blocks per allocator segment (1 MiB segments of 4 KiB blocks).
const BLOCKS_PER_SEGMENT: u32 = 256;

/// Builds an adapter block device from an I/O ring + a fresh bounce frame.
fn make_device(ring: nvme::IoRing, geometry: NamespaceGeometry) -> Option<Device> {
    let bounce = frame::alloc()?;
    // SAFETY: the ring is controller-registered and core-local (boot hart);
    // `bounce` is a fresh, page-aligned, identity-mapped frame (kernel VA ==
    // DMA address) that is never freed.
    unsafe {
        NvmeBlockDevice::new(
            ring,
            1,
            geometry,
            DmaPage {
                kernel: bounce as *mut u8,
                device: bounce as u64,
            },
        )
    }
}

/// Segment allocator sized to the device the adapter presents.
fn allocator(device: &Device) -> Option<SegmentAllocator> {
    let geom = SegmentGeom::new(device.block_count(), BLOCKS_PER_SEGMENT, 1).ok()?;
    Some(SegmentAllocator::new(geom))
}

/// Formats, writes, commits, remounts (on a second queue pair), reads back.
pub fn demo(mut controller: nvme::Controller) {
    let Some(second_ring) = controller.create_extra_io_ring(2) else {
        crate::kprintln!("[feox] rfs: no second I/O queue for the remount proof");
        return;
    };
    let Some((first_ring, geometry)) = controller.into_io_ring() else {
        crate::kprintln!("[feox] rfs: controller has no I/O ring");
        return;
    };
    let Some(device) = make_device(first_ring, geometry) else {
        crate::kprintln!("[feox] rfs: unsupported namespace geometry");
        return;
    };
    let Some(seg_alloc) = allocator(&device) else {
        crate::kprintln!("[feox] rfs: device too small for the segment allocator");
        return;
    };
    crate::kprintln!(
        "[feox] rfs: formatting {} x {} byte blocks on the NVMe namespace...",
        device.block_count(),
        device.block_size()
    );

    // Format + create + write + commit, then drop the first handle entirely.
    let mut fs: Fs = match block_on(Filesystem::format(device, seg_alloc, DigestMode::Fast64)) {
        Ok(fs) => fs,
        Err(error) => {
            crate::kprintln!("[feox] rfs: format failed: {:?}", error);
            return;
        }
    };
    let written = block_on(fs.create(ROOT_INO, b"feox.txt", 0o644))
        .and_then(|ino| block_on(fs.write(ino, 0, PAYLOAD)).map(|()| ino))
        .and_then(|ino| block_on(fs.sync()).map(|()| ino));
    let ino = match written {
        Ok(ino) => ino,
        Err(error) => {
            crate::kprintln!("[feox] rfs: create/write/sync failed: {:?}", error);
            return;
        }
    };
    drop(fs);
    crate::kprintln!(
        "[feox] rfs: /feox.txt (ino {}) written ({} bytes) and committed; remounting on queue 2...",
        ino,
        PAYLOAD.len()
    );

    // Remount through a different queue pair: only the media is shared.
    let Some(second_device) = make_device(second_ring, geometry) else {
        crate::kprintln!("[feox] rfs: second device construction failed");
        return;
    };
    let Some(second_alloc) = allocator(&second_device) else {
        crate::kprintln!("[feox] rfs: second allocator construction failed");
        return;
    };
    let mut reopened: Fs = match block_on(Filesystem::open(second_device, second_alloc)) {
        Ok(fs) => fs,
        Err(error) => {
            crate::kprintln!("[feox] rfs: remount failed: {:?}", error);
            return;
        }
    };
    let read_back = block_on(reopened.lookup(ROOT_INO, b"feox.txt"))
        .and_then(|found| found.ok_or(rfs_core::StorageError::NotFound))
        .and_then(|found_ino| block_on(reopened.read(found_ino, 0, PAYLOAD.len())));
    let matched = match read_back {
        Ok(data) => data.as_slice() == PAYLOAD,
        Err(error) => {
            crate::kprintln!("[feox] rfs: read-back failed: {:?}", error);
            false
        }
    };

    crate::kprintln!(
        "[feox] milestone 23: RFS on NVMe (format -> write -> commit -> remount on a second queue -> read back, match={}, ok={}).",
        matched,
        matched
    );
}
