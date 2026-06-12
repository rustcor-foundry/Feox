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

use feox_asi::{CapHandle, CapType, Duration, EventSlot, MapFlags};
use feox_libos as libos;

/// QEMU user-net addresses (the kernel's M8 stack uses the same).
const GUEST_IP: [u8; 4] = [10, 0, 2, 15];
const GATEWAY_IP: [u8; 4] = [10, 0, 2, 2];

/// Packet buffer layout within the one-page capability: TX frame at 0,
/// RX landing area at 2048.
const TX_OFF: u64 = 0;
const RX_OFF: u64 = 2048;

const TIMEOUT: Duration = Duration::from_nanos(2_000_000_000);

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
        _ => libos::exit(0xb3f),
    }
}

/// Role 0: ARP who-has the gateway, entirely from user space.
fn arp_round_trip() -> ! {
    let net = Net::open();

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
            let mac_sum: usize = (0..6).map(|i| net.rx_byte(22 + i) as usize).sum();
            libos::exit(0x3000 | (mac_sum & 0xFFF));
        }
    }
}
