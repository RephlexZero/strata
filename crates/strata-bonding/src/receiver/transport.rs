//! # Transport-based Bonding Receiver
//!
//! Receives packets sent via `strata-transport` wire format across multiple
//! UDP links, decodes and recovers via the transport-layer receiver (FEC,
//! reordering), strips the bonding header, then feeds payloads into a
//! shared [`ReassemblyBuffer`] for multi-link jitter buffering.

use crate::protocol::header::BondingHeader;
use crate::receiver::aggregator::{Packet, ReassemblyBuffer, ReassemblyConfig, ReassemblyStats};
use anyhow::Result;
use bytes::{Bytes, BytesMut};
use crossbeam_channel::{bounded, Receiver, Sender};
use quanta::Instant;
use std::net::{SocketAddr, UdpSocket};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::thread;
use std::time::Duration;
use strata_transport::pool::TimestampClock;
use strata_transport::receiver::{Receiver as TransportReceiver, ReceiverConfig, ReceiverEvent};
use strata_transport::session::RttTracker;
use strata_transport::wire::{ControlBody, Packet as WirePacket, PacketHeader};
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
    /// Spawns a reader thread running a monoio event loop (io_uring on
    /// Linux ≥5.1, epoll fallback) that asynchronously receives datagrams,
    /// decodes them through the transport receiver, and feeds results into
    /// the shared reassembly buffer.
    pub fn add_link(&self, bind_addr: SocketAddr) -> Result<()> {
        let socket = UdpSocket::bind(bind_addr)?;
        self.add_link_socket(socket)
    }

    /// Add a link from an already-bound UDP socket.
    pub fn add_link_socket(&self, socket: UdpSocket) -> Result<()> {
        let local_addr = socket.local_addr()?;

        let input_tx = self
            .input_tx
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Receiver shut down"))?
            .clone();
        let running = self.running.clone();
        let stats = self.stats.clone();

        let handle = thread::Builder::new()
            .name(format!("strata-rcv-{}", local_addr))
            .spawn(move || {
                let mut rt = crate::build_monoio_runtime!();
                rt.block_on(async move {
                    let mono_socket = monoio::net::udp::UdpSocket::from_std(socket)
                        .expect("failed to convert socket for monoio");
                    link_reader_async(mono_socket, input_tx, running, stats).await;
                });
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

    /// Returns a shared handle to the reassembly stats for external polling
    /// (e.g., Prometheus metrics server).
    pub fn stats_handle(&self) -> Arc<Mutex<ReassemblyStats>> {
        self.stats.clone()
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

/// Per-link reader loop (async, runs on a monoio event loop).
///
/// Uses io_uring (or epoll fallback) for async UDP receives, feeding
/// datagrams into a `strata_transport::Receiver` for FEC decoding
/// and reorder. Delivered payloads have the bonding header stripped
/// and are pushed into the shared reassembly channel.
async fn link_reader_async(
    socket: monoio::net::udp::UdpSocket,
    input_tx: Sender<Packet>,
    running: Arc<AtomicBool>,
    reassembly_stats: Arc<Mutex<ReassemblyStats>>,
) {
    let mut transport_rx = TransportReceiver::new(ReceiverConfig::default());
    let mut buf = vec![0u8; 65536];
    let clock = TimestampClock::new();
    let mut last_ack = std::time::Instant::now();
    let ack_interval = Duration::from_millis(50);
    let mut last_report = std::time::Instant::now();
    let report_interval = Duration::from_secs(1);
    // Track bytes delivered for goodput calculation.
    let mut prev_bytes_delivered: u64 = 0;
    let mut prev_report_time = std::time::Instant::now();
    // Most recently seen sender address on this socket.
    let mut sender_addr: Option<std::net::SocketAddr> = None;

    while running.load(Ordering::Relaxed) {
        // Await next datagram with a timeout so we can check the running flag.
        match monoio::time::timeout(Duration::from_millis(50), socket.recv_from(buf)).await {
            Ok((Ok((n, addr)), returned_buf)) => {
                sender_addr = Some(addr);
                let raw = Bytes::copy_from_slice(&returned_buf[..n]);

                // Check for control packets (Ping) before handing to transport_rx.
                // Respond with Pong immediately.
                if let Some(pong_bytes) = try_make_pong(&returned_buf[..n], &clock) {
                    let _ = socket.send_to(pong_bytes, addr).await;
                }

                transport_rx.receive(raw);
                buf = returned_buf;

                for event in transport_rx.drain_events() {
                    match event {
                        ReceiverEvent::Deliver(delivered) => {
                            if let Some((header, original_payload)) =
                                BondingHeader::unwrap(delivered.payload)
                            {
                                let packet = Packet {
                                    seq_id: header.seq_id,
                                    payload: original_payload,
                                    arrival_time: quanta::Instant::now(),
                                };
                                if input_tx.send(packet).is_err() {
                                    return;
                                }
                            } else {
                                debug!("Dropped packet with invalid bonding header");
                            }
                        }
                        ReceiverEvent::SendAck(ack) => {
                            if let Some(addr) = sender_addr {
                                let pkt_bytes = encode_control_packet(&ack, &clock);
                                let _ = socket.send_to(pkt_bytes, addr).await;
                            }
                        }
                        ReceiverEvent::SendNack(nack) => {
                            if let Some(addr) = sender_addr {
                                let pkt_bytes = encode_nack_packet(&nack, &clock);
                                let _ = socket.send_to(pkt_bytes, addr).await;
                            }
                        }
                    }
                }

                // Periodically generate and send ACKs.
                if last_ack.elapsed() >= ack_interval {
                    let ack = transport_rx.generate_ack();
                    if let Some(addr) = sender_addr {
                        let pkt_bytes = encode_control_packet(&ack, &clock);
                        let _ = socket.send_to(pkt_bytes, addr).await;
                    }
                    // Also generate NACKs for missing packets.
                    if let Some(nack) = transport_rx.generate_nacks() {
                        if let Some(addr) = sender_addr {
                            let pkt_bytes = encode_nack_packet(&nack, &clock);
                            let _ = socket.send_to(pkt_bytes, addr).await;
                        }
                    }
                    last_ack = std::time::Instant::now();
                }

                // Periodically send ReceiverReport.
                if last_report.elapsed() >= report_interval {
                    if let Some(addr) = sender_addr {
                        let rx_stats = transport_rx.stats();
                        let now = std::time::Instant::now();
                        let dt = now.duration_since(prev_report_time).as_secs_f64();

                        // Compute goodput from bytes delivered since last report
                        let cur_bytes = rx_stats.bytes_received;
                        let delta_bytes = cur_bytes.saturating_sub(prev_bytes_delivered);
                        let goodput_bps = if dt > 0.01 {
                            ((delta_bytes as f64 * 8.0) / dt) as u64
                        } else {
                            0
                        };
                        prev_bytes_delivered = cur_bytes;
                        prev_report_time = now;

                        // FEC repair rate
                        let total = rx_stats.packets_received.max(1);
                        let fec_rate =
                            (rx_stats.fec_recoveries as f64 / total as f64).clamp(0.0, 1.0);

                        // Jitter buffer depth from reassembly stats
                        let jitter_ms = reassembly_stats
                            .lock()
                            .map(|s| s.current_latency_ms as u32)
                            .unwrap_or(0);

                        // Residual loss: (lost - fec_recovered) / total, after FEC
                        let delivered = rx_stats.packets_delivered;
                        let residual = total.saturating_sub(delivered);
                        let loss_after_fec = (residual as f64 / total as f64).clamp(0.0, 1.0);

                        let report = strata_transport::wire::ReceiverReportPacket {
                            goodput_bps,
                            fec_repair_rate: (fec_rate * 10000.0) as u16,
                            jitter_buffer_ms: jitter_ms,
                            loss_after_fec: (loss_after_fec * 10000.0) as u16,
                        };
                        let pkt_bytes = encode_receiver_report(&report, &clock);
                        let _ = socket.send_to(pkt_bytes, addr).await;
                    }
                    last_report = std::time::Instant::now();
                }
            }
            Ok((Err(e), returned_buf)) => {
                buf = returned_buf;
                warn!("Link reader recv error: {}", e);
                monoio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(_elapsed) => {
                // Timeout — re-check running flag. Buffer ownership was consumed
                // by the cancelled io_uring op; allocate a fresh one.
                buf = vec![0u8; 65536];

                // Still send periodic ACKs even when idle.
                if last_ack.elapsed() >= ack_interval {
                    if let Some(addr) = sender_addr {
                        let ack = transport_rx.generate_ack();
                        let pkt_bytes = encode_control_packet(&ack, &clock);
                        let _ = socket.send_to(pkt_bytes, addr).await;
                    }
                    last_ack = std::time::Instant::now();
                }
            }
        }
    }
}

/// Try to decode a Ping control packet and produce a Pong response.
fn try_make_pong(data: &[u8], clock: &TimestampClock) -> Option<Vec<u8>> {
    use strata_transport::wire::Packet as WP;
    use strata_transport::wire::PacketType;
    let mut cursor: &[u8] = data;
    let pkt = WP::decode(&mut cursor)?;
    if pkt.header.packet_type != PacketType::Control {
        return None;
    }
    let mut payload_cursor = &pkt.payload[..];
    if let Some(ControlBody::Ping(ping)) = ControlBody::decode(&mut payload_cursor) {
        let pong = RttTracker::make_pong(&ping, clock.now_us());
        let mut body = BytesMut::with_capacity(16);
        pong.encode(&mut body);
        let body_bytes = body.freeze();
        let header = PacketHeader::control(0, clock.now_us(), body_bytes.len() as u16);
        let pkt = WirePacket {
            header,
            payload: body_bytes,
        };
        Some(pkt.encode().to_vec())
    } else {
        None
    }
}

/// Encode an ACK as a wire-format control packet.
fn encode_control_packet(
    ack: &strata_transport::wire::AckPacket,
    clock: &TimestampClock,
) -> Vec<u8> {
    let mut body = BytesMut::with_capacity(16);
    ack.encode(&mut body);
    let body_bytes = body.freeze();
    let header = PacketHeader::control(0, clock.now_us(), body_bytes.len() as u16);
    let pkt = WirePacket {
        header,
        payload: body_bytes,
    };
    pkt.encode().to_vec()
}

/// Encode a NACK as a wire-format control packet.
fn encode_nack_packet(
    nack: &strata_transport::wire::NackPacket,
    clock: &TimestampClock,
) -> Vec<u8> {
    let mut body = BytesMut::with_capacity(64);
    nack.encode(&mut body);
    let body_bytes = body.freeze();
    let header = PacketHeader::control(0, clock.now_us(), body_bytes.len() as u16);
    let pkt = WirePacket {
        header,
        payload: body_bytes,
    };
    pkt.encode().to_vec()
}

/// Encode a ReceiverReport as a wire-format control packet.
fn encode_receiver_report(
    report: &strata_transport::wire::ReceiverReportPacket,
    clock: &TimestampClock,
) -> Vec<u8> {
    let mut body = BytesMut::with_capacity(24);
    report.encode(&mut body);
    let body_bytes = body.freeze();
    let header = PacketHeader::control(0, clock.now_us(), body_bytes.len() as u16);
    let pkt = WirePacket {
        header,
        payload: body_bytes,
    };
    pkt.encode().to_vec()
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
