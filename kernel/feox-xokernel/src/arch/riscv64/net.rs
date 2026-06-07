//! Networking: a virtio-net (virtio-mmio) driver and a link-layer self-test
//! (milestone 8a).
//!
//! Brings up the QEMU virt virtio-net device over the virtio-mmio transport
//! (flat register layout, simpler than virtio-pci capability walking), sets up
//! split RX/TX virtqueues, and proves TX+RX end to end with an ARP exchange:
//! send "who has <gateway>" and receive the gateway's MAC from QEMU's user
//! (slirp) network.
//!
//! The [`NetDevice`] trait is the device-agnostic seam: the IP/UDP/DHCP stack
//! (8b/8c) and the future JH7110 `dwmac` driver sit on either side of it, so
//! moving to real hardware is a driver swap, not a stack rewrite.

use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{Ordering, fence};

use super::frame;

/// A minimal network device: a MAC address, frame transmit, and non-blocking
/// frame receive (copies into `buf`, returns the Ethernet frame length).
pub trait NetDevice {
    /// The device's 6-byte MAC address.
    fn mac(&self) -> [u8; 6];
    /// Transmits one Ethernet frame; returns whether it was accepted.
    fn send(&mut self, frame: &[u8]) -> bool;
    /// Non-blocking receive: copies the next Ethernet frame into `buf` and
    /// returns its length, or `None` if no frame is pending.
    fn poll_recv(&mut self, buf: &mut [u8]) -> Option<usize>;
}

// ---- virtio-mmio transport -------------------------------------------------

/// First virtio-mmio transport on QEMU virt; 8 slots, 0x1000 apart.
const VIRTIO_MMIO_BASE: usize = 0x1000_1000;
const VIRTIO_MMIO_STRIDE: usize = 0x1000;
const VIRTIO_MMIO_SLOTS: usize = 8;
const VIRTIO_MAGIC: u32 = 0x7472_6976; // "virt"
const VIRTIO_ID_NET: u32 = 1;

// virtio-mmio registers.
const R_MAGIC: usize = 0x000;
const R_DEVICE_ID: usize = 0x008;
const R_DEVICE_FEATURES: usize = 0x010;
const R_DEVICE_FEATURES_SEL: usize = 0x014;
const R_DRIVER_FEATURES: usize = 0x020;
const R_DRIVER_FEATURES_SEL: usize = 0x024;
const R_QUEUE_SEL: usize = 0x030;
const R_QUEUE_NUM: usize = 0x038;
const R_QUEUE_READY: usize = 0x044;
const R_QUEUE_NOTIFY: usize = 0x050;
const R_STATUS: usize = 0x070;
const R_QUEUE_DESC_LOW: usize = 0x080;
const R_QUEUE_DESC_HIGH: usize = 0x084;
const R_QUEUE_DRIVER_LOW: usize = 0x090;
const R_QUEUE_DRIVER_HIGH: usize = 0x094;
const R_QUEUE_DEVICE_LOW: usize = 0x0a0;
const R_QUEUE_DEVICE_HIGH: usize = 0x0a4;
const R_CONFIG: usize = 0x100;

// Status bits.
const S_ACKNOWLEDGE: u32 = 1;
const S_DRIVER: u32 = 2;
const S_DRIVER_OK: u32 = 4;
const S_FEATURES_OK: u32 = 8;

// Features.
const F_NET_MAC: u32 = 5; // VIRTIO_NET_F_MAC (bit 5)
const F_VERSION_1: u32 = 32; // VIRTIO_F_VERSION_1 (bit 32)

// Virtqueue.
const QSIZE: u16 = 8;
const DESC_OFF: usize = 0;
const AVAIL_OFF: usize = 128; // after 8 * 16-byte descriptors
const USED_OFF: usize = 256; // 4-byte aligned, past the avail ring
const DESC_F_WRITE: u16 = 2;

const RX_BUF_SIZE: usize = 2048;
/// virtio-net header prepended to every frame (v1 / VERSION_1 layout = 12 B).
const NET_HDR_LEN: usize = 12;

const RX_QUEUE: u16 = 0;
const TX_QUEUE: u16 = 1;

fn mmio_r(base: usize, off: usize) -> u32 {
    // SAFETY: `base` is a mapped virtio-mmio transport; `off` a valid register.
    unsafe { read_volatile((base + off) as *const u32) }
}
fn mmio_w(base: usize, off: usize, value: u32) {
    // SAFETY: as above; register writes drive the device.
    unsafe { write_volatile((base + off) as *mut u32, value) }
}

fn zero_frame(frame: usize) {
    for i in 0..512 {
        // SAFETY: fresh identity-mapped allocator frame.
        unsafe { write_volatile((frame as *mut u64).add(i), 0) };
    }
}

/// A split virtqueue backed by one frame (desc + avail + used).
struct VirtQueue {
    base: usize,
    avail_idx: u16,
    used_seen: u16,
}

impl VirtQueue {
    fn new(frame: usize) -> Self {
        zero_frame(frame);
        Self {
            base: frame,
            avail_idx: 0,
            used_seen: 0,
        }
    }

    fn set_desc(&self, i: u16, addr: u64, len: u32, flags: u16, next: u16) {
        let d = self.base + DESC_OFF + usize::from(i) * 16;
        // SAFETY: `d` is within the queue frame's descriptor table.
        unsafe {
            write_volatile(d as *mut u64, addr);
            write_volatile((d + 8) as *mut u32, len);
            write_volatile((d + 12) as *mut u16, flags);
            write_volatile((d + 14) as *mut u16, next);
        }
    }

    /// Publishes descriptor `desc_idx` into the avail ring and bumps the index.
    fn push_avail(&mut self, desc_idx: u16) {
        let slot = self.avail_idx % QSIZE;
        let ring = self.base + AVAIL_OFF + 4 + usize::from(slot) * 2;
        // SAFETY: avail ring slot within the queue frame.
        unsafe { write_volatile(ring as *mut u16, desc_idx) };
        self.avail_idx = self.avail_idx.wrapping_add(1);
        fence(Ordering::SeqCst);
        // SAFETY: avail.idx field (offset +2 in the avail ring).
        unsafe { write_volatile((self.base + AVAIL_OFF + 2) as *mut u16, self.avail_idx) };
    }

    /// Returns the device-reported used.idx.
    fn used_idx(&self) -> u16 {
        // SAFETY: used.idx field (offset +2 in the used ring).
        unsafe { read_volatile((self.base + USED_OFF + 2) as *const u16) }
    }

    /// Pops one completed used element as `(descriptor index, bytes written)`.
    fn pop_used(&mut self) -> Option<(u16, u32)> {
        if self.used_seen == self.used_idx() {
            return None;
        }
        fence(Ordering::Acquire);
        let slot = self.used_seen % QSIZE;
        let elem = self.base + USED_OFF + 4 + usize::from(slot) * 8;
        // SAFETY: used ring element within the queue frame.
        let (id, len) = unsafe {
            (
                read_volatile(elem as *const u32),
                read_volatile((elem + 4) as *const u32),
            )
        };
        self.used_seen = self.used_seen.wrapping_add(1);
        Some((id as u16, len))
    }
}

/// A virtio-net device over virtio-mmio.
pub struct VirtioNet {
    base: usize,
    mac: [u8; 6],
    rx: VirtQueue,
    tx: VirtQueue,
    rx_bufs: [usize; QSIZE as usize],
    tx_buf: usize,
}

impl VirtioNet {
    fn queue_setup(base: usize, queue: u16, frame: usize) {
        mmio_w(base, R_QUEUE_SEL, u32::from(queue));
        mmio_w(base, R_QUEUE_NUM, u32::from(QSIZE));
        mmio_w(base, R_QUEUE_DESC_LOW, (frame as u64) as u32);
        mmio_w(base, R_QUEUE_DESC_HIGH, (frame as u64 >> 32) as u32);
        let avail = (frame + AVAIL_OFF) as u64;
        mmio_w(base, R_QUEUE_DRIVER_LOW, avail as u32);
        mmio_w(base, R_QUEUE_DRIVER_HIGH, (avail >> 32) as u32);
        let used = (frame + USED_OFF) as u64;
        mmio_w(base, R_QUEUE_DEVICE_LOW, used as u32);
        mmio_w(base, R_QUEUE_DEVICE_HIGH, (used >> 32) as u32);
        mmio_w(base, R_QUEUE_READY, 1);
    }

    /// Finds and initializes the first virtio-net transport, or `None`.
    pub fn probe() -> Option<Self> {
        let mut base = 0usize;
        for slot in 0..VIRTIO_MMIO_SLOTS {
            let candidate = VIRTIO_MMIO_BASE + slot * VIRTIO_MMIO_STRIDE;
            if mmio_r(candidate, R_MAGIC) == VIRTIO_MAGIC
                && mmio_r(candidate, R_DEVICE_ID) == VIRTIO_ID_NET
            {
                base = candidate;
                break;
            }
        }
        if base == 0 {
            crate::kprintln!("[feox] net: no virtio-net transport found");
            return None;
        }

        // Reset, then ACKNOWLEDGE + DRIVER.
        mmio_w(base, R_STATUS, 0);
        mmio_w(base, R_STATUS, S_ACKNOWLEDGE);
        mmio_w(base, R_STATUS, S_ACKNOWLEDGE | S_DRIVER);

        // Negotiate features: require VERSION_1, accept NET_MAC.
        mmio_w(base, R_DEVICE_FEATURES_SEL, 0);
        let feat_lo = mmio_r(base, R_DEVICE_FEATURES);
        mmio_w(base, R_DRIVER_FEATURES_SEL, 0);
        mmio_w(base, R_DRIVER_FEATURES, feat_lo & (1 << F_NET_MAC));
        mmio_w(base, R_DRIVER_FEATURES_SEL, 1);
        mmio_w(base, R_DRIVER_FEATURES, 1 << (F_VERSION_1 - 32));

        mmio_w(base, R_STATUS, S_ACKNOWLEDGE | S_DRIVER | S_FEATURES_OK);
        if mmio_r(base, R_STATUS) & S_FEATURES_OK == 0 {
            crate::kprintln!("[feox] net: device rejected feature negotiation");
            return None;
        }

        // Read the MAC from device config (NET_MAC negotiated).
        let mut mac = [0u8; 6];
        for (i, b) in mac.iter_mut().enumerate() {
            // SAFETY: config space within the mapped transport.
            *b = unsafe { read_volatile((base + R_CONFIG + i) as *const u8) };
        }

        let (Some(rx_frame), Some(tx_frame), Some(tx_buf)) =
            (frame::alloc(), frame::alloc(), frame::alloc())
        else {
            crate::kprintln!("[feox] net: out of frames for queues");
            return None;
        };
        let rx = VirtQueue::new(rx_frame);
        let tx = VirtQueue::new(tx_frame);

        Self::queue_setup(base, RX_QUEUE, rx_frame);
        Self::queue_setup(base, TX_QUEUE, tx_frame);

        let mut dev = Self {
            base,
            mac,
            rx,
            tx,
            rx_bufs: [0; QSIZE as usize],
            tx_buf,
        };

        // Post one receive buffer per descriptor.
        for i in 0..QSIZE {
            let Some(buf) = frame::alloc() else {
                crate::kprintln!("[feox] net: out of frames for RX buffers");
                return None;
            };
            dev.rx_bufs[usize::from(i)] = buf;
            dev.rx
                .set_desc(i, buf as u64, RX_BUF_SIZE as u32, DESC_F_WRITE, 0);
            dev.rx.push_avail(i);
        }
        mmio_w(base, R_QUEUE_NOTIFY, u32::from(RX_QUEUE));

        // DRIVER_OK — device is live.
        mmio_w(base, R_STATUS, S_ACKNOWLEDGE | S_DRIVER | S_FEATURES_OK | S_DRIVER_OK);

        crate::kprintln!(
            "[feox] net: virtio-net @ {:#x} mac={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            base,
            mac[0],
            mac[1],
            mac[2],
            mac[3],
            mac[4],
            mac[5]
        );
        Some(dev)
    }
}

impl NetDevice for VirtioNet {
    fn mac(&self) -> [u8; 6] {
        self.mac
    }

    fn send(&mut self, payload: &[u8]) -> bool {
        if NET_HDR_LEN + payload.len() > RX_BUF_SIZE {
            return false;
        }
        // Zero the 12-byte virtio-net header, then copy the Ethernet frame.
        zero_frame(self.tx_buf);
        for (i, &byte) in payload.iter().enumerate() {
            // SAFETY: tx_buf is a 4 KiB frame; offset bounded above.
            unsafe { write_volatile((self.tx_buf + NET_HDR_LEN + i) as *mut u8, byte) };
        }
        let total = (NET_HDR_LEN + payload.len()) as u32;
        self.tx.set_desc(0, self.tx_buf as u64, total, 0, 0);
        self.tx.push_avail(0);
        fence(Ordering::SeqCst);
        mmio_w(self.base, R_QUEUE_NOTIFY, u32::from(TX_QUEUE));
        // Fire-and-forget: a single in-flight TX for the link-layer test; the
        // RX reply is the end-to-end proof. Completion reaping comes with the
        // stack in 8b/8c.
        true
    }

    fn poll_recv(&mut self, out: &mut [u8]) -> Option<usize> {
        let (desc, len) = self.rx.pop_used()?;
        let buf = self.rx_bufs[usize::from(desc)];
        let frame_len = (len as usize).saturating_sub(NET_HDR_LEN);
        let n = frame_len.min(out.len());
        for (i, slot) in out.iter_mut().enumerate().take(n) {
            // SAFETY: reading within the RX buffer frame past the net header.
            *slot = unsafe { read_volatile((buf + NET_HDR_LEN + i) as *const u8) };
        }
        // Re-post the buffer so RX keeps flowing.
        self.rx
            .set_desc(desc, buf as u64, RX_BUF_SIZE as u32, DESC_F_WRITE, 0);
        self.rx.push_avail(desc);
        mmio_w(self.base, R_QUEUE_NOTIFY, u32::from(RX_QUEUE));
        Some(n)
    }
}

// ---- link-layer self-test (ARP) --------------------------------------------

/// QEMU user-net guest address and gateway (slirp defaults).
const GUEST_IP: [u8; 4] = [10, 0, 2, 15];
const GATEWAY_IP: [u8; 4] = [10, 0, 2, 2];

const ETHERTYPE_ARP: u16 = 0x0806;

/// Builds an ARP "who-has `target_ip`" request from `mac`/`GUEST_IP` into `out`,
/// returning its length.
fn build_arp_request(mac: [u8; 6], target_ip: [u8; 4], out: &mut [u8; 42]) {
    out[0..6].copy_from_slice(&[0xFF; 6]); // dst: broadcast
    out[6..12].copy_from_slice(&mac); // src
    out[12..14].copy_from_slice(&ETHERTYPE_ARP.to_be_bytes());
    // ARP body.
    out[14..16].copy_from_slice(&1u16.to_be_bytes()); // htype: ethernet
    out[16..18].copy_from_slice(&0x0800u16.to_be_bytes()); // ptype: IPv4
    out[18] = 6; // hlen
    out[19] = 4; // plen
    out[20..22].copy_from_slice(&1u16.to_be_bytes()); // oper: request
    out[22..28].copy_from_slice(&mac); // sender HW
    out[28..32].copy_from_slice(&GUEST_IP); // sender proto
    out[32..38].copy_from_slice(&[0; 6]); // target HW (unknown)
    out[38..42].copy_from_slice(&target_ip); // target proto
}

/// Brings up virtio-net and performs an ARP round-trip with the gateway,
/// proving link-layer TX and RX. Gated to QEMU by the caller.
pub fn selftest() {
    let Some(mut dev) = VirtioNet::probe() else {
        return;
    };

    let mut request = [0u8; 42];
    build_arp_request(dev.mac(), GATEWAY_IP, &mut request);
    if !dev.send(&request) {
        crate::kprintln!("[feox] net: ARP request TX timed out");
        return;
    }
    crate::kprintln!(
        "[feox] net: ARP who-has {}.{}.{}.{} sent",
        GATEWAY_IP[0],
        GATEWAY_IP[1],
        GATEWAY_IP[2],
        GATEWAY_IP[3]
    );

    let mut frame_buf = [0u8; RX_BUF_SIZE];
    let mut rx_seen = 0u32;
    for _ in 0..10_000_000u32 {
        if let Some(n) = dev.poll_recv(&mut frame_buf) {
            rx_seen += 1;
            if n >= 42 {
                let ethertype = u16::from_be_bytes([frame_buf[12], frame_buf[13]]);
                let oper = u16::from_be_bytes([frame_buf[20], frame_buf[21]]);
                if ethertype == ETHERTYPE_ARP && oper == 2 {
                    let m = &frame_buf[22..28]; // sender HW = gateway MAC
                    crate::kprintln!(
                        "[feox] net: ARP reply — gateway {}.{}.{}.{} is at {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                        GATEWAY_IP[0], GATEWAY_IP[1], GATEWAY_IP[2], GATEWAY_IP[3],
                        m[0], m[1], m[2], m[3], m[4], m[5]
                    );
                    crate::kprintln!("[feox] milestone 8a: virtio-net link up (ARP round-trip).");
                    return;
                }
            }
        }
        core::hint::spin_loop();
    }
    crate::kprintln!(
        "[feox] net: no ARP reply (rx_frames={} tx_used={} rx_used={})",
        rx_seen,
        dev.tx.used_idx(),
        dev.rx.used_idx()
    );
}
