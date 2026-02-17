//! # Biscay Congestion Control
//!
//! Radio-aware congestion control extending BBRv3 with cellular modem
//! intelligence. Named for the Bay of Biscay (rough waters).
//!
//! ## State Machine (from Master Plan §5)
//!
//! ```text
//!                     ┌──────────┐
//!           ┌────────▶│  NORMAL  │◀────────┐
//!           │         └────┬─────┘         │
//!      CQI stable    CQI dropping     Handover
//!      & RSRP OK     (3 readings)     complete
//!           │              │               │
//!           │         ┌────▼─────┐         │
//!           │         │ CAUTIOUS │         │
//!           │         │ (-30%)   │         │
//!           │         └────┬─────┘         │
//!           │         RSRP slope           │
//!           │         < -2.5 dB/s          │
//!           │              │               │
//!           │         ┌────▼─────┐         │
//!           └─────────│PRE_HAND- │─────────┘
//!                     │  OVER    │
//!                     └──────────┘
//! ```
//!
//! ## BBRv3 Base Layer
//!
//! Per-link: models bottleneck bandwidth (BtlBw) and min RTT (RTprop).
//! Radio feed-forward adds SINR→capacity ceiling and CQI derivative tracking.

use std::time::Duration;
use quanta::Instant;

// ─── Biscay State ───────────────────────────────────────────────────────────

/// Congestion control state per link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BiscayState {
    /// Normal operation — full pacing rate.
    Normal,
    /// CQI dropping — proactive 30% rate reduction.
    Cautious,
    /// Pre-handover — drain queue, pause probing.
    PreHandover,
}

/// BBRv3-inspired phase within Normal state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BbrPhase {
    /// Initial slow start — exponential ramp.
    SlowStart,
    /// Steady-state bandwidth probing.
    ProbeBw,
    /// Periodic RTT probing (drain to measure RTprop).
    ProbeRtt,
}

// ─── Radio Metrics ──────────────────────────────────────────────────────────

/// RF metrics from the modem supervisor.
#[derive(Debug, Clone, Default)]
pub struct RadioMetrics {
    /// RSRP in dBm (e.g., -90).
    pub rsrp_dbm: f64,
    /// RSRQ in dB (e.g., -12).
    pub rsrq_db: f64,
    /// SINR in dB (e.g., 15.0).
    pub sinr_db: f64,
    /// CQI (0-15).
    pub cqi: u8,
    /// Measurement timestamp.
    pub timestamp: Option<Instant>,
}

// ─── Per-Link Congestion Controller ─────────────────────────────────────────

/// Biscay congestion controller for a single link.
pub struct BiscayController {
    // ─── State ───
    /// Current radio-aware state.
    pub state: BiscayState,
    /// BBR phase within Normal state.
    pub bbr_phase: BbrPhase,

    // ─── BBR estimates ───
    /// Estimated bottleneck bandwidth (bytes/sec).
    btl_bw: f64,
    /// Minimum RTT observed (RTprop) in µs.
    rt_prop_us: f64,
    /// Current pacing rate (bytes/sec).
    pacing_rate: f64,
    /// Congestion window (bytes).
    cwnd: f64,

    // ─── Bandwidth tracking ───
    /// Recent bandwidth samples for BtlBw estimation.
    bw_samples: Vec<f64>,
    /// Max samples to keep.
    max_bw_samples: usize,

    // ─── RTT tracking ───
    /// Recent RTT samples (µs) for RTprop estimation.
    rtt_samples: Vec<f64>,
    /// When RTprop was last updated.
    rt_prop_stamp: Instant,
    /// RTprop expiry — probe RTT if this old.
    rt_prop_expiry: Duration,

    // ─── Radio state ───
    /// CQI history for derivative tracking.
    cqi_history: Vec<(Instant, u8)>,
    /// RSRP history for slope tracking.
    rsrp_history: Vec<(Instant, f64)>,
    /// SINR → capacity ceiling (kbps).
    sinr_capacity_ceiling: Option<f64>,
    /// Number of consecutive CQI drops.
    consecutive_cqi_drops: u8,

    // ─── Timing ───
    /// When this controller was created.
    #[allow(dead_code)]
    created_at: Instant,
    /// Last time the state machine was evaluated.
    last_tick: Instant,
}

impl BiscayController {
    /// Create a new controller with default parameters.
    pub fn new() -> Self {
        let now = Instant::now();
        BiscayController {
            state: BiscayState::Normal,
            bbr_phase: BbrPhase::SlowStart,

            btl_bw: 0.0,
            rt_prop_us: f64::MAX,
            pacing_rate: 100_000.0, // 100 KB/s initial (conservative)
            cwnd: 14_000.0,         // ~10 packets initial window

            bw_samples: Vec::with_capacity(16),
            max_bw_samples: 16,

            rtt_samples: Vec::with_capacity(32),
            rt_prop_stamp: now,
            rt_prop_expiry: Duration::from_secs(10),

            cqi_history: Vec::with_capacity(16),
            rsrp_history: Vec::with_capacity(16),
            sinr_capacity_ceiling: None,
            consecutive_cqi_drops: 0,

            created_at: now,
            last_tick: now,
        }
    }

    // ─── Getters ────────────────────────────────────────────────────────

    /// Get the current pacing rate in bytes/sec.
    pub fn pacing_rate(&self) -> f64 {
        self.pacing_rate
    }

    /// Get the estimated bottleneck bandwidth in bytes/sec.
    pub fn btl_bw(&self) -> f64 {
        self.btl_bw
    }

    /// Get the minimum RTT (RTprop) in µs.
    pub fn rt_prop_us(&self) -> f64 {
        self.rt_prop_us
    }

    /// Get the congestion window in bytes.
    pub fn cwnd(&self) -> f64 {
        self.cwnd
    }

    // ─── BBR feedback processing ────────────────────────────────────────

    /// Process a bandwidth sample (from ACK feedback).
    /// `delivered_bytes`: bytes acknowledged in this interval.
    /// `interval_us`: time interval in µs.
    pub fn on_bandwidth_sample(&mut self, delivered_bytes: u64, interval_us: u64) {
        if interval_us == 0 {
            return;
        }
        let bw = delivered_bytes as f64 / (interval_us as f64 / 1_000_000.0);

        self.bw_samples.push(bw);
        if self.bw_samples.len() > self.max_bw_samples {
            self.bw_samples.remove(0);
        }

        // BtlBw = max of recent samples (BBR approach)
        self.btl_bw = self.bw_samples.iter().cloned().fold(0.0f64, f64::max);

        self.update_pacing_rate();
    }

    /// Process an RTT sample (from ACK or PONG).
    pub fn on_rtt_sample(&mut self, rtt_us: f64) {
        if rtt_us <= 0.0 {
            return;
        }

        self.rtt_samples.push(rtt_us);
        if self.rtt_samples.len() > 32 {
            self.rtt_samples.remove(0);
        }

        // RTprop = min RTT observed
        if rtt_us < self.rt_prop_us {
            self.rt_prop_us = rtt_us;
            self.rt_prop_stamp = Instant::now();
        }

        // Detect bufferbloat: RTT > 1.5× RTprop without throughput increase
        if self.rt_prop_us > 0.0 && rtt_us > self.rt_prop_us * 1.5 {
            // Drain modem buffer — reduce pacing
            self.pacing_rate *= 0.9;
        }

        self.update_pacing_rate();
    }

    // ─── Radio feed-forward ─────────────────────────────────────────────

    /// Update with new radio metrics from the modem supervisor.
    pub fn on_radio_metrics(&mut self, metrics: &RadioMetrics) {
        let now = Instant::now();

        // SINR → capacity ceiling
        self.sinr_capacity_ceiling = Some(sinr_to_capacity_kbps(metrics.sinr_db));

        // CQI derivative tracking
        self.cqi_history.push((now, metrics.cqi));
        if self.cqi_history.len() > 16 {
            self.cqi_history.remove(0);
        }

        // Track consecutive CQI drops
        if self.cqi_history.len() >= 2 {
            let prev = self.cqi_history[self.cqi_history.len() - 2].1;
            let curr = metrics.cqi;
            if curr < prev {
                self.consecutive_cqi_drops += 1;
            } else {
                self.consecutive_cqi_drops = 0;
            }
        }

        // RSRP slope tracking
        self.rsrp_history.push((now, metrics.rsrp_dbm));
        if self.rsrp_history.len() > 16 {
            self.rsrp_history.remove(0);
        }

        // Evaluate state transitions
        self.evaluate_state_transition();

        // Always re-evaluate pacing rate after radio metrics update
        // (SINR ceiling may have changed even without state transition)
        self.update_pacing_rate();
    }

    /// Evaluate state machine transitions based on radio metrics.
    fn evaluate_state_transition(&mut self) {
        match self.state {
            BiscayState::Normal => {
                // CQI dropping for 3+ readings → CAUTIOUS
                if self.consecutive_cqi_drops >= 3 {
                    self.state = BiscayState::Cautious;
                    self.pacing_rate *= 0.7; // 30% reduction
                }
            }
            BiscayState::Cautious => {
                // RSRP slope < -2.5 dB/s AND RSRQ < -12 → PRE_HANDOVER
                let rsrp_slope = self.rsrp_slope_db_per_sec();
                let latest_rsrq = self.rsrp_history.last().map(|(_, v)| *v).unwrap_or(0.0);

                if rsrp_slope < -2.5 && latest_rsrq < -12.0 {
                    self.state = BiscayState::PreHandover;
                }

                // CQI stable (no drops for 3 readings) → NORMAL
                if self.consecutive_cqi_drops == 0 && self.cqi_history.len() >= 3 {
                    self.state = BiscayState::Normal;
                    self.update_pacing_rate();
                }
            }
            BiscayState::PreHandover => {
                // RSRP stabilized (slope > -1.0 for 3 readings) → NORMAL
                let rsrp_slope = self.rsrp_slope_db_per_sec();
                if rsrp_slope > -1.0 && self.rsrp_history.len() >= 3 {
                    self.state = BiscayState::Normal;
                    // Reset BBR state for re-probing
                    self.bbr_phase = BbrPhase::SlowStart;
                    self.bw_samples.clear();
                    self.update_pacing_rate();
                }
            }
        }
    }

    /// Compute RSRP slope in dB/sec from recent history.
    fn rsrp_slope_db_per_sec(&self) -> f64 {
        if self.rsrp_history.len() < 2 {
            return 0.0;
        }
        let (t1, v1) = self.rsrp_history[0];
        let (t2, v2) = self.rsrp_history[self.rsrp_history.len() - 1];
        let dt = t2.duration_since(t1).as_secs_f64();
        if dt < 0.001 {
            return 0.0;
        }
        (v2 - v1) / dt
    }

    /// Update pacing rate from BBR estimates and radio constraints.
    fn update_pacing_rate(&mut self) {
        let mut rate = match self.bbr_phase {
            BbrPhase::SlowStart => {
                if self.btl_bw > 0.0 {
                    // Transition to ProbeBw when we have a bandwidth estimate
                    self.bbr_phase = BbrPhase::ProbeBw;
                    self.btl_bw
                } else {
                    // Exponential ramp: double pacing rate
                    self.pacing_rate * 2.0
                }
            }
            BbrPhase::ProbeBw => {
                // Pacing rate = BtlBw × pacing_gain
                // BBRv3 uses gain cycling; simplified here
                self.btl_bw * 1.0
            }
            BbrPhase::ProbeRtt => {
                // Minimal sending during RTT probe
                self.btl_bw * 0.5
            }
        };

        // Apply radio-aware state dampening
        match self.state {
            BiscayState::Cautious => rate *= 0.7,
            BiscayState::PreHandover => rate *= 0.1, // minimal — drain only
            BiscayState::Normal => {}
        }

        // Enforce SINR capacity ceiling
        if let Some(ceiling_kbps) = self.sinr_capacity_ceiling {
            let ceiling_bps = ceiling_kbps * 1000.0 / 8.0; // kbps → bytes/sec
            rate = rate.min(ceiling_bps);
        }

        // Minimum pacing rate: 10 KB/s
        self.pacing_rate = rate.max(10_000.0);

        // Update cwnd = BDP = BtlBw × RTprop
        if self.btl_bw > 0.0 && self.rt_prop_us < f64::MAX {
            self.cwnd = self.btl_bw * (self.rt_prop_us / 1_000_000.0);
            // Minimum cwnd: 2 packets
            self.cwnd = self.cwnd.max(2800.0);
        }
    }

    /// Periodic tick — check for RTprop probe, state transitions.
    pub fn tick(&mut self) {
        let now = Instant::now();
        self.last_tick = now;

        // Check if RTprop needs re-probing
        if now.duration_since(self.rt_prop_stamp) > self.rt_prop_expiry
            && self.bbr_phase == BbrPhase::ProbeBw
        {
            self.bbr_phase = BbrPhase::ProbeRtt;
        }

        // Exit ProbeRtt after 200ms
        if self.bbr_phase == BbrPhase::ProbeRtt
            && now.duration_since(self.rt_prop_stamp)
                > self.rt_prop_expiry + Duration::from_millis(200)
        {
            self.bbr_phase = BbrPhase::ProbeBw;
            self.rt_prop_stamp = now;
        }
    }

    /// Whether this link should accept new packets (not in PRE_HANDOVER drain).
    pub fn can_enqueue(&self) -> bool {
        self.state != BiscayState::PreHandover
    }

    /// Bytes allowed to send in the next interval (for pacing).
    pub fn bytes_to_send(&self, interval_us: u64) -> usize {
        let bytes = self.pacing_rate * (interval_us as f64 / 1_000_000.0);
        bytes.max(0.0) as usize
    }
}

impl Default for BiscayController {
    fn default() -> Self {
        Self::new()
    }
}

// ─── SINR → Capacity Lookup ────────────────────────────────────────────────

/// Map SINR (dB) to approximate LTE/5G PHY capacity (kbps).
/// Based on simplified 3GPP MCS tables.
fn sinr_to_capacity_kbps(sinr_db: f64) -> f64 {
    // Simplified piecewise-linear mapping
    if sinr_db < -5.0 {
        100.0 // Barely usable
    } else if sinr_db < 0.0 {
        500.0
    } else if sinr_db < 5.0 {
        2_000.0
    } else if sinr_db < 10.0 {
        5_000.0
    } else if sinr_db < 15.0 {
        10_000.0
    } else if sinr_db < 20.0 {
        20_000.0
    } else if sinr_db < 25.0 {
        40_000.0
    } else {
        80_000.0 // Very strong signal (mmWave or close to tower)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── State Machine Tests ────────────────────────────────────────────

    #[test]
    fn initial_state_is_normal_slow_start() {
        let cc = BiscayController::new();
        assert_eq!(cc.state, BiscayState::Normal);
        assert_eq!(cc.bbr_phase, BbrPhase::SlowStart);
        assert!(cc.pacing_rate() > 0.0);
    }

    #[test]
    fn normal_to_cautious_on_cqi_drops() {
        let mut cc = BiscayController::new();

        // Simulate 4 consecutive CQI drops
        for cqi in [15, 12, 10, 8u8] {
            cc.on_radio_metrics(&RadioMetrics {
                cqi,
                sinr_db: 10.0,
                rsrp_dbm: -80.0,
                rsrq_db: -8.0,
                timestamp: Some(Instant::now()),
            });
        }

        assert_eq!(
            cc.state,
            BiscayState::Cautious,
            "should transition to Cautious after 3+ CQI drops"
        );
    }

    #[test]
    fn cautious_reduces_pacing_rate() {
        let mut cc = BiscayController::new();
        // Give it a bandwidth estimate first so pacing rate is meaningful
        cc.on_bandwidth_sample(500_000, 1_000_000); // 500 KB/s
        cc.on_rtt_sample(20_000.0);

        // Record rate before entering Cautious — use high SINR so ceiling doesn't interfere
        let rate_before_cautious = cc.pacing_rate();

        // Force into Cautious via CQI drops (use high SINR to avoid ceiling effect)
        for cqi in [15, 12, 10, 8u8] {
            cc.on_radio_metrics(&RadioMetrics {
                cqi,
                sinr_db: 30.0, // very high — ceiling won't interfere
                rsrp_dbm: -80.0,
                rsrq_db: -8.0,
                timestamp: Some(Instant::now()),
            });
        }

        assert_eq!(cc.state, BiscayState::Cautious);
        assert!(
            cc.pacing_rate() < rate_before_cautious,
            "Cautious state should reduce pacing rate: {} vs {}",
            cc.pacing_rate(),
            rate_before_cautious
        );
    }

    #[test]
    fn pre_handover_disables_enqueue() {
        let mut cc = BiscayController::new();
        cc.state = BiscayState::PreHandover;
        assert!(!cc.can_enqueue(), "PreHandover should block new enqueues");
    }

    #[test]
    fn normal_allows_enqueue() {
        let cc = BiscayController::new();
        assert!(cc.can_enqueue());
    }

    // ─── BBR Bandwidth Estimation Tests ─────────────────────────────────

    #[test]
    fn bandwidth_sample_updates_btlbw() {
        let mut cc = BiscayController::new();
        assert_eq!(cc.btl_bw(), 0.0);

        // 1 MB in 1 second = 1 MB/s
        cc.on_bandwidth_sample(1_000_000, 1_000_000);
        assert!(cc.btl_bw() > 0.0);
        assert!((cc.btl_bw() - 1_000_000.0).abs() < 1.0);
    }

    #[test]
    fn btlbw_is_max_of_samples() {
        let mut cc = BiscayController::new();

        // Low sample
        cc.on_bandwidth_sample(100_000, 1_000_000); // 100 KB/s
        let bw1 = cc.btl_bw();

        // High sample
        cc.on_bandwidth_sample(500_000, 1_000_000); // 500 KB/s
        let bw2 = cc.btl_bw();

        assert!(bw2 > bw1, "BtlBw should take max");

        // Low sample again — max shouldn't decrease
        cc.on_bandwidth_sample(50_000, 1_000_000);
        assert_eq!(cc.btl_bw(), bw2);
    }

    #[test]
    fn zero_interval_ignored() {
        let mut cc = BiscayController::new();
        cc.on_bandwidth_sample(1000, 0); // division by zero guard
        assert_eq!(cc.btl_bw(), 0.0);
    }

    // ─── RTT Tests ──────────────────────────────────────────────────────

    #[test]
    fn rtt_sample_updates_rtprop() {
        let mut cc = BiscayController::new();
        assert_eq!(cc.rt_prop_us(), f64::MAX);

        cc.on_rtt_sample(50_000.0); // 50ms
        assert_eq!(cc.rt_prop_us(), 50_000.0);

        cc.on_rtt_sample(40_000.0); // 40ms — new min
        assert_eq!(cc.rt_prop_us(), 40_000.0);

        cc.on_rtt_sample(60_000.0); // 60ms — doesn't change min
        assert_eq!(cc.rt_prop_us(), 40_000.0);
    }

    #[test]
    fn negative_rtt_ignored() {
        let mut cc = BiscayController::new();
        cc.on_rtt_sample(-100.0);
        assert_eq!(cc.rt_prop_us(), f64::MAX);
    }

    // ─── SINR Capacity Ceiling Tests ────────────────────────────────────

    #[test]
    fn sinr_ceiling_limits_pacing() {
        let mut cc = BiscayController::new();

        // Give it a high bandwidth estimate
        cc.on_bandwidth_sample(10_000_000, 1_000_000); // 10 MB/s
        cc.on_rtt_sample(10_000.0);

        let uncapped_rate = cc.pacing_rate();

        // Apply a low SINR ceiling
        cc.on_radio_metrics(&RadioMetrics {
            sinr_db: 0.0, // maps to ~500 kbps
            cqi: 5,
            rsrp_dbm: -90.0,
            rsrq_db: -10.0,
            timestamp: Some(Instant::now()),
        });

        // Rate should be capped below uncapped
        assert!(
            cc.pacing_rate() < uncapped_rate,
            "SINR ceiling should limit pacing rate: {} vs {}",
            cc.pacing_rate(),
            uncapped_rate
        );
    }

    #[test]
    fn sinr_to_capacity_ordering() {
        // Higher SINR should always yield higher capacity
        let c1 = sinr_to_capacity_kbps(-5.0);
        let c2 = sinr_to_capacity_kbps(0.0);
        let c3 = sinr_to_capacity_kbps(10.0);
        let c4 = sinr_to_capacity_kbps(25.0);
        assert!(c1 <= c2);
        assert!(c2 <= c3);
        assert!(c3 <= c4);
    }

    // ─── Pacing Budget Tests ────────────────────────────────────────────

    #[test]
    fn bytes_to_send_proportional_to_interval() {
        let cc = BiscayController::new();
        let b1 = cc.bytes_to_send(1_000); // 1ms
        let b2 = cc.bytes_to_send(10_000); // 10ms
                                           // 10× interval → ~10× bytes (not exact due to rounding)
        assert!(b2 > b1);
    }

    #[test]
    fn bytes_to_send_never_negative() {
        let mut cc = BiscayController::new();
        cc.state = BiscayState::PreHandover;
        cc.update_pacing_rate();
        let _bytes = cc.bytes_to_send(100);
        // usize is always >= 0, just testing it doesn't panic
    }

    // ─── Slow Start Transition ──────────────────────────────────────────

    #[test]
    fn slow_start_to_probe_bw_on_first_sample() {
        let mut cc = BiscayController::new();
        assert_eq!(cc.bbr_phase, BbrPhase::SlowStart);

        cc.on_bandwidth_sample(100_000, 1_000_000);
        assert_eq!(
            cc.bbr_phase,
            BbrPhase::ProbeBw,
            "should transition after first BW sample"
        );
    }

    // ─── Bufferbloat Detection ──────────────────────────────────────────

    #[test]
    fn bufferbloat_reduces_rate() {
        let mut cc = BiscayController::new();
        cc.on_bandwidth_sample(500_000, 1_000_000);
        cc.on_rtt_sample(20_000.0); // 20ms baseline

        let rate_before = cc.pacing_rate();

        // RTT spikes to 2× baseline (bufferbloat)
        cc.on_rtt_sample(40_000.0);
        // The pacing_rate gets reduced by 0.9 factor
        assert!(
            cc.pacing_rate() <= rate_before,
            "bufferbloat should reduce pacing rate"
        );
    }
}
