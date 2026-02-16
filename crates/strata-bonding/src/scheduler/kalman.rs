//! # Kalman Filter — Link Quality Estimation
//!
//! Two-state Kalman filter for smoothing noisy RTT and capacity
//! measurements per link.  State vector: `[value, velocity]`.
//!
//! The velocity component enables prediction during measurement gaps
//! and detects trends (e.g., degrading link before loss spikes).

/// A two-state Kalman filter: [value, velocity].
#[derive(Debug, Clone)]
pub struct KalmanFilter {
    // ─── State ───
    /// Estimated value.
    x: f64,
    /// Estimated velocity (change per tick).
    v: f64,

    // ─── Covariance P (2×2 symmetric) ───
    p00: f64,
    p01: f64,
    p11: f64,

    // ─── Tuning ───
    /// Process noise for value.
    q_value: f64,
    /// Process noise for velocity.
    q_velocity: f64,
    /// Measurement noise variance.
    r: f64,

    /// Whether we've received at least one measurement.
    initialized: bool,
}

/// Configuration for a Kalman filter instance.
#[derive(Debug, Clone)]
pub struct KalmanConfig {
    /// Process noise for the value state. Higher = more reactive to changes.
    pub q_value: f64,
    /// Process noise for the velocity state.
    pub q_velocity: f64,
    /// Measurement noise variance. Higher = smoother output, more lag.
    pub r: f64,
}

impl KalmanConfig {
    /// Suitable for RTT smoothing (values in milliseconds).
    pub fn for_rtt() -> Self {
        KalmanConfig {
            q_value: 0.5,
            q_velocity: 0.1,
            r: 10.0,
        }
    }

    /// Suitable for capacity/throughput estimation (values in kbps).
    pub fn for_capacity() -> Self {
        KalmanConfig {
            q_value: 50.0,
            q_velocity: 5.0,
            r: 500.0,
        }
    }

    /// Suitable for RSRP/signal strength (values in dBm).
    pub fn for_signal() -> Self {
        KalmanConfig {
            q_value: 1.0,
            q_velocity: 0.2,
            r: 5.0,
        }
    }
}

impl KalmanFilter {
    pub fn new(config: &KalmanConfig) -> Self {
        KalmanFilter {
            x: 0.0,
            v: 0.0,
            p00: 1000.0, // Large initial uncertainty
            p01: 0.0,
            p11: 1000.0,
            q_value: config.q_value,
            q_velocity: config.q_velocity,
            r: config.r,
            initialized: false,
        }
    }

    /// Current estimated value.
    pub fn value(&self) -> f64 {
        self.x
    }

    /// Current estimated velocity (change per tick).
    pub fn velocity(&self) -> f64 {
        self.v
    }

    /// Whether a trend is positive (increasing).
    pub fn is_increasing(&self) -> bool {
        self.v > 0.0
    }

    /// Uncertainty in the current estimate (sqrt of variance).
    pub fn uncertainty(&self) -> f64 {
        self.p00.sqrt()
    }

    /// Has received at least one measurement.
    pub fn is_initialized(&self) -> bool {
        self.initialized
    }

    /// Predict step: advance state by one time step (dt = 1).
    pub fn predict(&mut self) {
        // State prediction: x' = x + v, v' = v
        self.x += self.v;

        // Covariance prediction: P' = F*P*F' + Q
        // F = [[1, 1], [0, 1]]
        let new_p00 = self.p00 + 2.0 * self.p01 + self.p11 + self.q_value;
        let new_p01 = self.p01 + self.p11 + self.q_velocity;
        let new_p11 = self.p11 + self.q_velocity;

        self.p00 = new_p00;
        self.p01 = new_p01;
        self.p11 = new_p11;
    }

    /// Update step: incorporate a new measurement.
    pub fn update(&mut self, measurement: f64) {
        if !self.initialized {
            // First measurement: initialize directly
            self.x = measurement;
            self.v = 0.0;
            self.initialized = true;
            return;
        }

        // Predict first
        self.predict();

        // Innovation
        let y = measurement - self.x;

        // Innovation covariance: S = H*P*H' + R = P[0,0] + R
        let s = self.p00 + self.r;

        // Kalman gain: K = P*H'/S
        let k0 = self.p00 / s;
        let k1 = self.p01 / s;

        // State update
        self.x += k0 * y;
        self.v += k1 * y;

        // Covariance update: P = (I - K*H)*P
        let new_p00 = self.p00 - k0 * self.p00;
        let new_p01 = self.p01 - k0 * self.p01;
        let new_p11 = self.p11 - k1 * self.p01;

        self.p00 = new_p00;
        self.p01 = new_p01;
        self.p11 = new_p11;
    }

    /// Predict value N steps ahead without modifying state.
    pub fn predict_ahead(&self, steps: u32) -> f64 {
        self.x + self.v * steps as f64
    }

    /// Reset to uninitialized state.
    pub fn reset(&mut self) {
        self.x = 0.0;
        self.v = 0.0;
        self.p00 = 1000.0;
        self.p01 = 0.0;
        self.p11 = 1000.0;
        self.initialized = false;
    }
}

/// Per-link quality estimator using multiple Kalman filters.
pub struct LinkQualityEstimator {
    pub rtt: KalmanFilter,
    pub capacity: KalmanFilter,
    pub signal: KalmanFilter,
}

impl LinkQualityEstimator {
    pub fn new() -> Self {
        LinkQualityEstimator {
            rtt: KalmanFilter::new(&KalmanConfig::for_rtt()),
            capacity: KalmanFilter::new(&KalmanConfig::for_capacity()),
            signal: KalmanFilter::new(&KalmanConfig::for_signal()),
        }
    }

    /// Composite quality score [0, 1]. Higher = better link.
    ///
    /// Weights: RTT (40%), capacity (40%), signal trend (20%).
    pub fn quality_score(&self) -> f64 {
        if !self.rtt.is_initialized() {
            return 0.5; // No data yet
        }

        // RTT score: lower is better, normalize roughly to [0,1]
        // 10ms → 1.0, 200ms → 0.0
        let rtt_score = (1.0 - (self.rtt.value() - 10.0) / 190.0).clamp(0.0, 1.0);

        // Capacity score: normalize to [0,1] with 50 Mbps as "excellent"
        let cap_score = if self.capacity.is_initialized() {
            (self.capacity.value() / 50000.0).clamp(0.0, 1.0) // kbps
        } else {
            0.5
        };

        // Signal trend: penalize degrading signal
        let signal_penalty = if self.signal.is_initialized() && self.signal.velocity() < -1.0 {
            0.1 // 10% penalty for rapidly degrading signal
        } else {
            0.0
        };

        0.4 * rtt_score + 0.4 * cap_score + 0.2 * (1.0 - signal_penalty)
    }
}

impl Default for LinkQualityEstimator {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── Basic Filtering ────────────────────────────────────────────────

    #[test]
    fn first_measurement_sets_value() {
        let mut kf = KalmanFilter::new(&KalmanConfig::for_rtt());
        assert!(!kf.is_initialized());

        kf.update(50.0);
        assert!(kf.is_initialized());
        assert!((kf.value() - 50.0).abs() < 0.01);
    }

    #[test]
    fn smooths_noisy_measurements() {
        let mut kf = KalmanFilter::new(&KalmanConfig::for_rtt());

        // True RTT = 50ms, noisy measurements
        let measurements = [55.0, 48.0, 52.0, 47.0, 53.0, 49.0, 51.0, 50.0, 48.0, 52.0];
        for &m in &measurements {
            kf.update(m);
        }

        // Should converge near 50ms
        assert!(
            (kf.value() - 50.0).abs() < 5.0,
            "expected ~50ms, got {}",
            kf.value()
        );
    }

    #[test]
    fn detects_increasing_trend() {
        let mut kf = KalmanFilter::new(&KalmanConfig::for_rtt());

        // Linearly increasing RTT (link degrading)
        for i in 0..20 {
            kf.update(50.0 + i as f64 * 2.0);
        }

        assert!(kf.is_increasing(), "should detect increasing RTT trend");
        assert!(
            kf.velocity() > 0.5,
            "velocity should be positive: {}",
            kf.velocity()
        );
    }

    #[test]
    fn detects_decreasing_trend() {
        let mut kf = KalmanFilter::new(&KalmanConfig::for_rtt());

        // Linearly decreasing RTT (link improving)
        for i in 0..20 {
            kf.update(100.0 - i as f64 * 2.0);
        }

        assert!(!kf.is_increasing(), "should detect decreasing RTT trend");
        assert!(
            kf.velocity() < -0.5,
            "velocity should be negative: {}",
            kf.velocity()
        );
    }

    // ─── Prediction ─────────────────────────────────────────────────────

    #[test]
    fn predict_ahead_uses_velocity() {
        let mut kf = KalmanFilter::new(&KalmanConfig::for_rtt());

        // Feed a linear trend
        for i in 0..20 {
            kf.update(10.0 + i as f64 * 5.0);
        }

        let current = kf.value();
        let predicted = kf.predict_ahead(5);

        // Since RTT is increasing, prediction should be above current
        assert!(
            predicted > current,
            "predicted ({predicted}) should exceed current ({current})"
        );
    }

    // ─── Reset ──────────────────────────────────────────────────────────

    #[test]
    fn reset_clears_state() {
        let mut kf = KalmanFilter::new(&KalmanConfig::for_rtt());
        kf.update(50.0);
        kf.update(55.0);

        kf.reset();
        assert!(!kf.is_initialized());
        assert!((kf.value() - 0.0).abs() < 0.01);
    }

    // ─── Config Presets ─────────────────────────────────────────────────

    #[test]
    fn capacity_filter_smooths_throughput() {
        let mut kf = KalmanFilter::new(&KalmanConfig::for_capacity());

        // 10 Mbps with jitter
        let measurements = [10000.0, 9500.0, 10500.0, 9800.0, 10200.0];
        for &m in &measurements {
            kf.update(m);
        }

        assert!(
            (kf.value() - 10000.0).abs() < 2000.0,
            "should be near 10000 kbps, got {}",
            kf.value()
        );
    }

    #[test]
    fn signal_filter_tracks_rsrp() {
        let mut kf = KalmanFilter::new(&KalmanConfig::for_signal());

        // -80 dBm with noise
        let measurements = [-78.0, -82.0, -79.0, -81.0, -80.0];
        for &m in &measurements {
            kf.update(m);
        }

        assert!(
            (kf.value() - (-80.0)).abs() < 5.0,
            "should be near -80 dBm, got {}",
            kf.value()
        );
    }

    // ─── Link Quality Estimator ─────────────────────────────────────────

    #[test]
    fn quality_score_no_data_returns_default() {
        let est = LinkQualityEstimator::new();
        assert!((est.quality_score() - 0.5).abs() < 0.01);
    }

    #[test]
    fn quality_score_good_link() {
        let mut est = LinkQualityEstimator::new();

        // Good link: 15ms RTT, 20 Mbps, -70 dBm signal
        for _ in 0..10 {
            est.rtt.update(15.0);
            est.capacity.update(20000.0);
            est.signal.update(-70.0);
        }

        let score = est.quality_score();
        assert!(
            score > 0.6,
            "good link should have high quality score, got {score}"
        );
    }

    #[test]
    fn quality_penalizes_degrading_signal() {
        let mut est = LinkQualityEstimator::new();

        // Start with good signal, then degrade rapidly
        for _ in 0..5 {
            est.rtt.update(20.0);
            est.capacity.update(20000.0);
            est.signal.update(-70.0);
        }
        let score_before = est.quality_score();

        for i in 0..10 {
            est.signal.update(-70.0 - i as f64 * 5.0); // Rapid degradation
        }
        let score_after = est.quality_score();

        assert!(
            score_after < score_before,
            "degrading signal should lower quality: before={score_before}, after={score_after}"
        );
    }

    // ─── Uncertainty ────────────────────────────────────────────────────

    #[test]
    fn uncertainty_decreases_with_measurements() {
        let mut kf = KalmanFilter::new(&KalmanConfig::for_rtt());
        kf.update(50.0);
        let unc_1 = kf.uncertainty();

        for _ in 0..10 {
            kf.update(50.0);
        }
        let unc_10 = kf.uncertainty();

        assert!(
            unc_10 < unc_1,
            "uncertainty should decrease: after 1={unc_1}, after 10={unc_10}"
        );
    }
}
