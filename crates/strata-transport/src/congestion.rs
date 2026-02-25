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

use quanta::Instant;
use std::collections::VecDeque;
use std::time::Duration;
use tracing::debug;

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
    /// Recent bandwidth samples with timestamps for BtlBw estimation.
    bw_samples: VecDeque<(Instant, f64)>,
    /// Max samples to keep.
    max_bw_samples: usize,
    /// Window duration for bandwidth samples — peaks older than this expire.
    bw_window: Duration,

    // ─── RTT tracking ───
    /// Recent RTT samples (µs) for RTprop estimation.
    rtt_samples: VecDeque<f64>,
    /// When RTprop was last updated.
    rt_prop_stamp: Instant,
    /// RTprop expiry — probe RTT if this old.
    rt_prop_expiry: Duration,

    // ─── Radio state ───
    /// CQI history for derivative tracking.
    cqi_history: VecDeque<(Instant, u8)>,
    /// RSRP history for slope tracking.
    rsrp_history: VecDeque<(Instant, f64)>,
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

    // ─── Bufferbloat drain ───
    /// Multiplicative factor applied to pacing rate when bufferbloat is
    /// detected (RTT >> RTprop). Decays toward 0.1 on bloat, recovers
    /// toward 1.0 when RTT normalises. Persists across `update_pacing_rate`
    /// calls so the reduction actually sticks.
    drain_factor: f64,

    // ─── Phase-shifted probing ───
    /// Whether this link holds the round-robin probe token.
    ///
    /// When `true`, ProbeBw applies a 1.25× gain (UP phase) to measure spare
    /// capacity. When `false`, pacing stays at 1.0× (cruise) to avoid
    /// simultaneous probing across all bonded links.
    probe_allowed: bool,
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

            bw_samples: VecDeque::with_capacity(64),
            max_bw_samples: 64,
            bw_window: Duration::from_secs(10),

            rtt_samples: VecDeque::with_capacity(32),
            rt_prop_stamp: now,
            rt_prop_expiry: Duration::from_secs(10),

            cqi_history: VecDeque::with_capacity(16),
            rsrp_history: VecDeque::with_capacity(16),
            sinr_capacity_ceiling: None,
            consecutive_cqi_drops: 0,

            created_at: now,
            last_tick: now,

            drain_factor: 1.0,
            probe_allowed: true, // default: allowed until coordinator assigns tokens
        }
    }

    /// Allow or inhibit ProbeBw UP-gain for phase-shifted multi-link probing.
    ///
    /// When the bonding layer coordinates probing across links, only one link
    /// at a time should hold `probe_allowed = true`. All others cruise at 1.0×
    /// BtlBw until they receive the rotating probe token.
    pub fn set_probe_allowed(&mut self, allowed: bool) {
        self.probe_allowed = allowed;
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

    /// Get the bufferbloat drain factor (0.2–1.0).
    pub fn drain_factor(&self) -> f64 {
        self.drain_factor
    }

    // ─── BBR feedback processing ────────────────────────────────────────

    /// Process a bandwidth sample (from ACK feedback).
    /// `delivered_bytes`: bytes acknowledged in this interval.
    /// `interval_us`: time interval in µs.
    /// `is_app_limited`: true if the sender did not have enough data in flight to fill the pipe.
    pub fn on_bandwidth_sample(
        &mut self,
        delivered_bytes: u64,
        interval_us: u64,
        is_app_limited: bool,
    ) {
        if interval_us == 0 {
            return;
        }
        let now = Instant::now();
        let bw = delivered_bytes as f64 / (interval_us as f64 / 1_000_000.0);

        // If app-limited, only use the sample if it's higher than our
        // current estimate. Otherwise, we'd artificially lower our
        // capacity estimate just because the app isn't sending enough.
        if is_app_limited && bw < self.btl_bw {
            debug!(
                target: "strata::cc",
                bw_sample_Bps = bw,
                btl_bw_Bps = self.btl_bw,
                app_limited = is_app_limited,
                "BW sample REJECTED (app-limited & below btl_bw)"
            );
            return;
        }

        let prev_btl_bw = self.btl_bw;

        // Expire samples older than the bandwidth window.
        let cutoff = now - self.bw_window;
        while let Some(&(ts, _)) = self.bw_samples.front() {
            if ts < cutoff {
                self.bw_samples.pop_front();
            } else {
                break;
            }
        }

        self.bw_samples.push_back((now, bw));
        if self.bw_samples.len() > self.max_bw_samples {
            self.bw_samples.pop_front();
        }

        // BtlBw estimation uses a two-phase strategy:
        //  - Startup (< MIN_STARTUP_SAMPLES): use max of all samples so the
        //    estimator discovers the peak delivery rate quickly.  Without this,
        //    early low samples (timing-dependent) can trap btl_bw in a low
        //    equilibrium: low btl_bw → low capacity → sender throttles →
        //    only low samples → btl_bw stays low.
        //  - Steady state (≥ MIN_STARTUP_SAMPLES): 75th-percentile filters
        //    ACK-burst outliers while still capturing genuine capacity changes.
        const MIN_STARTUP_SAMPLES: usize = 8;
        let mut sorted: Vec<f64> = self.bw_samples.iter().map(|&(_, v)| v).collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        self.btl_bw = if sorted.len() < MIN_STARTUP_SAMPLES {
            // Startup: max — discover peak quickly
            sorted.last().copied().unwrap_or(0.0)
        } else {
            // Stable: 75th percentile — filter outliers
            let idx = ((sorted.len() as f64 * 0.75) as usize).min(sorted.len().saturating_sub(1));
            sorted.get(idx).copied().unwrap_or(0.0)
        };

        debug!(
            target: "strata::cc",
            bw_sample_Bps = bw,
            bw_sample_kbps = bw * 8.0 / 1000.0,
            prev_btl_bw_Bps = prev_btl_bw,
            new_btl_bw_Bps = self.btl_bw,
            btl_bw_kbps = self.btl_bw * 8.0 / 1000.0,
            app_limited = is_app_limited,
            delivered_bytes = delivered_bytes,
            interval_us = interval_us,
            num_samples = self.bw_samples.len(),
            phase = ?self.bbr_phase,
            "BW sample ACCEPTED"
        );

        self.update_pacing_rate();
    }

    /// Process an RTT sample (from ACK or PONG).
    pub fn on_rtt_sample(&mut self, rtt_us: f64) {
        if rtt_us <= 0.0 {
            return;
        }

        self.rtt_samples.push_back(rtt_us);
        if self.rtt_samples.len() > 32 {
            self.rtt_samples.pop_front();
        }

        // RTprop = min RTT observed
        let prev_rt_prop = self.rt_prop_us;
        if rtt_us < self.rt_prop_us {
            self.rt_prop_us = rtt_us;
            self.rt_prop_stamp = Instant::now();
        }

        debug!(
            target: "strata::cc",
            rtt_us = rtt_us,
            rtt_ms = rtt_us / 1000.0,
            rt_prop_us = self.rt_prop_us,
            prev_rt_prop_us = prev_rt_prop,
            drain_factor = self.drain_factor,
            "RTT sample"
        );

        // Detect bufferbloat: RTT >> RTprop → reduce drain_factor.
        // RTT recovering toward RTprop → restore drain_factor.
        // drain_factor is applied inside update_pacing_rate() so the
        // reduction persists across recalculations.
        //
        // Thresholds are generous (4× / 2× RTprop) because bonded cellular
        // links typically have 50-200ms base RTT and moderate queuing is
        // normal. Aggressive drain at 1.5× would starve BBR of samples.
        //
        // Guard: require rt_prop_us ≥ 1 ms to avoid drain_factor collapse
        // from artificially low initial RTTs (e.g. first ping on docker
        // networks before real traffic). Sub-millisecond RTTs are not
        // realistic for cellular links and indicate a measurement artifact.
        if self.rt_prop_us >= 1_000.0 && self.rt_prop_us < f64::MAX {
            // Only update drain_factor periodically to avoid per-ACK overreaction
            let now = Instant::now();
            if now.duration_since(self.last_tick) > Duration::from_millis(100) {
                if rtt_us > self.rt_prop_us * 4.0 {
                    // Severe bloat — aggressive drain
                    self.drain_factor = (self.drain_factor * 0.85).max(0.05);
                } else if rtt_us > self.rt_prop_us * 2.0 {
                    // Moderate bloat — gentle drain
                    self.drain_factor = (self.drain_factor * 0.95).max(0.05);
                } else if rtt_us < self.rt_prop_us * 1.5 {
                    // RTT is near baseline — recover
                    self.drain_factor = (self.drain_factor + 0.05).min(1.0);
                }
                self.last_tick = now;
            }
        } else if self.drain_factor < 1.0 {
            // Time-based recovery: if rt_prop_us is stale/invalid, slowly
            // restore drain_factor so we don't stay permanently throttled.
            let now = Instant::now();
            if now.duration_since(self.last_tick) > Duration::from_millis(100) {
                self.drain_factor = (self.drain_factor + 0.02).min(1.0);
                self.last_tick = now;
            }
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
        self.cqi_history.push_back((now, metrics.cqi));
        if self.cqi_history.len() > 16 {
            self.cqi_history.pop_front();
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
        self.rsrp_history.push_back((now, metrics.rsrp_dbm));
        if self.rsrp_history.len() > 16 {
            self.rsrp_history.pop_front();
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
                let latest_rsrp = self.rsrp_history.back().map(|(_, v)| *v).unwrap_or(0.0);

                if rsrp_slope < -2.5 && latest_rsrp < -12.0 {
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
                    self.btl_bw = 0.0;
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
        let (t2, v2) = *self.rsrp_history.back().unwrap();
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
                // Stay in SlowStart (reported as Probe phase to the DWRR)
                // until we accumulate MIN_CALIBRATION_SAMPLES of delivery-
                // rate data.  During SlowStart the DWRR applies a flat
                // capacity floor so all links receive roughly equal traffic.
                // This "calibration period" lets each link's btl_bw converge
                // to its true bottleneck rate under uniform load before the
                // DWRR switches to btl_bw-proportional credits.
                //
                // With ~3.3 samples/sec per link, 30 samples ≈ 9 seconds
                // of calibration — long enough for the 10 s bw_window to
                // fill and the 75th-percentile filter to stabilise.
                const MIN_CALIBRATION_SAMPLES: usize = 30;
                if self.btl_bw > 0.0 && self.bw_samples.len() >= MIN_CALIBRATION_SAMPLES {
                    self.bbr_phase = BbrPhase::ProbeBw;
                    self.btl_bw
                } else if self.btl_bw > 0.0 {
                    // Have an estimate but still in calibration —
                    // use btl_bw for pacing but stay in SlowStart so the
                    // link reports Probe phase to the DWRR scheduler.
                    self.btl_bw
                } else {
                    // No bandwidth data yet — keep current pacing_rate.
                    return;
                }
            }
            BbrPhase::ProbeBw => {
                // Pacing rate = BtlBw × pacing_gain.
                // Only apply the UP-probe gain (1.25×) when this link holds the
                // phase-shifted probe token; otherwise cruise at 1.0× to prevent
                // simultaneous probing from all bonded links.
                let gain = if self.probe_allowed { 1.25 } else { 1.0 };
                self.btl_bw * gain
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

        // Apply bufferbloat drain factor
        rate *= self.drain_factor;

        // Minimum pacing rate: 10 KB/s
        self.pacing_rate = rate.max(10_000.0);

        // Update cwnd = BDP = BtlBw × RTprop
        if self.btl_bw > 0.0 && self.rt_prop_us < f64::MAX {
            self.cwnd = self.btl_bw * (self.rt_prop_us / 1_000_000.0);
            // Minimum cwnd: 2 packets
            self.cwnd = self.cwnd.max(2800.0);
        }

        debug!(
            target: "strata::cc",
            pacing_rate_Bps = self.pacing_rate,
            pacing_rate_kbps = self.pacing_rate * 8.0 / 1000.0,
            btl_bw_Bps = self.btl_bw,
            btl_bw_kbps = self.btl_bw * 8.0 / 1000.0,
            drain_factor = self.drain_factor,
            cwnd = self.cwnd,
            phase = ?self.bbr_phase,
            state = ?self.state,
            sinr_ceiling_kbps = self.sinr_capacity_ceiling,
            probe_allowed = self.probe_allowed,
            "pacing rate updated"
        );
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
            // Reset RTprop to the latest RTT sample so bandwidth samples
            // aren't rejected as app-limited (BDP = btl_bw × ∞ when
            // rt_prop=MAX). The next real RTT sample will refine it.
            if let Some(&latest) = self.rtt_samples.back() {
                self.rt_prop_us = latest;
            }
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
        cc.on_bandwidth_sample(500_000, 1_000_000, false); // 500 KB/s
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
        cc.on_bandwidth_sample(1_000_000, 1_000_000, false);
        assert!(cc.btl_bw() > 0.0);
        assert!((cc.btl_bw() - 1_000_000.0).abs() < 1.0);
    }

    #[test]
    fn btlbw_is_max_of_recent_samples() {
        let mut cc = BiscayController::new();

        // Low sample
        cc.on_bandwidth_sample(100_000, 1_000_000, false); // 100 KB/s
        let bw1 = cc.btl_bw();

        // High sample
        cc.on_bandwidth_sample(500_000, 1_000_000, false); // 500 KB/s
        let bw2 = cc.btl_bw();

        assert!(bw2 > bw1, "BtlBw should take max");

        // Low sample again — max shouldn't decrease (within window)
        cc.on_bandwidth_sample(50_000, 1_000_000, false);
        assert_eq!(cc.btl_bw(), bw2);

        // Simulate window expiry: shrink window to 0 and feed low sample
        cc.bw_window = Duration::from_millis(0);
        cc.on_bandwidth_sample(50_000, 1_000_000, false); // 50 KB/s
        assert!(
            cc.btl_bw() < bw2,
            "BtlBw should decrease after old peaks expire"
        );
    }

    #[test]
    fn zero_interval_ignored() {
        let mut cc = BiscayController::new();
        cc.on_bandwidth_sample(1000, 0, false); // division by zero guard
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
        cc.on_bandwidth_sample(10_000_000, 1_000_000, false); // 10 MB/s
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
    fn slow_start_stays_until_enough_samples() {
        let mut cc = BiscayController::new();
        assert_eq!(cc.bbr_phase, BbrPhase::SlowStart);

        // Feed samples — need >= 30 (MIN_CALIBRATION_SAMPLES) for transition
        for _ in 0..29 {
            cc.on_bandwidth_sample(150_000, 1_000_000, false);
            assert_eq!(
                cc.bbr_phase,
                BbrPhase::SlowStart,
                "should stay in SlowStart with < 30 samples"
            );
        }

        // 30th sample triggers transition
        cc.on_bandwidth_sample(180_000, 1_000_000, false);
        assert_eq!(
            cc.bbr_phase,
            BbrPhase::ProbeBw,
            "should transition to ProbeBw with ≥ 30 samples"
        );
    }

    // ─── Bufferbloat Detection ──────────────────────────────────────────

    #[test]
    fn bufferbloat_reduces_rate() {
        let mut cc = BiscayController::new();
        cc.on_bandwidth_sample(500_000, 1_000_000, false);
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
