//! # Transport-based Bonding Receiver
//!
//! Receives packets sent via `strata-transport` wire format across multiple
//! UDP links, decodes and recovers via the transport-layer receiver (FEC,
//! reordering), strips the bonding header, then feeds payloads into a
//! shared [`ReassemblyBuffer`] for multi-link jitter buffering.

use crate::protocol::header::BondingHeader;
use crate::receiver::aggregator::{
    Packet, ReassemblyBuffer, ReassemblyConfig, ReassemblyLinkStats, ReassemblyStats,
};
use anyhow::Result;
use bytes::{Bytes, BytesMut};
use crossbeam_channel::{Receiver, Sender, bounded};
use quanta::Instant;
use std::collections::BTreeMap;
use std::net::{SocketAddr, UdpSocket};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::thread;
use std::time::Duration;
use strata_transport::pool::TimestampClock;
use strata_transport::receiver::{Receiver as TransportReceiver, ReceiverConfig, ReceiverEvent};
use strata_transport::session::RttTracker;
use strata_transport::wire::{ControlBody, Packet as WirePacket, PacketHeader};
use tracing::{debug, info, warn};

/// Bind a UDP socket with `SO_REUSEADDR`.
///
/// The egress watchdog rebuilds this receiver's pipeline in the same process,
/// which rebinds the same ports moments after the old sockets are dropped.
/// Under SQPOLL io_uring the kernel releases a closed socket's ring-registered
/// fd asynchronously (`io_ring_exit_work`), so a plain rebind can transiently
/// see the old port as still busy — worse under sustained load, where field
/// run orangepi-128932 saw every one of 5 retries over ~5s hit EADDRINUSE.
/// `SO_REUSEADDR` lets the new bind proceed regardless of that lingering
/// kernel-side reference.
fn bind_udp_reuseaddr(addr: SocketAddr) -> Result<UdpSocket> {
    use std::os::fd::{FromRawFd, IntoRawFd};

    let domain = if addr.is_ipv4() {
        socket2::Domain::IPV4
    } else {
        socket2::Domain::IPV6
    };
    let socket = socket2::Socket::new(domain, socket2::Type::DGRAM, None)?;
    socket.set_reuse_address(true)?;
    socket.bind(&addr.into())?;
    Ok(unsafe { UdpSocket::from_raw_fd(socket.into_raw_fd()) })
}

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
/// A delivered payload with a discontinuity flag.
///
/// When `discont` is `true`, one or more packets were lost before this
/// payload — downstream consumers (e.g. GStreamer tsdemux) should resync.
pub type DeliveredPayload = (Bytes, bool);

#[derive(Clone, Debug, Default)]
struct LinkRuntimeStats {
    packets_received: u64,
    packets_delivered: u64,
    bytes_received: u64,
    loss_rate: f64,
}

pub struct TransportBondingReceiver {
    input_tx: Option<Sender<Packet>>,
    output_tx: Option<Sender<DeliveredPayload>>,
    /// Public so GStreamer (or any consumer) can pull ordered payloads.
    pub output_rx: Receiver<DeliveredPayload>,
    running: Arc<AtomicBool>,
    stats: Arc<Mutex<ReassemblyStats>>,
    link_stats: Arc<Mutex<BTreeMap<usize, LinkRuntimeStats>>>,
    next_link_id: AtomicUsize,
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
        // Decouple the downstream output channel from the reassembly capacity.
        // The reassembly buffer sizes the per-sequence window; the output
        // channel absorbs downstream back-pressure while the GStreamer sink
        // is momentarily blocked (segment rotation, disk/tmpfs flush, etc.).
        // Undersizing here turns a ~100ms sink stall into torrent of packet
        // drops that looks like heavy network loss to the rest of the pipeline.
        let output_capacity = config.buffer_capacity.saturating_mul(8).max(8192);
        let (output_tx, output_rx) = bounded::<DeliveredPayload>(output_capacity);
        let (input_tx, input_rx) = bounded::<Packet>(config.buffer_capacity);
        let running = Arc::new(AtomicBool::new(true));
        let stats = Arc::new(Mutex::new(ReassemblyStats::default()));
        let link_stats = Arc::new(Mutex::new(BTreeMap::<usize, LinkRuntimeStats>::new()));

        let stats_clone = stats.clone();
        let link_stats_clone = link_stats.clone();
        let running_clone = running.clone();
        let output_tx_clone = output_tx.clone();

        let jitter_handle = thread::Builder::new()
            .name("strata-rcv-jitter".into())
            .spawn(move || {
                let mut buffer = ReassemblyBuffer::with_config(0, config);
                let tick_interval = Duration::from_millis(10);
                let mut dropped_since_log: u64 = 0;
                let mut total_dropped: u64 = 0;
                // A dropped payload is a hole in the byte stream; the next
                // payload we manage to send must carry DISCONT so the egress
                // resyncs rather than splicing across the drop.
                let mut carry_discont = false;
                let mut last_drop_log = Instant::now();
                let drop_log_interval = Duration::from_secs(1);

                while running_clone.load(Ordering::Relaxed) {
                    // Drain all available input packets (non-blocking after
                    // the first recv_timeout), so the link readers never stall
                    // waiting on a full input channel.
                    match input_rx.recv_timeout(tick_interval) {
                        Ok(packet) => {
                            buffer.push_with_ts(
                                packet.seq_id,
                                packet.payload,
                                packet.arrival_time,
                                packet.send_ts_us,
                            );
                            // Drain any additional queued packets without blocking.
                            while let Ok(p) = input_rx.try_recv() {
                                buffer.push_with_ts(
                                    p.seq_id,
                                    p.payload,
                                    p.arrival_time,
                                    p.send_ts_us,
                                );
                            }
                        }
                        Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                        Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                    }

                    let now = Instant::now();
                    let ready = buffer.tick(now);

                    if let Ok(mut s) = stats_clone.lock() {
                        let mut snapshot = buffer.get_stats();
                        if let Ok(link) = link_stats_clone.lock() {
                            snapshot.per_link = link
                                .iter()
                                .map(|(link_id, ls)| ReassemblyLinkStats {
                                    link_id: *link_id,
                                    packets_received: ls.packets_received,
                                    packets_delivered: ls.packets_delivered,
                                    bytes_received: ls.bytes_received,
                                    loss_rate: ls.loss_rate,
                                })
                                .collect();
                        }
                        *s = snapshot;
                    }

                    for mut p in ready {
                        // Use try_send to avoid blocking the jitter thread
                        // when the downstream consumer (GStreamer) stalls.
                        // Dropping late frames is better than deadlocking
                        // the entire receive pipeline.
                        //
                        // A dropped payload is itself a discontinuity, and if
                        // the dropped payload was *already* flagged DISCONT,
                        // destroying it would lose that marker entirely —
                        // downstream would then splice the hole with no resync
                        // signal and the decoder would render a corrupt AU.
                        // Carry the flag onto the next payload we DO send.
                        if carry_discont {
                            p.1 = true;
                            carry_discont = false;
                        }
                        if output_tx_clone.try_send(p).is_err() {
                            // The drop itself is a discontinuity (and may have
                            // carried a DISCONT we just lost) — flag the next
                            // successful send so the marker is never erased.
                            carry_discont = true;
                            dropped_since_log += 1;
                            total_dropped += 1;
                        }
                    }
                    if dropped_since_log > 0
                        && now.duration_since(last_drop_log) >= drop_log_interval
                    {
                        tracing::warn!(
                            dropped = dropped_since_log,
                            total = total_dropped,
                            "output channel full, dropping packets (downstream stalled)"
                        );
                        dropped_since_log = 0;
                        last_drop_log = now;
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
            link_stats,
            next_link_id: AtomicUsize::new(0),
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
        let socket = bind_udp_reuseaddr(bind_addr)?;
        self.add_link_socket(socket)
    }

    /// Add a link from an already-bound UDP socket.
    pub fn add_link_socket(&self, socket: UdpSocket) -> Result<()> {
        let local_addr = socket.local_addr()?;
        let link_id = self.next_link_id.fetch_add(1, Ordering::Relaxed);

        let input_tx = self
            .input_tx
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Receiver shut down"))?
            .clone();
        let running = self.running.clone();
        let stats = self.stats.clone();
        let link_stats = self.link_stats.clone();

        let handle = thread::Builder::new()
            .name(format!("strata-rcv-{}-{}", link_id, local_addr))
            .spawn(move || {
                let mut rt = crate::build_monoio_runtime!();
                rt.block_on(async move {
                    let mono_socket = monoio::net::udp::UdpSocket::from_std(socket)
                        .expect("failed to convert socket for monoio");
                    link_reader_async(link_id, mono_socket, input_tx, running, stats, link_stats)
                        .await;
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

/// Per-link relative one-way-delay GRADIENT tracker (F3).
///
/// For each data packet we sample `rel = receiver_now_us − sender_send_ts_us`.
/// The sender/receiver clocks have an arbitrary *constant* offset (no PTP);
/// it cancels because we only ever report a *difference of `rel` values*.
///
/// The signal is **short-window-min minus long-window-min** — NOT
/// mean-minus-min. This is the LEDBAT/Copa insight and the key to
/// jitter-immunity (the doctrine's explicit Wi-Fi landmine: "aggregation
/// bursts look like bufferbloat"). Jitter only ever *adds* delay above the
/// propagation floor, so on an uncongested-but-jittery link BOTH minima sit
/// at that floor and the gradient is ≈0. A mean-vs-min signal, by contrast,
/// reads a permanent false "standing queue" equal to the jitter spread and
/// drains forever. The short min rises above the long min only when a
/// genuine standing queue forms (the floor itself moves up).
struct DelayGradientTracker {
    /// `(arrival_instant, rel_us)` within the long baseline window.
    window: std::collections::VecDeque<(std::time::Instant, i64)>,
    /// Long baseline length (true propagation floor, ~10 s — mirrors
    /// Biscay's `rt_prop` window so drift/one-off bloat expires out).
    window_len: Duration,
    /// Short window length whose *minimum* is the "current" delay floor.
    /// Long enough to contain several packets so a momentary gap doesn't
    /// empty it, short enough to track a rising queue promptly.
    short_len: Duration,
}

impl DelayGradientTracker {
    fn new() -> Self {
        Self {
            window: std::collections::VecDeque::with_capacity(512),
            window_len: Duration::from_secs(10),
            short_len: Duration::from_millis(750),
        }
    }

    /// Feed one data-packet sample. `rel_us` may be negative (clock offset);
    /// only its variation matters.
    fn observe(&mut self, now: std::time::Instant, rel_us: i64) {
        // Guard against a u32 µs clock wrap / reset: an impossibly large
        // negative jump vs the current baseline is not real queue drain.
        if let Some(&(_, min_rel)) = self.window.iter().min_by_key(|&&(_, r)| r)
            && rel_us < min_rel - 2_000_000
        {
            self.window.clear();
        }
        self.window.push_back((now, rel_us));
        while let Some(&(ts, _)) = self.window.front() {
            if now.duration_since(ts) > self.window_len {
                self.window.pop_front();
            } else {
                break;
            }
        }
    }

    /// Current queue-building magnitude in microseconds (≥ 0):
    /// `min(rel over last short_len) − min(rel over full window)`.
    ///
    /// Jitter-immune (both terms are minima at the propagation floor when
    /// uncongested); positive only when the delay floor itself rises.
    fn gradient_us(&self) -> u32 {
        let Some(&(last_ts, _)) = self.window.back() else {
            return 0;
        };
        // Need a full short window of history before trusting the signal,
        // otherwise the very first samples make short_min == long_min == the
        // only sample (gradient 0) or a sparse short window over-reacts.
        let Some(&(first_ts, _)) = self.window.front() else {
            return 0;
        };
        if last_ts.duration_since(first_ts) < self.short_len {
            return 0;
        }
        let mut long_min = i64::MAX;
        let mut short_min = i64::MAX;
        for &(ts, r) in &self.window {
            if r < long_min {
                long_min = r;
            }
            if last_ts.duration_since(ts) <= self.short_len && r < short_min {
                short_min = r;
            }
        }
        if long_min == i64::MAX || short_min == i64::MAX {
            return 0;
        }
        (short_min - long_min).max(0).min(u32::MAX as i64) as u32
    }
}

/// Per-link reader loop (async, runs on a monoio event loop).
///
/// Uses io_uring (or epoll fallback) for async UDP receives, feeding
/// datagrams into a `strata_transport::Receiver` for FEC decoding
/// and reorder. Delivered payloads have the bonding header stripped
/// and are pushed into the shared reassembly channel.
async fn link_reader_async(
    link_id: usize,
    socket: monoio::net::udp::UdpSocket,
    input_tx: Sender<Packet>,
    running: Arc<AtomicBool>,
    reassembly_stats: Arc<Mutex<ReassemblyStats>>,
    link_stats: Arc<Mutex<BTreeMap<usize, LinkRuntimeStats>>>,
) {
    let config = ReceiverConfig {
        nack_rearm_ms: 100,      // Re-ask for lost frames less frantically
        max_nack_retries: 10,    // Give cellular links up to 1000ms to deliver packets
        reorder_capacity: 16384, // Ensure the buffer accommodates wider delay jumps
        ..Default::default()
    };
    let mut transport_rx = TransportReceiver::new(config);
    let mut buf = vec![0u8; 65536];
    let clock = TimestampClock::new();
    let mut last_ack = std::time::Instant::now();
    let ack_interval = Duration::from_millis(15); // 10-20ms max delay
    let mut packets_since_ack = 0;
    let mut last_report = std::time::Instant::now();
    let report_interval = Duration::from_secs(1);
    // Track bytes delivered for goodput calculation.
    let mut prev_bytes_delivered: u64 = 0;
    let mut prev_report_time = std::time::Instant::now();
    // Track packet counters for per-interval loss (not cumulative lifetime).
    let mut prev_highest_seq: u64 = 0;
    let mut prev_packets_delivered: u64 = 0;
    let mut prev_fec_recoveries: u64 = 0;
    // Track reassembly-level lost-packet counter for ground-truth loss.
    // Only counts truly lost packets (not late arrivals — those are a
    // jitter issue handled by jitter_buffer_ms / delay_pressure).
    let mut prev_reassembly_lost: u64 = 0;
    let mut prev_reassembly_delivered: u64 = 0;
    let mut prev_reassembly_late: u64 = 0;
    // Most recently seen sender address on this socket.
    let mut sender_addr: Option<std::net::SocketAddr> = None;
    // F3: per-link relative one-way-delay gradient (queue-build detector).
    let mut grad_tracker = DelayGradientTracker::new();

    // ── Per-link RX diagnostics ─────────────────────────────────────────
    // A blackholed link receives nothing, so its receiver stats never
    // appear in the aggregate strata-stats message — making the failure
    // invisible. Log the first datagram ever seen on this link, then a
    // periodic per-link RX heartbeat that fires EVEN WITH ZERO TRAFFIC so
    // a dead link is visibly and continuously reported (rx=0) rather than
    // silently absent.
    let mut first_packet_logged = false;
    let mut last_rx_log = std::time::Instant::now();
    let rx_log_interval = Duration::from_secs(2);
    let mut prev_rx_packets: u64 = 0;
    let mut prev_rx_bytes: u64 = 0;

    while running.load(Ordering::Relaxed) {
        // Await next datagram with a timeout so we can check the running flag.
        match monoio::time::timeout(Duration::from_millis(50), socket.recv_from(buf)).await {
            Ok((Ok((n, addr)), returned_buf)) => {
                sender_addr = Some(addr);
                if !first_packet_logged {
                    first_packet_logged = true;
                    info!(
                        link_id,
                        peer = %addr,
                        bytes = n,
                        "rx link admitted: first datagram received"
                    );
                }
                let raw = Bytes::copy_from_slice(&returned_buf[..n]);

                // F3: sample the relative one-way-delay gradient on DATA
                // packets. Decode just the header; `timestamp_us` is the
                // sender's send time. `rel = recv_now − send_ts` has a
                // constant clock offset that cancels in the reported
                // `ewma − windowed_min` gradient.
                {
                    let mut hdr_cur = &returned_buf[..n];
                    if let Some(hdr) = PacketHeader::decode(&mut hdr_cur)
                        && hdr.packet_type == strata_transport::wire::PacketType::Data
                    {
                        let rel_us = clock.now_us() as i64 - hdr.timestamp_us as i64;
                        grad_tracker.observe(std::time::Instant::now(), rel_us);
                    }
                }

                // Check for control packets (Ping) before handing to transport_rx.
                // Respond with Pong immediately.
                if let Some(pong_bytes) = try_make_pong(&returned_buf[..n], &clock) {
                    let _ = socket.send_to(pong_bytes, addr).await;
                }

                transport_rx.receive(raw);
                packets_since_ack += 1;
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
                                    send_ts_us: delivered.timestamp_us,
                                };
                                // Non-blocking: drop packet rather than stall
                                // the async reader (and ACK/NACK generation).
                                let _ = input_tx.try_send(packet);
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
                        ReceiverEvent::SendPpdReport(ppd) => {
                            if let Some(addr) = sender_addr {
                                let pkt_bytes = encode_ppd_report(&ppd, &clock);
                                let _ = socket.send_to(pkt_bytes, addr).await;
                            }
                        }
                    }
                }

                // Hybrid ACK policy: send ACK if max delay elapsed OR packet threshold reached.
                if packets_since_ack >= 12 || last_ack.elapsed() >= ack_interval {
                    let ack = transport_rx.generate_ack();
                    if let Some(addr) = sender_addr {
                        let pkt_bytes = encode_control_packet(&ack, &clock);
                        let _ = socket.send_to(pkt_bytes, addr).await;
                    }
                    // Also generate NACKs for missing packets.
                    if let Some(nack) = transport_rx.generate_nacks()
                        && let Some(addr) = sender_addr
                    {
                        let pkt_bytes = encode_nack_packet(&nack, &clock);
                        let _ = socket.send_to(pkt_bytes, addr).await;
                    }
                    // Drain deliveries produced by gap-skipping during
                    // ACK/NACK generation (irrecoverable loss handling).
                    for event in transport_rx.drain_events() {
                        if let ReceiverEvent::Deliver(delivered) = event
                            && let Some((header, original_payload)) =
                                BondingHeader::unwrap(delivered.payload)
                        {
                            let packet = Packet {
                                seq_id: header.seq_id,
                                payload: original_payload,
                                arrival_time: quanta::Instant::now(),
                                send_ts_us: delivered.timestamp_us,
                            };
                            let _ = input_tx.try_send(packet);
                        }
                    }
                    last_ack = std::time::Instant::now();
                    packets_since_ack = 0;
                }

                // Periodically send ReceiverReport.
                if last_report.elapsed() >= report_interval {
                    if let Some(addr) = sender_addr {
                        let rx_stats = transport_rx.stats();
                        let now = std::time::Instant::now();
                        let dt = now.duration_since(prev_report_time).as_secs_f64();

                        // Compute goodput from bytes actually delivered (not
                        // just received). During a reorder stall, bytes_received
                        // keeps growing while packets_delivered is flat — using
                        // bytes_received would report positive goodput during a
                        // stall, defeating the adaptation layer's EWMA gate.
                        let cur_bytes = rx_stats.bytes_delivered;
                        let delta_bytes = cur_bytes.saturating_sub(prev_bytes_delivered);
                        let goodput_bps = if dt > 0.01 {
                            ((delta_bytes as f64 * 8.0) / dt) as u64
                        } else {
                            0
                        };
                        prev_bytes_delivered = cur_bytes;
                        prev_report_time = now;

                        // Jitter buffer depth from reassembly stats
                        let jitter_ms = reassembly_stats
                            .lock()
                            .map(|s| s.current_latency_ms as u32)
                            .unwrap_or(0);

                        // Residual loss over this interval (delta, not cumulative).
                        // Using lifetime totals causes startup queue-flood losses to
                        // permanently inflate the ratio even after recovery.
                        let delta_seq = rx_stats
                            .highest_delivered_seq
                            .saturating_sub(prev_highest_seq);
                        let delta_delivered = rx_stats
                            .packets_delivered
                            .saturating_sub(prev_packets_delivered);
                        let delta_fec = rx_stats.fec_recoveries.saturating_sub(prev_fec_recoveries);
                        prev_highest_seq = rx_stats.highest_delivered_seq;
                        prev_packets_delivered = rx_stats.packets_delivered;
                        prev_fec_recoveries = rx_stats.fec_recoveries;
                        let residual = delta_seq.saturating_sub(delta_delivered);
                        let loss_after_fec = if delta_seq > 0 {
                            (residual as f64 / delta_seq as f64).clamp(0.0, 1.0)
                        } else {
                            0.0
                        };

                        let fec_rate = if delta_seq > 0 {
                            (delta_fec as f64 / delta_seq as f64).clamp(0.0, 1.0)
                        } else {
                            0.0
                        };

                        // Reassembly-level ground-truth loss: captures cross-link
                        // reorder loss that is invisible to per-link transport
                        // stats.  Only counts truly LOST packets, not late
                        // arrivals — late packets are a jitter issue (addressed
                        // by jitter_buffer_ms → delay_pressure) not congestion.
                        // Including late packets caused false-positive EWMA
                        // spikes (~40%) from brief jitter bursts on LTE, leading
                        // to destructive bitrate oscillation.
                        let (reassembly_loss, late_rate_f) =
                            if let Ok(snap) = reassembly_stats.lock() {
                                let d_lost = snap.lost_packets.saturating_sub(prev_reassembly_lost);
                                let d_del = snap
                                    .packets_delivered
                                    .saturating_sub(prev_reassembly_delivered);
                                let d_late = snap.late_packets.saturating_sub(prev_reassembly_late);
                                prev_reassembly_lost = snap.lost_packets;
                                prev_reassembly_delivered = snap.packets_delivered;
                                prev_reassembly_late = snap.late_packets;
                                let total = d_lost + d_del;
                                let loss = if total > 0 {
                                    (d_lost as f64 / total as f64).clamp(0.0, 1.0)
                                } else {
                                    0.0
                                };
                                // Express late arrivals against delivered packets so
                                // the ratio is directly comparable to loss_after_fec
                                // for the sender's delay-pressure input.
                                let late = if d_del > 0 {
                                    (d_late as f64 / d_del as f64).clamp(0.0, 1.0)
                                } else {
                                    0.0
                                };
                                (loss, late)
                            } else {
                                (0.0, 0.0)
                            };

                        // Use the worse signal: transport-layer FEC-residual OR
                        // reassembly-layer lost packets.  Transport catches
                        // per-link FEC exhaustion; reassembly catches cross-link
                        // reorder and irrecoverable loss.
                        let report_loss = loss_after_fec.max(reassembly_loss);

                        // Per-link diagnostic stats: use per-link transport loss
                        // (not reassembly) so operators can tell WHICH link is
                        // degraded.  Both readers share the same reassembly
                        // counters, so using reassembly_loss here would make
                        // all links show identical values.
                        if let Ok(mut per_link) = link_stats.lock() {
                            per_link.insert(
                                link_id,
                                LinkRuntimeStats {
                                    packets_received: rx_stats.packets_received,
                                    packets_delivered: rx_stats.packets_delivered,
                                    bytes_received: rx_stats.bytes_received,
                                    loss_rate: loss_after_fec,
                                },
                            );
                        }

                        let report = strata_transport::wire::ReceiverReportPacket {
                            goodput_bps,
                            fec_repair_rate: (fec_rate * 10000.0) as u16,
                            jitter_buffer_ms: jitter_ms,
                            loss_after_fec: (report_loss * 10000.0) as u16,
                            late_rate: (late_rate_f * 10000.0) as u16,
                            // Per-link cumulative bytes delivered to reassembly.
                            // Sender's saturation-probe path uses this as the
                            // true throughput signal (independent of the modem
                            // TX queue, unlike sender-side observed_bytes).
                            bytes_delivered: cur_bytes,
                            // F3: relative OWD gradient — queue-building
                            // magnitude in µs, drives delay-bounded backoff
                            // on the sender before loss appears.
                            delay_gradient_us: grad_tracker.gradient_us(),
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

                        // Drain gap-skip deliveries.
                        for event in transport_rx.drain_events() {
                            if let ReceiverEvent::Deliver(delivered) = event
                                && let Some((header, original_payload)) =
                                    BondingHeader::unwrap(delivered.payload)
                            {
                                let packet = Packet {
                                    seq_id: header.seq_id,
                                    payload: original_payload,
                                    arrival_time: quanta::Instant::now(),
                                    send_ts_us: delivered.timestamp_us,
                                };
                                let _ = input_tx.try_send(packet);
                            }
                        }
                    }
                    last_ack = std::time::Instant::now();
                    packets_since_ack = 0;
                }
            }
        }

        // Per-link RX heartbeat — runs every iteration regardless of the
        // match arm, so a link that has received NOTHING still emits a
        // line every `rx_log_interval` (rx_pkts=0), making a blackholed
        // link continuously visible instead of silently absent.
        if last_rx_log.elapsed() >= rx_log_interval {
            let s = transport_rx.stats();
            let d_pkts = s.packets_received.saturating_sub(prev_rx_packets);
            let d_bytes = s.bytes_received.saturating_sub(prev_rx_bytes);
            prev_rx_packets = s.packets_received;
            prev_rx_bytes = s.bytes_received;
            let secs = last_rx_log.elapsed().as_secs_f64().max(0.001);
            info!(
                link_id,
                rx_pkts_total = s.packets_received,
                rx_pkts_delta = d_pkts,
                rx_kbps = (d_bytes as f64 * 8.0 / secs / 1000.0) as u64,
                delivered_total = s.packets_delivered,
                fec_recoveries = s.fec_recoveries,
                fec_corrupt_dropped = s.fec_corrupt_dropped,
                peer = sender_addr
                    .map(|a| a.to_string())
                    .unwrap_or_else(|| "<none>".into()),
                "rx link heartbeat"
            );
            last_rx_log = std::time::Instant::now();
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

/// Encode a PpdReport as a wire-format control packet.
fn encode_ppd_report(
    ppd: &strata_transport::wire::PpdReportPacket,
    clock: &TimestampClock,
) -> Vec<u8> {
    let mut body = BytesMut::with_capacity(20);
    ppd.encode(&mut body);
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
    fn delay_gradient_is_clock_offset_immune() {
        // A large CONSTANT clock offset (sender epoch ≠ receiver epoch)
        // must cancel: only the variation of rel matters.
        let mut t = DelayGradientTracker::new();
        let base = std::time::Instant::now();
        const OFFSET: i64 = 1_000_000_000; // huge constant skew
        // ~1.5 s of steady samples (must exceed the 750 ms short window).
        for i in 0u64..60 {
            t.observe(
                base + std::time::Duration::from_millis(i * 25),
                OFFSET + (i % 2) as i64,
            );
        }
        assert!(
            t.gradient_us() < 50,
            "constant offset must cancel; gradient ≈ 0, got {}",
            t.gradient_us()
        );
    }

    #[test]
    fn delay_gradient_is_jitter_immune() {
        // The Wi-Fi landmine (doctrine §2): heavy STATIONARY jitter must
        // NOT register as a standing queue. The old mean-vs-min signal
        // reported ~½·jitter-spread of permanent false queue here and
        // drained forever; short-min vs long-min must read ≈ 0 because the
        // delay FLOOR never moves.
        let mut t = DelayGradientTracker::new();
        let base = std::time::Instant::now();
        let mut seed = 12345u64;
        for i in 0u64..120 {
            // crude LCG jitter in [0, 30_000) µs above a fixed 8 ms floor.
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let jitter = (seed >> 33) % 30_000;
            t.observe(
                base + std::time::Duration::from_millis(i * 25),
                8_000 + jitter as i64,
            );
        }
        assert!(
            t.gradient_us() < 4_000,
            "stationary jitter must not look like a standing queue, got {} µs",
            t.gradient_us()
        );
    }

    #[test]
    fn delay_gradient_detects_queue_growth() {
        let mut t = DelayGradientTracker::new();
        let base = std::time::Instant::now();
        // ~1.25 s low baseline (exceeds the short window).
        for i in 0u64..50 {
            t.observe(base + std::time::Duration::from_millis(i * 25), 5_000);
        }
        // The delay FLOOR itself rises by 40 ms (a real standing queue),
        // sustained for ~1.25 s so it fills the short window.
        for i in 50u64..100 {
            t.observe(base + std::time::Duration::from_millis(i * 25), 45_000);
        }
        assert!(
            t.gradient_us() > 30_000,
            "a risen delay floor must surface as a positive gradient, got {}",
            t.gradient_us()
        );
    }

    #[test]
    fn delay_gradient_handles_clock_wrap() {
        let mut t = DelayGradientTracker::new();
        let base = std::time::Instant::now();
        for i in 0u64..40 {
            t.observe(base + std::time::Duration::from_millis(i * 25), 10_000);
        }
        // A u32 µs wrap makes rel jump hugely negative — must reset, not
        // report a giant bogus gradient.
        t.observe(base + std::time::Duration::from_millis(1_100), -3_000_000);
        assert_eq!(
            t.gradient_us(),
            0,
            "a clock wrap must reset the baseline, not corrupt the gradient"
        );
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
            None,
        );

        // Build bonding-header-wrapped payload and send
        let payload = Bytes::from_static(b"hello strata");
        let header = crate::protocol::header::BondingHeader::new(0);
        let wrapped = header.wrap(payload.clone());

        use crate::net::interface::LinkSender;
        sender.send(&wrapped).unwrap();

        // Wait for the packet to arrive and be processed
        match rcv.output_rx.recv_timeout(Duration::from_secs(2)) {
            Ok((received, _discont)) => {
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
            None,
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
        for (i, (data, _discont)) in received.iter().enumerate() {
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
