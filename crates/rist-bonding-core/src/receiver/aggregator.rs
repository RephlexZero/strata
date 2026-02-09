use bytes::Bytes;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

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
    pub lost_packets: u64,
    pub late_packets: u64,
    pub duplicate_packets: u64,

    // Adaptive Latency Calculation
    last_arrival: Option<Instant>,
    avg_iat: f64,
    jitter_smoothed: f64,
    jitter_samples: VecDeque<f64>,
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
}

impl Default for ReassemblyConfig {
    fn default() -> Self {
        Self {
            start_latency: Duration::from_millis(50),
            buffer_capacity: 2048,
            skip_after: None,
            jitter_latency_multiplier: 4.0,
            max_latency_ms: 500,
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
}

fn percentile(samples: &VecDeque<f64>, pct: f64) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let mut v: Vec<f64> = samples.iter().copied().collect();
    let idx = ((v.len() - 1) as f64 * pct).round() as usize;
    let idx = idx.min(v.len() - 1);
    // Use select_nth_unstable for O(n) partial sort instead of full O(n log n) sort.
    v.select_nth_unstable_by(idx, |a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    v[idx]
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
            lost_packets: 0,
            late_packets: 0,
            duplicate_packets: 0,
            last_arrival: None,
            avg_iat: 0.0,
            jitter_smoothed: 0.0,
            jitter_samples: VecDeque::with_capacity(128),
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

            // Update target latency: Start Latency + multiplier * p95(Jitter)
            let jitter_est = if self.jitter_samples.len() >= 5 {
                percentile(&self.jitter_samples, 0.95)
            } else {
                self.jitter_smoothed
            };
            let jitter_ms = jitter_est * 1000.0;
            let additional_latency =
                Duration::from_millis((self.jitter_latency_multiplier * jitter_ms) as u64);

            self.latency = (self.start_latency + additional_latency).min(self.max_latency);
        }
        self.last_arrival = Some(now);

        if seq_id < self.next_seq {
            // Late packet, drop
            self.late_packets += 1;
            return;
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

    pub fn tick(&mut self, now: Instant) -> Vec<Bytes> {
        let mut released = Vec::new();
        let skip_after = self.skip_after.unwrap_or(self.latency);
        let release_after = self
            .skip_after
            .map(|v| v.min(self.latency))
            .unwrap_or(self.latency);

        // While loop to process available packets or skip gaps
        loop {
            // Case 1: We have the next packet
            let idx = self.buffer_index(self.next_seq);
            if let Some(packet) = &self.buffer[idx] {
                if packet.seq_id == self.next_seq {
                    // Check if it has satisfied the latency requirement
                    if now.duration_since(packet.arrival_time) >= release_after {
                        let p = self.buffer[idx].take().unwrap();
                        self.buffered = self.buffered.saturating_sub(1);
                        released.push(p.payload);
                        self.next_seq += 1;
                        continue;
                    }
                    // Not ready yet
                    break;
                }
            }

            // Case 2: We have a gap (missing next_seq)
            if let Some((first_seq, first_arrival)) = self.find_next_available() {
                if now.duration_since(first_arrival) >= skip_after {
                    let skipped = first_seq.saturating_sub(self.next_seq);
                    self.lost_packets += skipped;
                    self.advance_window(first_seq);
                    continue;
                }
            }

            // No packets or waiting for gap to fill
            break;
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
            if let Some(packet) = &self.buffer[idx] {
                if packet.seq_id == seq {
                    self.buffer[idx] = None;
                    self.buffered = self.buffered.saturating_sub(1);
                }
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
        assert_eq!(out[0], p1);
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
        assert_eq!(out[0], Bytes::from_static(b"P0"));
        assert_eq!(out[1], Bytes::from_static(b"P1"));
        assert_eq!(out[2], Bytes::from_static(b"P2"));
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
        assert_eq!(out[0], Bytes::from_static(b"P1"));
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
            jitter_latency_multiplier: 4.0,
            max_latency_ms: 500,
        };
        let mut buf = ReassemblyBuffer::with_config(0, config);
        let start = Instant::now();

        // Missing seq 0, seq 1 arrives
        buf.push(1, Bytes::from_static(b"P1"), start);

        // At 30ms, aggressive skip should release P1
        let out = buf.tick(start + Duration::from_millis(30));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0], Bytes::from_static(b"P1"));
    }

    #[test]
    fn test_far_ahead_packet_advances_window() {
        let config = ReassemblyConfig {
            start_latency: Duration::from_millis(10),
            buffer_capacity: 8,
            skip_after: None,
            jitter_latency_multiplier: 4.0,
            max_latency_ms: 500,
        };
        let mut buf = ReassemblyBuffer::with_config(0, config);
        let start = Instant::now();

        // Push far ahead packet to force window advance
        buf.push(20, Bytes::from_static(b"P20"), start);
        let out = buf.tick(start + Duration::from_millis(10));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0], Bytes::from_static(b"P20"));
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
            buffer_capacity: 64,
            skip_after: None,
            jitter_latency_multiplier: 100.0,
            max_latency_ms: 200,
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
            skip_after: None,
            jitter_latency_multiplier: 4.0,
            max_latency_ms: 500,
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
}
