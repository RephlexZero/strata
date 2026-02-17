//! # Transport Adapter
//!
//! Bridges `strata-transport`'s Sender/Receiver with the bonding crate's
//! `LinkSender` trait. This adapter encapsulates the wire-format encode/decode
//! and reliability layer (FEC + ARQ) behind the existing scheduling interface.

use anyhow::Result;
use bytes::Bytes;
use std::net::UdpSocket;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use crate::net::interface::{LinkMetrics, LinkPhase, LinkSender};
use strata_transport::pool::Priority;
use strata_transport::sender::{Sender, SenderConfig};
use strata_transport::session::RttTracker;

/// A link backed by `strata-transport::Sender`.
pub struct TransportLink {
    /// Unique link ID.
    id: usize,
    /// The strata-transport sender (handles FEC, ARQ, wire format).
    sender: Mutex<Sender>,
    /// RTT tracker for this link.
    rtt: Mutex<RttTracker>,
    /// UDP socket for this link.
    socket: UdpSocket,
    /// Total bytes sent through this link.
    bytes_sent: AtomicU64,
    /// Total packets sent.
    packets_sent: AtomicU64,
}

impl TransportLink {
    /// Create a new transport link.
    ///
    /// `socket` should be bound and connected to the remote peer.
    pub fn new(id: usize, socket: UdpSocket, config: SenderConfig) -> Self {
        TransportLink {
            id,
            sender: Mutex::new(Sender::new(config)),
            rtt: Mutex::new(RttTracker::new()),
            socket,
            bytes_sent: AtomicU64::new(0),
            packets_sent: AtomicU64::new(0),
        }
    }

    /// Send data through the transport layer (encode → wire → socket).
    fn transport_send(&self, data: &[u8], priority: Priority) -> Result<usize> {
        let mut sender = self.sender.lock().unwrap();

        sender.send(Bytes::copy_from_slice(data), priority);

        let mut total_bytes = 0;
        let outputs: Vec<_> = sender.drain_output().collect();
        for output in outputs {
            match self.socket.send(&output.data) {
                Ok(n) => {
                    total_bytes += n;
                }
                Err(e) => {
                    tracing::warn!(link_id = self.id, error = %e, "send failed");
                }
            }
        }

        self.bytes_sent
            .fetch_add(total_bytes as u64, Ordering::Relaxed);
        self.packets_sent.fetch_add(1, Ordering::Relaxed);

        Ok(total_bytes)
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
                    sender.process_ack(ack);
                }
                ControlBody::Nack(nack) => {
                    sender.process_nack(nack);
                }
                ControlBody::Pong(pong) => {
                    let mut rtt = self.rtt.lock().unwrap();
                    rtt.handle_pong(pong);
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

        let mut total_bytes = 0;
        let outputs: Vec<_> = sender.drain_output().collect();
        for output in outputs {
            match self.socket.send(&output.data) {
                Ok(n) => {
                    total_bytes += n;
                }
                Err(e) => {
                    tracing::warn!(link_id = self.id, error = %e, "FEC send failed");
                }
            }
        }

        Ok(total_bytes)
    }

    /// Get the current RTT estimate in milliseconds.
    pub fn rtt_ms(&self) -> f64 {
        let rtt = self.rtt.lock().unwrap();
        rtt.srtt_us() / 1000.0
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

        LinkMetrics {
            rtt_ms: rtt.srtt_us() / 1000.0,
            capacity_bps: 0.0, // Filled by modem/kalman layer
            loss_rate: sender.stats().loss_rate(),
            observed_bps: 0.0,
            observed_bytes: self.bytes_sent.load(Ordering::Relaxed),
            queue_depth: sender.output_queue_len(),
            max_queue: 0,
            alive: true,
            phase: LinkPhase::Live,
            os_up: Some(true),
            mtu: None,
            iface: None,
            link_kind: Some("strata-transport".into()),
        }
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
        TransportLink::new(id, socket, SenderConfig::default())
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
