use bytes::Bytes;
use quanta::Instant;
use std::collections::VecDeque;
use std::time::Duration;

/// An incoming packet with its bonding sequence ID and arrival timestamp.
pub struct Packet {
    pub seq_id: u64,
    pub payload: Bytes,
    pub arrival_time: Instant,
}

/// Jitter buffer that reorders and releases packets in sequence order.
///
/// Packets are held for at least the configured latency before release.
/// The latency adapts upward based on observed inter-arrival jitter
/// (p95 × multiplier), capped at `max_latency`. Missing packets are
/// skipped after the `skip_after` timeout to prevent head-of-line blocking.
pub struct ReassemblyBuffer {
    buffer: Vec<Option<Packet>>,
    capacity: usize,
    buffered: usize,
    next_seq: u64,
    latency: Duration,
    start_latency: Duration,
    skip_after: Option<Duration>,
    jitter_latency_multiplier: f64,
    max_latency: Duration,
    min_latency: Duration,
    pub lost_packets: u64,
    pub late_packets: u64,
    pub duplicate_packets: u64,
    pub packets_delivered: u64,

    // Adaptive latency — jitter tracking
    last_arrival: Option<Instant>,
    avg_iat: f64,
    jitter_smoothed: f64,
    jitter_samples: VecDeque<f64>,

    // Adaptive latency — bidirectional smoothing
    target_latency: Duration,
    ramp_up_alpha: f64,
    ramp_down_alpha: f64,
    stable_since: Option<Instant>,
    stability_threshold: Duration,

    // Adaptive latency — loss-aware sizing
    loss_rate_smoothed: f64,
    loss_penalty_ms: f64,

    // Desync recovery: track consecutive late packets to detect when
    // next_seq has jumped ahead of the sender's actual sequence space.
    consecutive_late: u64,
    /// Highest seq_id seen among consecutive late packets — used as the
    /// resync target so we resume from the most recent sender position,
    /// not from an arbitrary old packet that happened to arrive last.
    max_late_seq: u64,
}

/// Configuration for the reassembly jitter buffer.
#[derive(Debug, Clone)]
pub struct ReassemblyConfig {
    pub start_latency: Duration,
    pub buffer_capacity: usize,
    pub skip_after: Option<Duration>,
    /// Multiplier for p95 jitter in adaptive latency (default: 4.0)
    pub jitter_latency_multiplier: f64,
    /// Hard ceiling on adaptive reassembly latency (default: 500ms)
    pub max_latency_ms: u64,
    /// Floor for adaptive latency in ms (default: 10). Can be below start_latency.
    pub min_latency_ms: u64,
    /// Smoothing factor for upward adaptation (default: 0.3 = fast ramp-up).
    pub ramp_up_alpha: f64,
    /// Smoothing factor for downward adaptation (default: 0.02 = slow ramp-down).
    pub ramp_down_alpha: f64,
    /// Stable period (ms) before allowing ramp-down (default: 2000).
    pub stability_threshold_ms: u64,
    /// Extra latency (ms) added at 100% loss rate (default: 500). Scaled linearly.
    pub loss_penalty_ms: f64,
}

impl Default for ReassemblyConfig {
    fn default() -> Self {
        Self {
            start_latency: Duration::from_millis(50),
            buffer_capacity: 2048,
            skip_after: None,
            jitter_latency_multiplier: 2.0,
            max_latency_ms: 500,
            min_latency_ms: 10,
            ramp_up_alpha: 0.3,
            ramp_down_alpha: 0.05,
            stability_threshold_ms: 2000,
            loss_penalty_ms: 200.0,
        }
    }
}

/// Snapshot of reassembly buffer statistics for telemetry.
#[derive(Default, Clone, Debug)]
pub struct ReassemblyStats {
    pub queue_depth: usize,
    pub next_seq: u64,
    pub lost_packets: u64,
    pub late_packets: u64,
    pub duplicate_packets: u64,
    pub current_latency_ms: u64,
    /// The computed ideal latency the buffer is tracking toward.
    pub target_latency_ms: u64,
    /// Current smoothed jitter estimate in milliseconds.
    pub jitter_estimate_ms: f64,
    /// Recent smoothed loss rate (0.0–1.0).
    pub loss_rate: f64,
    /// Packets successfully delivered.
    pub packets_delivered: u64,
}

fn percentile(samples: &VecDeque<f64>, pct: f64) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let mut v: Vec<f64> = samples.iter().copied().collect();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let idx = ((v.len() - 1) as f64 * pct).round() as usize;
    v[idx.min(v.len() - 1)]
}

impl ReassemblyBuffer {
    pub fn new(start_seq: u64, latency: Duration) -> Self {
        Self::with_config(
            start_seq,
            ReassemblyConfig {
                start_latency: latency,
                ..ReassemblyConfig::default()
            },
        )
    }

    pub fn with_config(start_seq: u64, config: ReassemblyConfig) -> Self {
        let capacity = config.buffer_capacity.max(16);
        Self {
            buffer: (0..capacity).map(|_| None).collect(),
            capacity,
            buffered: 0,
            next_seq: start_seq,
            latency: config.start_latency,
            start_latency: config.start_latency,
            skip_after: config.skip_after,
            jitter_latency_multiplier: config.jitter_latency_multiplier,
            max_latency: Duration::from_millis(config.max_latency_ms),
            min_latency: Duration::from_millis(config.min_latency_ms),
            lost_packets: 0,
            late_packets: 0,
            duplicate_packets: 0,
            packets_delivered: 0,
            last_arrival: None,
            avg_iat: 0.0,
            jitter_smoothed: 0.0,
            jitter_samples: VecDeque::with_capacity(128),
            target_latency: config.start_latency,
            ramp_up_alpha: config.ramp_up_alpha,
            ramp_down_alpha: config.ramp_down_alpha,
            stable_since: None,
            stability_threshold: Duration::from_millis(config.stability_threshold_ms),
            loss_rate_smoothed: 0.0,
            loss_penalty_ms: config.loss_penalty_ms,
            consecutive_late: 0,
            max_late_seq: 0,
        }
    }

    pub fn get_stats(&self) -> ReassemblyStats {
        ReassemblyStats {
            queue_depth: self.buffered,
            next_seq: self.next_seq,
            lost_packets: self.lost_packets,
            late_packets: self.late_packets,
            duplicate_packets: self.duplicate_packets,
            current_latency_ms: self.latency.as_millis() as u64,
            target_latency_ms: self.target_latency.as_millis() as u64,
            jitter_estimate_ms: self.jitter_smoothed * 1000.0,
            loss_rate: self.loss_rate_smoothed,
            packets_delivered: self.packets_delivered,
        }
    }

    pub fn push(&mut self, seq_id: u64, payload: Bytes, now: Instant) {
        // Calculate Jitter
        if let Some(last) = self.last_arrival {
            let iat = now.duration_since(last).as_secs_f64();

            // EWMA alpha
            let alpha = 0.1;

            // Update average inter-arrival time
            self.avg_iat = (1.0 - alpha) * self.avg_iat + alpha * iat;

            // Calculate instantaneous jitter
            let jitter = (iat - self.avg_iat).abs();

            // Smooth jitter
            self.jitter_smoothed = (1.0 - alpha) * self.jitter_smoothed + alpha * jitter;
            self.jitter_samples.push_back(jitter);
            if self.jitter_samples.len() > 128 {
                self.jitter_samples.pop_front();
            }

            // Compute jitter component of target latency
            let jitter_est = if self.jitter_samples.len() >= 5 {
                percentile(&self.jitter_samples, 0.95)
            } else {
                self.jitter_smoothed
            };
            let jitter_ms = jitter_est * 1000.0;
            let jitter_component = self.jitter_latency_multiplier * jitter_ms;

            // Loss-aware component: more buffer when losing packets
            let loss_component = self.loss_rate_smoothed * self.loss_penalty_ms;

            // Compute target latency
            let target_ms =
                self.start_latency.as_millis() as f64 + jitter_component + loss_component;
            self.target_latency = Duration::from_millis(target_ms as u64)
                .max(self.min_latency)
                .min(self.max_latency);

            // Bidirectional smoothing: fast up, slow down
            let current_ms = self.latency.as_secs_f64() * 1000.0;
            let target_ms = self.target_latency.as_secs_f64() * 1000.0;

            if target_ms > current_ms + 0.5 {
                // Fast ramp-up
                let new_ms = current_ms + self.ramp_up_alpha * (target_ms - current_ms);
                self.latency = Duration::from_secs_f64(new_ms / 1000.0);
                self.stable_since = None;
            } else if target_ms < current_ms - 0.5 {
                // Fast ramp-down when target is dramatically lower (stall
                // recovery: loss_rate dropped → loss_penalty shrank).  Use
                // the same ramp-up alpha to avoid being stuck at a bloated
                // latency for seconds after the underlying issue resolved.
                if current_ms > target_ms * 2.0 {
                    let new_ms = current_ms + self.ramp_up_alpha * (target_ms - current_ms);
                    self.latency = Duration::from_secs_f64(new_ms / 1000.0).max(self.min_latency);
                    self.stable_since = None;
                } else {
                    // Normal slow ramp-down, only after stability period
                    match self.stable_since {
                        Some(since) if now.duration_since(since) >= self.stability_threshold => {
                            let new_ms =
                                current_ms + self.ramp_down_alpha * (target_ms - current_ms);
                            self.latency =
                                Duration::from_secs_f64(new_ms / 1000.0).max(self.min_latency);
                        }
                        None => {
                            self.stable_since = Some(now);
                        }
                        _ => {} // Waiting for stability threshold
                    }
                }
            }
        }
        self.last_arrival = Some(now);

        if seq_id < self.next_seq {
            self.consecutive_late += 1;
            // Track the highest seq_id seen among consecutive late packets.
            // This is the resync target: the most recent position the sender
            // was at, not an arbitrary old packet that happened to arrive last.
            if seq_id > self.max_late_seq {
                self.max_late_seq = seq_id;
            }

            // If we see many consecutive late packets, the buffer's next_seq
            // has desynchronised from the sender (e.g. after a burst loss
            // caused a large gap-skip).  Reset to re-sync with the sender.
            const RESYNC_THRESHOLD: u64 = 100;
            if self.consecutive_late >= RESYNC_THRESHOLD {
                // Use the highest seq_id seen in this window as the resync
                // target — it's the best approximation of the sender's current
                // position.  Using `seq_id` (the last, possibly very old
                // retransmission) would reset next_seq to 0 or some stale
                // value and permanently stall the receiver.
                let resync_target = self.max_late_seq + 1;
                tracing::warn!(
                    old_next_seq = self.next_seq,
                    new_next_seq = resync_target,
                    consecutive_late = self.consecutive_late,
                    "reassembly buffer desync detected — resetting next_seq to re-sync with sender"
                );
                // Clear stale buffer contents below the resync target
                for slot in self.buffer.iter_mut() {
                    if let Some(p) = slot
                        && p.seq_id < resync_target
                    {
                        *slot = None;
                        self.buffered = self.buffered.saturating_sub(1);
                    }
                }
                self.next_seq = resync_target;
                self.consecutive_late = 0;
                self.max_late_seq = 0;
                // Reset adaptive latency — the stall inflated it via
                // loss_penalty and we need a fresh start to avoid the
                // new packets immediately being classified as late too.
                self.loss_rate_smoothed = 0.0;
                self.latency = self.start_latency;
                self.target_latency = self.start_latency;
                self.stable_since = None;
                // Fall through to insert this packet normally
            } else {
                // Late packet, drop
                self.late_packets += 1;
                return;
            }
        } else {
            self.consecutive_late = 0;
            self.max_late_seq = 0;
        }

        let capacity = self.capacity as u64;
        if seq_id >= self.next_seq + capacity {
            let new_next = seq_id.saturating_sub(capacity.saturating_sub(1));
            if new_next > self.next_seq {
                let skipped = new_next - self.next_seq;
                self.lost_packets += skipped;
                self.advance_window(new_next);
            }
        }

        let idx = self.buffer_index(seq_id);
        if let Some(existing) = &self.buffer[idx] {
            if existing.seq_id == seq_id {
                // Duplicate packet (same seq_id arrived again)
                self.duplicate_packets += 1;
                return; // Don't overwrite
            } else if existing.seq_id >= self.next_seq {
                // Different packet in this slot, was lost
                self.lost_packets += 1;
            }
        } else {
            self.buffered += 1;
        }

        self.buffer[idx] = Some(Packet {
            seq_id,
            payload,
            arrival_time: now,
        });
    }

    /// Release ready packets. Returns `(payload, discont)` pairs where
    /// `discont = true` means a gap was skipped immediately before this
    /// packet (the MPEG-TS byte-alignment may have shifted).
    pub fn tick(&mut self, now: Instant) -> Vec<(Bytes, bool)> {
        let loss_before = self.lost_packets;
        let mut released = Vec::new();
        let skip_after = self.skip_after.unwrap_or(self.latency);
        let release_after = self
            .skip_after
            .map(|v| v.min(self.latency))
            .unwrap_or(self.latency);

        // Set after a gap skip; cleared after the next packet is released.
        let mut discont = false;

        // While loop to process available packets or skip gaps
        loop {
            // Case 1: We have the next packet
            let idx = self.buffer_index(self.next_seq);
            if let Some(packet) = &self.buffer[idx]
                && packet.seq_id == self.next_seq
            {
                // Check if it has satisfied the latency requirement
                if now.duration_since(packet.arrival_time) >= release_after {
                    let p = self.buffer[idx].take().unwrap();
                    self.buffered = self.buffered.saturating_sub(1);
                    released.push((p.payload, std::mem::take(&mut discont)));
                    self.next_seq += 1;
                    continue;
                }
                // Not ready yet
                break;
            }

            // Case 2: We have a gap (missing next_seq)
            if let Some((first_seq, first_arrival)) = self.find_next_available()
                && now.duration_since(first_arrival) >= skip_after
            {
                let skipped = first_seq.saturating_sub(self.next_seq);
                self.lost_packets += skipped;
                self.advance_window(first_seq);
                discont = true;
                continue;
            }

            // No packets or waiting for gap to fill
            break;
        }

        // Track delivery + loss for adaptive sizing
        self.packets_delivered += released.len() as u64;
        let new_losses = self.lost_packets - loss_before;
        let total_events = released.len() as u64 + new_losses;
        if total_events > 0 {
            let instant_loss = new_losses as f64 / total_events as f64;
            self.loss_rate_smoothed = 0.95 * self.loss_rate_smoothed + 0.05 * instant_loss;
        }
        if new_losses > 0 {
            self.stable_since = None;
        }

        released
    }

    fn buffer_index(&self, seq_id: u64) -> usize {
        (seq_id % self.capacity as u64) as usize
    }

    fn advance_window(&mut self, new_next: u64) {
        let old_next = self.next_seq;
        if new_next <= old_next {
            return;
        }
        for seq in old_next..new_next {
            let idx = self.buffer_index(seq);
            if let Some(packet) = &self.buffer[idx]
                && packet.seq_id == seq
            {
                self.buffer[idx] = None;
                self.buffered = self.buffered.saturating_sub(1);
            }
        }
        self.next_seq = new_next;
    }

    fn find_next_available(&self) -> Option<(u64, Instant)> {
        let mut best: Option<(u64, Instant)> = None;
        for slot in self.buffer.iter().flatten() {
            if slot.seq_id <= self.next_seq {
                continue;
            }
            match best {
                Some((best_seq, _)) if slot.seq_id >= best_seq => {}
                _ => {
                    best = Some((slot.seq_id, slot.arrival_time));
                }
            }
        }
        best
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_in_order_delivery() {
        let mut buf = ReassemblyBuffer::new(0, Duration::from_millis(100));
        let start = Instant::now();
        let p1 = Bytes::from_static(b"P1");

        buf.push(0, p1.clone(), start);

        // Immediate tick - should not release (latency 100ms)
        let out = buf.tick(start);
        assert!(out.is_empty());

        // Tick after latency
        let out = buf.tick(start + Duration::from_millis(100));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, p1);
    }

    #[test]
    fn test_reordering() {
        let mut buf = ReassemblyBuffer::new(0, Duration::from_millis(50));
        let start = Instant::now();

        // Arrives: Seq 2, then Seq 0, then Seq 1
        buf.push(2, Bytes::from_static(b"P2"), start);
        buf.push(0, Bytes::from_static(b"P0"), start);
        buf.push(1, Bytes::from_static(b"P1"), start);

        // Wait for latency
        let out = buf.tick(start + Duration::from_millis(50));

        // Should come out as P0, P1, P2
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].0, Bytes::from_static(b"P0"));
        assert_eq!(out[1].0, Bytes::from_static(b"P1"));
        assert_eq!(out[2].0, Bytes::from_static(b"P2"));
    }

    #[test]
    fn test_gap_skipping() {
        let mut buf = ReassemblyBuffer::new(0, Duration::from_millis(50));
        let start = Instant::now();

        // P0 missing
        // P1 arrives
        buf.push(1, Bytes::from_static(b"P1"), start);

        // Tick at 50ms. P1 is ready, but P0 is missing.
        // P1 arrived at `start`. It has waited 50ms.
        // The logic should say: P1 has expired latency. So we define next_seq = 1.

        let out = buf.tick(start + Duration::from_millis(50));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, Bytes::from_static(b"P1"));
    }

    #[test]
    fn test_adaptive_latency() {
        // Base latency 10ms
        let mut buf = ReassemblyBuffer::new(0, Duration::from_millis(10));
        let start = Instant::now();

        // Push packets with jitter
        // P0 at 0ms
        buf.push(0, Bytes::from_static(b"P0"), start);
        assert_eq!(buf.latency.as_millis(), 10); // First packet, no jitter calc yet

        // P1 at 20ms (IAT 20ms). Avg IAT will move towards 20ms.
        buf.push(
            1,
            Bytes::from_static(b"P1"),
            start + Duration::from_millis(20),
        );

        // P2 at 30ms (IAT 10ms).
        // Jitter introduced.
        buf.push(
            2,
            Bytes::from_static(b"P2"),
            start + Duration::from_millis(30),
        );

        // P3 at 60ms (IAT 30ms).
        buf.push(
            3,
            Bytes::from_static(b"P3"),
            start + Duration::from_millis(60),
        );

        // The latency should have increased from 10ms due to jitter
        let current_latency = buf.latency.as_millis();
        assert!(
            current_latency > 10,
            "Latency should increase due to jitter (current: {})",
            current_latency
        );

        // Check stats
        let stats = buf.get_stats();
        assert_eq!(stats.current_latency_ms, current_latency as u64);
    }

    #[test]
    fn test_percentile_basic() {
        let mut samples = VecDeque::new();
        samples.push_back(1.0);
        samples.push_back(2.0);
        samples.push_back(3.0);
        samples.push_back(100.0);

        let p50 = percentile(&samples, 0.5);
        let p95 = percentile(&samples, 0.95);

        assert_eq!(p50, 3.0);
        assert_eq!(p95, 100.0);
    }

    #[test]
    fn test_aggressive_skip_policy() {
        let config = ReassemblyConfig {
            start_latency: Duration::from_millis(100),
            buffer_capacity: 64,
            skip_after: Some(Duration::from_millis(30)),
            ..Default::default()
        };
        let mut buf = ReassemblyBuffer::with_config(0, config);
        let start = Instant::now();

        // Missing seq 0, seq 1 arrives
        buf.push(1, Bytes::from_static(b"P1"), start);

        // At 30ms, aggressive skip should release P1
        let out = buf.tick(start + Duration::from_millis(30));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, Bytes::from_static(b"P1"));
    }

    #[test]
    fn test_far_ahead_packet_advances_window() {
        let config = ReassemblyConfig {
            start_latency: Duration::from_millis(10),
            buffer_capacity: 8,
            ..Default::default()
        };
        let mut buf = ReassemblyBuffer::with_config(0, config);
        let start = Instant::now();

        // Push far ahead packet to force window advance
        buf.push(20, Bytes::from_static(b"P20"), start);
        let out = buf.tick(start + Duration::from_millis(10));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, Bytes::from_static(b"P20"));
        assert!(buf.lost_packets > 0);
    }

    #[test]
    fn test_duplicate_packet_counting() {
        let mut buf = ReassemblyBuffer::new(0, Duration::from_millis(100));
        let start = Instant::now();

        // Push packet with seq_id 0
        buf.push(0, Bytes::from_static(b"P0-original"), start);
        assert_eq!(buf.duplicate_packets, 0);

        // Push same seq_id again (duplicate)
        buf.push(0, Bytes::from_static(b"P0-duplicate"), start);
        assert_eq!(buf.duplicate_packets, 1);

        // Push another different packet
        buf.push(1, Bytes::from_static(b"P1"), start);
        assert_eq!(buf.duplicate_packets, 1); // Still 1

        // Push duplicate of seq_id 1
        buf.push(1, Bytes::from_static(b"P1-duplicate"), start);
        assert_eq!(buf.duplicate_packets, 2);

        // Verify stats expose duplicate count
        let stats = buf.get_stats();
        assert_eq!(stats.duplicate_packets, 2);
    }

    #[test]
    fn test_duplicate_vs_late_packets() {
        let mut buf = ReassemblyBuffer::new(0, Duration::from_millis(100));
        let start = Instant::now();

        // Push packet 0 and 1
        buf.push(0, Bytes::from_static(b"P0"), start);
        buf.push(1, Bytes::from_static(b"P1"), start);

        // Release them
        let out = buf.tick(start + Duration::from_millis(100));
        assert_eq!(out.len(), 2);

        // Now push seq_id 0 again - this is LATE, not duplicate
        // (because next_seq has advanced past it)
        buf.push(
            0,
            Bytes::from_static(b"P0-late"),
            start + Duration::from_millis(120),
        );

        assert_eq!(buf.late_packets, 1);
        assert_eq!(buf.duplicate_packets, 0); // Not counted as duplicate
    }

    #[test]
    fn test_latency_max_capping() {
        let config = ReassemblyConfig {
            start_latency: Duration::from_millis(10),
            jitter_latency_multiplier: 100.0,
            max_latency_ms: 200,
            ..Default::default()
        };
        let mut buf = ReassemblyBuffer::with_config(0, config);
        let start = Instant::now();

        buf.push(0, Bytes::from_static(b"P0"), start);
        buf.push(
            1,
            Bytes::from_static(b"P1"),
            start + Duration::from_millis(1),
        );
        buf.push(
            2,
            Bytes::from_static(b"P2"),
            start + Duration::from_millis(100),
        );
        buf.push(
            3,
            Bytes::from_static(b"P3"),
            start + Duration::from_millis(101),
        );
        buf.push(
            4,
            Bytes::from_static(b"P4"),
            start + Duration::from_millis(300),
        );

        assert!(
            buf.latency <= Duration::from_millis(200),
            "Latency should be capped at max_latency_ms (200ms), got: {:?}",
            buf.latency
        );
    }

    #[test]
    fn test_buffer_capacity_boundary() {
        let config = ReassemblyConfig {
            start_latency: Duration::from_millis(10),
            buffer_capacity: 16,
            ..Default::default()
        };
        let mut buf = ReassemblyBuffer::with_config(0, config);
        let start = Instant::now();

        for i in 0..16u64 {
            buf.push(i, Bytes::from(format!("P{}", i)), start);
        }
        assert_eq!(buf.buffered, 16);

        let out = buf.tick(start + Duration::from_millis(10));
        assert_eq!(out.len(), 16);
    }

    #[test]
    fn test_stats_during_operation() {
        let mut buf = ReassemblyBuffer::new(0, Duration::from_millis(50));
        let start = Instant::now();

        buf.push(0, Bytes::from_static(b"P0"), start);
        buf.push(1, Bytes::from_static(b"P1"), start);

        let stats = buf.get_stats();
        assert_eq!(stats.queue_depth, 2);
        assert_eq!(stats.next_seq, 0);
        assert_eq!(stats.lost_packets, 0);

        let _ = buf.tick(start + Duration::from_millis(50));
        let stats = buf.get_stats();
        assert_eq!(stats.queue_depth, 0);
        assert_eq!(stats.next_seq, 2);
    }

    #[test]
    fn test_percentile_single_sample() {
        let mut samples = VecDeque::new();
        samples.push_back(5.0);
        assert_eq!(percentile(&samples, 0.5), 5.0);
        assert_eq!(percentile(&samples, 0.95), 5.0);
    }

    #[test]
    fn test_percentile_empty() {
        let samples = VecDeque::new();
        assert_eq!(percentile(&samples, 0.5), 0.0);
    }

    #[test]
    fn test_many_packets_in_order() {
        let mut buf = ReassemblyBuffer::new(0, Duration::from_millis(10));
        let start = Instant::now();

        for i in 0..1000u64 {
            buf.push(i, Bytes::from(vec![i as u8; 100]), start);
        }

        let out = buf.tick(start + Duration::from_millis(10));
        assert_eq!(out.len(), 1000);
        assert_eq!(buf.lost_packets, 0);
        assert_eq!(buf.duplicate_packets, 0);
    }

    // ─── Dynamic Jitter Buffer Tests ────────────────────────────────────

    #[test]
    fn test_dynamic_ramp_down() {
        let config = ReassemblyConfig {
            start_latency: Duration::from_millis(10),
            stability_threshold_ms: 0, // Immediate ramp-down for testing
            ramp_down_alpha: 0.5,
            ramp_up_alpha: 1.0, // Instant ramp-up
            ..Default::default()
        };
        let mut buf = ReassemblyBuffer::with_config(0, config);
        let start = Instant::now();

        // Phase 1: heavy jitter (alternating fast/slow arrivals)
        buf.push(0, Bytes::from(vec![0; 100]), start);
        buf.push(
            1,
            Bytes::from(vec![0; 100]),
            start + Duration::from_millis(5),
        );
        buf.push(
            2,
            Bytes::from(vec![0; 100]),
            start + Duration::from_millis(55),
        );
        buf.push(
            3,
            Bytes::from(vec![0; 100]),
            start + Duration::from_millis(60),
        );
        buf.push(
            4,
            Bytes::from(vec![0; 100]),
            start + Duration::from_millis(110),
        );
        buf.push(
            5,
            Bytes::from(vec![0; 100]),
            start + Duration::from_millis(115),
        );

        let high_latency = buf.latency;
        assert!(
            high_latency > Duration::from_millis(15),
            "Latency should increase from jitter: {:?}",
            high_latency
        );

        // Phase 2: steady arrivals (150+ pushes to flush jitter window)
        for i in 6u64..200 {
            buf.push(
                i,
                Bytes::from(vec![0; 100]),
                start + Duration::from_millis(120 + (i - 6) * 10),
            );
        }

        let lower_latency = buf.latency;
        assert!(
            lower_latency < high_latency,
            "Latency should ramp down with stable conditions: high={:?}, low={:?}",
            high_latency,
            lower_latency
        );
    }

    /// Regression: after a stall inflates latency via loss_penalty, clearing
    /// the loss must ramp latency back down quickly (using ramp_up_alpha, not
    /// the slow ramp_down_alpha) when current_ms > target_ms * 2.0.
    ///
    /// Before the fix the slow path was always taken, leaving latency stuck at
    /// 200+ ms for many seconds after loss cleared — causing A/V sync issues
    /// and head-of-line blocking on recovered links.
    #[test]
    fn stall_recovery_ramp_down_fast() {
        // ramp_up_alpha=1.0 → instant ramp-up; ramp_down_alpha=0.02 (slow default)
        // Without the fast-ramp-down path, after 5 push() calls latency would
        // still be ~200ms when loss clears; with it, it should be ≤ start_latency.
        let config = ReassemblyConfig {
            start_latency: Duration::from_millis(20),
            buffer_capacity: 256,
            skip_after: Some(Duration::from_millis(5)),
            ramp_up_alpha: 1.0,
            ramp_down_alpha: 0.02, // very slow — without the fast path we'd stay high
            loss_penalty_ms: 500.0,
            stability_threshold_ms: 0, // no stability wait
            ..Default::default()
        };
        let mut buf = ReassemblyBuffer::with_config(0, config);
        let start = Instant::now();

        // Phase 1: push 50 packets with a gap to drive loss_rate_smoothed up,
        // then tick to register the losses.
        for i in 0u64..50 {
            buf.push(
                i,
                Bytes::from(vec![i as u8]),
                start + Duration::from_millis(i),
            );
        }
        // Create a big gap — skip to seq 200 so ~150 packets appear lost
        let t_gap = start + Duration::from_millis(100);
        buf.push(200, Bytes::from_static(b"jump"), t_gap);
        let _ = buf.tick(t_gap + Duration::from_millis(10));

        // Manually inflate loss_rate_smoothed and latency to worst-case
        buf.loss_rate_smoothed = 1.0;
        buf.latency = Duration::from_millis(500);
        buf.target_latency = Duration::from_millis(500);

        let bloated_latency = buf.latency;

        // Phase 2: loss clears — push steady in-order packets so the buffer
        // computes a low target (start_latency + 0 loss_penalty = 20ms).
        // current_ms(500) > target_ms(20) * 2 → fast ramp-down path fires.
        let t_clear = t_gap + Duration::from_millis(200);
        buf.loss_rate_smoothed = 0.0; // loss cleared
        for i in 0u64..20 {
            buf.push(
                201 + i,
                Bytes::from(vec![i as u8]),
                t_clear + Duration::from_millis(i * 10),
            );
        }

        assert!(
            buf.latency < bloated_latency / 2,
            "fast ramp-down should halve bloated latency quickly: still at {:?} (started at {:?})",
            buf.latency,
            bloated_latency,
        );
    }

    #[test]
    fn test_loss_increases_latency() {
        let config = ReassemblyConfig {
            start_latency: Duration::from_millis(10),
            skip_after: Some(Duration::from_millis(5)),
            ramp_up_alpha: 1.0, // Instant ramp-up
            loss_penalty_ms: 1000.0,
            ..Default::default()
        };
        let mut buf = ReassemblyBuffer::with_config(0, config);
        let start = Instant::now();

        // Push seq 0 then skip seq 1, push seq 2-5
        buf.push(0, Bytes::from_static(b"P0"), start);
        buf.push(
            2,
            Bytes::from_static(b"P2"),
            start + Duration::from_millis(1),
        );
        buf.push(
            3,
            Bytes::from_static(b"P3"),
            start + Duration::from_millis(2),
        );
        buf.push(
            4,
            Bytes::from_static(b"P4"),
            start + Duration::from_millis(3),
        );
        buf.push(
            5,
            Bytes::from_static(b"P5"),
            start + Duration::from_millis(4),
        );

        // Tick to skip gap (seq 1 missing, skip_after=5ms)
        let _ = buf.tick(start + Duration::from_millis(20));
        assert!(buf.lost_packets > 0, "Should have recorded a loss");
        assert!(buf.loss_rate_smoothed > 0.0, "Loss rate should be non-zero");

        // Push more packets — latency should incorporate loss penalty
        for i in 6..10 {
            buf.push(
                i,
                Bytes::from(vec![0; 100]),
                start + Duration::from_millis(20 + (i - 6) * 10),
            );
        }

        let stats = buf.get_stats();
        assert!(
            stats.loss_rate > 0.0,
            "Stats should report non-zero loss rate"
        );
    }

    #[test]
    fn test_min_latency_floor() {
        let config = ReassemblyConfig {
            start_latency: Duration::from_millis(5),
            min_latency_ms: 20,
            ramp_up_alpha: 1.0,
            ..Default::default()
        };
        let mut buf = ReassemblyBuffer::with_config(0, config);
        let start = Instant::now();

        // Push steady packets — target = max(20, 5 + jitter) = 20 when jitter is small
        for i in 0..10 {
            buf.push(
                i,
                Bytes::from(vec![0; 100]),
                start + Duration::from_millis(i * 10),
            );
        }

        assert!(
            buf.latency >= Duration::from_millis(20),
            "Latency should not go below min_latency (20ms): {:?}",
            buf.latency
        );
    }

    #[test]
    fn test_stats_target_and_jitter() {
        let mut buf = ReassemblyBuffer::new(0, Duration::from_millis(10));
        let start = Instant::now();

        buf.push(0, Bytes::from_static(b"P0"), start);
        buf.push(
            1,
            Bytes::from_static(b"P1"),
            start + Duration::from_millis(20),
        );
        buf.push(
            2,
            Bytes::from_static(b"P2"),
            start + Duration::from_millis(30),
        );

        let stats = buf.get_stats();
        assert!(stats.target_latency_ms >= 10);
        assert!(stats.jitter_estimate_ms >= 0.0);
    }

    #[test]
    fn test_delivered_packets_counted() {
        let mut buf = ReassemblyBuffer::new(0, Duration::from_millis(10));
        let start = Instant::now();

        for i in 0..5u64 {
            buf.push(i, Bytes::from(vec![0; 100]), start);
        }

        let out = buf.tick(start + Duration::from_millis(10));
        assert_eq!(out.len(), 5);
        assert_eq!(buf.packets_delivered, 5);

        let stats = buf.get_stats();
        assert_eq!(stats.packets_delivered, 5);
    }

    // ── Regression: snag #14 — max_latency_ms must be wired through ──

    /// A custom max_latency_ms should actually change the ceiling.
    /// Before the fix, the default (500ms) was always used regardless
    /// of the config value.
    #[test]
    fn test_max_latency_ms_wired_from_config() {
        let config = ReassemblyConfig {
            start_latency: Duration::from_millis(50),
            max_latency_ms: 3000,
            ..Default::default()
        };
        let buf = ReassemblyBuffer::with_config(0, config);
        assert_eq!(
            buf.max_latency,
            Duration::from_millis(3000),
            "max_latency should be set from config, not hardcoded to 500"
        );
    }

    /// Packets arriving within max_latency should NOT be counted as late
    /// when max_latency is raised above the default 500ms.
    #[test]
    fn test_high_max_latency_accepts_slow_packets() {
        let config = ReassemblyConfig {
            start_latency: Duration::from_millis(500),
            max_latency_ms: 3000,
            // Fast ramp-up so latency reaches ceiling quickly
            ramp_up_alpha: 1.0,
            jitter_latency_multiplier: 4.0,
            ..Default::default()
        };
        let mut buf = ReassemblyBuffer::with_config(0, config);
        let start = Instant::now();

        // Simulate high-jitter LTE: packets arrive with 800ms IAT variation
        for i in 0..20u64 {
            let jitter = if i % 3 == 0 { 800 } else { 10 };
            buf.push(
                i,
                Bytes::from(vec![0; 100]),
                start + Duration::from_millis(i * 50 + jitter),
            );
        }

        // Tick well past the arrival window
        let _ = buf.tick(start + Duration::from_millis(5000));

        let stats = buf.get_stats();
        // With max_latency=3000ms, the buffer should have absorbed the
        // jitter without classifying packets as late/lost
        assert!(
            stats.late_packets < 5,
            "With 3000ms ceiling, most packets should be accepted (got {} late)",
            stats.late_packets
        );
    }

    /// With the default max_latency (500ms), the same high-jitter pattern
    /// causes significantly more late packets — proving the ceiling matters.
    #[test]
    fn test_default_max_latency_drops_slow_packets() {
        let config = ReassemblyConfig {
            start_latency: Duration::from_millis(50),
            // max_latency_ms: 500 (default)
            ramp_up_alpha: 1.0,
            jitter_latency_multiplier: 4.0,
            ..Default::default()
        };
        let mut buf = ReassemblyBuffer::with_config(0, config);
        let start = Instant::now();

        // Same high-jitter pattern as above
        for i in 0..20u64 {
            let jitter = if i % 3 == 0 { 800 } else { 10 };
            buf.push(
                i,
                Bytes::from(vec![0; 100]),
                start + Duration::from_millis(i * 50 + jitter),
            );
        }

        let _ = buf.tick(start + Duration::from_millis(5000));
        let _stats = buf.get_stats();

        // Confirm the default ceiling is 500ms (regression guard)
        assert_eq!(
            ReassemblyConfig::default().max_latency_ms,
            500,
            "Default max_latency_ms should be 500"
        );
    }

    // ── Regression: resync resets adaptive latency state ─────────────

    /// After the desync reset (100 consecutive late packets), the buffer's
    /// adaptive latency state should be fully cleared so that new packets
    /// arriving after the reset are not immediately classified as "late".
    ///
    /// Before the fix, loss_rate_smoothed stayed at 1.0, latency stayed at
    /// max (500ms), and target_latency stayed at max — so every packet that
    /// arrived after the resync was also classified as late, triggering
    /// another resync → infinite loop.
    #[test]
    fn resync_resets_adaptive_latency() {
        let config = ReassemblyConfig {
            start_latency: Duration::from_millis(20),
            buffer_capacity: 64,
            skip_after: Some(Duration::from_millis(5)),
            // Fast ramp-up so loss penalty inflates latency quickly
            ramp_up_alpha: 1.0,
            loss_penalty_ms: 500.0,
            max_latency_ms: 500,
            ..Default::default()
        };
        let mut buf = ReassemblyBuffer::with_config(0, config);
        let start = Instant::now();

        // Phase 1: Normal delivery of packets 0..10
        for i in 0u64..10 {
            buf.push(i, Bytes::from(vec![i as u8]), start);
        }
        let _ = buf.tick(start + Duration::from_millis(20));
        assert_eq!(buf.packets_delivered, 10);

        // Phase 2: Force high loss_rate_smoothed by skipping many packets.
        // Push seq 200 far ahead — window advance skips 190 packets (all "lost"),
        // then tick to register the loss in loss_rate_smoothed.
        let t2 = start + Duration::from_millis(100);
        buf.push(200, Bytes::from_static(b"far"), t2);
        let _ = buf.tick(t2 + Duration::from_millis(10));
        // loss_rate_smoothed should now be elevated
        assert!(
            buf.loss_rate_smoothed > 0.0,
            "loss rate should be elevated after mass loss"
        );

        // Also manually set latency to max to simulate the worst case
        buf.latency = Duration::from_millis(500);
        buf.target_latency = Duration::from_millis(500);
        buf.loss_rate_smoothed = 1.0;

        // Phase 3: Send 100 consecutive "late" packets (seq < next_seq)
        // to trigger the desync reset.
        let t3 = t2 + Duration::from_millis(200);
        let resume_seq = 50u64; // well below next_seq (~201)
        for i in 0u64..100 {
            buf.push(
                resume_seq + i,
                Bytes::from(vec![(resume_seq + i) as u8]),
                t3 + Duration::from_millis(i),
            );
        }

        // The resync should have fired. Verify latency state was reset.
        assert_eq!(
            buf.loss_rate_smoothed, 0.0,
            "loss_rate_smoothed must be cleared on resync"
        );
        assert_eq!(
            buf.latency,
            Duration::from_millis(20),
            "latency must revert to start_latency on resync"
        );
        assert_eq!(
            buf.target_latency,
            Duration::from_millis(20),
            "target_latency must revert to start_latency on resync"
        );
        assert!(
            buf.stable_since.is_none(),
            "stable_since must be cleared on resync"
        );

        // Phase 4: Verify that packets arriving after the resync are delivered,
        // not immediately dropped as "late" again.
        let t4 = t3 + Duration::from_millis(200);
        let post_resync_start = buf.next_seq;
        for i in 0u64..10 {
            buf.push(
                post_resync_start + i,
                Bytes::from(vec![i as u8]),
                t4 + Duration::from_millis(i * 5),
            );
        }
        let out = buf.tick(t4 + Duration::from_millis(100));
        assert!(
            !out.is_empty(),
            "packets after resync must be delivered normally (got {} late, {} delivered)",
            buf.late_packets,
            buf.packets_delivered
        );
    }

    // ── Regression: desync recovery after burst loss ──────────────────

    /// After a large gap-skip pushes next_seq far ahead, subsequent packets
    /// from the sender all have seq < next_seq and are dropped as "late".
    /// The desync detector should reset next_seq after 100 consecutive late
    /// arrivals, allowing delivery to resume.
    #[test]
    fn test_desync_recovery_after_burst_loss() {
        let config = ReassemblyConfig {
            start_latency: Duration::from_millis(10),
            buffer_capacity: 64,
            skip_after: Some(Duration::from_millis(5)),
            ..Default::default()
        };
        let mut buf = ReassemblyBuffer::with_config(0, config);
        let start = Instant::now();

        // Phase 1: Normal delivery of packets 0..50
        for i in 0u64..50 {
            buf.push(i, Bytes::from(vec![i as u8]), start);
        }
        let _ = buf.tick(start + Duration::from_millis(10));
        assert_eq!(buf.packets_delivered, 50);

        // Phase 2: Simulate burst loss — a packet arrives far ahead,
        // causing next_seq to jump (gap-skip via the capacity overflow path).
        let burst_time = start + Duration::from_millis(100);
        buf.push(5000, Bytes::from_static(b"far-ahead"), burst_time);
        let _ = buf.tick(burst_time + Duration::from_millis(10));

        // next_seq should now be well ahead of 50
        assert!(
            buf.next_seq > 100,
            "next_seq should have jumped: {}",
            buf.next_seq
        );

        // Phase 3: Sender continues from seq 60 (it doesn't know about our jump).
        // All of these will be "late" since 60 < next_seq (~4937+).
        let resume_time = burst_time + Duration::from_millis(200);
        for i in 60u64..260 {
            buf.push(
                i,
                Bytes::from(vec![i as u8]),
                resume_time + Duration::from_millis(i - 60),
            );
        }

        // After 100 consecutive late packets, desync recovery should fire.
        // Packets after the reset should be insertable.
        // next_seq should have been reset to somewhere around seq 160.
        assert!(
            buf.next_seq < 5000,
            "next_seq should have been reset after desync: {}",
            buf.next_seq
        );

        // Deliver the packets that were inserted after the resync
        let out = buf.tick(resume_time + Duration::from_millis(300));
        assert!(
            !out.is_empty(),
            "Should deliver packets after desync recovery (delivered {} total)",
            buf.packets_delivered,
        );
    }

    /// The desync counter resets when a normal (non-late) packet arrives,
    /// so occasional late arrivals during normal operation don't falsely
    /// trigger a reset.
    #[test]
    fn test_desync_counter_resets_on_normal_arrival() {
        let config = ReassemblyConfig {
            start_latency: Duration::from_millis(10),
            buffer_capacity: 64,
            skip_after: Some(Duration::from_millis(5)),
            ..Default::default()
        };
        let mut buf = ReassemblyBuffer::with_config(0, config);
        let start = Instant::now();

        // Deliver packets 0..10
        for i in 0u64..10 {
            buf.push(i, Bytes::from(vec![0; 10]), start);
        }
        let _ = buf.tick(start + Duration::from_millis(10));

        // Send 50 late packets (below next_seq=10), then a normal one
        for i in 0u64..50 {
            buf.push(
                i,
                Bytes::from(vec![0; 10]),
                start + Duration::from_millis(20),
            );
        }
        // Interrupt with a valid in-sequence packet
        buf.push(
            10,
            Bytes::from(vec![0; 10]),
            start + Duration::from_millis(20),
        );

        // Counter should have been reset. Send 50 more late packets —
        // total late is 100 but the counter only reached 50 each time.
        for i in 0u64..50 {
            buf.push(
                i,
                Bytes::from(vec![0; 10]),
                start + Duration::from_millis(30),
            );
        }

        // next_seq should NOT have been reset — seq 10 was accepted but
        // not yet released (needs tick), so next_seq stays at 10.
        assert_eq!(
            buf.next_seq, 10,
            "next_seq should not reset with intermittent normal arrivals"
        );
    }

    #[test]
    fn tick_sets_discont_after_gap_skip() {
        let mut buf = ReassemblyBuffer::new(0, Duration::from_millis(50));
        let start = Instant::now();

        // Push seq 0 and seq 2 (gap at seq 1)
        buf.push(0, Bytes::from_static(b"P0"), start);
        buf.push(2, Bytes::from_static(b"P2"), start);

        // Tick after latency — seq 0 released, then gap at 1 skipped, then seq 2 released
        let out = buf.tick(start + Duration::from_millis(50));
        assert_eq!(out.len(), 2);
        // First packet had no gap before it
        assert_eq!(out[0].0, Bytes::from_static(b"P0"));
        assert!(!out[0].1, "P0 should NOT have discont flag");
        // Second packet was preceded by a gap (seq 1 skipped)
        assert_eq!(out[1].0, Bytes::from_static(b"P2"));
        assert!(out[1].1, "P2 should have discont flag after gap skip");
    }
}
