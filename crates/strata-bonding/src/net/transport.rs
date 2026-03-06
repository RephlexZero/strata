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
use crate::scheduler::oracle::CapacityOracle;
use strata_transport::congestion::{BbrPhase, BiscayController, BiscayState};

/// Per-link token bucket for send-path pacing.
///
/// Refills at the BiscayController's pacing_rate (bytes/sec) with a burst cap
/// of 100 ms worth of data.  Tokens are deducted after each send based on
/// actual wire bytes (including FEC/ARQ overhead).  When tokens are negative
/// the next send is rejected, which the scheduler records as a failed send and
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
    /// EWMA-smoothed ACK rate (bits/sec), from global total_received.
    ack_rate_ewma_bps: Mutex<f64>,
    /// Per-link ACK rate (bits/sec), from per-link packets_acked.
    per_link_ack_rate_bps: Mutex<f64>,
    /// Previous per-link packets_acked snapshot.
    prev_pkts_acked: AtomicU64,
    /// Timestamp of previous per-link packets_acked snapshot.
    prev_pkts_acked_us: AtomicU64,
    /// Latest receiver report from the remote receiver (if any).
    receiver_report: Mutex<Option<ReceiverReportPacket>>,
    /// Network interface name (e.g. "eth1").
    iface: Option<String>,
    /// Token bucket pacer — limits per-link send rate to pacing_rate.
    pacing: Mutex<PacingState>,
    /// Paced send queue.
    paced_queue: Mutex<std::collections::VecDeque<strata_transport::sender::OutputPacket>>,
    /// Capacity oracle — independent of BBR btl_bw.
    oracle: Mutex<CapacityOracle>,
    /// Previous retransmissions snapshot for per-interval loss_rate.
    prev_retransmissions: AtomicU64,
    /// Previous packets_sent snapshot for per-interval loss_rate.
    prev_loss_pkts_sent: AtomicU64,
    /// Consecutive high-loss windows (loss > 50%). When this reaches 3,
    /// the link reports `alive = false` so the scheduler stops routing
    /// packets into a black hole.
    consecutive_high_loss: std::sync::atomic::AtomicU32,
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
        // Increase kernel send buffer to absorb initial encoder burst before
        // BBR pacing kicks in. Default ~212KB is too small for HD video
        // keyframes; 512KB prevents EAGAIN storms at startup.
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            let fd = socket.as_raw_fd();
            let buf_size: libc::c_int = 524_288; // 512 KB
            unsafe {
                libc::setsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    libc::SO_SNDBUF,
                    &buf_size as *const _ as *const libc::c_void,
                    std::mem::size_of::<libc::c_int>() as libc::socklen_t,
                );
            }
        }
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
            per_link_ack_rate_bps: Mutex::new(0.0),
            prev_pkts_acked: AtomicU64::new(0),
            prev_pkts_acked_us: AtomicU64::new(0),
            receiver_report: Mutex::new(None),
            iface,
            pacing: Mutex::new(PacingState {
                tokens: 10_000.0, // Bootstrap burst — enough for initial probes
                last_refill: std::time::Instant::now(),
            }),
            paced_queue: Mutex::new(std::collections::VecDeque::new()),
            oracle: Mutex::new(CapacityOracle::new()),
            prev_retransmissions: AtomicU64::new(0),
            prev_loss_pkts_sent: AtomicU64::new(0),
            consecutive_high_loss: std::sync::atomic::AtomicU32::new(0),
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
        // Cap queue to prevent bufferbloat from retransmits.
        // BDP at 5Mbps/100ms ≈ 44 packets; 100 gives 2× margin.
        // At 500, queue holds 700KB = 2.8s at 2Mbps → RTT bloats to 500ms+.
        const MAX_PACED_QUEUE: usize = 100;
        while q.len() > MAX_PACED_QUEUE {
            q.pop_front();
        }
        drop(q);

        self.flush_paced();

        // Return data.len() to pretend we sent it all (or the actual bytes sent?)
        // The trait expects the number of bytes accepted.
        Ok(data.len())
    }

    /// Flush any pending packets in the paced send queue.
    pub fn flush_paced(&self) {
        let cc_pacing_rate = self.congestion.lock().unwrap().pacing_rate();
        // Floor: don't let the CC starve a link below 30% of the oracle's
        // slow-decaying peak estimate. Using peak_cap() (not estimated_cap())
        // prevents a death spiral where oracle collapse → pacing collapse →
        // less delivery → further oracle collapse.
        let peak_cap_bytes = self.oracle.lock().unwrap().peak_cap() / 8.0;
        let pacing_rate = cc_pacing_rate.max(peak_cap_bytes * 0.5);
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
            let (total_bytes, pkts_sent) = self.send_batch(&to_send);

            if pkts_sent < to_send.len() {
                let mut q = self.paced_queue.lock().unwrap();
                let mut p = self.pacing.lock().unwrap();
                // Refund tokens and push back in REVERSE order to maintain sequence
                for pkt in to_send.into_iter().skip(pkts_sent).rev() {
                    p.tokens += pkt.data.len() as f64;
                    q.push_front(pkt);
                }
            }

            self.bytes_sent
                .fetch_add(total_bytes as u64, Ordering::Relaxed);
            self.packets_sent
                .fetch_add(pkts_sent as u64, Ordering::Relaxed);
        }
    }

    /// Batch-send outputs via quinn-udp with GSO when possible.
    /// Returns `(bytes_sent, packets_sent)`.
    fn send_batch(&self, outputs: &[strata_transport::sender::OutputPacket]) -> (usize, usize) {
        if outputs.is_empty() {
            return (0, 0);
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
        let mut pkts_sent = 0;

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
                        Ok(()) => {
                            total_bytes += buf.len();
                            pkts_sent += end - i;
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            break;
                        }
                        Err(e) => {
                            tracing::warn!(link_id = self.id, error = %e, "GSO send failed, falling back");
                            // Fallback: send individually
                            for output in &outputs[i..end] {
                                match self.send_single(&output.data) {
                                    Ok(len) => {
                                        total_bytes += len;
                                        pkts_sent += 1;
                                    }
                                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                                        return (total_bytes, pkts_sent);
                                    }
                                    Err(_) => {
                                        pkts_sent += 1; // Count as processed to avoid retry loops on permanent errors
                                    }
                                }
                            }
                        }
                    }
                } else {
                    // Single packet — no GSO needed
                    match self.send_single(&outputs[i].data) {
                        Ok(len) => {
                            total_bytes += len;
                            pkts_sent += 1;
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            break;
                        }
                        Err(_) => {
                            pkts_sent += 1;
                        }
                    }
                }
                i = end;
            }
        } else {
            // No GSO support — send individually
            for output in outputs {
                match self.send_single(&output.data) {
                    Ok(len) => {
                        total_bytes += len;
                        pkts_sent += 1;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        break;
                    }
                    Err(_) => {
                        pkts_sent += 1;
                    }
                }
            }
        }

        (total_bytes, pkts_sent)
    }

    /// Send a single datagram via quinn-udp.
    fn send_single(&self, data: &[u8]) -> std::io::Result<usize> {
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
            Ok(()) => Ok(data.len()),
            Err(e) => {
                if e.kind() != std::io::ErrorKind::WouldBlock {
                    tracing::warn!(link_id = self.id, error = %e, "send failed");
                }
                Err(e)
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
                            // idle between scheduler bursts.  Reset the baseline
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
                                // app-limited.  The EDPF scheduler controls how
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
                    // Drain retransmits into paced queue so they actually get
                    // sent. Without this, retransmits pile up in the sender's
                    // internal output_queue and inflate queue_depth, keeping
                    // the BDP cap permanently blocked.
                    let outputs: Vec<_> = sender.drain_output().collect();
                    if !outputs.is_empty() {
                        let mut q = self.paced_queue.lock().unwrap();
                        q.extend(outputs);
                        // Cap queue — same as transport_send.
                        const MAX_PACED_QUEUE: usize = 100;
                        while q.len() > MAX_PACED_QUEUE {
                            q.pop_front();
                        }
                    }
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
                ControlBody::PpdReport(ppd) => {
                    let capacity_bps = ppd.capacity_bps as f64;
                    self.oracle
                        .lock()
                        .unwrap()
                        .observe_packet_pair(capacity_bps);
                    tracing::debug!(
                        target: "strata::transport",
                        link_id = self.id,
                        capacity_bps = ppd.capacity_bps,
                        dispersion_us = ppd.dispersion_us,
                        packet_size = ppd.packet_size,
                        "PPD report received"
                    );
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
        // ── Snapshot sender state and release lock immediately ──────────
        // Holding the sender lock during oracle/CC computations blocks
        // process_ack() in the feedback recv loop, causing ACK batching
        // that inflates delivery rate measurements.
        let (stats, sender_queue_depth) = {
            let sender = self.sender.lock().unwrap();
            let s = sender.stats().clone();
            let q = sender.output_queue_len();
            (s, q)
        };

        let rtt_ms = {
            let rtt = self.rtt.lock().unwrap();
            rtt.srtt_us() / 1000.0
        };

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
            // normal scheduler idle gaps (multi-link round-robin) but will
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

        // --- Capacity estimate: Oracle (primary) + BBR btl_bw (fallback) ---
        //
        // The CapacityOracle provides a stable, feedback-loop-free capacity
        // estimate for EDPF scheduling. btl_bw is only used as a fallback
        // before the first saturation probe completes.
        let mut cc = self.congestion.lock().unwrap();
        // Drive ProbeRtt phase transitions — without this, RTprop never
        // re-probes and BBR capacity estimates go stale on cellular links.
        cc.tick();
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

        // Feed ACK-confirmed delivery rate to the Oracle (passive lower-bound).
        // Use per-link bytes_acked (actual payload sizes summed during ACK
        // processing) — NOT packets_acked * estimated_size, which is inaccurate
        // due to FEC packets and varying payload sizes.
        let per_link_ack_bytes = stats.bytes_acked;
        let now_ack_us = {
            static EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
            let epoch = EPOCH.get_or_init(std::time::Instant::now);
            epoch.elapsed().as_micros() as u64
        };
        // Read without swapping — only commit the snapshot when the interval
        // condition is met. This lets the interval accumulate across multiple
        // get_metrics() calls until ≥ 500ms has passed.
        let prev_bytes = self.prev_pkts_acked.load(Ordering::Relaxed);
        let prev_us = self.prev_pkts_acked_us.load(Ordering::Relaxed);
        let interval_us = now_ack_us.saturating_sub(prev_us);

        // Require ≥ 500ms between rate samples to avoid spikes from batch
        // ACKs. This gives a smoother, more accurate delivery rate.
        let per_link_ack_rate = if interval_us >= 500_000 && prev_us > 0 {
            // Commit the snapshot for next interval
            self.prev_pkts_acked
                .store(per_link_ack_bytes, Ordering::Relaxed);
            self.prev_pkts_acked_us.store(now_ack_us, Ordering::Relaxed);

            let delta = per_link_ack_bytes.saturating_sub(prev_bytes);
            let mut ewma = self.per_link_ack_rate_bps.lock().unwrap();
            if delta > 0 {
                let rate = (delta as f64 * 8.0) / (interval_us as f64 / 1_000_000.0);
                if *ewma == 0.0 {
                    *ewma = rate;
                } else {
                    *ewma = 0.2 * rate + 0.8 * *ewma;
                }
            } else {
                // No new ACKs in this 500ms window — decay the EWMA.
                // Without this, the rate stays frozen at a peak value
                // indefinitely, inflating the oracle's capacity estimate.
                *ewma *= 0.5;
                if *ewma < 1000.0 {
                    *ewma = 0.0;
                }
            }
            *ewma
        } else if prev_us == 0 {
            // First call — seed the baseline
            self.prev_pkts_acked
                .store(per_link_ack_bytes, Ordering::Relaxed);
            self.prev_pkts_acked_us.store(now_ack_us, Ordering::Relaxed);
            0.0
        } else {
            *self.per_link_ack_rate_bps.lock().unwrap()
        };

        let mut oracle = self.oracle.lock().unwrap();

        // Use the best available delivery rate signal for the oracle.
        // per_link_ack_rate underreports when the sender pool overflows
        // (entries expire before ACKs arrive). The receiver-reported goodput
        // is more accurate since it measures actual delivered data.
        let goodput = *self.goodput_ewma_bps.lock().unwrap();
        let delivery_signal = if goodput > 100_000.0 {
            goodput
        } else {
            per_link_ack_rate
        };

        if delivery_signal > 0.0 {
            oracle.observe_delivery(delivery_signal);
        }
        // Update baseline RTT for downshift detection
        oracle.update_baseline_rtt(rtt_ms);
        // Apply time-based confidence decay
        oracle.tick();

        // Check for downshift conditions (handover/severe degradation)
        // Uses cumulative retransmit_ratio — correct for detecting handovers.
        let cumulative_loss = if stats.packets_acked > 0 && stats.packets_sent > 0 {
            stats.retransmit_ratio()
        } else {
            0.0
        };
        if oracle.should_reset(rtt_ms, cumulative_loss) {
            oracle.reset_on_downshift();
        }

        // Per-interval loss_rate for the scheduling/adaptation layer.
        // Cumulative retransmit_ratio is permanently inflated by startup
        // burst losses — same bug we fixed receiver-side for loss_after_fec.
        let prev_retx = self.prev_retransmissions.load(Ordering::Relaxed);
        let prev_sent = self.prev_loss_pkts_sent.load(Ordering::Relaxed);
        let delta_retx = stats.retransmissions.saturating_sub(prev_retx);
        let delta_sent = stats.packets_sent.saturating_sub(prev_sent).max(1);
        self.prev_retransmissions
            .store(stats.retransmissions, Ordering::Relaxed);
        self.prev_loss_pkts_sent
            .store(stats.packets_sent, Ordering::Relaxed);
        let loss_rate = (delta_retx as f64 / delta_sent as f64).clamp(0.0, 1.0);

        // Track consecutive high-loss windows for link death detection.
        // Mark dead when loss > 50% for 3+ consecutive metric windows so the
        // scheduler stops routing packets into a black hole.
        // Require sufficient packet volume to avoid false positives during
        // idle periods when delta_sent is just 1 (the .max(1) floor).
        if loss_rate > 0.50 && delta_sent >= 5 {
            self.consecutive_high_loss.fetch_add(1, Ordering::Relaxed);
        } else {
            self.consecutive_high_loss.store(0, Ordering::Relaxed);
        }
        let high_loss_count = self.consecutive_high_loss.load(Ordering::Relaxed);
        let alive = high_loss_count < 3;
        if !alive {
            tracing::warn!(
                link_id = self.id,
                loss_rate = loss_rate,
                consecutive_windows = high_loss_count,
                "link marked dead: sustained high loss"
            );
        }

        // Capacity: prefer Oracle → BBR btl_bw → ack_delivery_bps fallback.
        // When Oracle and BBR are both stale (no real bandwidth samples),
        // use ack_delivery_bps as a direct proxy for achievable throughput.
        let oracle_cap = oracle.estimated_cap();
        let btl_bw_capped = if btl_bw_bps > 0.0 {
            let capped = if per_link_ack_rate > 100_000.0 {
                btl_bw_bps.min(per_link_ack_rate * 1.5)
            } else {
                btl_bw_bps
            };
            capped.clamp(100_000.0, 50_000_000.0)
        } else {
            0.0
        };
        let capacity_bps = if oracle_cap > 0.0 {
            oracle_cap
        } else if btl_bw_capped > 0.0 {
            btl_bw_capped
        } else if per_link_ack_rate > 100_000.0 {
            // Fallback: use ack delivery rate with 20% headroom as capacity.
            // This prevents the adapter from using the 5 Mbps floor when
            // actual achievable throughput is known from ACK measurements.
            per_link_ack_rate * 1.2
        } else {
            0.0
        };
        drop(oracle);

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
            queue_depth: sender_queue_depth + self.paced_queue.lock().unwrap().len(),
            max_queue: 0,
            alive,
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
            ack_delivery_bps: per_link_ack_rate,
            ack_bytes: per_link_ack_bytes,
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

    fn complete_saturation_probe(&self, peak_bps: f64) {
        self.oracle.lock().unwrap().complete_probe(peak_bps);
        // Seed the congestion controller so btl_bw reflects the probe-measured
        // physical capacity, not just the delivery rate under scheduler allocation.
        self.congestion
            .lock()
            .unwrap()
            .seed_bandwidth(peak_bps / 8.0);
    }

    fn inject_ppd_pair(&self) {
        let pair = {
            let mut sender = self.sender.lock().unwrap();
            sender.inject_ppd_pair(1200) // use default MTU size
        };
        // Send directly, bypassing pacing, to keep packets back-to-back
        if !pair.is_empty() {
            let (total_bytes, pkts_sent) = self.send_batch(&pair);
            self.bytes_sent
                .fetch_add(total_bytes as u64, Ordering::Relaxed);
            self.packets_sent
                .fetch_add(pkts_sent as u64, Ordering::Relaxed);
        }
    }

    fn set_saturation_probe_active(&self, active: bool) {
        self.oracle.lock().unwrap().set_probe_active(active);
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
