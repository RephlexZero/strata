//! # Link Health Scoring
//!
//! Composite health score (0–100) from RF and transport metrics.
//!
//! $$\text{Score} = w_1 \cdot \text{norm(SINR)} + w_2 \cdot \text{norm(RSRQ)} + w_3 \cdot (1 - \text{loss\_rate}) + w_4 \cdot (1 - \text{norm(jitter)})$$

use crate::scheduler::kalman::{KalmanConfig, KalmanFilter};

/// Raw RF metrics from a cellular modem (via QMI/MBIM).
#[derive(Debug, Clone, Copy, Default)]
pub struct RfMetrics {
    /// Reference Signal Received Power in dBm. Range: -140 to -44.
    pub rsrp_dbm: f64,
    /// Reference Signal Received Quality in dB. Range: -20 to -3.
    pub rsrq_db: f64,
    /// Signal-to-Interference-plus-Noise Ratio in dB. Range: -20 to 30.
    pub sinr_db: f64,
    /// Channel Quality Indicator. Range: 0–15.
    pub cqi: u8,
}

/// Transport-level metrics.
#[derive(Debug, Clone, Copy, Default)]
pub struct TransportMetrics {
    /// Packet loss rate [0, 1].
    pub loss_rate: f64,
    /// One-way jitter in milliseconds.
    pub jitter_ms: f64,
    /// Smoothed RTT in milliseconds.
    pub rtt_ms: f64,
}

/// Weights for the composite health score.
#[derive(Debug, Clone, Copy)]
pub struct HealthWeights {
    /// Weight for SINR (default 0.35).
    pub sinr: f64,
    /// Weight for RSRQ (default 0.20).
    pub rsrq: f64,
    /// Weight for loss rate (default 0.30).
    pub loss: f64,
    /// Weight for jitter (default 0.15).
    pub jitter: f64,
}

impl Default for HealthWeights {
    fn default() -> Self {
        HealthWeights {
            sinr: 0.35,
            rsrq: 0.20,
            loss: 0.30,
            jitter: 0.15,
        }
    }
}

/// Per-link health estimator with Kalman-smoothed metrics.
pub struct LinkHealth {
    /// Kalman filter for SINR.
    sinr_filter: KalmanFilter,
    /// Kalman filter for RSRQ.
    rsrq_filter: KalmanFilter,
    /// Kalman filter for loss rate.
    loss_filter: KalmanFilter,
    /// Kalman filter for jitter.
    jitter_filter: KalmanFilter,
    /// Scoring weights.
    weights: HealthWeights,
    /// Last computed score.
    last_score: f64,
    /// Whether we've had any measurements.
    initialized: bool,
}

impl LinkHealth {
    pub fn new() -> Self {
        Self::with_weights(HealthWeights::default())
    }

    pub fn with_weights(weights: HealthWeights) -> Self {
        LinkHealth {
            sinr_filter: KalmanFilter::new(&KalmanConfig::for_signal()),
            rsrq_filter: KalmanFilter::new(&KalmanConfig::for_signal()),
            loss_filter: KalmanFilter::new(&KalmanConfig {
                q_value: 0.01,
                q_velocity: 0.001,
                r: 0.05,
            }),
            jitter_filter: KalmanFilter::new(&KalmanConfig::for_rtt()),
            weights,
            last_score: 50.0,
            initialized: false,
        }
    }

    /// Feed RF metrics from the modem.
    pub fn update_rf(&mut self, metrics: &RfMetrics) {
        self.sinr_filter.update(metrics.sinr_db);
        self.rsrq_filter.update(metrics.rsrq_db);
        self.initialized = true;
        self.recompute_score();
    }

    /// Feed transport-level metrics.
    pub fn update_transport(&mut self, metrics: &TransportMetrics) {
        self.loss_filter.update(metrics.loss_rate);
        self.jitter_filter.update(metrics.jitter_ms);
        self.initialized = true;
        self.recompute_score();
    }

    /// Current composite health score (0–100). Higher = healthier.
    pub fn score(&self) -> f64 {
        self.last_score
    }

    /// Whether the link is in good health (score > 50).
    pub fn is_healthy(&self) -> bool {
        self.last_score > 50.0
    }

    /// Whether SINR trend is degrading (possible impending handover).
    pub fn is_sinr_degrading(&self) -> bool {
        self.sinr_filter.is_initialized() && self.sinr_filter.velocity() < -0.5
    }

    /// Estimated SINR in `steps` ticks from now.
    pub fn predicted_sinr(&self, steps: u32) -> f64 {
        self.sinr_filter.predict_ahead(steps)
    }

    /// Recompute the composite score from current Kalman estimates.
    fn recompute_score(&mut self) {
        if !self.initialized {
            return;
        }

        // Normalize SINR: -20 dB → 0, +30 dB → 1
        let sinr_norm = if self.sinr_filter.is_initialized() {
            ((self.sinr_filter.value() + 20.0) / 50.0).clamp(0.0, 1.0)
        } else {
            0.5
        };

        // Normalize RSRQ: -20 dB → 0, -3 dB → 1
        let rsrq_norm = if self.rsrq_filter.is_initialized() {
            ((self.rsrq_filter.value() + 20.0) / 17.0).clamp(0.0, 1.0)
        } else {
            0.5
        };

        // Loss: 0% → 1, 100% → 0
        let loss_score = if self.loss_filter.is_initialized() {
            (1.0 - self.loss_filter.value()).clamp(0.0, 1.0)
        } else {
            1.0
        };

        // Jitter: 0ms → 1, 100ms → 0
        let jitter_norm = if self.jitter_filter.is_initialized() {
            (1.0 - self.jitter_filter.value() / 100.0).clamp(0.0, 1.0)
        } else {
            0.5
        };

        let raw = self.weights.sinr * sinr_norm
            + self.weights.rsrq * rsrq_norm
            + self.weights.loss * loss_score
            + self.weights.jitter * jitter_norm;

        self.last_score = (raw * 100.0).clamp(0.0, 100.0);
    }
}

impl Default for LinkHealth {
    fn default() -> Self {
        Self::new()
    }
}

/// CQI → approximate maximum throughput mapping (3GPP TS 36.213 Table 7.2.3-1).
///
/// Returns throughput in kbps for a 10 MHz LTE channel.
pub fn cqi_to_throughput_kbps(cqi: u8) -> f64 {
    match cqi {
        0 => 0.0,
        1 => 1_000.0,
        2 => 2_000.0,
        3 => 3_500.0,
        4 => 5_000.0,
        5 => 7_500.0,
        6 => 10_000.0,
        7 => 13_000.0,
        8 => 17_000.0,
        9 => 22_000.0,
        10 => 28_000.0,
        11 => 35_000.0,
        12 => 43_000.0,
        13 => 52_000.0,
        14 => 63_000.0,
        15 => 75_000.0,
        _ => 75_000.0,
    }
}

/// SINR → rough capacity ceiling in kbps (empirical mapping for LTE 10 MHz).
pub fn sinr_to_capacity_kbps(sinr_db: f64) -> f64 {
    if sinr_db < -5.0 {
        0.0
    } else if sinr_db < 0.0 {
        1000.0
    } else if sinr_db < 5.0 {
        5000.0
    } else if sinr_db < 10.0 {
        15000.0
    } else if sinr_db < 15.0 {
        30000.0
    } else if sinr_db < 20.0 {
        50000.0
    } else {
        75000.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── Basic Health Scoring ───────────────────────────────────────────

    #[test]
    fn initial_score_is_50() {
        let health = LinkHealth::new();
        assert!((health.score() - 50.0).abs() < 0.1);
    }

    #[test]
    fn good_rf_increases_score() {
        let mut health = LinkHealth::new();
        for _ in 0..10 {
            health.update_rf(&RfMetrics {
                rsrp_dbm: -70.0,
                rsrq_db: -5.0,
                sinr_db: 25.0,
                cqi: 14,
            });
        }
        assert!(
            health.score() > 60.0,
            "good RF should yield high score, got {}",
            health.score()
        );
    }

    #[test]
    fn poor_rf_decreases_score() {
        let mut health = LinkHealth::new();
        for _ in 0..10 {
            health.update_rf(&RfMetrics {
                rsrp_dbm: -130.0,
                rsrq_db: -18.0,
                sinr_db: -10.0,
                cqi: 2,
            });
        }
        assert!(
            health.score() < 50.0,
            "poor RF should yield low score, got {}",
            health.score()
        );
    }

    #[test]
    fn high_loss_reduces_score() {
        let mut health = LinkHealth::new();
        // Start with good RF
        for _ in 0..5 {
            health.update_rf(&RfMetrics {
                rsrp_dbm: -70.0,
                rsrq_db: -5.0,
                sinr_db: 20.0,
                cqi: 12,
            });
        }
        let before = health.score();

        // Add high loss
        for _ in 0..10 {
            health.update_transport(&TransportMetrics {
                loss_rate: 0.3,
                jitter_ms: 50.0,
                rtt_ms: 100.0,
            });
        }
        let after = health.score();

        assert!(
            after < before,
            "high loss should reduce score: before={before}, after={after}"
        );
    }

    // ─── Health Status ──────────────────────────────────────────────────

    #[test]
    fn healthy_with_good_metrics() {
        let mut health = LinkHealth::new();
        for _ in 0..10 {
            health.update_rf(&RfMetrics {
                rsrp_dbm: -75.0,
                rsrq_db: -6.0,
                sinr_db: 20.0,
                cqi: 12,
            });
            health.update_transport(&TransportMetrics {
                loss_rate: 0.01,
                jitter_ms: 5.0,
                rtt_ms: 30.0,
            });
        }
        assert!(health.is_healthy());
    }

    #[test]
    fn sinr_degradation_detected() {
        let mut health = LinkHealth::new();
        for i in 0..20 {
            health.update_rf(&RfMetrics {
                rsrp_dbm: -70.0,
                rsrq_db: -5.0,
                sinr_db: 25.0 - i as f64 * 2.0, // Degrading SINR
                cqi: 12,
            });
        }
        assert!(health.is_sinr_degrading(), "should detect degrading SINR");
    }

    // ─── CQI/SINR Mapping ──────────────────────────────────────────────

    #[test]
    fn cqi_0_is_zero() {
        assert_eq!(cqi_to_throughput_kbps(0), 0.0);
    }

    #[test]
    fn cqi_15_is_max() {
        assert_eq!(cqi_to_throughput_kbps(15), 75_000.0);
    }

    #[test]
    fn cqi_monotonically_increasing() {
        for cqi in 0..15 {
            assert!(cqi_to_throughput_kbps(cqi) <= cqi_to_throughput_kbps(cqi + 1));
        }
    }

    #[test]
    fn sinr_negative_is_zero() {
        assert_eq!(sinr_to_capacity_kbps(-10.0), 0.0);
    }

    #[test]
    fn sinr_high_is_max() {
        assert_eq!(sinr_to_capacity_kbps(25.0), 75_000.0);
    }

    // ─── Prediction ─────────────────────────────────────────────────────

    #[test]
    fn predicted_sinr_with_trend() {
        let mut health = LinkHealth::new();
        // Feed degrading SINR
        for i in 0..15 {
            health.update_rf(&RfMetrics {
                rsrp_dbm: -70.0,
                rsrq_db: -5.0,
                sinr_db: 20.0 - i as f64,
                cqi: 12,
            });
        }
        let current = health.predicted_sinr(0);
        let future = health.predicted_sinr(5);
        assert!(
            future < current,
            "degrading SINR prediction should be lower: current={current}, future={future}"
        );
    }
}
