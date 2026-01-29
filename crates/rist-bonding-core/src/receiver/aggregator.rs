use bytes::Bytes;
use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

pub struct Packet {
    pub seq_id: u64,
    pub payload: Bytes,
    pub arrival_time: Instant,
}

pub struct ReassemblyBuffer {
    buffer: BTreeMap<u64, Packet>,
    next_seq: u64,
    latency: Duration,
    start_latency: Duration,
    pub lost_packets: u64,
    pub late_packets: u64,

    // Adaptive Latency Calculation
    last_arrival: Option<Instant>,
    avg_iat: f64,
    jitter_smoothed: f64,
    jitter_samples: VecDeque<f64>,
}

#[derive(Default, Clone, Debug)]
pub struct ReassemblyStats {
    pub queue_depth: usize,
    pub next_seq: u64,
    pub lost_packets: u64,
    pub late_packets: u64,
    pub current_latency_ms: u64,
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
        Self {
            buffer: BTreeMap::new(),
            next_seq: start_seq,
            latency,
            start_latency: latency,
            lost_packets: 0,
            late_packets: 0,
            last_arrival: None,
            avg_iat: 0.0,
            jitter_smoothed: 0.0,
            jitter_samples: VecDeque::with_capacity(128),
        }
    }

    pub fn get_stats(&self) -> ReassemblyStats {
        ReassemblyStats {
            queue_depth: self.buffer.len(),
            next_seq: self.next_seq,
            lost_packets: self.lost_packets,
            late_packets: self.late_packets,
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

            // Update target latency: Start Latency + 4 * p95(Jitter)
            let jitter_est = if self.jitter_samples.len() >= 5 {
                percentile(&self.jitter_samples, 0.95)
            } else {
                self.jitter_smoothed
            };
            let jitter_ms = jitter_est * 1000.0;
            let additional_latency = Duration::from_millis((4.0 * jitter_ms) as u64);

            self.latency = self.start_latency + additional_latency;
        }
        self.last_arrival = Some(now);

        if seq_id < self.next_seq {
            // Late packet, drop
            self.late_packets += 1;
            return;
        }
        self.buffer.insert(
            seq_id,
            Packet {
                seq_id,
                payload,
                arrival_time: now,
            },
        );
    }

    pub fn tick(&mut self, now: Instant) -> Vec<Bytes> {
        let mut released = Vec::new();

        // While loop to process available packets or skip gaps
        loop {
            // Case 1: We have the next packet
            if let Some(packet) = self.buffer.get(&self.next_seq) {
                // Check if it has satisfied the latency requirement
                if now.duration_since(packet.arrival_time) >= self.latency {
                    if let Some(p) = self.buffer.remove(&self.next_seq) {
                        released.push(p.payload);
                        self.next_seq += 1;
                    }
                } else {
                    // Not ready yet
                    break;
                }
            }
            // Case 2: We have a gap (missing next_seq)
            else {
                // Check if any future packet has timed out (waiting too long)
                // If the earliest future packet has exceeded latency, we skip to it.
                if let Some((&first_seq, first_packet)) = self.buffer.iter().next() {
                    if now.duration_since(first_packet.arrival_time) >= self.latency {
                        // The gap is declared lost. Skip to first_seq.
                        let skipped = first_seq.saturating_sub(self.next_seq);
                        self.lost_packets += skipped;
                        self.next_seq = first_seq;
                        // Continue loop to process this packet (it will match Case 1)
                        continue;
                    }
                }

                // No packets or waiting for gap to fill
                break;
            }
        }

        released
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
}
