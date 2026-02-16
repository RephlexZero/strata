//! # BLEST — BLocking ESTimation
//!
//! Head-of-line blocking guard for multi-link scheduling.
//!
//! Before IoDS assigns a packet to a slower link, BLEST checks whether that
//! assignment would cause Head-of-Line blocking at the receiver:
//!
//! ```text
//! block_time = slow_link_OWD - fast_link_OWD
//! ```
//!
//! If `block_time > threshold`, the slow link is skipped even if it has
//! available bandwidth. A dynamic penalty factor δ scales with recent
//! blocking events.

use std::collections::HashMap;

/// BLEST guard configuration.
#[derive(Debug, Clone)]
pub struct BlestConfig {
    /// Maximum acceptable blocking time in seconds.
    pub blocking_threshold_secs: f64,
    /// Penalty decay factor per decision round (0.0-1.0).
    pub penalty_decay: f64,
    /// Maximum penalty factor (multiplicative).
    pub max_penalty: f64,
}

impl Default for BlestConfig {
    fn default() -> Self {
        BlestConfig {
            blocking_threshold_secs: 0.050, // 50ms
            penalty_decay: 0.95,
            max_penalty: 5.0,
        }
    }
}

/// Per-link BLEST state.
#[derive(Debug, Clone)]
struct LinkBlestState {
    /// One-way delay estimate in seconds.
    owd_secs: f64,
    /// Dynamic penalty factor (1.0 = no penalty).
    penalty: f64,
    /// Number of recent blocking events.
    block_events: u32,
}

/// BLEST head-of-line blocking guard.
pub struct BlestGuard {
    config: BlestConfig,
    links: HashMap<usize, LinkBlestState>,
}

impl BlestGuard {
    pub fn new(config: BlestConfig) -> Self {
        BlestGuard {
            config,
            links: HashMap::new(),
        }
    }

    /// Register or update a link's OWD estimate.
    pub fn update_link_owd(&mut self, link_id: usize, owd_secs: f64) {
        let entry = self.links.entry(link_id).or_insert(LinkBlestState {
            owd_secs: 0.0,
            penalty: 1.0,
            block_events: 0,
        });
        entry.owd_secs = owd_secs;
    }

    /// Remove a link from tracking.
    pub fn remove_link(&mut self, link_id: usize) {
        self.links.remove(&link_id);
    }

    /// Check if assigning to `candidate_link_id` would cause blocking
    /// relative to the fastest available link.
    ///
    /// Returns `true` if the assignment is acceptable (no blocking),
    /// `false` if it would cause unacceptable blocking.
    pub fn allows_assignment(&mut self, candidate_link_id: usize) -> bool {
        let candidate_owd = match self.links.get(&candidate_link_id) {
            Some(s) => s.owd_secs * s.penalty,
            None => return true, // Unknown link = allow
        };

        // Find minimum OWD across all links (the "fastest" link)
        let min_owd = self
            .links
            .values()
            .map(|s| s.owd_secs)
            .fold(f64::MAX, f64::min);

        if min_owd == f64::MAX {
            return true; // No links registered
        }

        let block_time = candidate_owd - min_owd;

        if block_time > self.config.blocking_threshold_secs {
            // Record blocking event and increase penalty
            if let Some(state) = self.links.get_mut(&candidate_link_id) {
                state.block_events += 1;
                state.penalty = (state.penalty * 1.2).min(self.config.max_penalty);
            }
            false
        } else {
            true
        }
    }

    /// Decay penalties for all links. Call periodically (e.g., every 100ms).
    pub fn decay_penalties(&mut self) {
        let decay = self.config.penalty_decay;
        for state in self.links.values_mut() {
            state.penalty = 1.0 + (state.penalty - 1.0) * decay;
            if state.penalty < 1.001 {
                state.penalty = 1.0;
            }
        }
    }

    /// Get the current penalty factor for a link.
    pub fn penalty(&self, link_id: usize) -> f64 {
        self.links.get(&link_id).map(|s| s.penalty).unwrap_or(1.0)
    }

    /// Get the number of blocking events for a link.
    pub fn block_events(&self, link_id: usize) -> u32 {
        self.links
            .get(&link_id)
            .map(|s| s.block_events)
            .unwrap_or(0)
    }
}

impl Default for BlestGuard {
    fn default() -> Self {
        Self::new(BlestConfig::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── Basic Blocking Detection ───────────────────────────────────────

    #[test]
    fn allows_when_owd_within_threshold() {
        let mut guard = BlestGuard::default();
        guard.update_link_owd(0, 0.010); // 10ms
        guard.update_link_owd(1, 0.020); // 20ms

        // Difference is 10ms, threshold is 50ms → allowed
        assert!(guard.allows_assignment(1));
    }

    #[test]
    fn blocks_when_owd_exceeds_threshold() {
        let mut guard = BlestGuard::default();
        guard.update_link_owd(0, 0.010); // 10ms
        guard.update_link_owd(1, 0.200); // 200ms

        // Difference = 190ms > 50ms threshold → blocked
        assert!(!guard.allows_assignment(1));
    }

    #[test]
    fn fastest_link_always_allowed() {
        let mut guard = BlestGuard::default();
        guard.update_link_owd(0, 0.010); // Fastest
        guard.update_link_owd(1, 0.200);

        assert!(guard.allows_assignment(0)); // Fastest = 0ms block time
    }

    // ─── Penalty Accumulation ───────────────────────────────────────────

    #[test]
    fn blocking_increases_penalty() {
        let mut guard = BlestGuard::default();
        guard.update_link_owd(0, 0.010);
        guard.update_link_owd(1, 0.200);

        assert_eq!(guard.penalty(1), 1.0);

        // Trigger blocking
        guard.allows_assignment(1);
        assert!(
            guard.penalty(1) > 1.0,
            "penalty should increase after blocking"
        );
        assert_eq!(guard.block_events(1), 1);
    }

    #[test]
    fn repeated_blocking_accumulates_penalty() {
        let mut guard = BlestGuard::default();
        guard.update_link_owd(0, 0.010);
        guard.update_link_owd(1, 0.200);

        for _ in 0..5 {
            guard.allows_assignment(1);
        }

        assert!(guard.penalty(1) > 1.5, "penalty should accumulate");
        assert_eq!(guard.block_events(1), 5);
    }

    #[test]
    fn penalty_capped_at_max() {
        let mut guard = BlestGuard::new(BlestConfig {
            max_penalty: 2.0,
            ..Default::default()
        });
        guard.update_link_owd(0, 0.010);
        guard.update_link_owd(1, 0.200);

        for _ in 0..100 {
            guard.allows_assignment(1);
        }

        assert!(guard.penalty(1) <= 2.0 + f64::EPSILON);
    }

    // ─── Penalty Decay ──────────────────────────────────────────────────

    #[test]
    fn penalty_decays_toward_one() {
        let mut guard = BlestGuard::default();
        guard.update_link_owd(0, 0.010);
        guard.update_link_owd(1, 0.200);

        // Build up penalty
        for _ in 0..5 {
            guard.allows_assignment(1);
        }
        let peak_penalty = guard.penalty(1);

        // Decay
        for _ in 0..100 {
            guard.decay_penalties();
        }

        assert!(guard.penalty(1) < peak_penalty);
        assert!(
            (guard.penalty(1) - 1.0).abs() < 0.01,
            "penalty should decay back to ~1.0"
        );
    }

    // ─── Unknown Link ───────────────────────────────────────────────────

    #[test]
    fn unknown_link_allowed() {
        let mut guard = BlestGuard::default();
        guard.update_link_owd(0, 0.010);

        assert!(guard.allows_assignment(99)); // Link 99 not registered
    }

    // ─── Link Removal ───────────────────────────────────────────────────

    #[test]
    fn removed_link_no_longer_tracked() {
        let mut guard = BlestGuard::default();
        guard.update_link_owd(0, 0.010);
        guard.update_link_owd(1, 0.200);

        guard.remove_link(1);
        assert_eq!(guard.penalty(1), 1.0); // Default for missing link
    }

    // ─── Custom Threshold ───────────────────────────────────────────────

    #[test]
    fn custom_threshold() {
        // Very tight threshold — 5ms
        let mut guard = BlestGuard::new(BlestConfig {
            blocking_threshold_secs: 0.005,
            ..Default::default()
        });
        guard.update_link_owd(0, 0.010);
        guard.update_link_owd(1, 0.020);

        // 10ms difference > 5ms threshold → blocked
        assert!(!guard.allows_assignment(1));
    }
}
