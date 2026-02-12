//! Shared Bottleneck Detection (RFC 8382).
//!
//! Identifies when multiple links share the same physical bottleneck by
//! computing per-link OWD statistics (skewness, variability, oscillation
//! frequency, packet loss) and grouping links with correlated behaviour.
//!
//! When two or more links share a bottleneck the coupled AI factor
//! (`coupled_alpha` in the NADA controller) should be applied to avoid
//! over-aggressive aggregate probing on the shared segment.
//!
//! # Algorithm (RFC 8382 §3, 5-step summary)
//!
//! 1. **Sample**: Collect N OWD samples per link over T-second base intervals.
//! 2. **Statistics**: Compute per-link skew_est (skewness), var_est (MAD-based
//!    variability), freq_est (oscillation), and pkt_loss.
//! 3. **Summarise**: Aggregate base-interval statistics across M intervals.
//! 4. **Threshold**: Classify each link as "bottlenecked" if skew/var exceed
//!    configurable thresholds.
//! 5. **Group**: Cluster bottlenecked links with similar delay distributions.

use std::collections::{HashMap, VecDeque};

/// Per-link statistics tracker for SBD.
#[derive(Debug, Clone)]
pub struct LinkSbdState {
    /// Ring buffer of recent OWD samples (ms).
    delay_samples: VecDeque<f64>,
    /// Number of packets received in current base interval.
    pkt_count: u64,
    /// Number of packets lost in current base interval.
    pkt_lost: u64,
    /// Previous base-interval mean delay (for oscillation detection).
    prev_mean: f64,
    /// Accumulated sign-change counter (for freq_est).
    sign_changes: u32,
    /// History of base-interval skew_est values (for M-interval summary).
    skew_history: VecDeque<f64>,
    /// History of base-interval var_est values.
    var_history: VecDeque<f64>,
    /// History of base-interval freq_est values.
    freq_history: VecDeque<f64>,
    /// History of base-interval loss rate values.
    loss_history: VecDeque<f64>,
}

impl Default for LinkSbdState {
    fn default() -> Self {
        Self::new()
    }
}

impl LinkSbdState {
    pub fn new() -> Self {
        Self {
            delay_samples: VecDeque::with_capacity(64),
            pkt_count: 0,
            pkt_lost: 0,
            prev_mean: 0.0,
            sign_changes: 0,
            skew_history: VecDeque::with_capacity(16),
            var_history: VecDeque::with_capacity(16),
            freq_history: VecDeque::with_capacity(16),
            loss_history: VecDeque::with_capacity(16),
        }
    }

    /// Record a new delay sample (OWD in ms).
    pub fn record_delay(&mut self, delay_ms: f64) {
        self.delay_samples.push_back(delay_ms);
        self.pkt_count += 1;
    }

    /// Record a packet loss event.
    pub fn record_loss(&mut self) {
        self.pkt_lost += 1;
    }
}

/// SBD engine managing per-link state and bottleneck grouping.
#[derive(Debug)]
pub struct SbdEngine {
    /// Per-link SBD statistics.
    link_states: HashMap<usize, LinkSbdState>,
    /// Number of samples per base interval (config: sbd_n).
    n: usize,
    /// Skewness threshold for bottleneck classification (config: sbd_c_s).
    c_s: f64,
    /// Hysteresis threshold (config: sbd_c_h).
    c_h: f64,
    /// Loss threshold (config: sbd_p_l).
    p_l: f64,
    /// Number of base intervals for summary (M = 3 default).
    m: usize,
}

impl SbdEngine {
    /// Create a new SBD engine with the given configuration parameters.
    pub fn new(n: usize, c_s: f64, c_h: f64, p_l: f64) -> Self {
        Self {
            link_states: HashMap::new(),
            n: n.max(5),
            c_s,
            c_h,
            p_l,
            m: 3,
        }
    }

    /// Register a link for SBD tracking.
    pub fn add_link(&mut self, link_id: usize) {
        self.link_states.entry(link_id).or_default();
    }

    /// Remove a link from SBD tracking.
    pub fn remove_link(&mut self, link_id: usize) {
        self.link_states.remove(&link_id);
    }

    /// Record a delay sample for a link.
    pub fn record_delay(&mut self, link_id: usize, delay_ms: f64) {
        if let Some(state) = self.link_states.get_mut(&link_id) {
            state.record_delay(delay_ms);
        }
    }

    /// Record a loss event for a link.
    pub fn record_loss(&mut self, link_id: usize) {
        if let Some(state) = self.link_states.get_mut(&link_id) {
            state.record_loss();
        }
    }

    /// Process base-interval statistics for all links.
    ///
    /// Should be called every `sbd_interval_ms`.  Computes per-link
    /// skew_est, var_est, freq_est, and loss rate for the most recent
    /// base interval, then appends to the M-interval history.
    pub fn process_interval(&mut self) {
        let n = self.n;
        for state in self.link_states.values_mut() {
            // Only process if we have enough samples
            if state.delay_samples.len() < 2 {
                // Not enough data — push zeros to keep history aligned
                push_bounded(&mut state.skew_history, 0.0, 16);
                push_bounded(&mut state.var_history, 0.0, 16);
                push_bounded(&mut state.freq_history, 0.0, 16);
                let loss_rate = if state.pkt_count > 0 {
                    state.pkt_lost as f64 / (state.pkt_count + state.pkt_lost) as f64
                } else {
                    0.0
                };
                push_bounded(&mut state.loss_history, loss_rate, 16);
                state.delay_samples.clear();
                state.pkt_count = 0;
                state.pkt_lost = 0;
                continue;
            }

            // Trim to most recent N samples
            while state.delay_samples.len() > n {
                state.delay_samples.pop_front();
            }

            let samples: Vec<f64> = state.delay_samples.iter().copied().collect();
            let count = samples.len() as f64;

            // --- Step 2a: Compute mean ---
            let mean = samples.iter().sum::<f64>() / count;

            // --- Step 2b: skew_est (skewness via mean vs median comparison) ---
            let mut sorted = samples.clone();
            sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let median = if sorted.len().is_multiple_of(2) {
                (sorted[sorted.len() / 2 - 1] + sorted[sorted.len() / 2]) / 2.0
            } else {
                sorted[sorted.len() / 2]
            };
            let skew_est = mean - median;

            // --- Step 2c: var_est (MAD — Median Absolute Deviation) ---
            let mut abs_devs: Vec<f64> = samples.iter().map(|x| (x - median).abs()).collect();
            abs_devs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let var_est = if abs_devs.len().is_multiple_of(2) {
                (abs_devs[abs_devs.len() / 2 - 1] + abs_devs[abs_devs.len() / 2]) / 2.0
            } else {
                abs_devs[abs_devs.len() / 2]
            };

            // --- Step 2d: freq_est (oscillation frequency) ---
            // Count sign changes in the deviation from mean
            let mut sign_changes = 0u32;
            let mut prev_sign = 0i32;
            for &s in &samples {
                let sign = if s > mean {
                    1
                } else if s < mean {
                    -1
                } else {
                    0
                };
                if sign != 0 && prev_sign != 0 && sign != prev_sign {
                    sign_changes += 1;
                }
                if sign != 0 {
                    prev_sign = sign;
                }
            }
            let freq_est = sign_changes as f64 / count;
            state.sign_changes = sign_changes;

            // --- Loss rate for this interval ---
            let total_pkts = state.pkt_count + state.pkt_lost;
            let loss_rate = if total_pkts > 0 {
                state.pkt_lost as f64 / total_pkts as f64
            } else {
                0.0
            };

            // Normalize skew_est and var_est by the mean for scale-independence
            let norm_skew = if mean.abs() > 1e-9 {
                skew_est / mean
            } else {
                0.0
            };
            let norm_var = if mean.abs() > 1e-9 {
                var_est / mean
            } else {
                0.0
            };

            push_bounded(&mut state.skew_history, norm_skew, 16);
            push_bounded(&mut state.var_history, norm_var, 16);
            push_bounded(&mut state.freq_history, freq_est, 16);
            push_bounded(&mut state.loss_history, loss_rate, 16);

            state.prev_mean = mean;
            state.delay_samples.clear();
            state.pkt_count = 0;
            state.pkt_lost = 0;
        }
    }

    /// Run the 5-step grouping algorithm (RFC 8382 §3).
    ///
    /// Returns a map from link_id → group_id. Links in the same group
    /// are believed to share a bottleneck. Group 0 = no bottleneck detected.
    pub fn compute_groups(&self) -> HashMap<usize, usize> {
        let m = self.m;
        let mut groups: HashMap<usize, usize> = HashMap::new();
        let mut bottlenecked: Vec<(usize, f64, f64)> = Vec::new(); // (link_id, avg_skew, avg_var)

        for (&link_id, state) in &self.link_states {
            // Need at least M base-interval summaries
            if state.skew_history.len() < m {
                groups.insert(link_id, 0);
                continue;
            }

            // --- Step 3: Summarise across M intervals ---
            let recent_skew: Vec<f64> = state.skew_history.iter().rev().take(m).copied().collect();
            let recent_var: Vec<f64> = state.var_history.iter().rev().take(m).copied().collect();
            let recent_loss: Vec<f64> = state.loss_history.iter().rev().take(m).copied().collect();

            let avg_skew = recent_skew.iter().sum::<f64>() / m as f64;
            let avg_var = recent_var.iter().sum::<f64>() / m as f64;
            let avg_loss = recent_loss.iter().sum::<f64>() / m as f64;

            // --- Step 4: Threshold classification ---
            // A link is considered bottlenecked if:
            //   skew_est > C_S  AND  (var_est > C_H  OR  loss > P_L)
            let is_bottlenecked =
                avg_skew > self.c_s && (avg_var > self.c_h || avg_loss > self.p_l);

            if is_bottlenecked {
                bottlenecked.push((link_id, avg_skew, avg_var));
            } else {
                groups.insert(link_id, 0);
            }
        }

        // --- Step 5: Group bottlenecked links ---
        // Simple clustering: links with similar skew/var profiles share a group.
        // We use a greedy nearest-neighbour approach with a tolerance of 2*C_H.
        let mut next_group = 1usize;
        let tolerance = 2.0 * self.c_h.abs().max(0.05);

        for (link_id, skew, var) in &bottlenecked {
            let mut assigned = false;
            // Check if this link can join an existing group
            for (other_id, other_skew, other_var) in &bottlenecked {
                if *other_id == *link_id {
                    continue;
                }
                if let Some(&other_group) = groups.get(other_id) {
                    if other_group > 0 {
                        // Check similarity
                        let skew_diff = (skew - other_skew).abs();
                        let var_diff = (var - other_var).abs();
                        if skew_diff < tolerance && var_diff < tolerance {
                            groups.insert(*link_id, other_group);
                            assigned = true;
                            break;
                        }
                    }
                }
            }
            if !assigned {
                groups.insert(*link_id, next_group);
                next_group += 1;
            }
        }

        groups
    }
}

/// Push a value into a bounded VecDeque, evicting the oldest if at capacity.
fn push_bounded(deque: &mut VecDeque<f64>, value: f64, max_len: usize) {
    deque.push_back(value);
    while deque.len() > max_len {
        deque.pop_front();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sbd_no_data_returns_group_zero() {
        let engine = SbdEngine::new(50, 0.1, 0.3, 0.1);
        let groups = engine.compute_groups();
        assert!(groups.is_empty());
    }

    #[test]
    fn sbd_single_link_not_bottlenecked() {
        let mut engine = SbdEngine::new(10, 0.1, 0.3, 0.1);
        engine.add_link(1);

        // Feed uniform delay — no skewness
        for _interval in 0..5 {
            for i in 0..10 {
                engine.record_delay(1, 10.0 + (i as f64) * 0.01);
            }
            engine.process_interval();
        }

        let groups = engine.compute_groups();
        assert_eq!(
            groups.get(&1),
            Some(&0),
            "Uniform delay should not trigger bottleneck"
        );
    }

    #[test]
    fn sbd_skewed_delay_triggers_bottleneck() {
        // Use thresholds that match our test distribution.
        // Distribution: 7×5ms + 3×50ms → mean=18.5, median=5, norm_skew≈0.73
        // MAD is 0 (7 identical values), so we rely on loss: p_l=0.05
        // Loss: 1 loss per 11 packets ≈ 9.1% > 5%.
        let mut engine = SbdEngine::new(10, 0.05, 0.01, 0.05);
        engine.add_link(1);

        // Feed heavily right-skewed delay (most samples low, a few very high)
        for _ in 0..5 {
            for _ in 0..7 {
                engine.record_delay(1, 5.0);
            }
            for _ in 0..3 {
                engine.record_delay(1, 50.0);
            }
            // Also report loss to satisfy the (var > c_h OR loss > p_l) condition
            // when var_est / mean is below c_h. Loss ensures at least one trigger path.
            engine.record_loss(1);
            engine.process_interval();
        }

        let groups = engine.compute_groups();
        let group = groups.get(&1).copied().unwrap_or(0);
        // With right-skewed data, mean > median → skew_est > 0 → bottleneck
        assert!(
            group > 0,
            "Skewed delay distribution should trigger bottleneck detection, got group {}",
            group
        );
    }

    #[test]
    fn sbd_two_links_same_bottleneck() {
        let mut engine = SbdEngine::new(10, 0.05, 0.01, 0.05);
        engine.add_link(1);
        engine.add_link(2);

        // Feed similar skewed distributions to both links, with loss
        for _ in 0..5 {
            for _ in 0..7 {
                engine.record_delay(1, 5.0);
                engine.record_delay(2, 5.5);
            }
            for _ in 0..3 {
                engine.record_delay(1, 50.0);
                engine.record_delay(2, 52.0);
            }
            engine.record_loss(1);
            engine.record_loss(2);
            engine.process_interval();
        }

        let groups = engine.compute_groups();
        let g1 = groups.get(&1).copied().unwrap_or(0);
        let g2 = groups.get(&2).copied().unwrap_or(0);
        assert!(g1 > 0, "Link 1 should be bottlenecked");
        assert!(g2 > 0, "Link 2 should be bottlenecked");
        assert_eq!(g1, g2, "Links with similar profiles should share a group");
    }

    #[test]
    fn sbd_loss_triggers_bottleneck() {
        let mut engine = SbdEngine::new(10, 0.05, 1.0, 0.05);
        engine.add_link(1);

        // Feed skewed delay with loss
        for _ in 0..5 {
            for _ in 0..7 {
                engine.record_delay(1, 5.0);
            }
            for _ in 0..3 {
                engine.record_delay(1, 50.0);
            }
            // Add losses: 2 out of 12 ≈ 16.7% > p_l=5%
            engine.record_loss(1);
            engine.record_loss(1);
            engine.process_interval();
        }

        let groups = engine.compute_groups();
        let g = groups.get(&1).copied().unwrap_or(0);
        assert!(g > 0, "High loss should contribute to bottleneck detection");
    }

    #[test]
    fn sbd_add_remove_link() {
        let mut engine = SbdEngine::new(50, 0.1, 0.3, 0.1);
        engine.add_link(1);
        engine.add_link(2);
        assert_eq!(engine.link_states.len(), 2);
        engine.remove_link(1);
        assert_eq!(engine.link_states.len(), 1);
        assert!(!engine.link_states.contains_key(&1));
    }

    #[test]
    fn push_bounded_evicts_oldest() {
        let mut d = VecDeque::new();
        for i in 0..20 {
            push_bounded(&mut d, i as f64, 5);
        }
        assert_eq!(d.len(), 5);
        assert_eq!(d[0], 15.0);
        assert_eq!(d[4], 19.0);
    }
}
