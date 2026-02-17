//! # Transport Statistics
//!
//! Per-link and aggregate statistics for the Strata transport layer.
//! All stats are designed for Prometheus export and JSON serialization.

use serde::Serialize;
use quanta::Instant;

// ─── Sender Stats ───────────────────────────────────────────────────────────

/// Aggregate sender-side statistics.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SenderStats {
    /// Total packets sent (including retransmissions).
    pub packets_sent: u64,
    /// Total original bytes sent (payload only).
    pub bytes_sent: u64,
    /// Packets acknowledged by receiver.
    pub packets_acked: u64,
    /// Retransmissions triggered by NACKs.
    pub retransmissions: u64,
    /// Packets that expired from the send buffer without ACK.
    pub packets_expired: u64,
    /// FEC repair packets sent.
    pub fec_repairs_sent: u64,
    /// Last measured RTT in µs.
    pub last_rtt_us: u64,
}

impl SenderStats {
    pub fn new() -> Self {
        Self::default()
    }

    /// Estimated loss rate (unacked / sent).
    pub fn loss_rate(&self) -> f64 {
        if self.packets_sent == 0 {
            0.0
        } else {
            let unacked = self.packets_sent.saturating_sub(self.packets_acked);
            unacked as f64 / self.packets_sent as f64
        }
    }

    /// Retransmission overhead ratio.
    pub fn retransmit_ratio(&self) -> f64 {
        if self.packets_sent == 0 {
            0.0
        } else {
            self.retransmissions as f64 / self.packets_sent as f64
        }
    }
}

// ─── Receiver Stats ─────────────────────────────────────────────────────────

/// Aggregate receiver-side statistics.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ReceiverStats {
    /// Total packets received (including duplicates and late).
    pub packets_received: u64,
    /// Total original bytes received (payload only).
    pub bytes_received: u64,
    /// Packets delivered to the application (unique + in-order).
    pub packets_delivered: u64,
    /// Duplicate packets received (same seq_no).
    pub duplicates: u64,
    /// Packets received after playout deadline.
    pub late_packets: u64,
    /// Packets recovered via FEC.
    pub fec_recoveries: u64,
    /// NACKs sent.
    pub nacks_sent: u64,
    /// Highest contiguous sequence delivered.
    pub highest_delivered_seq: u64,
    /// Current jitter buffer depth in packets.
    pub jitter_buffer_depth: u32,
}

impl ReceiverStats {
    pub fn new() -> Self {
        Self::default()
    }

    /// Effective goodput rate: unique bytes delivered vs total received.
    pub fn goodput_ratio(&self) -> f64 {
        if self.packets_received == 0 {
            0.0
        } else {
            self.packets_delivered as f64 / self.packets_received as f64
        }
    }
}

// ─── Per-Link Stats ─────────────────────────────────────────────────────────

/// Per-link statistics snapshot.
#[derive(Debug, Clone, Serialize)]
pub struct LinkStats {
    /// Link identifier.
    pub link_id: u8,
    /// Smoothed RTT in µs.
    pub srtt_us: f64,
    /// RTT variance in µs.
    pub rttvar_us: f64,
    /// Minimum RTT observed in µs.
    pub min_rtt_us: f64,
    /// Estimated capacity in bytes/sec.
    pub capacity_bps: f64,
    /// Current pacing rate in bytes/sec.
    pub pacing_rate_bps: f64,
    /// Congestion window in bytes.
    pub cwnd: f64,
    /// Observed loss rate (0.0-1.0).
    pub loss_rate: f64,
    /// Packets sent on this link.
    pub packets_sent: u64,
    /// Packets received on this link.
    pub packets_received: u64,
    /// Whether the link is currently active.
    pub active: bool,
    /// Congestion control state name.
    pub cc_state: String,
}

// ─── Rate Counter ───────────────────────────────────────────────────────────

/// Windowed rate counter for computing bytes/sec or packets/sec.
pub struct RateCounter {
    /// Recent samples: (timestamp, value).
    samples: Vec<(Instant, u64)>,
    /// Window duration.
    window: std::time::Duration,
}

impl RateCounter {
    pub fn new(window: std::time::Duration) -> Self {
        RateCounter {
            samples: Vec::with_capacity(128),
            window,
        }
    }

    /// Record a sample.
    pub fn record(&mut self, value: u64) {
        let now = Instant::now();
        self.samples.push((now, value));
        self.cleanup();
    }

    /// Get the rate: sum of values in window / window duration (per second).
    pub fn rate(&self) -> f64 {
        let now = Instant::now();
        let cutoff = now - self.window;
        let sum: u64 = self
            .samples
            .iter()
            .filter(|(t, _)| *t >= cutoff)
            .map(|(_, v)| v)
            .sum();
        sum as f64 / self.window.as_secs_f64()
    }

    /// Total count within the window.
    pub fn count_in_window(&self) -> u64 {
        let cutoff = Instant::now() - self.window;
        self.samples
            .iter()
            .filter(|(t, _)| *t >= cutoff)
            .map(|(_, v)| v)
            .sum()
    }

    fn cleanup(&mut self) {
        let cutoff = Instant::now() - self.window;
        self.samples.retain(|(t, _)| *t >= cutoff);
    }
}

// ─── EWMA ───────────────────────────────────────────────────────────────────

/// Exponentially weighted moving average.
#[derive(Debug, Clone)]
pub struct Ewma {
    /// Smoothing factor (0.0 - 1.0). Higher = more responsive.
    alpha: f64,
    /// Current smoothed value.
    value: f64,
    /// Whether the first sample has been applied.
    initialized: bool,
}

impl Ewma {
    /// Create a new EWMA with the given smoothing factor.
    pub fn new(alpha: f64) -> Self {
        assert!((0.0..=1.0).contains(&alpha), "alpha must be in [0, 1]");
        Ewma {
            alpha,
            value: 0.0,
            initialized: false,
        }
    }

    /// Update with a new sample and return the smoothed value.
    pub fn update(&mut self, sample: f64) -> f64 {
        if !self.initialized {
            self.value = sample;
            self.initialized = true;
        } else {
            self.value = self.alpha * sample + (1.0 - self.alpha) * self.value;
        }
        self.value
    }

    /// Get the current smoothed value.
    pub fn value(&self) -> f64 {
        self.value
    }

    /// Reset to uninitialized state.
    pub fn reset(&mut self) {
        self.value = 0.0;
        self.initialized = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    // ─── SenderStats Tests ──────────────────────────────────────────────

    #[test]
    fn sender_loss_rate_zero_when_all_acked() {
        let mut stats = SenderStats::new();
        stats.packets_sent = 100;
        stats.packets_acked = 100;
        assert_eq!(stats.loss_rate(), 0.0);
    }

    #[test]
    fn sender_loss_rate_correct() {
        let mut stats = SenderStats::new();
        stats.packets_sent = 100;
        stats.packets_acked = 90;
        assert!((stats.loss_rate() - 0.10).abs() < 0.001);
    }

    #[test]
    fn sender_loss_rate_zero_div() {
        let stats = SenderStats::new();
        assert_eq!(stats.loss_rate(), 0.0);
    }

    #[test]
    fn sender_retransmit_ratio() {
        let mut stats = SenderStats::new();
        stats.packets_sent = 100;
        stats.retransmissions = 5;
        assert!((stats.retransmit_ratio() - 0.05).abs() < 0.001);
    }

    // ─── ReceiverStats Tests ────────────────────────────────────────────

    #[test]
    fn receiver_goodput_ratio() {
        let mut stats = ReceiverStats::new();
        stats.packets_received = 110;
        stats.packets_delivered = 100;
        assert!((stats.goodput_ratio() - 100.0 / 110.0).abs() < 0.001);
    }

    #[test]
    fn receiver_goodput_zero_div() {
        let stats = ReceiverStats::new();
        assert_eq!(stats.goodput_ratio(), 0.0);
    }

    // ─── EWMA Tests ────────────────────────────────────────────────────

    #[test]
    fn ewma_first_sample_sets_value() {
        let mut ewma = Ewma::new(0.125);
        ewma.update(100.0);
        assert_eq!(ewma.value(), 100.0);
    }

    #[test]
    fn ewma_smooths_toward_new_value() {
        let mut ewma = Ewma::new(0.5);
        ewma.update(100.0);
        let v = ewma.update(200.0);
        assert!(
            (v - 150.0).abs() < 0.001,
            "EWMA 0.5 should average: got {v}"
        );
    }

    #[test]
    fn ewma_high_alpha_is_responsive() {
        let mut fast = Ewma::new(0.9);
        let mut slow = Ewma::new(0.1);

        fast.update(100.0);
        slow.update(100.0);

        fast.update(200.0);
        slow.update(200.0);

        // Fast EWMA should be closer to 200
        assert!(fast.value() > slow.value());
    }

    #[test]
    fn ewma_reset() {
        let mut ewma = Ewma::new(0.5);
        ewma.update(100.0);
        ewma.reset();
        assert_eq!(ewma.value(), 0.0);
        ewma.update(50.0);
        assert_eq!(ewma.value(), 50.0);
    }

    // ─── RateCounter Tests ──────────────────────────────────────────────

    #[test]
    fn rate_counter_basic() {
        let mut counter = RateCounter::new(Duration::from_secs(1));
        counter.record(1000);
        counter.record(2000);
        let rate = counter.rate();
        assert!(rate > 0.0, "rate should be positive: {rate}");
    }

    #[test]
    fn rate_counter_count_in_window() {
        let mut counter = RateCounter::new(Duration::from_secs(10));
        counter.record(100);
        counter.record(200);
        counter.record(300);
        assert_eq!(counter.count_in_window(), 600);
    }

    // ─── LinkStats Tests ────────────────────────────────────────────────

    #[test]
    fn link_stats_serialization() {
        let stats = LinkStats {
            link_id: 1,
            srtt_us: 50_000.0,
            rttvar_us: 5_000.0,
            min_rtt_us: 40_000.0,
            capacity_bps: 5_000_000.0,
            pacing_rate_bps: 4_500_000.0,
            cwnd: 100_000.0,
            loss_rate: 0.02,
            packets_sent: 10_000,
            packets_received: 9_800,
            active: true,
            cc_state: "Normal".to_string(),
        };

        let json = serde_json::to_string(&stats).unwrap();
        assert!(json.contains("\"link_id\":1"));
        assert!(json.contains("\"active\":true"));
    }
}
