//! # Capacity Oracle
//!
//! Per-link physical capacity estimator, decoupled from BBR congestion control.
//!
//! The Oracle solves the multi-path scheduling feedback loop where
//! `btl_bw ≈ traffic_sent` under partial load, causing the scheduler to see
//! identical capacities for links with very different physical rates.
//!
//! Two estimation signals feed the oracle:
//! - **Delivery rate observations** — continuous, passive lower-bound updates
//! - **Saturation probes** — periodic, active upper-bound measurements
//!
//! All scheduling consumers (`capacity_bps`, EDPF, IoDS, BLEST)
//! read `estimated_cap()` instead of raw `btl_bw`.

use std::time::Instant;

/// Default confidence half-life in seconds. Without fresh evidence,
/// confidence decays to 50% after this interval.
const CONFIDENCE_HALF_LIFE_S: f64 = 30.0;

/// Per-link capacity estimator.
///
/// Maintains a lower bound (max observed delivery rate) and an upper bound
/// (peak from saturation probes) to produce a stable capacity estimate
/// for the EDPF scheduler.
#[derive(Debug)]
pub struct CapacityOracle {
    /// Best estimate of physical capacity (bps).
    estimated_cap: f64,
    /// Conservative floor: max delivery rate ever observed on this link.
    /// Only increases (except on explicit reset).
    lower_bound: f64,
    /// Peak from most recent saturation probe (bps).
    upper_bound: f64,
    /// Confidence in current estimate (0.0–1.0).
    confidence: f64,
    /// When the last saturation probe completed.
    last_probe: Instant,
    /// When the last evidence (any sample) was received.
    last_evidence: Instant,
    /// EWMA of RTT for downshift detection (ms).
    baseline_rtt_ms: f64,
    /// When the last downshift reset occurred, to prevent rapid re-triggering.
    last_reset: Instant,
    /// Whether a saturation probe is active on this link. When true,
    /// delivery observations are suppressed (the inflated traffic would
    /// corrupt the lower bound).
    probe_active: bool,
    /// Slow-decaying high-water mark of `estimated_cap`. Used as a stable
    /// reference for pacing floors so that transient delivery drops don't
    /// create a death spiral.
    peak_estimate: f64,
    /// When tick() was last called, for delta-based confidence decay.
    last_tick: Instant,
}

impl CapacityOracle {
    /// Create a new oracle with no initial estimate.
    pub fn new() -> Self {
        let now = Instant::now();
        Self {
            estimated_cap: 0.0,
            lower_bound: 0.0,
            upper_bound: 0.0,
            confidence: 0.0,
            last_probe: now,
            last_evidence: now,
            baseline_rtt_ms: 0.0,
            last_reset: now,
            probe_active: false,
            peak_estimate: 0.0,
            last_tick: now,
        }
    }

    /// Returns the current capacity estimate (bps).
    ///
    /// When confidence is high, returns close to `upper_bound` (probe result).
    /// When confidence is low, returns closer to `lower_bound` (conservative).
    /// Returns 0.0 before any evidence is available.
    #[inline]
    pub fn estimated_cap(&self) -> f64 {
        self.estimated_cap
    }

    /// Returns current confidence level (0.0–1.0).
    #[inline]
    pub fn confidence(&self) -> f64 {
        self.confidence
    }

    /// Returns the lower bound (max observed delivery rate, bps).
    #[inline]
    pub fn lower_bound(&self) -> f64 {
        self.lower_bound
    }

    /// Returns the upper bound (last saturation probe peak, bps).
    #[inline]
    pub fn upper_bound(&self) -> f64 {
        self.upper_bound
    }

    /// Returns the slow-decaying peak capacity estimate (bps).
    ///
    /// This high-water mark tracks the best oracle estimate ever seen,
    /// decaying at ~1%/sec. Used as a pacing floor reference that resists
    /// short-term delivery dips.
    #[inline]
    pub fn peak_cap(&self) -> f64 {
        self.peak_estimate
    }

    /// Feed a delivery rate observation (bps) from ongoing transport.
    ///
    /// Updates the lower bound using a slow EWMA that tracks sustained
    /// delivery rates. This allows the lower bound to decrease when traffic
    /// shifts away, while still reflecting genuine throughput capacity.
    pub fn observe_delivery(&mut self, delivery_bps: f64) {
        if delivery_bps <= 0.0 || self.probe_active {
            return;
        }

        // Cap the delivery signal to 2× current lower_bound. Bidirectional
        // shaping causes ACK batching (bunched arrivals after return-path
        // delay/loss) that can produce delivery spikes 5-10× the true link
        // rate. Without this cap a single spike can push lower_bound (and
        // thus estimated_cap) far above the physical capacity.
        let capped = if self.lower_bound > 100_000.0 {
            delivery_bps.min(self.lower_bound * 2.0)
        } else {
            delivery_bps
        };

        if self.lower_bound == 0.0 {
            self.lower_bound = capped;
        } else {
            // Asymmetric EWMA: rise quickly (α=0.3), fall slowly (α=0.05).
            // This tracks peak sustainable throughput without getting stuck
            // at a burst spike that exceeds the link's true capacity.
            let alpha = if capped > self.lower_bound { 0.3 } else { 0.05 };
            self.lower_bound = (1.0 - alpha) * self.lower_bound + alpha * capped;
        }

        tracing::trace!(
            target: "strata::oracle",
            delivery_kbps = delivery_bps / 1000.0,
            lower_bound_kbps = self.lower_bound / 1000.0,
            upper_bound_kbps = self.upper_bound / 1000.0,
            confidence = self.confidence,
            "observe_delivery"
        );

        self.last_evidence = Instant::now();
        self.recompute();
    }

    /// Record the result of a saturation probe.
    ///
    /// The peak observed delivery rate during a 400ms window where this
    /// link received ~100% of traffic. Sets the upper bound and raises
    /// confidence to maximum.
    pub fn complete_probe(&mut self, peak_bps: f64) {
        if peak_bps <= 0.0 {
            return;
        }

        tracing::info!(
            target: "strata::oracle",
            peak_kbps = peak_bps / 1000.0,
            old_upper_kbps = self.upper_bound / 1000.0,
            lower_bound_kbps = self.lower_bound / 1000.0,
            "complete_probe"
        );

        self.upper_bound = peak_bps;

        // The lower bound should never exceed the probe result — cap it.
        // This handles the case where a spurious delivery spike inflated
        // the lower bound above the true capacity measured by saturation.
        if self.lower_bound > peak_bps * 1.1 {
            self.lower_bound = peak_bps;
        }

        self.confidence = 1.0;
        self.last_probe = Instant::now();
        self.last_evidence = self.last_probe;
        self.recompute();
    }

    /// Signal a potential capacity change (handover, severe loss/RTT).
    ///
    /// Reduces confidence sharply but preserves the lower bound at a
    /// fraction of its current value, avoiding a full collapse that would
    /// starve the link.
    pub fn reset_on_downshift(&mut self) {
        self.confidence = 0.0;
        // Preserve 50% of the lower bound — the link likely still has
        // *some* capacity, just not what we previously measured.
        self.lower_bound *= 0.5;
        self.last_reset = Instant::now();
        // Keep upper_bound — it's still the best guess until the next probe.
        self.recompute();
    }

    /// Check if a downshift should trigger based on RTT spike.
    ///
    /// Only considers dramatic RTT increases (handover/route change),
    /// not cumulative loss ratios, which can only increase over time
    /// and would cause permanent re-triggering.
    pub fn should_reset(&self, rtt_ms: f64, _loss_rate: f64) -> bool {
        // Cooldown: don't reset more than once per 10 seconds.
        if self.last_reset.elapsed().as_secs_f64() < 10.0 {
            return false;
        }
        // Dramatic RTT spike: > 3× the baseline EWMA
        if self.baseline_rtt_ms > 5.0 && rtt_ms > self.baseline_rtt_ms * 3.0 {
            return true;
        }
        false
    }

    /// Update the baseline RTT for downshift detection.
    ///
    /// Uses an EWMA (α=0.05) of RTT to track a slowly-moving baseline.
    /// This is more robust than tracking the perpetual minimum, which
    /// would make the spike detector overly sensitive.
    pub fn update_baseline_rtt(&mut self, rtt_ms: f64) {
        if rtt_ms > 0.0 {
            if self.baseline_rtt_ms == 0.0 {
                self.baseline_rtt_ms = rtt_ms;
            } else {
                // Slow EWMA so normal jitter doesn't shift baseline quickly
                self.baseline_rtt_ms = 0.95 * self.baseline_rtt_ms + 0.05 * rtt_ms;
            }
        }
    }

    /// Apply time-based confidence decay and recompute the estimate.
    ///
    /// Called periodically (e.g. every `refresh_metrics()` cycle).
    pub fn tick(&mut self) {
        let now = Instant::now();
        let delta_s = now.duration_since(self.last_tick).as_secs_f64();
        self.last_tick = now;

        if delta_s > 0.0 && self.confidence > 0.0 {
            // Exponential decay: confidence halves every CONFIDENCE_HALF_LIFE_S
            let decay = (0.5_f64).powf(delta_s / CONFIDENCE_HALF_LIFE_S);
            self.confidence *= decay;
            // Floor at zero to avoid subnormal float arithmetic
            if self.confidence < 0.001 {
                self.confidence = 0.0;
            }
        }
        self.recompute();

        // Update slow-decaying peak: tracks the highest estimated_cap
        // and decays ~1%/sec (0.5% per tick at 2 ticks/sec) toward the
        // current estimate. Prevents short-term delivery dips from
        // collapsing the pacing floor.
        if self.estimated_cap > self.peak_estimate {
            self.peak_estimate = self.estimated_cap;
        } else {
            self.peak_estimate *= 0.999;
        }
    }

    /// Recompute `estimated_cap` from bounds and confidence.
    fn recompute(&mut self) {
        if self.upper_bound > 0.0 && self.lower_bound > 0.0 {
            // Lerp between conservative (lower) and probe-backed (upper)
            self.estimated_cap = lerp(self.lower_bound, self.upper_bound, self.confidence);
        } else if self.upper_bound > 0.0 {
            // Have probe data but no delivery floor yet
            self.estimated_cap = self.upper_bound * self.confidence;
        } else if self.lower_bound > 0.0 {
            // No probe yet — use lower bound as best-effort
            self.estimated_cap = self.lower_bound;
        }
        // else: no data at all → stays at 0.0 (caller falls back to btl_bw)
    }

    /// Accept a packet-pair capacity sample from the transport layer.
    ///
    /// PPD samples are noisier than saturation probes. They are capped
    /// relative to the observed delivery rate (lower_bound) and blended
    /// conservatively (30% new, 70% old) with a small confidence boost.
    pub fn observe_packet_pair(&mut self, capacity_bps: f64) {
        if capacity_bps <= 0.0 {
            return;
        }

        // Sanity cap: even after the receiver-side dispersion guard,
        // PPD can still over-estimate in buffered/simulated networks.
        // Never trust PPD above 3× observed delivery rate.
        let cap = if self.lower_bound > 100_000.0 {
            self.lower_bound * 3.0
        } else {
            // No delivery baseline yet — use a generous absolute ceiling
            50_000_000.0
        };
        let capped_bps = capacity_bps.min(cap);

        // Conservative blending: 30% new, 70% old
        if self.upper_bound > 0.0 {
            self.upper_bound = 0.3 * capped_bps + 0.7 * self.upper_bound;
        } else {
            self.upper_bound = capped_bps;
        }

        // Small confidence boost — PPD is supplementary, not authoritative
        self.confidence = (self.confidence + 0.05).min(1.0);
        self.last_evidence = Instant::now();
        self.recompute();
    }

    /// Set saturation probe active state. When active, delivery
    /// observations are suppressed to prevent inflated traffic rates
    /// from corrupting the lower bound.
    pub fn set_probe_active(&mut self, active: bool) {
        self.probe_active = active;
    }
}

impl Default for CapacityOracle {
    fn default() -> Self {
        Self::new()
    }
}

/// Linear interpolation between `a` and `b` by factor `t` (0.0–1.0).
#[inline]
fn lerp(a: f64, b: f64, t: f64) -> f64 {
    a + (b - a) * t
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn new_oracle_returns_zero() {
        let oracle = CapacityOracle::new();
        assert_eq!(oracle.estimated_cap(), 0.0);
        assert_eq!(oracle.confidence(), 0.0);
        assert_eq!(oracle.lower_bound(), 0.0);
        assert_eq!(oracle.upper_bound(), 0.0);
    }

    #[test]
    fn delivery_observation_sets_lower_bound() {
        let mut oracle = CapacityOracle::new();
        oracle.observe_delivery(3_000_000.0);
        assert_eq!(oracle.lower_bound(), 3_000_000.0);
        // With only lower bound, estimated_cap = lower_bound
        assert_eq!(oracle.estimated_cap(), 3_000_000.0);
    }

    #[test]
    fn lower_bound_rises_fast_falls_slow() {
        let mut oracle = CapacityOracle::new();
        oracle.observe_delivery(5_000_000.0);
        // Lower delivery: EWMA falls slowly (α=0.05)
        oracle.observe_delivery(3_000_000.0);
        // 0.95 * 5M + 0.05 * 3M = 4.9M
        assert!((oracle.lower_bound() - 4_900_000.0).abs() < 1.0);
        // Still close to the peak — slow decay preserves estimate
        assert!(oracle.lower_bound() > 4_500_000.0);
    }

    #[test]
    fn probe_sets_upper_bound_and_confidence() {
        let mut oracle = CapacityOracle::new();
        oracle.complete_probe(8_000_000.0);
        assert_eq!(oracle.upper_bound(), 8_000_000.0);
        assert_eq!(oracle.confidence(), 1.0);
        // With only upper bound at full confidence
        assert_eq!(oracle.estimated_cap(), 8_000_000.0);
    }

    #[test]
    fn lerp_between_bounds_with_confidence() {
        let mut oracle = CapacityOracle::new();
        oracle.observe_delivery(3_000_000.0);
        oracle.complete_probe(8_000_000.0);
        // confidence = 1.0, both bounds set → estimated = upper
        assert!((oracle.estimated_cap() - 8_000_000.0).abs() < 1.0);

        // Manually reduce confidence to test lerp
        oracle.confidence = 0.5;
        oracle.recompute();
        // lerp(3M, 8M, 0.5) = 5.5M
        assert!((oracle.estimated_cap() - 5_500_000.0).abs() < 1.0);

        oracle.confidence = 0.0;
        oracle.recompute();
        // lerp(3M, 8M, 0.0) = 3M
        assert!((oracle.estimated_cap() - 3_000_000.0).abs() < 1.0);
    }

    #[test]
    fn probe_caps_lower_bound_if_above_probe() {
        let mut oracle = CapacityOracle::new();
        // Spurious high delivery (e.g. burst)
        oracle.observe_delivery(12_000_000.0);
        assert_eq!(oracle.lower_bound(), 12_000_000.0);
        // Probe reveals true capacity is 8M
        oracle.complete_probe(8_000_000.0);
        // Lower bound should be capped to probe value
        assert_eq!(oracle.lower_bound(), 8_000_000.0);
    }

    #[test]
    fn reset_on_downshift_reduces_confidence_and_lower() {
        let mut oracle = CapacityOracle::new();
        oracle.observe_delivery(5_000_000.0);
        oracle.complete_probe(8_000_000.0);
        assert!(oracle.confidence() > 0.0);

        oracle.reset_on_downshift();
        assert_eq!(oracle.confidence(), 0.0);
        // Lower bound preserved at 50% (soft reset)
        assert!((oracle.lower_bound() - 2_500_000.0).abs() < 1.0);
        // Upper bound preserved as fallback
        assert_eq!(oracle.upper_bound(), 8_000_000.0);
        // With confidence=0: estimated = lerp(lower, upper, 0) = lower
        assert!((oracle.estimated_cap() - 2_500_000.0).abs() < 1.0);
    }

    #[test]
    fn should_reset_ignores_cumulative_loss() {
        let mut oracle = CapacityOracle::new();
        // Backdate last_reset to bypass cooldown
        oracle.last_reset = Instant::now() - Duration::from_secs(20);
        // Cumulative loss (even high) should NOT trigger reset
        assert!(!oracle.should_reset(50.0, 0.25));
        assert!(!oracle.should_reset(50.0, 2.5));
    }

    #[test]
    fn should_reset_on_rtt_spike() {
        let mut oracle = CapacityOracle::new();
        oracle.update_baseline_rtt(50.0);
        // Backdate last_reset to bypass cooldown
        oracle.last_reset = Instant::now() - Duration::from_secs(20);
        assert!(oracle.should_reset(160.0, 0.0)); // > 3x baseline
        assert!(!oracle.should_reset(140.0, 0.0)); // < 3x baseline
    }

    #[test]
    fn tick_decays_confidence() {
        let mut oracle = CapacityOracle::new();
        oracle.complete_probe(8_000_000.0);
        assert_eq!(oracle.confidence(), 1.0);

        // Simulate time passing by backdating last_tick
        oracle.last_tick = Instant::now() - Duration::from_secs(30);
        oracle.tick();
        // After one half-life, confidence should be ~0.5
        assert!(oracle.confidence() < 0.6);
        assert!(oracle.confidence() > 0.4);
    }

    #[test]
    fn zero_delivery_ignored() {
        let mut oracle = CapacityOracle::new();
        oracle.observe_delivery(0.0);
        oracle.observe_delivery(-100.0);
        assert_eq!(oracle.lower_bound(), 0.0);
        assert_eq!(oracle.estimated_cap(), 0.0);
    }

    #[test]
    fn zero_probe_ignored() {
        let mut oracle = CapacityOracle::new();
        oracle.complete_probe(0.0);
        oracle.complete_probe(-100.0);
        assert_eq!(oracle.upper_bound(), 0.0);
        assert_eq!(oracle.confidence(), 0.0);
    }

    #[test]
    fn baseline_rtt_uses_ewma() {
        let mut oracle = CapacityOracle::new();
        oracle.update_baseline_rtt(50.0); // First: set directly
        assert_eq!(oracle.baseline_rtt_ms, 50.0);
        oracle.update_baseline_rtt(40.0); // 0.95*50 + 0.05*40 = 49.5
        assert!((oracle.baseline_rtt_ms - 49.5).abs() < 0.01);
        oracle.update_baseline_rtt(60.0); // 0.95*49.5 + 0.05*60 = 50.025
        assert!((oracle.baseline_rtt_ms - 50.025).abs() < 0.01);
    }

    #[test]
    fn full_lifecycle() {
        let mut oracle = CapacityOracle::new();

        // Phase 1: No data — falls back
        assert_eq!(oracle.estimated_cap(), 0.0);

        // Phase 2: Delivery observations bootstrap lower bound
        oracle.observe_delivery(2_000_000.0);
        oracle.observe_delivery(2_500_000.0);
        // EWMA: 0.7 * 2M + 0.3 * 2.5M = 2.15M (rises fast α=0.3)
        let lb = oracle.lower_bound();
        assert!(
            lb > 2_000_000.0 && lb <= 2_500_000.0,
            "lower bound should be between first and second delivery: {lb}"
        );
        assert_eq!(oracle.estimated_cap(), lb);

        // Phase 3: First saturation probe reveals true capacity
        oracle.complete_probe(5_000_000.0);
        assert!((oracle.estimated_cap() - 5_000_000.0).abs() < 1.0);
        assert_eq!(oracle.confidence(), 1.0);

        // Phase 4: Confidence decays over time
        oracle.last_tick = Instant::now() - Duration::from_secs(60);
        oracle.tick();
        assert!(oracle.confidence() < 0.3);
        // Estimate drifts toward lower bound
        assert!(oracle.estimated_cap() < 4_000_000.0);
        assert!(oracle.estimated_cap() > lb);

        // Phase 5: New probe restores confidence
        oracle.complete_probe(5_200_000.0);
        assert_eq!(oracle.confidence(), 1.0);
        assert!((oracle.estimated_cap() - 5_200_000.0).abs() < 1.0);

        // Phase 6: Downshift (handover) — soft reset
        oracle.reset_on_downshift();
        assert_eq!(oracle.confidence(), 0.0);
        // Lower bound halved, upper bound preserved
        assert!(oracle.lower_bound() > 0.0);
        assert_eq!(oracle.upper_bound(), 5_200_000.0);
    }

    // ─── PPD (Packet-Pair Dispersion) Tests ─────────────────────────────

    #[test]
    fn ppd_sample_blends_into_upper_bound() {
        let mut oracle = CapacityOracle::new();
        // Need a delivery baseline for the 3× cap to matter
        oracle.observe_delivery(5_000_000.0);
        oracle.complete_probe(10_000_000.0);
        assert_eq!(oracle.upper_bound(), 10_000_000.0);

        // PPD sample at 8M — capped at 3×lower = 15M, so 8M is under cap.
        // Blends 30% new, 70% old: 0.3 * 8M + 0.7 * 10M = 9.4M
        oracle.observe_packet_pair(8_000_000.0);
        let expected = 0.3 * 8_000_000.0 + 0.7 * 10_000_000.0;
        assert!(
            (oracle.upper_bound() - expected).abs() < 1.0,
            "PPD should blend 30/70 into upper_bound"
        );
    }

    #[test]
    fn ppd_sample_boosts_confidence() {
        let mut oracle = CapacityOracle::new();
        // Start with zero confidence
        assert_eq!(oracle.confidence(), 0.0);
        // Will use 50M absolute cap since no delivery baseline
        oracle.observe_packet_pair(5_000_000.0);
        assert!(
            (oracle.confidence() - 0.05).abs() < 0.01,
            "PPD should boost confidence by 0.05"
        );
    }

    #[test]
    fn ppd_sample_caps_confidence_at_one() {
        let mut oracle = CapacityOracle::new();
        oracle.complete_probe(10_000_000.0);
        assert_eq!(oracle.confidence(), 1.0);
        // Additional PPD shouldn't push above 1.0
        oracle.observe_packet_pair(10_000_000.0);
        assert!(oracle.confidence() <= 1.0);
    }

    #[test]
    fn ppd_zero_ignored() {
        let mut oracle = CapacityOracle::new();
        oracle.observe_packet_pair(0.0);
        oracle.observe_packet_pair(-1000.0);
        assert_eq!(oracle.upper_bound(), 0.0);
        assert_eq!(oracle.confidence(), 0.0);
    }

    #[test]
    fn ppd_caps_at_3x_lower_bound() {
        let mut oracle = CapacityOracle::new();
        oracle.observe_delivery(3_000_000.0); // 3 Mbps lower bound
        // PPD reports 100 Mbps — should be capped at 9 Mbps (3× lower)
        oracle.observe_packet_pair(100_000_000.0);
        // upper_bound = 9M (capped), confidence < 1.0
        assert!(
            oracle.upper_bound() <= 10_000_000.0,
            "PPD should be capped at 3× lower_bound, got {}",
            oracle.upper_bound()
        );
    }

    #[test]
    fn ppd_updates_estimate() {
        let mut oracle = CapacityOracle::new();
        oracle.observe_delivery(2_000_000.0);
        // PPD at 6M — under 3× cap (6M). Capped to 6M.
        oracle.observe_packet_pair(6_000_000.0);
        // Upper bound set, confidence boosted → estimate should be above lower
        assert!(
            oracle.estimated_cap() > 2_000_000.0,
            "PPD should raise estimate above lower bound"
        );
    }
}
