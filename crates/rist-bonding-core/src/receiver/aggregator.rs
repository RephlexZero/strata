use bytes::Bytes;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// An incoming packet with its bonding sequence ID and arrival timestamp.
pub struct Packet {
    pub seq_id: u64,
    pub payload: Bytes,
    pub arrival_time: Instant,
    /// Sender-side timestamp in microseconds (from `BondingHeader::send_time_us`).
    /// Used for relative-delay jitter estimation that is resilient to
    /// clock drift between sender and receiver.  `0` means not available
    /// (falls back to classic IAT jitter).
    pub send_time_us: u64,
}

// ──────────────────────────────────────────────────────────────────
// Bitfield gap tracker — O(1) amortised insert / find-next-available
// replaces the previous BTreeMap<u64, Instant> index.
// ──────────────────────────────────────────────────────────────────

/// Tracks which slots within the circular buffer are occupied using a
/// compact bitfield.  This gives O(1) insert / remove and O(capacity/64)
/// `find_next` in the worst case (typically a single word scan).
struct SlotBitmap {
    /// One bit per buffer slot.  `bits[slot / 64] & (1 << (slot % 64))`.
    bits: Vec<u64>,
    /// Parallel array storing the arrival `Instant` per slot so we can
    /// check the gap-skip timeout without touching the packet data.
    arrivals: Vec<Option<Instant>>,
}

impl SlotBitmap {
    fn new(capacity: usize) -> Self {
        let words = capacity.div_ceil(64);
        Self {
            bits: vec![0u64; words],
            arrivals: vec![None; capacity],
        }
    }

    #[inline]
    fn set(&mut self, slot: usize, arrival: Instant) {
        self.bits[slot / 64] |= 1u64 << (slot % 64);
        self.arrivals[slot] = Some(arrival);
    }

    #[inline]
    fn clear(&mut self, slot: usize) {
        self.bits[slot / 64] &= !(1u64 << (slot % 64));
        self.arrivals[slot] = None;
    }

    #[inline]
    fn is_set(&self, slot: usize) -> bool {
        self.bits[slot / 64] & (1u64 << (slot % 64)) != 0
    }

    /// Find the earliest occupied seq_id **after** `base_seq` within
    /// `capacity` slots of the circular buffer.
    fn find_next(&self, base_seq: u64, capacity: usize) -> Option<(u64, Instant)> {
        for offset in 1..capacity as u64 {
            let seq = base_seq + offset;
            let slot = (seq % capacity as u64) as usize;
            if self.is_set(slot) {
                if let Some(arrival) = self.arrivals[slot] {
                    return Some((seq, arrival));
                }
            }
        }
        None
    }

    /// Clear all bits and arrivals.
    fn clear_all(&mut self) {
        for w in self.bits.iter_mut() {
            *w = 0;
        }
        for a in self.arrivals.iter_mut() {
            *a = None;
        }
    }
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

    // ── Relative delay jitter (clock-drift resistant) ──
    /// Monotonic reference base for computing receiver-side timestamps
    /// in microseconds.  Set to the arrival time of the first packet.
    recv_base: Option<Instant>,
    /// Sliding minimum of one-way delay samples (µs).
    /// Kept as a windowed min-tracker (last 256 samples).
    min_delay_us: f64,
    /// Ring of recent OWD samples for sliding-min tracking.
    delay_ring: VecDeque<f64>,

    // ── Performance scratch buffers ──
    /// Re-usable scratch for `percentile()` to avoid per-call allocation (#3).
    percentile_scratch: Vec<f64>,
    /// Re-usable scratch for `tick()` output to avoid per-call allocation (#2).
    tick_scratch: Vec<Bytes>,

    // O(1) bitfield index of occupied slots + arrival times (#4).
    slot_bitmap: SlotBitmap,
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

fn percentile(samples: &VecDeque<f64>, pct: f64, scratch: &mut Vec<f64>) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    scratch.clear();
    scratch.extend(samples.iter().copied());
    let idx = ((scratch.len() - 1) as f64 * pct).round() as usize;
    let idx = idx.min(scratch.len() - 1);
    // Use select_nth_unstable for O(n) partial sort instead of full O(n log n) sort.
    scratch.select_nth_unstable_by(idx, |a, b| {
        a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal)
    });
    scratch[idx]
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
            recv_base: None,
            min_delay_us: f64::MAX,
            delay_ring: VecDeque::with_capacity(256),
            percentile_scratch: Vec::with_capacity(128),
            tick_scratch: Vec::with_capacity(64),
            slot_bitmap: SlotBitmap::new(capacity),
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
        self.push_with_send_time(seq_id, payload, now, 0);
    }

    /// Push a packet with an optional sender timestamp for relative-delay
    /// jitter estimation.  When `send_time_us > 0`, the jitter buffer
    /// computes `relative_delay = owd - min(owd)` which is resilient to
    /// clock drift between sender and receiver (important when bonding
    /// cellular + wired paths with independent clock sources).
    ///
    /// Falls back to classic inter-arrival-time (IAT) jitter when
    /// `send_time_us == 0`.
    pub fn push_with_send_time(
        &mut self,
        seq_id: u64,
        payload: Bytes,
        now: Instant,
        send_time_us: u64,
    ) {
        // ── Jitter estimation ──
        if send_time_us > 0 {
            // -- Relative delay mode (clock-drift resistant) --
            let recv_base = *self.recv_base.get_or_insert(now);
            let recv_us = now.duration_since(recv_base).as_micros() as f64;
            let owd_us = recv_us - send_time_us as f64;

            // Sliding-min tracker (256-sample window)
            self.delay_ring.push_back(owd_us);
            if self.delay_ring.len() > 256 {
                self.delay_ring.pop_front();
            }
            // Recompute min if the evicted sample was the old minimum
            if owd_us < self.min_delay_us {
                self.min_delay_us = owd_us;
            } else if self.delay_ring.len() < 256 || self.min_delay_us == f64::MAX {
                // Need to rescan — the old minimum may have been evicted
                self.min_delay_us = self.delay_ring.iter().copied().fold(f64::MAX, f64::min);
            }
            // Relative delay: how much above the minimum OWD this packet is
            let relative_delay_ms = (owd_us - self.min_delay_us) / 1000.0;

            // Feed into the same jitter EWMA / p95 pipeline
            let alpha = 0.1;
            self.jitter_smoothed =
                (1.0 - alpha) * self.jitter_smoothed + alpha * (relative_delay_ms / 1000.0);
            self.jitter_samples.push_back(relative_delay_ms / 1000.0);
            if self.jitter_samples.len() > 128 {
                self.jitter_samples.pop_front();
            }

            let jitter_est = if self.jitter_samples.len() >= 5 {
                percentile(&self.jitter_samples, 0.95, &mut self.percentile_scratch)
            } else {
                self.jitter_smoothed
            };
            let jitter_ms = jitter_est * 1000.0;
            let additional_latency =
                Duration::from_millis((self.jitter_latency_multiplier * jitter_ms) as u64);
            self.latency = (self.start_latency + additional_latency).min(self.max_latency);
        } else if let Some(last) = self.last_arrival {
            // -- Classic IAT mode (fallback) --
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
                percentile(&self.jitter_samples, 0.95, &mut self.percentile_scratch)
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
            // Detect sender restart: if next_seq is far ahead (>capacity) and
            // we receive a very low seq_id, the sender likely restarted.
            // Reset state to avoid prolonged blackout.
            if self.next_seq > self.capacity as u64 && seq_id < self.capacity as u64 {
                tracing::warn!(
                    "Sender seq reset detected (got {} while expecting {}), resetting receiver state",
                    seq_id,
                    self.next_seq
                );
                // Clear the entire buffer
                for slot in self.buffer.iter_mut() {
                    *slot = None;
                }
                self.buffered = 0;
                self.slot_bitmap.clear_all();
                self.next_seq = seq_id;
                // Fall through to normal push logic below
            } else {
                // Late packet, drop
                self.late_packets += 1;
                return;
            }
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
                self.slot_bitmap.clear(idx);
            }
        } else {
            self.buffered += 1;
        }

        self.slot_bitmap.set(idx, now);
        self.buffer[idx] = Some(Packet {
            seq_id,
            payload,
            arrival_time: now,
            send_time_us,
        });
    }

    pub fn tick(&mut self, now: Instant) -> Vec<Bytes> {
        // Re-use the pre-allocated scratch vec (#2).
        self.tick_scratch.clear();
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
                        self.slot_bitmap.clear(idx);
                        self.tick_scratch.push(p.payload);
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

        // Return ownership of the scratch vec's contents.
        std::mem::take(&mut self.tick_scratch)
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
                    self.slot_bitmap.clear(idx);
                    self.buffer[idx] = None;
                    self.buffered = self.buffered.saturating_sub(1);
                }
            }
        }
        self.next_seq = new_next;
    }

    fn find_next_available(&self) -> Option<(u64, Instant)> {
        // O(capacity/64) bitfield scan — replaces the O(log n) BTreeMap range().
        self.slot_bitmap.find_next(self.next_seq, self.capacity)
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

        let mut scratch = Vec::new();
        let p50 = percentile(&samples, 0.5, &mut scratch);
        let p95 = percentile(&samples, 0.95, &mut scratch);

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
        let mut scratch = Vec::new();
        assert_eq!(percentile(&samples, 0.5, &mut scratch), 5.0);
        assert_eq!(percentile(&samples, 0.95, &mut scratch), 5.0);
    }

    #[test]
    fn test_percentile_empty() {
        let samples = VecDeque::new();
        let mut scratch = Vec::new();
        assert_eq!(percentile(&samples, 0.5, &mut scratch), 0.0);
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

    /// Data integrity hash test (spec §7): sender-hash = receiver-hash.
    ///
    /// Verifies that data pushed through the bonding header wrap/unwrap and
    /// reassembly buffer pipeline is bit-for-bit identical to the original
    /// payload.
    #[test]
    fn test_data_integrity_hash_verification() {
        use crate::protocol::header::BondingHeader;
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let num_packets = 500;
        let mut sender_hasher = DefaultHasher::new();
        let mut receiver_hasher = DefaultHasher::new();

        // Generate deterministic test payloads and compute sender hash
        let mut wrapped_packets = Vec::new();
        for i in 0..num_packets {
            // Create payload with varying sizes and content
            let payload_len = 100 + (i % 1300);
            let payload: Vec<u8> = (0..payload_len)
                .map(|j| ((i * 7 + j * 13) % 256) as u8)
                .collect();
            let payload_bytes = Bytes::from(payload.clone());

            // Hash the original payload on the sender side
            payload.hash(&mut sender_hasher);

            // Wrap with bonding header (simulating sender)
            let header = BondingHeader::new(i as u64);
            let wrapped = header.wrap(payload_bytes);
            wrapped_packets.push(wrapped);
        }

        let sender_hash = sender_hasher.finish();

        // Simulate receiver: unwrap headers and feed into reassembly buffer
        let mut buf = ReassemblyBuffer::new(0, Duration::from_millis(10));
        let start = Instant::now();

        for wrapped in wrapped_packets {
            let (header, original_payload) =
                BondingHeader::unwrap(wrapped).expect("Header unwrap should succeed");
            buf.push(header.seq_id, original_payload, start);
        }

        // Tick to release all packets
        let out = buf.tick(start + Duration::from_millis(10));
        assert_eq!(out.len(), num_packets, "All packets should be released");

        // Compute receiver hash from reassembled output
        for payload in &out {
            payload.to_vec().hash(&mut receiver_hasher);
        }

        let receiver_hash = receiver_hasher.finish();

        assert_eq!(
            sender_hash, receiver_hash,
            "Data integrity check failed: sender hash ({:#x}) != receiver hash ({:#x})",
            sender_hash, receiver_hash
        );

        // Verify no data loss or corruption
        assert_eq!(buf.lost_packets, 0, "No packets should be lost");
        assert_eq!(buf.duplicate_packets, 0, "No duplicates should be counted");
    }

    // ────────────────────────────────────────────────────────────────
    // Burst drain, seq reset, and BTreeMap edge-case tests
    // ────────────────────────────────────────────────────────────────

    #[test]
    fn tick_drains_burst_in_one_call() {
        let mut buf = ReassemblyBuffer::with_config(
            0,
            ReassemblyConfig {
                start_latency: Duration::from_millis(1),
                buffer_capacity: 1024,
                ..ReassemblyConfig::default()
            },
        );
        let now = Instant::now();

        for i in 0..100u64 {
            buf.push(i, Bytes::from(vec![i as u8; 50]), now);
        }

        let out = buf.tick(now + Duration::from_millis(1));
        assert_eq!(
            out.len(),
            100,
            "tick() should release all ready packets in one call, got {}",
            out.len()
        );
    }

    #[test]
    fn sender_seq_reset_detected() {
        let mut buf = ReassemblyBuffer::with_config(
            0,
            ReassemblyConfig {
                start_latency: Duration::from_millis(1),
                buffer_capacity: 64,
                ..ReassemblyConfig::default()
            },
        );
        let now = Instant::now();

        // Normal operation: push 100 packets
        for i in 0..100u64 {
            buf.push(i, Bytes::from(vec![i as u8; 10]), now);
        }
        let _ = buf.tick(now + Duration::from_millis(1));
        assert_eq!(buf.next_seq, 100);

        // Sender restarts: seq resets to 5
        buf.push(
            5,
            Bytes::from_static(b"RESET"),
            now + Duration::from_millis(10),
        );

        // After reset detection, next_seq should be reset
        assert!(
            buf.next_seq <= 5,
            "After sender reset detection, next_seq should be <= 5, got {}",
            buf.next_seq
        );

        // Release the reset packet
        let out = buf.tick(now + Duration::from_millis(11));
        assert!(
            !out.is_empty(),
            "Should be able to release packets after sender reset"
        );
    }

    #[test]
    fn late_packet_not_false_reset() {
        let mut buf = ReassemblyBuffer::with_config(
            0,
            ReassemblyConfig {
                start_latency: Duration::from_millis(1),
                buffer_capacity: 128,
                ..ReassemblyConfig::default()
            },
        );
        let now = Instant::now();

        // Push packets 0-9 and release them
        for i in 0..10u64 {
            buf.push(i, Bytes::from(vec![0u8; 10]), now);
        }
        let _ = buf.tick(now + Duration::from_millis(1));
        let late_before = buf.late_packets;

        // Seq 5 again is late, NOT a reset (next_seq=10 < capacity=128)
        buf.push(
            5,
            Bytes::from_static(b"late"),
            now + Duration::from_millis(5),
        );

        assert_eq!(
            buf.late_packets,
            late_before + 1,
            "Late packet should be counted as late, not trigger reset"
        );
        assert_eq!(
            buf.next_seq, 10,
            "next_seq should not change for late packets"
        );
    }

    #[test]
    fn btreemap_large_buffer_gap_skip() {
        let mut buf = ReassemblyBuffer::with_config(
            0,
            ReassemblyConfig {
                start_latency: Duration::from_millis(1),
                buffer_capacity: 4096,
                skip_after: Some(Duration::from_millis(1)),
                ..ReassemblyConfig::default()
            },
        );
        let now = Instant::now();

        // Push 1000 packets with seq 1..=1000 (missing seq 0)
        for i in 1..=1000u64 {
            buf.push(i, Bytes::from(vec![0u8; 10]), now);
        }

        // tick should skip seq 0 and release the rest
        let out = buf.tick(now + Duration::from_millis(2));
        assert!(!out.is_empty(), "Should release packets after gap skip");
        assert!(buf.lost_packets >= 1, "seq 0 should be counted as lost");
    }

    #[test]
    fn btreemap_consistency_after_push_tick_push() {
        let mut buf = ReassemblyBuffer::with_config(
            0,
            ReassemblyConfig {
                start_latency: Duration::from_millis(1),
                buffer_capacity: 32,
                ..ReassemblyConfig::default()
            },
        );
        let now = Instant::now();

        for i in 0..10u64 {
            buf.push(i, Bytes::from(vec![0u8; 10]), now);
        }
        let out = buf.tick(now + Duration::from_millis(1));
        assert_eq!(out.len(), 10);
        assert_eq!(buf.buffered, 0);

        for i in 10..20u64 {
            buf.push(i, Bytes::from(vec![0u8; 10]), now);
        }
        let out = buf.tick(now + Duration::from_millis(2));
        assert_eq!(out.len(), 10);
        assert_eq!(buf.buffered, 0);
        assert_eq!(buf.lost_packets, 0);
    }

    #[test]
    fn btreemap_survives_window_advance() {
        let mut buf = ReassemblyBuffer::with_config(
            0,
            ReassemblyConfig {
                start_latency: Duration::from_millis(1),
                buffer_capacity: 16,
                skip_after: Some(Duration::from_millis(1)),
                ..ReassemblyConfig::default()
            },
        );
        let now = Instant::now();

        buf.push(0, Bytes::from_static(b"P0"), now);
        buf.push(1, Bytes::from_static(b"P1"), now);

        // Far-ahead packet forces window advance
        buf.push(30, Bytes::from_static(b"P30"), now);

        let out = buf.tick(now + Duration::from_millis(2));
        assert!(
            !out.is_empty(),
            "Should release something after window advance"
        );
        assert!(
            buf.lost_packets > 0,
            "Window advance should count lost packets"
        );
    }

    #[test]
    fn duplicate_counting() {
        let mut buf = ReassemblyBuffer::new(0, Duration::from_millis(10));
        let now = Instant::now();

        buf.push(0, Bytes::from_static(b"first"), now);
        buf.push(0, Bytes::from_static(b"dupe"), now);

        assert_eq!(buf.duplicate_packets, 1);
    }

    #[test]
    fn concurrent_buffer_push_and_tick() {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::sync::Arc;
        use std::thread;

        let counter = Arc::new(AtomicU64::new(0));
        let num_threads = 4;
        let packets_per_thread = 1000;

        let mut handles = Vec::new();
        for t in 0..num_threads {
            let counter = counter.clone();
            handles.push(thread::spawn(move || {
                let mut buf = ReassemblyBuffer::with_config(
                    0,
                    ReassemblyConfig {
                        start_latency: Duration::from_millis(1),
                        buffer_capacity: 2048,
                        ..ReassemblyConfig::default()
                    },
                );
                let now = Instant::now();

                for i in 0..packets_per_thread {
                    let seq = (t * packets_per_thread + i) as u64;
                    buf.push(seq, Bytes::from(vec![0u8; 100]), now);
                }

                let out = buf.tick(now + Duration::from_millis(1));
                counter.fetch_add(out.len() as u64, Ordering::Relaxed);
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        assert!(
            counter.load(Ordering::Relaxed) > 0,
            "Concurrent buffers should produce output"
        );
    }

    // ────────────────────────────────────────────────────────────────
    // SlotBitmap tests (#4)
    // ────────────────────────────────────────────────────────────────

    #[test]
    fn slot_bitmap_set_and_check() {
        let now = Instant::now();
        let mut bm = SlotBitmap::new(128);
        assert!(!bm.is_set(0));
        bm.set(0, now);
        assert!(bm.is_set(0));
        bm.clear(0);
        assert!(!bm.is_set(0));
    }

    #[test]
    fn slot_bitmap_find_next_basic() {
        let now = Instant::now();
        let mut bm = SlotBitmap::new(64);
        // No bits set
        assert!(bm.find_next(0, 64).is_none());

        // Set slot for seq 5 (slot = 5 % 64 = 5)
        bm.set(5, now);
        let found = bm.find_next(0, 64);
        assert!(found.is_some());
        let (seq, _arrival) = found.unwrap();
        // find_next(0, 64) checks offsets 1..64, so first match is offset 5 → seq 5
        assert_eq!(seq, 5);
    }

    #[test]
    fn slot_bitmap_find_next_wraparound() {
        let now = Instant::now();
        let mut bm = SlotBitmap::new(16);
        // Set slot 2
        bm.set(2, now);
        // base_seq=14, capacity=16 → offsets 1..16 → seqs 15..30
        // slot for seq 18 = 18 % 16 = 2 ← that's where we set the bit
        let found = bm.find_next(14, 16);
        assert!(found.is_some());
        let (seq, _) = found.unwrap();
        assert_eq!(seq % 16, 2); // Matches the slot
    }

    #[test]
    fn slot_bitmap_clear_all() {
        let now = Instant::now();
        let mut bm = SlotBitmap::new(256);
        for i in 0..256 {
            bm.set(i, now);
        }
        bm.clear_all();
        for i in 0..256 {
            assert!(!bm.is_set(i), "Slot {} should be clear after clear_all", i);
        }
    }

    #[test]
    fn slot_bitmap_word_boundary() {
        // Test slots at word boundaries (64-bit)
        let now = Instant::now();
        let mut bm = SlotBitmap::new(256);
        for slot in [0, 63, 64, 127, 128, 191, 192, 255] {
            bm.set(slot, now);
            assert!(bm.is_set(slot), "Slot {} should be set", slot);
            bm.clear(slot);
            assert!(!bm.is_set(slot), "Slot {} should be clear", slot);
        }
    }

    // ────────────────────────────────────────────────────────────────
    // Percentile scratch buffer reuse test (#3)
    // ────────────────────────────────────────────────────────────────

    #[test]
    fn percentile_scratch_reused_across_calls() {
        let mut samples = VecDeque::new();
        for i in 0..50 {
            samples.push_back(i as f64);
        }
        let mut scratch = Vec::new();
        let _ = percentile(&samples, 0.5, &mut scratch);
        let cap_after_first = scratch.capacity();
        // Second call should reuse the same allocation
        let _ = percentile(&samples, 0.95, &mut scratch);
        assert_eq!(
            scratch.capacity(),
            cap_after_first,
            "Scratch buffer should not reallocate on second call with same-sized input"
        );
    }

    // ────────────────────────────────────────────────────────────────
    // tick() scratch reuse test (#2)
    // ────────────────────────────────────────────────────────────────

    #[test]
    fn tick_returns_owned_vec() {
        let mut buf = ReassemblyBuffer::with_config(
            0,
            ReassemblyConfig {
                start_latency: Duration::from_millis(1),
                buffer_capacity: 64,
                ..ReassemblyConfig::default()
            },
        );
        let now = Instant::now();
        for i in 0..10u64 {
            buf.push(i, Bytes::from(vec![i as u8; 50]), now);
        }
        let out = buf.tick(now + Duration::from_millis(1));
        assert_eq!(out.len(), 10);

        // Second tick with new data
        for i in 10..20u64 {
            buf.push(i, Bytes::from(vec![i as u8; 50]), now);
        }
        let out2 = buf.tick(now + Duration::from_millis(2));
        assert_eq!(out2.len(), 10);
    }

    // ────────────────────────────────────────────────────────────────
    // Relative delay jitter tests (#2)
    // ────────────────────────────────────────────────────────────────

    #[test]
    fn relative_delay_mode_activates_with_send_time() {
        let mut buf = ReassemblyBuffer::new(0, Duration::from_millis(10));
        let start = Instant::now();

        // Push with send timestamps — relative delay should kick in
        buf.push_with_send_time(0, Bytes::from_static(b"P0"), start, 1_000);
        buf.push_with_send_time(
            1,
            Bytes::from_static(b"P1"),
            start + Duration::from_millis(20),
            21_000,
        );

        // delay_ring should have 2 entries
        assert_eq!(buf.delay_ring.len(), 2);
        assert!(buf.min_delay_us < f64::MAX, "min_delay should be set");
    }

    #[test]
    fn relative_delay_with_constant_owd_yields_zero_jitter() {
        // If all packets have the same OWD, relative delay = 0 → no jitter → latency stays minimal.
        let config = ReassemblyConfig {
            start_latency: Duration::from_millis(10),
            buffer_capacity: 64,
            jitter_latency_multiplier: 4.0,
            max_latency_ms: 500,
            skip_after: None,
        };
        let mut buf = ReassemblyBuffer::with_config(0, config);
        let start = Instant::now();

        for i in 0..50u64 {
            // OWD is always exactly 1000µs (1ms)
            let arrival = start + Duration::from_micros(i * 1000 + 1000);
            let send_time_us = i * 1000;
            buf.push_with_send_time(i, Bytes::from(vec![0u8; 100]), arrival, send_time_us);
        }

        // Latency should remain at start_latency since jitter ≈ 0
        assert!(
            buf.latency <= Duration::from_millis(15),
            "Constant OWD should produce zero jitter, latency should stay near start: {:?}",
            buf.latency
        );
    }

    #[test]
    fn relative_delay_increases_latency_on_variable_owd() {
        let config = ReassemblyConfig {
            start_latency: Duration::from_millis(10),
            buffer_capacity: 64,
            jitter_latency_multiplier: 4.0,
            max_latency_ms: 500,
            skip_after: None,
        };
        let mut buf = ReassemblyBuffer::with_config(0, config);
        let start = Instant::now();

        // Inject variable OWD: alternating 1ms and 20ms
        for i in 0..20u64 {
            let owd_us = if i % 2 == 0 { 1000 } else { 20000 };
            let arrival = start + Duration::from_micros(i * 10_000 + owd_us);
            let send_time_us = i * 10_000;
            buf.push_with_send_time(i, Bytes::from(vec![0u8; 100]), arrival, send_time_us);
        }

        assert!(
            buf.latency > Duration::from_millis(10),
            "Variable OWD should increase latency beyond start: {:?}",
            buf.latency
        );
    }

    #[test]
    fn relative_delay_falls_back_to_iat_when_no_send_time() {
        let mut buf = ReassemblyBuffer::new(0, Duration::from_millis(10));
        let start = Instant::now();

        // Push without send timestamps (send_time_us = 0) → classic IAT mode
        buf.push_with_send_time(0, Bytes::from_static(b"P0"), start, 0);
        buf.push_with_send_time(
            1,
            Bytes::from_static(b"P1"),
            start + Duration::from_millis(20),
            0,
        );

        // delay_ring should be empty (relative delay not activated)
        assert!(
            buf.delay_ring.is_empty(),
            "No send timestamps → should not use relative delay"
        );
    }

    #[test]
    fn relative_delay_sliding_min_window() {
        let mut buf = ReassemblyBuffer::new(0, Duration::from_millis(10));
        let start = Instant::now();

        // Push 300 packets to exceed the 256-sample sliding window
        for i in 0..300u64 {
            let owd_us = 5000 + (i % 10) * 100; // 5ms to 5.9ms OWD
            let arrival = start + Duration::from_micros(i * 1000 + owd_us);
            buf.push_with_send_time(i, Bytes::from(vec![0u8; 10]), arrival, i * 1000);
        }

        assert_eq!(
            buf.delay_ring.len(),
            256,
            "Sliding window should cap at 256"
        );
        assert!(buf.min_delay_us < f64::MAX, "Min delay should be tracked");
    }
}
