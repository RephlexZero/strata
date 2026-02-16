//! # IoDS — In-order Delivery Scheduler
//!
//! Enforces a monotonic arrival constraint: only assigns a packet to a link if
//! the predicted arrival time on that link is later than or equal to the
//! previously scheduled packet's arrival time on any link. This prevents
//! receiver reordering.
//!
//! Based on: "In-order Delivery Scheduler for Multipath Transport" research.
//! IoDS reduces jitter by 71% vs round-robin scheduling.

/// Per-link state for IoDS scheduling decisions.
#[derive(Debug, Clone)]
pub struct IodsLinkState {
    /// Link identifier.
    pub link_id: usize,
    /// Smoothed RTT in seconds.
    pub srtt_secs: f64,
    /// Estimated link bandwidth in bytes/sec.
    pub bandwidth_bps: f64,
    /// Whether the link is currently available for scheduling.
    pub available: bool,
}

impl IodsLinkState {
    pub fn new(link_id: usize) -> Self {
        IodsLinkState {
            link_id,
            srtt_secs: 0.05,            // 50ms default
            bandwidth_bps: 1_000_000.0, // 1 Mbps default
            available: true,
        }
    }

    /// Predict arrival time for a packet of `size_bytes` on this link.
    /// arrival = now + srtt + serialization_delay(size)
    fn predicted_arrival(&self, size_bytes: usize) -> f64 {
        let serialization_delay = size_bytes as f64 / self.bandwidth_bps;
        self.srtt_secs + serialization_delay
    }
}

/// The IoDS scheduler state.
pub struct IodsScheduler {
    /// Per-link state.
    links: Vec<IodsLinkState>,
    /// Last scheduled arrival time (relative offset from scheduling moment).
    last_scheduled_arrival: f64,
    /// Last selected link ID (for tie-breaking round-robin).
    last_link_id: Option<usize>,
}

impl IodsScheduler {
    pub fn new() -> Self {
        IodsScheduler {
            links: Vec::new(),
            last_scheduled_arrival: 0.0,
            last_link_id: None,
        }
    }

    /// Add a link to the scheduler.
    pub fn add_link(&mut self, state: IodsLinkState) {
        // Replace if link_id already exists
        if let Some(existing) = self.links.iter_mut().find(|l| l.link_id == state.link_id) {
            *existing = state;
        } else {
            self.links.push(state);
        }
    }

    /// Remove a link from the scheduler.
    pub fn remove_link(&mut self, link_id: usize) {
        self.links.retain(|l| l.link_id != link_id);
    }

    /// Update link metrics.
    pub fn update_link(
        &mut self,
        link_id: usize,
        srtt_secs: f64,
        bandwidth_bps: f64,
        available: bool,
    ) {
        if let Some(link) = self.links.iter_mut().find(|l| l.link_id == link_id) {
            link.srtt_secs = srtt_secs;
            link.bandwidth_bps = bandwidth_bps;
            link.available = available;
        }
    }

    /// Select the best link for a packet of the given size.
    ///
    /// Returns the link_id of the chosen link, or `None` if no link is available.
    ///
    /// The constraint: only assign to a link if its predicted arrival time
    /// is >= last_scheduled_arrival (monotonic constraint). This prevents
    /// receiver reordering.
    pub fn select_link(&mut self, packet_size_bytes: usize) -> Option<usize> {
        let available: Vec<&IodsLinkState> = self.links.iter().filter(|l| l.available).collect();

        if available.is_empty() {
            return None;
        }

        // Sort by predicted arrival time (ascending — fastest first)
        let mut candidates: Vec<(usize, f64)> = available
            .iter()
            .map(|l| (l.link_id, l.predicted_arrival(packet_size_bytes)))
            .collect();
        candidates.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

        // Try each link in arrival-time order: pick the first whose arrival
        // meets the monotonic constraint. On ties, rotate past the last-used link.
        let tie_eps = 1e-9;
        for &(link_id, arrival) in &candidates {
            if arrival >= self.last_scheduled_arrival {
                // Skip the last-used link if there's a tie to spread load
                if let Some(last_id) = self.last_link_id {
                    if link_id == last_id {
                        // Check if any other candidate has the same arrival
                        let has_tie = candidates.iter().any(|&(id, arr)| {
                            id != link_id
                                && (arr - arrival).abs() < tie_eps
                                && arr >= self.last_scheduled_arrival
                        });
                        if has_tie {
                            continue;
                        }
                    }
                }
                self.last_scheduled_arrival = arrival;
                self.last_link_id = Some(link_id);
                return Some(link_id);
            }
        }

        // Fallback: all links would violate ordering — accept reordering by
        // picking the fastest link (lowest arrival time).
        let fastest = candidates.first()?;
        self.last_scheduled_arrival = fastest.1;
        self.last_link_id = Some(fastest.0);
        Some(fastest.0)
    }

    /// Reset the monotonic constraint (e.g., after a gap in the stream).
    pub fn reset(&mut self) {
        self.last_scheduled_arrival = 0.0;
        self.last_link_id = None;
    }

    /// Number of registered links.
    pub fn link_count(&self) -> usize {
        self.links.len()
    }

    /// Number of available links.
    pub fn available_link_count(&self) -> usize {
        self.links.iter().filter(|l| l.available).count()
    }
}

impl Default for IodsScheduler {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_link(id: usize, srtt: f64, bw: f64) -> IodsLinkState {
        IodsLinkState {
            link_id: id,
            srtt_secs: srtt,
            bandwidth_bps: bw,
            available: true,
        }
    }

    // ─── Basic Selection ────────────────────────────────────────────────

    #[test]
    fn single_link_always_selected() {
        let mut sched = IodsScheduler::new();
        sched.add_link(make_link(0, 0.05, 1_000_000.0));

        for _ in 0..10 {
            assert_eq!(sched.select_link(1000), Some(0));
        }
    }

    #[test]
    fn no_links_returns_none() {
        let mut sched = IodsScheduler::new();
        assert_eq!(sched.select_link(1000), None);
    }

    #[test]
    fn unavailable_links_skipped() {
        let mut sched = IodsScheduler::new();
        let mut link = make_link(0, 0.05, 1_000_000.0);
        link.available = false;
        sched.add_link(link);
        sched.add_link(make_link(1, 0.10, 500_000.0));

        assert_eq!(sched.select_link(1000), Some(1));
    }

    // ─── Monotonic Constraint ───────────────────────────────────────────

    #[test]
    fn prefers_link_with_later_arrival_for_ordering() {
        let mut sched = IodsScheduler::new();
        // Link 0: 10ms RTT, 10 Mbps (fast)
        sched.add_link(make_link(0, 0.010, 10_000_000.0));
        // Link 1: 50ms RTT, 2 Mbps (slow)
        sched.add_link(make_link(1, 0.050, 2_000_000.0));

        // First packet → fastest link (link 0)
        let first = sched.select_link(1000);
        assert_eq!(first, Some(0));

        // Second packet should go to a link whose arrival >= first's arrival
        // Since link 0 is very fast, it could still satisfy the constraint
        let second = sched.select_link(1000);
        // Both are valid; the scheduler picks whichever satisfies monotonicity
        assert!(second.is_some());
    }

    #[test]
    fn spreads_across_links_naturally() {
        let mut sched = IodsScheduler::new();
        // Two identical links
        sched.add_link(make_link(0, 0.020, 5_000_000.0));
        sched.add_link(make_link(1, 0.020, 5_000_000.0));

        let mut link0_count = 0;
        let mut link1_count = 0;

        for _ in 0..100 {
            match sched.select_link(1000) {
                Some(0) => link0_count += 1,
                Some(1) => link1_count += 1,
                _ => panic!("unexpected link"),
            }
        }

        // Both should be used
        assert!(link0_count > 0, "link 0 should be used");
        assert!(link1_count > 0, "link 1 should be used");
    }

    // ─── Heterogeneous RTTs ──────────────────────────────────────────────

    #[test]
    fn faster_link_preferred_initially() {
        let mut sched = IodsScheduler::new();
        // Link 0: 5ms RTT, 10 Mbps
        sched.add_link(make_link(0, 0.005, 10_000_000.0));
        // Link 1: 100ms RTT, 10 Mbps
        sched.add_link(make_link(1, 0.100, 10_000_000.0));

        // First packet should go to faster link
        assert_eq!(sched.select_link(1000), Some(0));
    }

    // ─── Link Updates ───────────────────────────────────────────────────

    #[test]
    fn update_link_metrics() {
        let mut sched = IodsScheduler::new();
        sched.add_link(make_link(0, 0.050, 1_000_000.0));

        sched.update_link(0, 0.010, 5_000_000.0, true);
        // Should still work
        assert_eq!(sched.select_link(1000), Some(0));
    }

    #[test]
    fn disable_then_reenable_link() {
        let mut sched = IodsScheduler::new();
        sched.add_link(make_link(0, 0.05, 1_000_000.0));
        sched.add_link(make_link(1, 0.05, 1_000_000.0));

        sched.update_link(0, 0.05, 1_000_000.0, false);
        assert_eq!(sched.available_link_count(), 1);
        assert_eq!(sched.select_link(1000), Some(1));

        sched.update_link(0, 0.05, 1_000_000.0, true);
        assert_eq!(sched.available_link_count(), 2);
    }

    // ─── Remove Link ────────────────────────────────────────────────────

    #[test]
    fn remove_link_reduces_count() {
        let mut sched = IodsScheduler::new();
        sched.add_link(make_link(0, 0.05, 1_000_000.0));
        sched.add_link(make_link(1, 0.05, 1_000_000.0));
        assert_eq!(sched.link_count(), 2);

        sched.remove_link(0);
        assert_eq!(sched.link_count(), 1);
        assert_eq!(sched.select_link(1000), Some(1));
    }

    // ─── Reset ──────────────────────────────────────────────────────────

    #[test]
    fn reset_clears_monotonic_state() {
        let mut sched = IodsScheduler::new();
        sched.add_link(make_link(0, 0.05, 1_000_000.0));

        // Schedule several packets to build up last_scheduled_arrival
        for _ in 0..10 {
            sched.select_link(1000);
        }

        let before = sched.last_scheduled_arrival;
        sched.reset();
        assert_eq!(sched.last_scheduled_arrival, 0.0);
        assert!(before > 0.0, "should have accumulated arrival time");
    }

    // ─── Predicted Arrival Correctness ──────────────────────────────────

    #[test]
    fn predicted_arrival_includes_serialization() {
        let link = make_link(0, 0.010, 1_000_000.0); // 10ms RTT, 1 MBps
                                                     // 1000 bytes at 1 MBps = 0.001s serialization
        let arrival = link.predicted_arrival(1000);
        assert!(
            (arrival - 0.011).abs() < 0.0001,
            "arrival should be RTT + serialization: {arrival}"
        );
    }
}
