//! # Transport Adapter
//!
//! Bridges `strata-transport`'s Sender/Receiver with the bonding crate's
//! `LinkSender` trait. This adapter encapsulates the wire-format encode/decode
//! and reliability layer (FEC + ARQ) behind the existing scheduling interface.
//!
//! Uses `quinn-udp` for GSO (Generic Segmentation Offload) batched sends,
//! reducing per-packet syscall overhead.

use anyhow::Result;
use bytes::{Bytes, BytesMut};
use quinn_udp::{Transmit, UdpSockRef, UdpSocketState};
use std::net::UdpSocket;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::net::interface::{LinkMetrics, LinkPhase, LinkSender};
use strata_transport::congestion::BiscayController;
use strata_transport::pool::Priority;
use strata_transport::pool::TimestampClock;
use strata_transport::sender::{Sender, SenderConfig};
use strata_transport::session::RttTracker;
use strata_transport::wire::{Packet, PacketHeader, ReceiverReportPacket};

/// A link backed by `strata-transport::Sender`.
///
/// Uses `quinn-udp` for GSO/GRO-accelerated UDP I/O when the kernel supports
/// it (Linux 4.18+ for GSO, 5.13+ for GRO).
pub struct TransportLink {
    /// Unique link ID.
    id: usize,
    /// The strata-transport sender (handles FEC, ARQ, wire format).
    sender: Mutex<Sender>,
    /// RTT tracker for this link.
    rtt: Mutex<RttTracker>,
    /// Clock for generating timestamps.
    clock: Mutex<TimestampClock>,
    /// Biscay congestion controller (BBR-based capacity estimation).
    congestion: Mutex<BiscayController>,
    /// UDP socket for this link.
    socket: UdpSocket,
    /// quinn-udp socket state for GSO/GRO.
    udp_state: UdpSocketState,
    /// Remote peer address (for quinn-udp Transmit).
    peer_addr: std::net::SocketAddr,
    /// Total bytes sent through this link.
    bytes_sent: AtomicU64,
    /// Total packets sent.
    packets_sent: AtomicU64,
    /// Snapshot of `bytes_sent` at the last rate computation.
    prev_rate_bytes: AtomicU64,
    /// Microsecond timestamp of the last rate computation.
    prev_rate_time_us: AtomicU64,
    /// EWMA-smoothed socket-level sending rate (bits/sec).
    rate_ewma_bps: Mutex<f64>,
    /// Cumulative bytes acknowledged (for delivery rate tracking).
    bytes_acked: AtomicU64,
    /// Microsecond timestamp of last delivery rate sample.
    prev_ack_time_us: AtomicU64,
    /// Snapshot of bytes_acked at last delivery rate sample.
    prev_ack_bytes: AtomicU64,
    /// Latest receiver report from the remote receiver (if any).
    receiver_report: Mutex<Option<ReceiverReportPacket>>,
    /// Network interface name (e.g. "eth1").
    iface: Option<String>,
}

impl TransportLink {
    /// Create a new transport link.
    ///
    /// `socket` should be bound and connected to the remote peer.
    pub fn new(id: usize, socket: UdpSocket, config: SenderConfig, iface: Option<String>) -> Self {
        let peer_addr = socket
            .peer_addr()
            .expect("socket must be connected before creating TransportLink");
        // Non-blocking so recv_feedback() can poll without stalling the worker.
        socket.set_nonblocking(true).ok();
        let udp_state = UdpSocketState::new(UdpSockRef::from(&socket))
            .expect("failed to initialize quinn-udp socket state");
        TransportLink {
            id,
            sender: Mutex::new(Sender::new(config)),
            rtt: Mutex::new(RttTracker::new()),
            clock: Mutex::new(TimestampClock::new()),
            congestion: Mutex::new(BiscayController::new()),
            socket,
            udp_state,
            peer_addr,
            bytes_sent: AtomicU64::new(0),
            packets_sent: AtomicU64::new(0),
            prev_rate_bytes: AtomicU64::new(0),
            prev_rate_time_us: AtomicU64::new(0),
            rate_ewma_bps: Mutex::new(0.0),
            bytes_acked: AtomicU64::new(0),
            prev_ack_time_us: AtomicU64::new(0),
            prev_ack_bytes: AtomicU64::new(0),
            receiver_report: Mutex::new(None),
            iface,
        }
    }

    /// Send data through the transport layer (encode → wire → socket).
    ///
    /// Uses GSO batching when outputs have uniform segment size.
    fn transport_send(&self, data: &[u8], priority: Priority) -> Result<usize> {
        let mut sender = self.sender.lock().unwrap();

        sender.send(Bytes::copy_from_slice(data), priority);

        let outputs: Vec<_> = sender.drain_output().collect();
        let total_bytes = self.send_batch(&outputs);

        self.bytes_sent
            .fetch_add(total_bytes as u64, Ordering::Relaxed);
        self.packets_sent.fetch_add(1, Ordering::Relaxed);

        Ok(total_bytes)
    }

    /// Batch-send outputs via quinn-udp with GSO when possible.
    fn send_batch(&self, outputs: &[strata_transport::sender::OutputPacket]) -> usize {
        if outputs.is_empty() {
            return 0;
        }

        let max_gso = self.udp_state.max_gso_segments();
        let mut total_bytes = 0;

        if max_gso > 1 {
            // Try GSO: group consecutive same-size outputs into batches
            let mut i = 0;
            while i < outputs.len() {
                let seg_len = outputs[i].data.len();
                let mut end = i + 1;

                // Collect consecutive outputs with the same length (up to max_gso)
                while end < outputs.len()
                    && outputs[end].data.len() == seg_len
                    && (end - i) < max_gso
                {
                    end += 1;
                }

                if end - i > 1 {
                    // GSO batch: concatenate into a single buffer
                    let mut buf = Vec::with_capacity(seg_len * (end - i));
                    for output in &outputs[i..end] {
                        buf.extend_from_slice(&output.data);
                    }

                    let transmit = Transmit {
                        destination: self.peer_addr,
                        ecn: None,
                        contents: &buf,
                        segment_size: Some(seg_len),
                        src_ip: None,
                    };

                    match self
                        .udp_state
                        .send(UdpSockRef::from(&self.socket), &transmit)
                    {
                        Ok(()) => total_bytes += buf.len(),
                        Err(e) => {
                            tracing::warn!(link_id = self.id, error = %e, "GSO send failed, falling back");
                            // Fallback: send individually
                            for output in &outputs[i..end] {
                                total_bytes += self.send_single(&output.data);
                            }
                        }
                    }
                } else {
                    // Single packet — no GSO needed
                    total_bytes += self.send_single(&outputs[i].data);
                }
                i = end;
            }
        } else {
            // No GSO support — send individually
            for output in outputs {
                total_bytes += self.send_single(&output.data);
            }
        }

        total_bytes
    }

    /// Send a single datagram via quinn-udp.
    fn send_single(&self, data: &[u8]) -> usize {
        let transmit = Transmit {
            destination: self.peer_addr,
            ecn: None,
            contents: data,
            segment_size: None,
            src_ip: None,
        };

        match self
            .udp_state
            .send(UdpSockRef::from(&self.socket), &transmit)
        {
            Ok(()) => data.len(),
            Err(e) => {
                tracing::warn!(link_id = self.id, error = %e, "send failed");
                0
            }
        }
    }

    /// Process an incoming ACK/NACK packet from the receiver.
    pub fn process_feedback(&self, data: &[u8]) -> Result<()> {
        use strata_transport::wire::{ControlBody, Packet, PacketType};

        let mut cursor = data;
        let packet = Packet::decode(&mut cursor)
            .ok_or_else(|| anyhow::anyhow!("failed to decode feedback packet"))?;

        if packet.header.packet_type != PacketType::Control {
            return Ok(());
        }

        let mut payload_cursor = &packet.payload[..];
        if let Some(ctrl) = ControlBody::decode(&mut payload_cursor) {
            let mut sender = self.sender.lock().unwrap();
            match &ctrl {
                ControlBody::Ack(ack) => {
                    let newly_acked = sender.process_ack(ack);
                    // Feed BiscayController with delivery rate sample.
                    // Approximate delivered bytes from newly ACKed packets × avg payload.
                    if newly_acked > 0 {
                        let avg_payload = 1200u64; // conservative MTU-sized estimate
                        let delivered_bytes = newly_acked as u64 * avg_payload;
                        self.bytes_acked
                            .fetch_add(delivered_bytes, Ordering::Relaxed);

                        // Compute delivery rate over the inter-ACK interval
                        let now_us = self.clock.lock().unwrap().now_us() as u64;
                        let total_acked = self.bytes_acked.load(Ordering::Relaxed);
                        let prev_bytes = self.prev_ack_bytes.load(Ordering::Relaxed);
                        let prev_us = self.prev_ack_time_us.load(Ordering::Relaxed);
                        let interval_us = now_us.saturating_sub(prev_us);

                        // Accumulate at least 10ms of ACKs to avoid ACK compression spikes
                        if interval_us >= 10_000 && prev_us > 0 {
                            self.prev_ack_bytes.store(total_acked, Ordering::Relaxed);
                            self.prev_ack_time_us.store(now_us, Ordering::Relaxed);
                            let delta_bytes = total_acked.saturating_sub(prev_bytes);
                            let mut cc = self.congestion.lock().unwrap();
                            cc.on_bandwidth_sample(delta_bytes, interval_us);
                        } else if prev_us == 0 {
                            self.prev_ack_bytes.store(total_acked, Ordering::Relaxed);
                            self.prev_ack_time_us.store(now_us, Ordering::Relaxed);
                        }
                    }
                }
                ControlBody::Nack(nack) => {
                    sender.process_nack(nack);
                }
                ControlBody::Pong(pong) => {
                    let mut rtt = self.rtt.lock().unwrap();
                    rtt.handle_pong(pong);
                    // Feed RTT sample to BiscayController
                    let rtt_us = rtt.srtt_us();
                    if rtt_us > 0.0 {
                        let mut cc = self.congestion.lock().unwrap();
                        cc.on_rtt_sample(rtt_us);
                    }
                }
                ControlBody::ReceiverReport(report) => {
                    *self.receiver_report.lock().unwrap() = Some(report.clone());
                }
                _ => {}
            }
        }

        Ok(())
    }

    /// Flush any pending FEC repair packets.
    pub fn flush_fec(&self) -> Result<usize> {
        let mut sender = self.sender.lock().unwrap();
        sender.flush_fec();

        let outputs: Vec<_> = sender.drain_output().collect();
        let total_bytes = self.send_batch(&outputs);

        Ok(total_bytes)
    }

    /// Get the current RTT estimate in milliseconds.
    pub fn rtt_ms(&self) -> f64 {
        let rtt = self.rtt.lock().unwrap();
        rtt.srtt_us() / 1000.0
    }

    /// Get the latest receiver report, if any.
    pub fn latest_receiver_report(&self) -> Option<ReceiverReportPacket> {
        self.receiver_report.lock().unwrap().clone()
    }
}

impl LinkSender for TransportLink {
    fn id(&self) -> usize {
        self.id
    }

    fn send(&self, packet: &[u8]) -> Result<usize> {
        self.transport_send(packet, Priority::Standard)
    }

    fn get_metrics(&self) -> LinkMetrics {
        let sender = self.sender.lock().unwrap();
        let rtt = self.rtt.lock().unwrap();
        let rtt_ms = rtt.srtt_us() / 1000.0;

        // --- Socket-level rate (includes FEC/retransmit overhead) ---
        let total_bytes = self.bytes_sent.load(Ordering::Relaxed);
        let now_us = self.clock.lock().unwrap().now_us() as u64;
        let prev_bytes = self.prev_rate_bytes.swap(total_bytes, Ordering::Relaxed);
        let prev_us = self.prev_rate_time_us.swap(now_us, Ordering::Relaxed);
        let dt_s = now_us.saturating_sub(prev_us) as f64 / 1_000_000.0;

        let socket_rate_bps = if dt_s >= 0.01 {
            let delta = total_bytes.saturating_sub(prev_bytes);
            (delta as f64 * 8.0) / dt_s
        } else {
            0.0
        };

        let mut ewma = self.rate_ewma_bps.lock().unwrap();
        if socket_rate_bps > 0.0 {
            if *ewma == 0.0 {
                *ewma = socket_rate_bps;
            } else {
                *ewma = 0.2 * socket_rate_bps + 0.8 * *ewma;
            }
        }
        let observed_bps = *ewma;

        // --- Capacity estimate from BiscayController (BBR-based) ---
        // Uses delivery-rate measurement from ACK feedback rather than
        // the Mathis formula. BtlBw is the windowed max of recent
        // delivery rate samples (bytes/sec), converted to bits/sec.
        let cc = self.congestion.lock().unwrap();
        let btl_bw_bps = cc.btl_bw() * 8.0;
        let capacity_bps = if btl_bw_bps > 0.0 {
            btl_bw_bps.clamp(100_000.0, 50_000_000.0)
        } else {
            0.0 // No data yet — scheduler will use capacity floor
        };

        // Only report transport-level loss when we have receiver feedback
        // (packets_acked > 0). Use retransmission ratio instead of the
        // unacked ratio — unacked counts in-flight packets as "lost" which
        // is misleading on high-BDP links.
        let stats = sender.stats();
        let loss_rate = if stats.packets_acked > 0 && stats.packets_sent > 0 {
            stats.retransmit_ratio()
        } else {
            0.0
        };

        LinkMetrics {
            rtt_ms,
            capacity_bps,
            loss_rate,
            observed_bps,
            observed_bytes: total_bytes,
            queue_depth: sender.output_queue_len(),
            max_queue: 0,
            alive: true,
            phase: LinkPhase::Live,
            os_up: Some(true),
            mtu: None,
            iface: self.iface.clone(),
            link_kind: Some("strata-transport".into()),
            transport: Some(crate::net::interface::TransportMetrics {
                packets_sent: stats.packets_sent,
                packets_acked: stats.packets_acked,
                retransmissions: stats.retransmissions,
                fec_repairs_sent: stats.fec_repairs_sent,
                packets_expired: stats.packets_expired,
            }),
            estimated_capacity_bps: capacity_bps,
            owd_ms: rtt_ms / 2.0,
            receiver_report: self.latest_receiver_report().map(|r| {
                crate::net::interface::ReceiverReportMetrics {
                    goodput_bps: r.goodput_bps,
                    fec_repair_rate: r.fec_repair_rate_f32(),
                    jitter_buffer_ms: r.jitter_buffer_ms,
                    loss_after_fec: r.loss_after_fec_f32(),
                }
            }),
        }
    }

    fn on_rf_metrics(&self, rf: &crate::modem::health::RfMetrics) {
        let radio = strata_transport::congestion::RadioMetrics {
            rsrp_dbm: rf.rsrp_dbm,
            rsrq_db: rf.rsrq_db,
            sinr_db: rf.sinr_db,
            cqi: rf.cqi,
            timestamp: Some(quanta::Instant::now()),
        };
        self.congestion.lock().unwrap().on_radio_metrics(&radio);
    }

    fn set_probe_allowed(&self, allowed: bool) {
        self.congestion.lock().unwrap().set_probe_allowed(allowed);
    }

    fn recv_feedback(&self) -> usize {
        let mut processed = 0;
        let mut buf = [0u8; 2048];

        // Drain all pending feedback datagrams (non-blocking).
        loop {
            match self.socket.recv(&mut buf) {
                Ok(n) if n > 0 => {
                    if self.process_feedback(&buf[..n]).is_ok() {
                        processed += 1;
                    }
                }
                _ => break,
            }
        }

        // Send periodic Pings for RTT measurement.
        let mut rtt = self.rtt.lock().unwrap();
        if rtt.needs_ping() {
            let ts = self.clock.lock().unwrap().now_us();
            let ping = rtt.make_ping(ts);
            let mut body = BytesMut::with_capacity(16);
            ping.encode(&mut body);
            let body_bytes = body.freeze();
            let header = PacketHeader::control(0, ts, body_bytes.len() as u16);
            let pkt = Packet {
                header,
                payload: body_bytes,
            };
            let encoded = pkt.encode();
            let _ = self.socket.send(&encoded);
        }

        processed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::UdpSocket;

    fn make_loopback_link(id: usize) -> TransportLink {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = socket.local_addr().unwrap();
        socket.connect(addr).unwrap();
        TransportLink::new(id, socket, SenderConfig::default(), None)
    }

    #[test]
    fn link_reports_id() {
        let link = make_loopback_link(42);
        assert_eq!(link.id(), 42);
    }

    #[test]
    fn link_send_and_receive() {
        let link = make_loopback_link(0);
        let data = b"hello transport";
        let result = link.send(data);
        assert!(result.is_ok());
        assert!(result.unwrap() > 0);
    }

    #[test]
    fn metrics_after_send() {
        let link = make_loopback_link(1);
        link.send(b"test packet").unwrap();
        let metrics = link.get_metrics();
        assert!(metrics.observed_bytes > 0);
    }

    #[test]
    fn flush_fec_succeeds() {
        let link = make_loopback_link(2);
        for i in 0..10 {
            let data = format!("packet {i}");
            link.send(data.as_bytes()).unwrap();
        }
        let result = link.flush_fec();
        assert!(result.is_ok());
    }

    #[test]
    fn initial_metrics_are_sane() {
        let link = make_loopback_link(3);
        let metrics = link.get_metrics();
        assert_eq!(metrics.observed_bytes, 0);
        assert_eq!(metrics.phase, LinkPhase::Live);
    }
}
