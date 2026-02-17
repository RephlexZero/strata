//! # Transport-based Bonding Receiver
//!
//! Receives packets sent via `strata-transport` wire format across multiple
//! UDP links, decodes and recovers via the transport-layer receiver (FEC,
//! reordering), strips the bonding header, then feeds payloads into a
//! shared [`ReassemblyBuffer`] for multi-link jitter buffering.

use crate::protocol::header::BondingHeader;
use crate::receiver::aggregator::{Packet, ReassemblyBuffer, ReassemblyConfig, ReassemblyStats};
use anyhow::Result;
use bytes::Bytes;
use crossbeam_channel::{bounded, Receiver, Sender};
use std::net::{SocketAddr, UdpSocket};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::thread;
use quanta::Instant;
use std::time::Duration;
use strata_transport::receiver::{Receiver as TransportReceiver, ReceiverConfig, ReceiverEvent};
use tracing::{debug, warn};

/// Multi-link bonding receiver backed by `strata-transport`.
///
/// Each link binds a UDP socket and spawns a reader thread that:
/// 1. Receives raw wire-format bytes from the network
/// 2. Feeds them to a per-link `strata_transport::Receiver` for FEC decoding
/// 3. Strips the `BondingHeader` from delivered payloads
/// 4. Pushes packets into a shared `ReassemblyBuffer` for multi-link ordering
///
/// The jitter-buffer thread ticks the buffer and emits ordered payloads on
/// `output_rx`, matching the same interface as the legacy `BondingReceiver`.
pub struct TransportBondingReceiver {
    input_tx: Option<Sender<Packet>>,
    output_tx: Option<Sender<Bytes>>,
    /// Public so GStreamer (or any consumer) can pull ordered payloads.
    pub output_rx: Receiver<Bytes>,
    running: Arc<AtomicBool>,
    stats: Arc<Mutex<ReassemblyStats>>,
    thread_handles: Mutex<Vec<thread::JoinHandle<()>>>,
}

impl TransportBondingReceiver {
    pub fn new(latency: Duration) -> Self {
        Self::new_with_config(ReassemblyConfig {
            start_latency: latency,
            ..ReassemblyConfig::default()
        })
    }

    pub fn new_with_config(config: ReassemblyConfig) -> Self {
        let (output_tx, output_rx) = bounded(100);
        let (input_tx, input_rx) = bounded::<Packet>(1000);
        let running = Arc::new(AtomicBool::new(true));
        let stats = Arc::new(Mutex::new(ReassemblyStats::default()));

        let stats_clone = stats.clone();
        let running_clone = running.clone();
        let output_tx_clone = output_tx.clone();

        let jitter_handle = thread::Builder::new()
            .name("strata-rcv-jitter".into())
            .spawn(move || {
                let mut buffer = ReassemblyBuffer::with_config(0, config);
                let tick_interval = Duration::from_millis(10);

                while running_clone.load(Ordering::Relaxed) {
                    match input_rx.recv_timeout(tick_interval) {
                        Ok(packet) => {
                            buffer.push(packet.seq_id, packet.payload, packet.arrival_time);
                        }
                        Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                        Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                    }

                    let now = Instant::now();
                    let ready = buffer.tick(now);

                    if let Ok(mut s) = stats_clone.lock() {
                        *s = buffer.get_stats();
                    }

                    for p in ready {
                        if output_tx_clone.send(p).is_err() {
                            return;
                        }
                    }
                }
            })
            .expect("failed to spawn jitter buffer thread");

        Self {
            input_tx: Some(input_tx),
            output_tx: Some(output_tx),
            output_rx,
            running,
            stats,
            thread_handles: Mutex::new(vec![jitter_handle]),
        }
    }

    /// Add a link by binding a UDP socket to `bind_addr`.
    ///
    /// Spawns a reader thread that receives strata-transport wire-format
    /// packets, decodes them through the transport receiver (FEC + reorder),
    /// extracts application data, strips the bonding header, and feeds the
    /// result into the shared reassembly buffer.
    pub fn add_link(&self, bind_addr: SocketAddr) -> Result<()> {
        let socket = UdpSocket::bind(bind_addr)?;
        socket.set_read_timeout(Some(Duration::from_millis(50)))?;

        // Enable SO_BUSY_POLL for reduced receive latency
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::io::AsRawFd;
            let fd = socket.as_raw_fd();
            let poll_us: libc::c_int = 50; // 50µs busy-poll budget
            unsafe {
                libc::setsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    libc::SO_BUSY_POLL,
                    &poll_us as *const _ as *const libc::c_void,
                    std::mem::size_of::<libc::c_int>() as libc::socklen_t,
                );
            }
        }

        let input_tx = self
            .input_tx
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Receiver shut down"))?
            .clone();
        let running = self.running.clone();

        let handle = thread::Builder::new()
            .name(format!("strata-rcv-{}", bind_addr))
            .spawn(move || {
                link_reader(socket, input_tx, running);
            })?;

        if let Ok(mut handles) = self.thread_handles.lock() {
            handles.push(handle);
        }

        Ok(())
    }

    /// Add a link from an already-bound UDP socket.
    pub fn add_link_socket(&self, socket: UdpSocket) -> Result<()> {
        socket.set_read_timeout(Some(Duration::from_millis(50)))?;

        let input_tx = self
            .input_tx
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Receiver shut down"))?
            .clone();
        let running = self.running.clone();

        let handle = thread::Builder::new()
            .name("strata-rcv-link".into())
            .spawn(move || {
                link_reader(socket, input_tx, running);
            })?;

        if let Ok(mut handles) = self.thread_handles.lock() {
            handles.push(handle);
        }

        Ok(())
    }

    pub fn shutdown(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        self.input_tx = None;
        self.output_tx = None;
        if let Ok(mut handles) = self.thread_handles.lock() {
            for handle in handles.drain(..) {
                let _ = handle.join();
            }
        }
    }

    pub fn get_stats(&self) -> ReassemblyStats {
        self.stats.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Check if the receiver is still running.
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }
}

impl Drop for TransportBondingReceiver {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
    }
}

/// Per-link reader loop.
///
/// Reads raw UDP datagrams, feeds them to a `strata_transport::Receiver`,
/// then processes delivered packets: strips the bonding header and pushes
/// them into the shared reassembly channel.
fn link_reader(socket: UdpSocket, input_tx: Sender<Packet>, running: Arc<AtomicBool>) {
    let mut transport_rx = TransportReceiver::new(ReceiverConfig::default());
    let mut buf = vec![0u8; 65536];

    while running.load(Ordering::Relaxed) {
        match socket.recv_from(&mut buf) {
            Ok((n, _addr)) => {
                let raw = Bytes::copy_from_slice(&buf[..n]);
                transport_rx.receive(raw);

                for event in transport_rx.drain_events() {
                    if let ReceiverEvent::Deliver(delivered) = event {
                        // The delivered payload is: BondingHeader + original payload
                        if let Some((header, original_payload)) =
                            BondingHeader::unwrap(delivered.payload)
                        {
                            let packet = Packet {
                                seq_id: header.seq_id,
                                payload: original_payload,
                                arrival_time: Instant::now(),
                            };
                            if input_tx.send(packet).is_err() {
                                return;
                            }
                        } else {
                            debug!("Dropped packet with invalid bonding header");
                        }
                    }
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                // Timeout — continue loop
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => {
                // Timeout on Windows-style timeout
            }
            Err(e) => {
                warn!("Link reader recv error: {}", e);
                thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::UdpSocket;

    #[test]
    fn new_receiver_has_empty_stats() {
        let rcv = TransportBondingReceiver::new(Duration::from_millis(50));
        let stats = rcv.get_stats();
        assert_eq!(stats.lost_packets, 0);
        assert_eq!(stats.late_packets, 0);
    }

    #[test]
    fn add_link_binds_successfully() {
        let rcv = TransportBondingReceiver::new(Duration::from_millis(50));
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        assert!(rcv.add_link(addr).is_ok());
    }

    #[test]
    fn shutdown_is_clean() {
        let mut rcv = TransportBondingReceiver::new(Duration::from_millis(50));
        rcv.add_link("127.0.0.1:0".parse().unwrap()).unwrap();
        rcv.shutdown();
        assert!(!rcv.is_running());
    }

    #[test]
    fn add_link_socket_works() {
        let rcv = TransportBondingReceiver::new(Duration::from_millis(50));
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        assert!(rcv.add_link_socket(socket).is_ok());
    }

    #[test]
    fn loopback_send_receive() {
        // Create receiver
        let rcv = TransportBondingReceiver::new(Duration::from_millis(20));
        let rcv_socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let rcv_addr = rcv_socket.local_addr().unwrap();
        rcv.add_link_socket(rcv_socket).unwrap();

        // Create a TransportLink sender
        let send_socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        send_socket.connect(rcv_addr).unwrap();
        let sender = crate::net::transport::TransportLink::new(
            0,
            send_socket,
            strata_transport::sender::SenderConfig::default(),
        );

        // Build bonding-header-wrapped payload and send
        let payload = Bytes::from_static(b"hello strata");
        let header = crate::protocol::header::BondingHeader::new(0);
        let wrapped = header.wrap(payload.clone());

        use crate::net::interface::LinkSender;
        sender.send(&wrapped).unwrap();

        // Wait for the packet to arrive and be processed
        match rcv.output_rx.recv_timeout(Duration::from_secs(2)) {
            Ok(received) => {
                assert_eq!(received, payload);
            }
            Err(e) => {
                panic!("Did not receive packet within timeout: {}", e);
            }
        }
    }

    #[test]
    fn multi_packet_ordering() {
        let rcv = TransportBondingReceiver::new(Duration::from_millis(20));
        let rcv_socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let rcv_addr = rcv_socket.local_addr().unwrap();
        rcv.add_link_socket(rcv_socket).unwrap();

        let send_socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        send_socket.connect(rcv_addr).unwrap();
        let sender = crate::net::transport::TransportLink::new(
            0,
            send_socket,
            strata_transport::sender::SenderConfig::default(),
        );

        use crate::net::interface::LinkSender;

        let count = 10;
        for i in 0..count {
            let payload = Bytes::from(format!("packet-{}", i));
            let header = crate::protocol::header::BondingHeader::new(i);
            let wrapped = header.wrap(payload);
            sender.send(&wrapped).unwrap();
        }

        // Collect all received packets
        let mut received = Vec::new();
        for _ in 0..count {
            match rcv.output_rx.recv_timeout(Duration::from_secs(3)) {
                Ok(data) => received.push(data),
                Err(_) => break,
            }
        }

        assert_eq!(
            received.len(),
            count as usize,
            "Expected {} packets, got {}",
            count,
            received.len()
        );

        // Verify order
        for (i, data) in received.iter().enumerate() {
            let expected = format!("packet-{}", i);
            assert_eq!(data, &Bytes::from(expected), "packet {} mismatch", i);
        }
    }

    #[test]
    fn add_link_after_shutdown_fails() {
        let mut rcv = TransportBondingReceiver::new(Duration::from_millis(50));
        rcv.shutdown();
        let result = rcv.add_link("127.0.0.1:0".parse().unwrap());
        assert!(result.is_err());
    }
}
