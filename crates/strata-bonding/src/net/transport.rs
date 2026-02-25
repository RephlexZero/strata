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
use strata_transport::congestion::{BbrPhase, BiscayController, BiscayState};

/// Per-link token bucket for send-path pacing.
///
/// Refills at the BiscayController's pacing_rate (bytes/sec) with a burst cap
/// of 100 ms worth of data.  Tokens are deducted after each send based on
/// actual wire bytes (including FEC/ARQ overhead).  When tokens are negative
/// the next send is rejected, which the DWRR records as a failed send and
/// feeds into congestion detection.
struct PacingState {
    tokens: f64,
    last_refill: std::time::Instant,
}
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
    /// EWMA-smoothed receiver goodput (bits/sec).
    goodput_ewma_bps: Mutex<f64>,
    /// Cumulative bytes acknowledged (for delivery rate tracking).
    bytes_acked: AtomicU64,
    /// Microsecond timestamp of last delivery rate sample.
    prev_ack_time_us: AtomicU64,
    /// Snapshot of bytes_acked at last delivery rate sample.
    prev_ack_bytes: AtomicU64,
    /// EWMA-smoothed ACK rate (bits/sec).
    ack_rate_ewma_bps: Mutex<f64>,
    /// Latest receiver report from the remote receiver (if any).
    receiver_report: Mutex<Option<ReceiverReportPacket>>,
    /// Network interface name (e.g. "eth1").
    iface: Option<String>,
    /// Token bucket pacer — limits per-link send rate to pacing_rate.
    pacing: Mutex<PacingState>,
    /// Paced send queue.
    paced_queue: Mutex<std::collections::VecDeque<strata_transport::sender::OutputPacket>>,
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
            goodput_ewma_bps: Mutex::new(0.0),
            bytes_acked: AtomicU64::new(0),
            prev_ack_time_us: AtomicU64::new(0),
            prev_ack_bytes: AtomicU64::new(0),
            ack_rate_ewma_bps: Mutex::new(0.0),
            receiver_report: Mutex::new(None),
            iface,
            pacing: Mutex::new(PacingState {
                tokens: 10_000.0, // Bootstrap burst — enough for initial probes
                last_refill: std::time::Instant::now(),
            }),
            paced_queue: Mutex::new(std::collections::VecDeque::new()),
        }
    }

    /// Send data through the transport layer (encode → wire → socket).
    ///
    /// Uses GSO batching when outputs have uniform segment size.
    fn transport_send(&self, data: &[u8], priority: Priority) -> Result<usize> {
        let mut sender = self.sender.lock().unwrap();
        sender.send(Bytes::copy_from_slice(data), priority);
        let outputs: Vec<_> = sender.drain_output().collect();

        let mut q = self.paced_queue.lock().unwrap();
        q.extend(outputs);
        drop(q);

        self.flush_paced();

        // Return data.len() to pretend we sent it all (or the actual bytes sent?)
        // The trait expects the number of bytes accepted.
        Ok(data.len())
    }

    /// Flush any pending packets in the paced send queue.
    pub fn flush_paced(&self) {
        let pacing_rate = self.congestion.lock().unwrap().pacing_rate();
        let mut p = self.pacing.lock().unwrap();
        let now = std::time::Instant::now();
        let elapsed = now.duration_since(p.last_refill).as_secs_f64();
        p.tokens += pacing_rate * elapsed;
        // Burst cap: 10 ms of data (or 10 KB minimum for startup)
        p.tokens = p.tokens.min((pacing_rate * 0.01).max(10_000.0));
        p.last_refill = now;

        let mut q = self.paced_queue.lock().unwrap();
        if q.is_empty() {
            return;
        }

        let mut to_send = Vec::new();
        while let Some(pkt) = q.front() {
            let len = pkt.data.len() as f64;
            // Allow sending if we have tokens, OR if we have a minimum burst debt
            // (e.g. allow going negative up to 1 MTU)
            if p.tokens >= 0.0 {
                p.tokens -= len;
                to_send.push(q.pop_front().unwrap());
            } else {
                break;
            }
        }
        drop(q);
        drop(p);

        if !to_send.is_empty() {
            let total_bytes = self.send_batch(&to_send);
            self.bytes_sent
                .fetch_add(total_bytes as u64, Ordering::Relaxed);
            self.packets_sent
                .fetch_add(to_send.len() as u64, Ordering::Relaxed);
        }
    }

    /// Batch-send outputs via quinn-udp with GSO when possible.
    fn send_batch(&self, outputs: &[strata_transport::sender::OutputPacket]) -> usize {
        if outputs.is_empty() {
            return 0;
        }

        let mut max_gso = self.udp_state.max_gso_segments();

        // Cap GSO batching in calibration mode to reduce burstiness
        {
            let cc = self.congestion.lock().unwrap();
            if matches!(cc.bbr_phase, BbrPhase::SlowStart) {
                max_gso = max_gso.min(4); // Small batches during probe
            }
        }

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
                    #[cfg(feature = "bursty_diag")]
                    tracing::info!(
                        target: "strata::bursty_diag",
                        link_id = self.id,
                        batch_size = end - i,
                        "GSO batch"
                    );

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
                    let _newly_acked = sender.process_ack(ack);

                    // ── Delivery rate measurement ──────────────────────
                    // Use the receiver's total_received counter — a smooth,
                    // monotonically-increasing count of unique data packets.
                    // This avoids dependency on the sender's packet pool
                    // capacity (which can fill up, causing newly_acked to
                    // drop to zero and halt BW measurement entirely).
                    let total_recv = ack.total_received.value();
                    let avg_payload = 1200u64;

                    if total_recv > 0 {
                        let total_recv_bytes = total_recv * avg_payload;
                        let prev_bytes_val = self.bytes_acked.load(Ordering::Relaxed);
                        if total_recv_bytes > prev_bytes_val {
                            self.bytes_acked.store(total_recv_bytes, Ordering::Relaxed);
                        }
                    }

                    let now_us = {
                        static EPOCH: std::sync::OnceLock<std::time::Instant> =
                            std::sync::OnceLock::new();
                        let epoch = EPOCH.get_or_init(std::time::Instant::now);
                        epoch.elapsed().as_micros() as u64
                    };
                    let total_acked = self.bytes_acked.load(Ordering::Relaxed);
                    let prev_bytes = self.prev_ack_bytes.load(Ordering::Relaxed);
                    let prev_us = self.prev_ack_time_us.load(Ordering::Relaxed);
                    let interval_us = now_us.saturating_sub(prev_us);

                    let srtt_us = {
                        let rtt = self.rtt.lock().unwrap();
                        rtt.srtt_us()
                    };
                    let min_interval_us = (srtt_us as u64).clamp(250_000, 1_000_000);

                    if interval_us >= min_interval_us && prev_us > 0 {
                        let delta_bytes = total_acked.saturating_sub(prev_bytes);
                        if delta_bytes > 0 {
                            // Idle-gap detection: if the interval is much
                            // larger than expected (> 4×SRTT), the link was
                            // idle between DWRR bursts.  Reset the baseline
                            // without computing a rate — the interval includes
                            // idle time which would dilute the measurement and
                            // underestimate the link's actual delivery rate.
                            let max_interval_us = (srtt_us as u64 * 4).clamp(500_000, 2_000_000);
                            if interval_us > max_interval_us {
                                self.prev_ack_bytes.store(total_acked, Ordering::Relaxed);
                                self.prev_ack_time_us.store(now_us, Ordering::Relaxed);
                            } else {
                                self.prev_ack_bytes.store(total_acked, Ordering::Relaxed);
                                self.prev_ack_time_us.store(now_us, Ordering::Relaxed);

                                let ack_rate_bps =
                                    (delta_bytes as f64 * 8.0) / (interval_us as f64 / 1_000_000.0);
                                let mut ewma = self.ack_rate_ewma_bps.lock().unwrap();
                                if *ewma == 0.0 {
                                    *ewma = ack_rate_bps;
                                } else {
                                    *ewma = 0.2 * ack_rate_bps + 0.8 * *ewma;
                                }

                                let mut cc = self.congestion.lock().unwrap();

                                #[cfg(feature = "bursty_diag")]
                                tracing::info!(
                                    target: "strata::bursty_diag",
                                    link_id = self.id,
                                    interval_us = interval_us,
                                    delta_bytes = delta_bytes,
                                    "ACK sample"
                                );

                                // In multi-link bonding, never mark samples as
                                // app-limited.  The DWRR scheduler controls how
                                // much each link receives — low in-flight during
                                // idle gaps between bursts is normal, not a sign
                                // that the app can't keep up.  Marking those
                                // samples app-limited causes the CC to reject all
                                // low samples, ratcheting btl_bw upward via a
                                // survivor-bias on high outliers only.
                                cc.on_bandwidth_sample(delta_bytes, interval_us, false);
                            }
                        }
                    } else if prev_us == 0 {
                        self.prev_ack_bytes.store(total_acked, Ordering::Relaxed);
                        self.prev_ack_time_us.store(now_us, Ordering::Relaxed);
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
                    let mut ewma = self.goodput_ewma_bps.lock().unwrap();
                    let goodput = report.goodput_bps as f64;
                    if *ewma == 0.0 {
                        *ewma = goodput;
                    } else {
                        *ewma = 0.5 * goodput + 0.5 * *ewma;
                    }
                    tracing::debug!(target: "strata::transport", link_id = self.id, goodput = goodput, ewma = *ewma, "Received ReceiverReport");
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

        let mut q = self.paced_queue.lock().unwrap();
        q.extend(outputs);
        drop(q);

        self.flush_paced();

        Ok(0)
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
        let now_us = {
            static EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
            let epoch = EPOCH.get_or_init(std::time::Instant::now);
            epoch.elapsed().as_micros() as u64
        };
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
        } else if *ewma > 0.0 {
            // Gentle decay when no data flows — slow enough to survive
            // normal DWRR idle gaps (multi-link round-robin) but will
            // eventually reach 0 if a link is truly idle for seconds.
            *ewma *= 0.98;
            if *ewma < 1000.0 {
                *ewma = 0.0;
            }
        }
        let observed_bps = *ewma;

        tracing::debug!(
            target: "strata::transport",
            link_id = self.id,
            socket_rate_bps = socket_rate_bps,
            ewma_bps = observed_bps,
            dt_s = dt_s,
            delta_bytes = total_bytes.saturating_sub(prev_bytes),
            total_bytes_sent = total_bytes,
            "get_metrics: observed rate"
        );

        // --- Capacity estimate from BiscayController (BBR-based) ---
        // Use btl_bw (discovered bottleneck bandwidth) as the capacity
        // signal for DWRR credits.  btl_bw is derived from peak ACK-based
        // delivery rates, so it discovers the actual link speed even when
        // the scheduler only sends a fraction of the link's capacity
        // (burst delivery rates at the bottleneck reflect the true rate).
        //
        // Using pacing_rate would create a feedback loop: low pacing →
        // low DWRR credits → less traffic → btl_bw stays low → pacing
        // stays low.  btl_bw breaks this loop by reflecting what the link
        // CAN do, not what the CC is currently allowing.
        let cc = self.congestion.lock().unwrap();
        let phase = match cc.state {
            BiscayState::PreHandover => LinkPhase::Degrade,
            BiscayState::Cautious => LinkPhase::Degrade,
            BiscayState::Normal => match cc.bbr_phase {
                BbrPhase::SlowStart => LinkPhase::Probe,
                BbrPhase::ProbeBw => LinkPhase::Live,
                // ProbeRtt is a short control cycle for RTprop refresh, not
                // a capacity-discovery startup phase. Mapping it to Probe
                // would trigger scheduler capacity-floor overrides and flatten
                // per-link estimates toward the floor.
                BbrPhase::ProbeRtt => LinkPhase::Live,
            },
        };
        let btl_bw_bps = cc.btl_bw() * 8.0;

        // --- Physics Guard ---
        // Clamp btl_bw to ack_rate × 1.5 to prevent overestimation.
        // Both btl_bw and ack_rate use the same total_received × 1200
        // measurement, so any systematic bias cancels out in the ratio.
        // The 1.5× headroom allows gradual capacity discovery for
        // under-utilized links while preventing the 75th-percentile
        // from drifting far above the link's actual delivery rate.
        let ack_rate = *self.ack_rate_ewma_bps.lock().unwrap();
        let capacity_bps = if btl_bw_bps > 0.0 {
            let capped = if ack_rate > 100_000.0 {
                btl_bw_bps.min(ack_rate * 1.5)
            } else {
                btl_bw_bps
            };
            capped.clamp(100_000.0, 50_000_000.0)
        } else {
            0.0 // No data yet — scheduler will use capacity floor
        };
        let btlbw_bps = if btl_bw_bps > 0.0 {
            Some(btl_bw_bps)
        } else {
            None
        };

        let rtprop_ms = if cc.rt_prop_us() < f64::MAX {
            Some(cc.rt_prop_us() / 1000.0)
        } else {
            None
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

        tracing::debug!(
            target: "strata::transport",
            link_id = self.id,
            capacity_bps = capacity_bps,
            btl_bw_bps = btl_bw_bps,
            observed_bps = observed_bps,
            pacing_rate_bps = cc.pacing_rate() * 8.0,
            phase = ?phase,
            rtt_ms = rtt_ms,
            rtprop_ms = rtprop_ms,
            loss_rate = loss_rate,
            drain_factor = cc.drain_factor(),
            pkts_sent = stats.packets_sent,
            pkts_acked = stats.packets_acked,
            "get_metrics: full snapshot"
        );

        LinkMetrics {
            rtt_ms,
            capacity_bps,
            loss_rate,
            observed_bps,
            observed_bytes: total_bytes,
            queue_depth: sender.output_queue_len() + self.paced_queue.lock().unwrap().len(),
            max_queue: 0,
            alive: true,
            phase,
            os_up: Some(true),
            mtu: None,
            iface: self.iface.clone(),
            link_kind: Some("strata-transport".into()),
            btlbw_bps,
            rtprop_ms,
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

        #[cfg(feature = "bursty_diag")]
        if processed > 0 {
            tracing::info!(
                target: "strata::bursty_diag",
                link_id = self.id,
                queue_depth = processed,
                "Feedback drained"
            );
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

    fn flush_paced(&self) {
        self.flush_paced();
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
        assert_eq!(metrics.phase, LinkPhase::Probe);
    }
}
