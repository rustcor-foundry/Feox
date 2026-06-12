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
const R_VERSION: usize = 0x004;
const R_DEVICE_ID: usize = 0x008;
const R_DEVICE_FEATURES: usize = 0x010;
const R_DEVICE_FEATURES_SEL: usize = 0x014;
const R_DRIVER_FEATURES: usize = 0x020;
const R_DRIVER_FEATURES_SEL: usize = 0x024;
const R_QUEUE_SEL: usize = 0x030;
const R_QUEUE_NUM: usize = 0x038;
const R_QUEUE_READY: usize = 0x044;
const R_QUEUE_NOTIFY: usize = 0x050;
const R_INTERRUPT_STATUS: usize = 0x060;
const R_INTERRUPT_ACK: usize = 0x064;
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
        // This driver implements only the modern (version 2) transport. QEMU's
        // virtio-mmio defaults to legacy; run it with
        // `-global virtio-mmio.force-legacy=false`.
        let version = mmio_r(base, R_VERSION);
        if version != 2 {
            crate::kprintln!("[feox] net: legacy virtio-mmio (v{}) unsupported", version);
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

        // DRIVER_OK — device is live. Per spec, queues must be configured
        // before this, and only used (notified) after it.
        mmio_w(base, R_STATUS, S_ACKNOWLEDGE | S_DRIVER | S_FEATURES_OK | S_DRIVER_OK);

        let mut dev = Self {
            base,
            mac,
            rx,
            tx,
            rx_bufs: [0; QSIZE as usize],
            tx_buf,
        };

        // Post one receive buffer per descriptor, then notify (after DRIVER_OK).
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
        fence(Ordering::SeqCst);
        mmio_w(base, R_QUEUE_NOTIFY, u32::from(RX_QUEUE));

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
        // Reap the completion so the single TX buffer/descriptor is free to
        // reuse for the next packet (DHCP sends several). Best-effort: proceed
        // even if no completion is seen within the bound.
        for _ in 0..1_000_000u32 {
            if self.tx.pop_used().is_some() {
                break;
            }
            core::hint::spin_loop();
        }
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

// ---- minimal IPv4 / ICMP / UDP / DHCP stack --------------------------------

/// QEMU user-net guest address and gateway (slirp defaults).
const GUEST_IP: [u8; 4] = [10, 0, 2, 15];
const GATEWAY_IP: [u8; 4] = [10, 0, 2, 2];

const ETHERTYPE_ARP: u16 = 0x0806;
const ETHERTYPE_IPV4: u16 = 0x0800;
const IP_PROTO_ICMP: u8 = 1;
const IP_PROTO_UDP: u8 = 17;
const BROADCAST_MAC: [u8; 6] = [0xFF; 6];
/// Ethernet header (14) + IPv4 (20) + UDP (8) — start of the BOOTP/DHCP body.
const DHCP_OFFSET: usize = 42;
const DHCP_MAGIC: [u8; 4] = [0x63, 0x82, 0x53, 0x63];

/// Internet checksum (RFC 1071): one's-complement sum of 16-bit words.
fn checksum16(data: &[u8]) -> u16 {
    let mut sum = 0u32;
    let mut i = 0;
    while i + 1 < data.len() {
        sum += u32::from(u16::from_be_bytes([data[i], data[i + 1]]));
        i += 2;
    }
    if i < data.len() {
        sum += u32::from(data[i]) << 8;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

/// Polls the device until a received frame satisfies `pred`, leaving it in
/// `buf`; returns the frame length, or `None` after a bounded spin.
fn poll_match(dev: &mut VirtioNet, buf: &mut [u8], pred: impl Fn(&[u8]) -> bool) -> Option<usize> {
    for _ in 0..10_000_000u32 {
        if let Some(n) = dev.poll_recv(buf) {
            if pred(&buf[..n]) {
                return Some(n);
            }
        }
        core::hint::spin_loop();
    }
    None
}

/// Builds an ARP "who-has `target_ip`" request into `out`.
fn build_arp_request(mac: [u8; 6], target_ip: [u8; 4], out: &mut [u8; 42]) {
    out[0..6].copy_from_slice(&BROADCAST_MAC);
    out[6..12].copy_from_slice(&mac);
    out[12..14].copy_from_slice(&ETHERTYPE_ARP.to_be_bytes());
    out[14..16].copy_from_slice(&1u16.to_be_bytes()); // htype: ethernet
    out[16..18].copy_from_slice(&ETHERTYPE_IPV4.to_be_bytes()); // ptype: IPv4
    out[18] = 6; // hlen
    out[19] = 4; // plen
    out[20..22].copy_from_slice(&1u16.to_be_bytes()); // oper: request
    out[22..28].copy_from_slice(&mac);
    out[28..32].copy_from_slice(&GUEST_IP);
    out[32..38].copy_from_slice(&[0; 6]);
    out[38..42].copy_from_slice(&target_ip);
}

/// Resolves `target_ip` to a MAC via ARP (8a).
fn arp_resolve(dev: &mut VirtioNet, target_ip: [u8; 4]) -> Option<[u8; 6]> {
    let mut req = [0u8; 42];
    build_arp_request(dev.mac(), target_ip, &mut req);
    dev.send(&req);
    let mut buf = [0u8; RX_BUF_SIZE];
    poll_match(dev, &mut buf, |f| {
        f.len() >= 42
            && u16::from_be_bytes([f[12], f[13]]) == ETHERTYPE_ARP
            && u16::from_be_bytes([f[20], f[21]]) == 2
    })?;
    let mut mac = [0u8; 6];
    mac.copy_from_slice(&buf[22..28]);
    Some(mac)
}

/// Builds an ICMP echo request to `dst_ip` (via `dst_mac`) into `pkt`.
fn build_icmp_echo(src_mac: [u8; 6], dst_mac: [u8; 6], dst_ip: [u8; 4], pkt: &mut [u8; 42]) {
    pkt[0..6].copy_from_slice(&dst_mac);
    pkt[6..12].copy_from_slice(&src_mac);
    pkt[12..14].copy_from_slice(&ETHERTYPE_IPV4.to_be_bytes());
    // IPv4 header (14..34).
    pkt[14] = 0x45; // version 4, IHL 5
    pkt[16..18].copy_from_slice(&28u16.to_be_bytes()); // total length
    pkt[18..20].copy_from_slice(&1u16.to_be_bytes()); // id
    pkt[22] = 64; // TTL
    pkt[23] = IP_PROTO_ICMP;
    pkt[26..30].copy_from_slice(&GUEST_IP);
    pkt[30..34].copy_from_slice(&dst_ip);
    let ip_csum = checksum16(&pkt[14..34]);
    pkt[24..26].copy_from_slice(&ip_csum.to_be_bytes());
    // ICMP (34..42): echo request.
    pkt[34] = 8;
    pkt[38..40].copy_from_slice(&0xfe0fu16.to_be_bytes()); // id
    pkt[40..42].copy_from_slice(&1u16.to_be_bytes()); // seq
    let icmp_csum = checksum16(&pkt[34..42]);
    pkt[36..38].copy_from_slice(&icmp_csum.to_be_bytes());
}

/// Sends an ICMP echo request to `dst_ip` (via `dst_mac`) and waits for the
/// echo reply (8b).
fn icmp_ping(dev: &mut VirtioNet, dst_mac: [u8; 6], dst_ip: [u8; 4]) -> bool {
    let mut pkt = [0u8; 42]; // eth(14) + ip(20) + icmp(8)
    build_icmp_echo(dev.mac(), dst_mac, dst_ip, &mut pkt);
    dev.send(&pkt);

    let mut buf = [0u8; RX_BUF_SIZE];
    poll_match(dev, &mut buf, |f| {
        if f.len() < 34 || u16::from_be_bytes([f[12], f[13]]) != ETHERTYPE_IPV4 {
            return false;
        }
        if (f[14] >> 4) != 4 || f[23] != IP_PROTO_ICMP {
            return false;
        }
        let ihl = ((f[14] & 0x0f) as usize) * 4;
        f.len() >= 14 + ihl + 1 && f[14 + ihl] == 0 // echo reply
    })
    .is_some()
}

/// Builds an Ethernet/IPv4/UDP/DHCP packet into `out`, returning its length.
fn build_dhcp(
    mac: [u8; 6],
    xid: u32,
    msg_type: u8,
    requested_ip: Option<[u8; 4]>,
    server_id: Option<[u8; 4]>,
    out: &mut [u8],
) -> usize {
    for b in out.iter_mut() {
        *b = 0;
    }
    // Ethernet.
    out[0..6].copy_from_slice(&BROADCAST_MAC);
    out[6..12].copy_from_slice(&mac);
    out[12..14].copy_from_slice(&ETHERTYPE_IPV4.to_be_bytes());
    // BOOTP at DHCP_OFFSET.
    let b = DHCP_OFFSET;
    out[b] = 1; // op: BOOTREQUEST
    out[b + 1] = 1; // htype: ethernet
    out[b + 2] = 6; // hlen
    out[b + 4..b + 8].copy_from_slice(&xid.to_be_bytes());
    out[b + 10..b + 12].copy_from_slice(&0x8000u16.to_be_bytes()); // flags: broadcast
    out[b + 28..b + 34].copy_from_slice(&mac); // chaddr
    out[b + 236..b + 240].copy_from_slice(&DHCP_MAGIC);
    // Options.
    let mut o = b + 240;
    out[o] = 53; // DHCP message type
    out[o + 1] = 1;
    out[o + 2] = msg_type;
    o += 3;
    out[o] = 55; // parameter request list: subnet, router, DNS
    out[o + 1] = 3;
    out[o + 2] = 1;
    out[o + 3] = 3;
    out[o + 4] = 6;
    o += 5;
    if let Some(ip) = requested_ip {
        out[o] = 50;
        out[o + 1] = 4;
        out[o + 2..o + 6].copy_from_slice(&ip);
        o += 6;
    }
    if let Some(sid) = server_id {
        out[o] = 54;
        out[o + 1] = 4;
        out[o + 2..o + 6].copy_from_slice(&sid);
        o += 6;
    }
    out[o] = 255; // end
    o += 1;

    let dhcp_len = o - b;
    let udp_len = 8 + dhcp_len;
    // UDP (34..42): bootpc(68) -> bootps(67), checksum 0 (optional for IPv4).
    out[34..36].copy_from_slice(&68u16.to_be_bytes());
    out[36..38].copy_from_slice(&67u16.to_be_bytes());
    out[38..40].copy_from_slice(&(udp_len as u16).to_be_bytes());
    // IPv4 (14..34).
    let ip_total = 20 + udp_len;
    out[14] = 0x45;
    out[16..18].copy_from_slice(&(ip_total as u16).to_be_bytes());
    out[18..20].copy_from_slice(&((xid & 0xFFFF) as u16).to_be_bytes()); // id
    out[22] = 64; // TTL
    out[23] = IP_PROTO_UDP;
    out[26..30].copy_from_slice(&[0, 0, 0, 0]); // src 0.0.0.0
    out[30..34].copy_from_slice(&[255, 255, 255, 255]); // dst broadcast
    let ip_csum = checksum16(&out[14..34]);
    out[24..26].copy_from_slice(&ip_csum.to_be_bytes());

    14 + ip_total
}

/// Returns the DHCP option `code`'s value bytes from a received frame.
fn dhcp_option(f: &[u8], code: u8) -> Option<&[u8]> {
    let mut i = DHCP_OFFSET + 240;
    while i < f.len() {
        match f[i] {
            255 => break,    // end
            0 => i += 1,     // pad
            opt => {
                if i + 1 >= f.len() {
                    break;
                }
                let len = f[i + 1] as usize;
                if i + 2 + len > f.len() {
                    break;
                }
                if opt == code {
                    return Some(&f[i + 2..i + 2 + len]);
                }
                i += 2 + len;
            }
        }
    }
    None
}

/// Returns `yiaddr` (the offered/assigned address) from a received DHCP frame.
fn dhcp_yiaddr(f: &[u8]) -> [u8; 4] {
    let y = DHCP_OFFSET + 16;
    [f[y], f[y + 1], f[y + 2], f[y + 3]]
}

/// Tests whether `f` is a DHCP reply for `xid` with message type `msg_type`.
fn dhcp_reply_is(f: &[u8], xid: u32, msg_type: u8) -> bool {
    if f.len() < DHCP_OFFSET + 240
        || u16::from_be_bytes([f[12], f[13]]) != ETHERTYPE_IPV4
        || f[23] != IP_PROTO_UDP
        || u16::from_be_bytes([f[36], f[37]]) != 68 // UDP dst port (bootpc)
    {
        return false;
    }
    let cookie = DHCP_OFFSET + 236;
    if f[cookie..cookie + 4] != DHCP_MAGIC {
        return false;
    }
    let x = DHCP_OFFSET + 4;
    if u32::from_be_bytes([f[x], f[x + 1], f[x + 2], f[x + 3]]) != xid {
        return false;
    }
    dhcp_option(f, 53).is_some_and(|v| v.first() == Some(&msg_type))
}

/// Runs a DHCP DISCOVER/OFFER/REQUEST/ACK exchange and returns the leased
/// address (8c). DHCP is UDP, so this exercises the UDP path too.
fn dhcp_lease(dev: &mut VirtioNet) -> Option<[u8; 4]> {
    let mac = dev.mac();
    let xid = 0xF0E0_0042u32;
    let mut pkt = [0u8; 400];
    let mut buf = [0u8; RX_BUF_SIZE];

    let len = build_dhcp(mac, xid, 1, None, None, &mut pkt); // DISCOVER
    dev.send(&pkt[..len]);
    let offer_len = poll_match(dev, &mut buf, |f| dhcp_reply_is(f, xid, 2))?; // OFFER
    let offer = &buf[..offer_len];
    let offered = dhcp_yiaddr(offer);
    let server_id = dhcp_option(offer, 54).and_then(|sid| {
        <[u8; 4]>::try_from(sid).ok()
    });

    let len = build_dhcp(mac, xid, 3, Some(offered), server_id, &mut pkt); // REQUEST
    dev.send(&pkt[..len]);
    let ack_len = poll_match(dev, &mut buf, |f| dhcp_reply_is(f, xid, 5))?; // ACK
    Some(dhcp_yiaddr(&buf[..ack_len]))
}

/// The live device kept after `selftest` for the interrupt lane (M19): the
/// transport, the resolved gateway MAC, and a shadow of the RX used index so
/// the interrupt handler can distinguish RX progress from TX completions
/// (virtio-mmio's ISR doesn't say which queue fired).
struct ActiveNet {
    dev: VirtioNet,
    gw_mac: [u8; 6],
    rx_seen: u16,
}

/// Boot-hart-only, same invariant as `frame.rs`.
static mut ACTIVE: Option<ActiveNet> = None;

/// Returns the live post-selftest device state, if any.
#[allow(static_mut_refs)]
fn active() -> Option<&'static mut ActiveNet> {
    // SAFETY: only the boot hart touches the net device (probe, selftest,
    // the trap path); no concurrent access exists.
    unsafe { ACTIVE.as_mut() }
}

/// The PLIC IRQ number of the live virtio-net transport (QEMU virt fixed
/// mapping: slot n of the virtio-mmio window is IRQ n + 1).
#[must_use]
pub fn irq_number() -> Option<u32> {
    let active = active()?;
    Some(((active.dev.base - VIRTIO_MMIO_BASE) / VIRTIO_MMIO_STRIDE) as u32 + 1)
}

/// Interrupt hook: acknowledges the device's ISR and reports whether the RX
/// used ring progressed since the last call (i.e. a frame actually arrived —
/// TX completions and config events return false).
pub fn on_interrupt() -> bool {
    let Some(active) = active() else {
        return false;
    };
    let status = mmio_r(active.dev.base, R_INTERRUPT_STATUS);
    if status != 0 {
        mmio_w(active.dev.base, R_INTERRUPT_ACK, status);
    }
    let used = active.dev.rx.used_idx();
    let progressed = used != active.rx_seen;
    active.rx_seen = used;
    progressed
}

/// The live device's MMIO base (for capability registration).
#[must_use]
pub fn mmio_base() -> Option<usize> {
    Some(active()?.dev.base)
}

/// The live device's MAC address.
#[must_use]
pub fn mac() -> Option<[u8; 6]> {
    Some(active()?.dev.mac())
}

/// The gateway MAC resolved during the selftest (demo bookkeeping).
#[must_use]
pub fn gateway_mac() -> Option<[u8; 6]> {
    Some(active()?.gw_mac)
}

/// Transmits one Ethernet frame on the live device (the net ASI lane's TX).
pub fn tx_frame(frame_bytes: &[u8]) -> bool {
    match active() {
        Some(active) => active.dev.send(frame_bytes),
        None => false,
    }
}

/// Receives one pending Ethernet frame into `out` (the net ASI lane's RX).
/// Returns the frame length, or `None` when nothing is pending.
pub fn rx_frame(out: &mut [u8]) -> Option<usize> {
    active()?.dev.poll_recv(out)
}

/// Discards every pending RX frame (so a demo starts from a quiet ring).
pub fn drain_rx() {
    let Some(active) = active() else { return };
    let mut scratch = [0u8; RX_BUF_SIZE];
    while active.dev.poll_recv(&mut scratch).is_some() {}
}

/// Sends one ICMP echo request to the gateway WITHOUT polling for the reply
/// — the reply arrives as an RX interrupt (the M19 demo's wake stimulus).
///
/// The RX used-index shadow is deliberately NOT touched here: slirp can
/// answer the echo before send() even finishes reaping the TX completion,
/// and refreshing the shadow at that point would absorb the reply — the
/// exact event the caller is waiting for. TX completions never move the RX
/// used index, so the existing shadow remains the correct baseline.
pub fn send_test_ping() -> bool {
    let Some(active) = active() else {
        return false;
    };
    let mut pkt = [0u8; 42];
    build_icmp_echo(active.dev.mac(), active.gw_mac, GATEWAY_IP, &mut pkt);
    active.dev.send(&pkt)
}

/// Brings up virtio-net and exercises the stack: ARP (link, 8a), ICMP ping
/// (IPv4, 8b), and a DHCP lease (UDP, 8c). Gated to QEMU by the caller.
/// Keeps the device (and gateway MAC) live afterwards for the IRQ lane.
pub fn selftest() {
    let Some(mut dev) = VirtioNet::probe() else {
        return;
    };

    let Some(gw_mac) = arp_resolve(&mut dev, GATEWAY_IP) else {
        crate::kprintln!("[feox] net: no ARP reply");
        return;
    };
    crate::kprintln!(
        "[feox] net: ARP reply — gateway {}.{}.{}.{} is at {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        GATEWAY_IP[0], GATEWAY_IP[1], GATEWAY_IP[2], GATEWAY_IP[3],
        gw_mac[0], gw_mac[1], gw_mac[2], gw_mac[3], gw_mac[4], gw_mac[5]
    );
    crate::kprintln!("[feox] milestone 8a: virtio-net link up (ARP round-trip).");

    if icmp_ping(&mut dev, gw_mac, GATEWAY_IP) {
        crate::kprintln!(
            "[feox] net: ICMP echo reply from {}.{}.{}.{}",
            GATEWAY_IP[0], GATEWAY_IP[1], GATEWAY_IP[2], GATEWAY_IP[3]
        );
        crate::kprintln!("[feox] milestone 8b: IPv4/ICMP ping ok.");
    } else {
        crate::kprintln!("[feox] net: no ICMP echo reply");
    }

    if let Some(ip) = dhcp_lease(&mut dev) {
        crate::kprintln!("[feox] net: DHCP lease {}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3]);
        crate::kprintln!("[feox] milestone 8c: UDP/DHCP lease obtained.");
        crate::kprintln!("[feox] milestone 8: networking complete.");
    } else {
        crate::kprintln!("[feox] net: DHCP failed");
    }

    // Keep the device live for the interrupt lane (M19). The RX shadow
    // starts at the current used index so only future arrivals count.
    let rx_seen = dev.rx.used_idx();
    // SAFETY: boot-hart-only static (see `active`).
    unsafe {
        ACTIVE = Some(ActiveNet {
            dev,
            gw_mac,
            rx_seen,
        });
    }
}
