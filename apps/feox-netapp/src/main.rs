#![no_std]
#![no_main]

//! feox-netapp: user-space networking over the capability lanes.
//!
//! Role 0 (M20): a complete ARP round trip from U-mode — discover the
//! `NetDevice` capability via `cap_list`, request a page of capability-backed
//! memory as the packet buffer, build an ARP who-has for the gateway, send it
//! with `NetSubmitTx`, and wait (interrupt-driven via `IrqAttach` + park) for
//! the reply with `NetPollRx`. Exits `0x3000 | (sum of gateway MAC bytes &
//! 0xFFF)`, which the kernel predicts. Distinct `0xbNN` exits name failures.
//!
//! Role 1 (M21): hand-rolled TCP, entirely in user space — connect to the
//! echo service the CI harness exposes at 10.0.2.100:7777 (QEMU `guestfwd`
//! running `cat`), complete the three-way handshake, send 8 bytes, receive
//! the echo, and close cleanly. Every frame (Ethernet/IPv4/TCP, checksums
//! included) is built and parsed by this app; the kernel only moves frames.
//! Exits `0x4000 | (sum of echoed bytes & 0xFFF)`.

use feox_asi::{CapHandle, CapType, Duration, EventSlot, MapFlags};
use feox_libos as libos;

/// QEMU user-net addresses (the kernel's M8 stack uses the same).
const GUEST_IP: [u8; 4] = [10, 0, 2, 15];
const GATEWAY_IP: [u8; 4] = [10, 0, 2, 2];

/// Packet buffer layout within the one-page capability: TX frame at 0,
/// RX landing area at 2048.
const TX_OFF: u64 = 0;
const RX_OFF: u64 = 2048;

/// Per-park timeout: 1 s (100 ticks at the 100 Hz scheduler). Well above any
/// legitimate round trip (those resolve in a tick or two) but below every
/// demo's run budget, so a genuine stall surfaces as the role's named
/// diagnostic exit rather than a silent budget stop.
const TIMEOUT: Duration = Duration::from_nanos(1_000_000_000);

/// RX wake slot (bss; attached to the net RX interrupt).
static SLOT: EventSlot = EventSlot::new();

struct Net {
    device: CapHandle,
    buffer: CapHandle,
    mac: [u8; 6],
    base: *mut u8,
}

impl Net {
    /// Standard setup for every role: find the device, map a packet buffer,
    /// attach the RX interrupt slot.
    fn open() -> Self {
        let Some(device) = libos::find_capability(CapType::NetDevice) else {
            libos::exit(0xb30);
        };
        let Some(info) = libos::net_get_info(device) else {
            libos::exit(0xb31);
        };
        let Some(buffer) = libos::cap_request_pages(1, true) else {
            libos::exit(0xb32);
        };
        let Some(region) = libos::mem_map(buffer, 0, 4096, MapFlags::READ | MapFlags::WRITE)
        else {
            libos::exit(0xb33);
        };
        if !libos::irq_attach(feox_asi::IRQ_SOURCE_NET_RX, &SLOT) {
            libos::exit(0xb34);
        }
        Self {
            device,
            buffer,
            mac: info.mac,
            base: region.base as *mut u8,
        }
    }

    fn tx_byte(&self, off: usize, value: u8) {
        // SAFETY: within the mapped one-page packet buffer.
        unsafe { self.base.add(off).write_volatile(value) };
    }

    fn rx_byte(&self, off: usize) -> u8 {
        // SAFETY: within the mapped one-page packet buffer.
        unsafe { self.base.add(RX_OFF as usize + off).read_volatile() }
    }

    fn write_tx(&self, off: usize, bytes: &[u8]) {
        for (i, &b) in bytes.iter().enumerate() {
            self.tx_byte(off + i, b);
        }
    }

    fn send(&self, length: u32) -> bool {
        libos::net_tx(self.device, self.buffer, TX_OFF, length)
    }

    /// Polls for one frame; parks on the RX interrupt slot when the ring is
    /// empty. Returns the frame length, or exits `fail` on timeout/error.
    /// The slot counter is observed BEFORE the poll: a frame landing between
    /// the empty poll and the park has already moved the counter, so the
    /// park returns immediately and the re-poll finds it — no lost frames.
    fn recv(&self, fail: usize) -> usize {
        loop {
            let observed = SLOT.load();
            let Some(len) = libos::net_rx(self.device, self.buffer, RX_OFF) else {
                libos::exit(fail);
            };
            if len > 0 {
                return len as usize;
            }
            match libos::park(&SLOT, observed, TIMEOUT) {
                Some(true) => {}
                Some(false) | None => libos::exit(fail),
            }
        }
    }
}

#[unsafe(no_mangle)]
extern "C" fn _start(role: usize) -> ! {
    match role {
        0 => arp_round_trip(),
        1 => tcp_echo(),
        2 => disk_read(),
        _ => libos::exit(0xb3f),
    }
}

/// Role 2 (M27): read disk LBA 0 through the storage lane — find the
/// StorageDevice capability, request a buffer page, submit a one-block read,
/// poll (yielding between polls) until complete, and exit with a checksum of
/// the first four bytes (the kernel's M6 self-test pattern, "FEOX").
fn disk_read() -> ! {
    let Some(device) = libos::find_capability(CapType::StorageDevice) else {
        libos::exit(0xb50);
    };
    let Some(buffer) = libos::cap_request_pages(1, true) else {
        libos::exit(0xb51);
    };
    let Some(region) = libos::mem_map(buffer, 0, 4096, MapFlags::READ | MapFlags::WRITE) else {
        libos::exit(0xb52);
    };
    let Some(token) = libos::storage_submit_read(device, 1, 0, buffer, 0) else {
        libos::exit(0xb53);
    };
    let mut polls = 0u32;
    let completion = loop {
        match libos::storage_poll(token) {
            Some(Some(completion)) => break completion,
            Some(None) => {
                polls += 1;
                if polls > 100_000 {
                    libos::exit(0xb54);
                }
                libos::yield_now();
            }
            None => libos::exit(0xb55),
        }
    };
    if completion.nvme_sct != 0 || completion.nvme_sc != 0 {
        libos::exit(0xb56);
    }
    let base = region.base as *const u8;
    // SAFETY: the kernel DMA'd one block into our mapped capability page.
    let sum: usize = (0..4).map(|i| unsafe { base.add(i).read_volatile() } as usize).sum();
    libos::exit(0x5000 | (sum & 0xFFF))
}

/// Resolves the gateway MAC via ARP (the dest MAC for everything routed).
fn arp_gateway(net: &Net) -> [u8; 6] {
    // Ethernet + ARP request (42 bytes).
    net.write_tx(0, &[0xff; 6]); // broadcast
    net.write_tx(6, &net.mac);
    net.write_tx(12, &0x0806u16.to_be_bytes()); // ARP
    net.write_tx(14, &1u16.to_be_bytes()); // htype ethernet
    net.write_tx(16, &0x0800u16.to_be_bytes()); // ptype IPv4
    net.write_tx(18, &[6, 4]); // hlen, plen
    net.write_tx(20, &1u16.to_be_bytes()); // oper: request
    net.write_tx(22, &net.mac);
    net.write_tx(28, &GUEST_IP);
    net.write_tx(32, &[0; 6]);
    net.write_tx(38, &GATEWAY_IP);
    if !net.send(42) {
        libos::exit(0xb36);
    }

    loop {
        let len = net.recv(0xb35);
        if len < 42 {
            continue;
        }
        let is_arp = net.rx_byte(12) == 0x08 && net.rx_byte(13) == 0x06;
        let is_reply = net.rx_byte(20) == 0 && net.rx_byte(21) == 2;
        let from_gw = (0..4).all(|i| net.rx_byte(28 + i) == GATEWAY_IP[i]);
        if is_arp && is_reply && from_gw {
            let mut mac = [0u8; 6];
            for (i, byte) in mac.iter_mut().enumerate() {
                *byte = net.rx_byte(22 + i);
            }
            return mac;
        }
    }
}

/// Role 0: ARP who-has the gateway, entirely from user space.
fn arp_round_trip() -> ! {
    let net = Net::open();
    let mac = arp_gateway(&net);
    let mac_sum: usize = mac.iter().map(|&b| b as usize).sum();
    libos::exit(0x3000 | (mac_sum & 0xFFF));
}

// ---- role 1: hand-rolled TCP ------------------------------------------------

/// The CI echo service (QEMU guestfwd running `cat`).
const TCP_PEER_IP: [u8; 4] = [10, 0, 2, 100];
const TCP_PEER_PORT: u16 = 7777;
const TCP_SRC_PORT: u16 = 49152;
/// What we send; `cat` echoes it byte for byte.
const PAYLOAD: &[u8; 8] = b"FEOXTCP!";
/// Our initial send sequence number.
const ISS: u32 = 0x0001_0000;

const FLAG_FIN: u8 = 0x01;
const FLAG_SYN: u8 = 0x02;
const FLAG_ACK: u8 = 0x10;
const FLAG_PSH: u8 = 0x08;

/// RFC 1071 ones-complement sum over `data`, starting from `acc`.
fn csum_add(mut acc: u32, data: &[u8]) -> u32 {
    let mut i = 0;
    while i + 1 < data.len() {
        acc += u32::from(u16::from_be_bytes([data[i], data[i + 1]]));
        i += 2;
    }
    if i < data.len() {
        acc += u32::from(u16::from_be_bytes([data[i], 0]));
    }
    acc
}

fn csum_fold(mut acc: u32) -> u16 {
    while acc >> 16 != 0 {
        acc = (acc & 0xFFFF) + (acc >> 16);
    }
    !(acc as u16)
}

/// Builds and transmits one TCP segment (Ethernet + IPv4 + TCP + payload).
fn send_tcp(
    net: &Net,
    dst_mac: [u8; 6],
    ip_id: u16,
    flags: u8,
    seq: u32,
    ack: u32,
    payload: &[u8],
) {
    let tcp_len = 20 + payload.len();
    let tot_len = 20 + tcp_len;
    let mut f = [0u8; 96];
    // Ethernet.
    f[0..6].copy_from_slice(&dst_mac);
    f[6..12].copy_from_slice(&net.mac);
    f[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
    // IPv4.
    f[14] = 0x45;
    f[16..18].copy_from_slice(&(tot_len as u16).to_be_bytes());
    f[18..20].copy_from_slice(&ip_id.to_be_bytes());
    f[20..22].copy_from_slice(&0x4000u16.to_be_bytes()); // DF
    f[22] = 64; // TTL
    f[23] = 6; // TCP
    f[26..30].copy_from_slice(&GUEST_IP);
    f[30..34].copy_from_slice(&TCP_PEER_IP);
    let ip_csum = csum_fold(csum_add(0, &f[14..34]));
    f[24..26].copy_from_slice(&ip_csum.to_be_bytes());
    // TCP.
    f[34..36].copy_from_slice(&TCP_SRC_PORT.to_be_bytes());
    f[36..38].copy_from_slice(&TCP_PEER_PORT.to_be_bytes());
    f[38..42].copy_from_slice(&seq.to_be_bytes());
    f[42..46].copy_from_slice(&ack.to_be_bytes());
    f[46] = 5 << 4; // data offset: 5 words, no options
    f[47] = flags;
    f[48..50].copy_from_slice(&4096u16.to_be_bytes()); // window
    f[54..54 + payload.len()].copy_from_slice(payload);
    // TCP checksum over the pseudo-header + segment.
    let mut acc = csum_add(0, &GUEST_IP);
    acc = csum_add(acc, &TCP_PEER_IP);
    acc = csum_add(acc, &[0, 6]);
    acc = csum_add(acc, &(tcp_len as u16).to_be_bytes());
    acc = csum_add(acc, &f[34..34 + tcp_len]);
    let tcp_csum = csum_fold(acc);
    f[50..52].copy_from_slice(&tcp_csum.to_be_bytes());

    net.write_tx(0, &f[..14 + tot_len]);
    if !net.send((14 + tot_len) as u32) {
        libos::exit(0xb45);
    }
}

/// One parsed inbound segment from the peer.
struct Segment {
    seq: u32,
    ack: u32,
    flags: u8,
    payload: [u8; 64],
    payload_len: usize,
}

/// Receives the next TCP segment from the peer (skipping unrelated frames),
/// exiting `fail` on timeout.
fn recv_tcp(net: &Net, fail: usize) -> Segment {
    loop {
        let len = net.recv(fail);
        if len < 54 {
            continue;
        }
        // IPv4 + TCP from the peer, addressed to our connection?
        if net.rx_byte(12) != 0x08 || net.rx_byte(13) != 0x00 {
            continue;
        }
        if net.rx_byte(14) >> 4 != 4 || net.rx_byte(23) != 6 {
            continue;
        }
        if (0..4).any(|i| net.rx_byte(26 + i) != TCP_PEER_IP[i]) {
            continue;
        }
        let ihl = ((net.rx_byte(14) & 0x0F) as usize) * 4;
        let tcp = 14 + ihl;
        let sport = u16::from_be_bytes([net.rx_byte(tcp), net.rx_byte(tcp + 1)]);
        let dport = u16::from_be_bytes([net.rx_byte(tcp + 2), net.rx_byte(tcp + 3)]);
        if sport != TCP_PEER_PORT || dport != TCP_SRC_PORT {
            continue;
        }
        let read_u32 = |off: usize| {
            u32::from_be_bytes([
                net.rx_byte(off),
                net.rx_byte(off + 1),
                net.rx_byte(off + 2),
                net.rx_byte(off + 3),
            ])
        };
        let tot_len =
            u16::from_be_bytes([net.rx_byte(16), net.rx_byte(17)]) as usize;
        let data_off = ((net.rx_byte(tcp + 12) >> 4) as usize) * 4;
        let payload_start = tcp + data_off;
        let payload_len = tot_len.saturating_sub(ihl + data_off).min(64);
        let mut payload = [0u8; 64];
        for (i, slot) in payload.iter_mut().enumerate().take(payload_len) {
            *slot = net.rx_byte(payload_start + i);
        }
        return Segment {
            seq: read_u32(tcp + 4),
            ack: read_u32(tcp + 8),
            flags: net.rx_byte(tcp + 13),
            payload,
            payload_len,
        };
    }
}

/// Role 1: connect, echo 8 bytes, close — TCP by hand.
fn tcp_echo() -> ! {
    let net = Net::open();
    let gw_mac = arp_gateway(&net);
    let mut ip_id = 1u16;
    let mut tx = |id: &mut u16, flags, seq, ack, payload: &[u8]| {
        *id += 1;
        send_tcp(&net, gw_mac, *id, flags, seq, ack, payload);
    };

    // Three-way handshake.
    tx(&mut ip_id, FLAG_SYN, ISS, 0, &[]);
    let synack = recv_tcp(&net, 0xb41);
    if synack.flags & (FLAG_SYN | FLAG_ACK) != (FLAG_SYN | FLAG_ACK)
        || synack.ack != ISS.wrapping_add(1)
    {
        libos::exit(0xb44);
    }
    let mut rcv_nxt = synack.seq.wrapping_add(1);
    let snd_data = ISS.wrapping_add(1);
    tx(&mut ip_id, FLAG_ACK, snd_data, rcv_nxt, &[]);

    // Send the payload; collect the echo (any segmentation).
    tx(&mut ip_id, FLAG_PSH | FLAG_ACK, snd_data, rcv_nxt, PAYLOAD);
    let snd_fin = snd_data.wrapping_add(PAYLOAD.len() as u32);
    let mut echoed = [0u8; 8];
    let mut got = 0usize;
    let mut peer_fin: Option<u32> = None;
    while got < PAYLOAD.len() {
        let seg = recv_tcp(&net, 0xb42);
        if seg.payload_len > 0 && seg.seq == rcv_nxt {
            let take = seg.payload_len.min(PAYLOAD.len() - got);
            echoed[got..got + take].copy_from_slice(&seg.payload[..take]);
            got += take;
            rcv_nxt = rcv_nxt.wrapping_add(seg.payload_len as u32);
            tx(&mut ip_id, FLAG_ACK, snd_fin, rcv_nxt, &[]);
        } else if seg.payload_len > 0 {
            // Out-of-order / duplicate: re-assert our position.
            tx(&mut ip_id, FLAG_ACK, snd_fin, rcv_nxt, &[]);
        }
        if seg.flags & FLAG_FIN != 0 {
            peer_fin = Some(seg.seq.wrapping_add(seg.payload_len as u32));
        }
    }
    if echoed != *PAYLOAD {
        libos::exit(0xb46);
    }

    // Active close: FIN, then wait for our FIN's ACK or the peer's FIN.
    tx(&mut ip_id, FLAG_FIN | FLAG_ACK, snd_fin, rcv_nxt, &[]);
    let fin_acked = ISS.wrapping_add(PAYLOAD.len() as u32 + 2);
    loop {
        if let Some(fin_seq) = peer_fin {
            // ACK the peer's FIN (fin_seq already counts its payload) and
            // finish.
            rcv_nxt = fin_seq.wrapping_add(1);
            tx(&mut ip_id, FLAG_ACK, fin_acked, rcv_nxt, &[]);
            break;
        }
        let seg = recv_tcp(&net, 0xb43);
        if seg.flags & FLAG_FIN != 0 {
            peer_fin = Some(seg.seq.wrapping_add(seg.payload_len as u32));
            continue;
        }
        if seg.flags & FLAG_ACK != 0 && seg.ack == fin_acked {
            break; // our FIN is acknowledged; peer's FIN may follow, demo done
        }
    }

    let echo_sum: usize = echoed.iter().map(|&b| b as usize).sum();
    libos::exit(0x4000 | (echo_sum & 0xFFF))
}
