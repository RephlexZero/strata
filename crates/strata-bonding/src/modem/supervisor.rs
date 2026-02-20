//! # Modem Supervisor
//!
//! Manages per-link health tracking and generates adaptation events based
//! on RF/transport metric changes. This is the "modem intelligence daemon"
//! that bridges raw modem telemetry with the bonding scheduler.
//!
//! In production, an external poller (QMI/MBIM/AT command thread) pushes
//! `RfMetrics` and `TransportMetrics` in. The supervisor collates them,
//! runs each link's Kalman-smoothed health estimator, and surfaces
//! actionable events to the scheduler.

use std::collections::HashMap;

use super::health::{
    cqi_to_throughput_kbps, sinr_to_capacity_kbps, LinkHealth, RfMetrics, TransportMetrics,
};

/// Events emitted by the supervisor to drive scheduler decisions.
#[derive(Debug, Clone, PartialEq)]
pub enum SupervisorEvent {
    /// A link's health crossed below the usable threshold.
    LinkDegraded { link_id: usize, score: f64 },
    /// A link recovered above the usable threshold.
    LinkRecovered { link_id: usize, score: f64 },
    /// SINR trend predicts imminent degradation (possible handover).
    HandoverWarning { link_id: usize, predicted_sinr: f64 },
    /// Aggregate capacity changed significantly.
    CapacityChanged {
        total_capacity_kbps: f64,
        alive_links: usize,
    },
}

/// Configuration for the supervisor.
#[derive(Debug, Clone)]
pub struct SupervisorConfig {
    /// Score threshold below which a link is considered degraded.
    pub degraded_threshold: f64,
    /// Score threshold above which a degraded link is considered recovered.
    /// Must be > degraded_threshold to provide hysteresis.
    pub recovery_threshold: f64,
    /// SINR threshold (dB) for handover warning.
    pub handover_sinr_threshold: f64,
    /// Minimum capacity change (ratio) to trigger CapacityChanged event.
    pub capacity_change_ratio: f64,
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        SupervisorConfig {
            degraded_threshold: 40.0,
            recovery_threshold: 55.0,
            handover_sinr_threshold: 3.0,
            capacity_change_ratio: 0.15,
        }
    }
}

/// Per-link state tracked by the supervisor.
struct LinkState {
    health: LinkHealth,
    degraded: bool,
    last_rf: Option<RfMetrics>,
}

impl LinkState {
    fn new() -> Self {
        LinkState {
            health: LinkHealth::new(),
            degraded: false,
            last_rf: None,
        }
    }
}

/// Modem supervisor managing all link health estimators.
pub struct ModemSupervisor {
    config: SupervisorConfig,
    links: HashMap<usize, LinkState>,
    prev_total_capacity_kbps: f64,
}

impl ModemSupervisor {
    pub fn new(config: SupervisorConfig) -> Self {
        ModemSupervisor {
            config,
            links: HashMap::new(),
            prev_total_capacity_kbps: 0.0,
        }
    }

    /// Register a new link to be tracked.
    pub fn register_link(&mut self, link_id: usize) {
        self.links.entry(link_id).or_insert_with(LinkState::new);
    }

    /// Remove a link from tracking.
    pub fn remove_link(&mut self, link_id: usize) {
        self.links.remove(&link_id);
    }

    /// Number of tracked links.
    pub fn link_count(&self) -> usize {
        self.links.len()
    }

    /// Push RF metrics for a specific link. Returns any events triggered.
    pub fn update_rf(&mut self, link_id: usize, metrics: &RfMetrics) -> Vec<SupervisorEvent> {
        let mut events = Vec::new();
        let state = self.links.entry(link_id).or_insert_with(LinkState::new);

        state.health.update_rf(metrics);
        state.last_rf = Some(*metrics);

        // Check degradation/recovery
        self.check_link_status(link_id, &mut events);

        // Check handover warning
        self.check_handover(link_id, &mut events);

        // Check aggregate capacity
        self.check_capacity(&mut events);

        events
    }

    /// Push transport metrics for a specific link. Returns any events triggered.
    pub fn update_transport(
        &mut self,
        link_id: usize,
        metrics: &TransportMetrics,
    ) -> Vec<SupervisorEvent> {
        let mut events = Vec::new();
        let state = self.links.entry(link_id).or_insert_with(LinkState::new);

        state.health.update_transport(metrics);

        // Check degradation/recovery
        self.check_link_status(link_id, &mut events);

        // Check aggregate capacity
        self.check_capacity(&mut events);

        events
    }

    /// Get the health score for a specific link.
    pub fn link_score(&self, link_id: usize) -> Option<f64> {
        self.links.get(&link_id).map(|s| s.health.score())
    }

    /// Whether a link is currently marked as degraded.
    pub fn is_degraded(&self, link_id: usize) -> bool {
        self.links.get(&link_id).is_some_and(|s| s.degraded)
    }

    /// Get the estimated capacity for a specific link (kbps).
    pub fn link_capacity_kbps(&self, link_id: usize) -> f64 {
        self.links
            .get(&link_id)
            .and_then(|s| s.last_rf.as_ref())
            .map(|rf| {
                let sinr_cap = sinr_to_capacity_kbps(rf.sinr_db);
                let cqi_cap = cqi_to_throughput_kbps(rf.cqi);
                // Use the more conservative estimate
                sinr_cap.min(cqi_cap)
            })
            .unwrap_or(0.0)
    }

    /// Total estimated capacity across all tracked links (kbps).
    pub fn total_capacity_kbps(&self) -> f64 {
        self.links
            .keys()
            .map(|&id| self.link_capacity_kbps(id))
            .sum()
    }

    /// Get a snapshot of link capacities for the adaptation module.
    pub fn link_capacities(&self) -> Vec<crate::adaptation::LinkCapacity> {
        self.links
            .iter()
            .map(|(&link_id, state)| {
                let capacity = state
                    .last_rf
                    .as_ref()
                    .map(|rf| sinr_to_capacity_kbps(rf.sinr_db).min(cqi_to_throughput_kbps(rf.cqi)))
                    .unwrap_or(0.0);

                crate::adaptation::LinkCapacity {
                    link_id,
                    capacity_kbps: capacity,
                    alive: !state.degraded,
                    loss_rate: 0.0, // filled in by transport metrics separately
                    rtt_ms: 0.0,
                    queue_depth: None,
                }
            })
            .collect()
    }

    // ─── Internal ───────────────────────────────────────────────────────

    fn check_link_status(&mut self, link_id: usize, events: &mut Vec<SupervisorEvent>) {
        let Some(state) = self.links.get_mut(&link_id) else {
            return;
        };

        let score = state.health.score();

        if state.degraded {
            // Hysteresis: require higher score to recover
            if score > self.config.recovery_threshold {
                state.degraded = false;
                events.push(SupervisorEvent::LinkRecovered { link_id, score });
            }
        } else if score < self.config.degraded_threshold {
            state.degraded = true;
            events.push(SupervisorEvent::LinkDegraded { link_id, score });
        }
    }

    fn check_handover(&self, link_id: usize, events: &mut Vec<SupervisorEvent>) {
        let Some(state) = self.links.get(&link_id) else {
            return;
        };

        if state.health.is_sinr_degrading() {
            let predicted = state.health.predicted_sinr(5);
            if predicted < self.config.handover_sinr_threshold {
                events.push(SupervisorEvent::HandoverWarning {
                    link_id,
                    predicted_sinr: predicted,
                });
            }
        }
    }

    fn check_capacity(&mut self, events: &mut Vec<SupervisorEvent>) {
        let total = self.total_capacity_kbps();
        let alive = self.links.values().filter(|s| !s.degraded).count();

        if self.prev_total_capacity_kbps > 0.0 {
            let ratio =
                (total - self.prev_total_capacity_kbps).abs() / self.prev_total_capacity_kbps;
            if ratio >= self.config.capacity_change_ratio {
                events.push(SupervisorEvent::CapacityChanged {
                    total_capacity_kbps: total,
                    alive_links: alive,
                });
            }
        }

        self.prev_total_capacity_kbps = total;
    }
}

impl Default for ModemSupervisor {
    fn default() -> Self {
        Self::new(SupervisorConfig::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn good_rf() -> RfMetrics {
        RfMetrics {
            rsrp_dbm: -75.0,
            rsrq_db: -6.0,
            sinr_db: 20.0,
            cqi: 12,
        }
    }

    fn poor_rf() -> RfMetrics {
        RfMetrics {
            rsrp_dbm: -130.0,
            rsrq_db: -18.0,
            sinr_db: -10.0,
            cqi: 1,
        }
    }

    fn good_transport() -> TransportMetrics {
        TransportMetrics {
            loss_rate: 0.01,
            jitter_ms: 5.0,
            rtt_ms: 30.0,
        }
    }

    fn bad_transport() -> TransportMetrics {
        TransportMetrics {
            loss_rate: 0.30,
            jitter_ms: 80.0,
            rtt_ms: 200.0,
        }
    }

    // ─── Registration ───────────────────────────────────────────────────

    #[test]
    fn register_and_remove_link() {
        let mut sup = ModemSupervisor::default();
        sup.register_link(0);
        sup.register_link(1);
        assert_eq!(sup.link_count(), 2);

        sup.remove_link(1);
        assert_eq!(sup.link_count(), 1);
    }

    #[test]
    fn auto_registers_on_update() {
        let mut sup = ModemSupervisor::default();
        sup.update_rf(42, &good_rf());
        assert_eq!(sup.link_count(), 1);
        assert!(sup.link_score(42).is_some());
    }

    // ─── Health Tracking ────────────────────────────────────────────────

    #[test]
    fn good_link_stays_healthy() {
        let mut sup = ModemSupervisor::default();
        for _ in 0..10 {
            sup.update_rf(0, &good_rf());
            sup.update_transport(0, &good_transport());
        }
        assert!(!sup.is_degraded(0));
        assert!(sup.link_score(0).unwrap() > 50.0);
    }

    #[test]
    fn poor_link_becomes_degraded() {
        let mut sup = ModemSupervisor::default();
        let mut events = Vec::new();
        for _ in 0..20 {
            events.extend(sup.update_rf(0, &poor_rf()));
            events.extend(sup.update_transport(0, &bad_transport()));
        }
        assert!(sup.is_degraded(0));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, SupervisorEvent::LinkDegraded { link_id: 0, .. })),
            "should emit LinkDegraded event"
        );
    }

    // ─── Hysteresis ─────────────────────────────────────────────────────

    #[test]
    fn recovery_requires_higher_threshold() {
        let mut sup = ModemSupervisor::new(SupervisorConfig {
            degraded_threshold: 40.0,
            recovery_threshold: 55.0,
            ..Default::default()
        });

        // Degrade the link
        for _ in 0..20 {
            sup.update_rf(0, &poor_rf());
            sup.update_transport(0, &bad_transport());
        }
        assert!(sup.is_degraded(0));

        // Provide mildly good metrics then excellent metrics,
        // capturing all events throughout.
        let mid_rf = RfMetrics {
            rsrp_dbm: -90.0,
            rsrq_db: -10.0,
            sinr_db: 8.0,
            cqi: 6,
        };
        let mut recovered = false;
        let mut final_score = 0.0;
        // Mid-level recovery phase
        for _ in 0..5 {
            let rf_ev = sup.update_rf(0, &mid_rf);
            let tp_ev = sup.update_transport(0, &good_transport());
            for e in rf_ev.iter().chain(tp_ev.iter()) {
                if matches!(e, SupervisorEvent::LinkRecovered { .. }) {
                    recovered = true;
                }
            }
        }
        // Full recovery phase
        for _ in 0..60 {
            let rf_events = sup.update_rf(0, &good_rf());
            let tp_events = sup.update_transport(0, &good_transport());
            final_score = sup.link_score(0).unwrap_or(0.0);
            for e in rf_events.iter().chain(tp_events.iter()) {
                if matches!(e, SupervisorEvent::LinkRecovered { .. }) {
                    recovered = true;
                }
            }
        }
        assert!(
            recovered,
            "should eventually recover with good metrics, final score: {final_score}"
        );
    }

    // ─── Capacity Estimation ────────────────────────────────────────────

    #[test]
    fn capacity_from_rf_metrics() {
        let mut sup = ModemSupervisor::default();
        sup.update_rf(0, &good_rf());
        let cap = sup.link_capacity_kbps(0);
        assert!(
            cap > 0.0,
            "should have non-zero capacity with good RF, got {cap}"
        );
    }

    #[test]
    fn total_capacity_aggregates() {
        let mut sup = ModemSupervisor::default();
        sup.update_rf(0, &good_rf());
        sup.update_rf(1, &good_rf());
        let total = sup.total_capacity_kbps();
        let single = sup.link_capacity_kbps(0);
        assert!(
            (total - single * 2.0).abs() < 0.01,
            "total should be sum of links: total={total}, single={single}"
        );
    }

    // ─── Capacity Change Events ─────────────────────────────────────────

    #[test]
    fn capacity_change_event() {
        let mut sup = ModemSupervisor::new(SupervisorConfig {
            capacity_change_ratio: 0.10,
            ..Default::default()
        });

        // Baseline
        sup.update_rf(0, &good_rf());
        sup.update_rf(1, &good_rf());

        // Kill one link → should trigger capacity change
        let events = sup.update_rf(
            1,
            &RfMetrics {
                rsrp_dbm: -140.0,
                rsrq_db: -20.0,
                sinr_db: -15.0,
                cqi: 0,
            },
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, SupervisorEvent::CapacityChanged { .. })),
            "should emit CapacityChanged when a link drops"
        );
    }

    // ─── Link Capacities Snapshot ───────────────────────────────────────

    #[test]
    fn link_capacities_snapshot() {
        let mut sup = ModemSupervisor::default();
        sup.update_rf(0, &good_rf());
        sup.update_rf(1, &good_rf());

        let caps = sup.link_capacities();
        assert_eq!(caps.len(), 2);
        assert!(caps.iter().all(|c| c.alive));
        assert!(caps.iter().all(|c| c.capacity_kbps > 0.0));
    }

    // ─── Handover Warning ───────────────────────────────────────────────

    #[test]
    fn handover_warning_on_degrading_sinr() {
        let mut sup = ModemSupervisor::new(SupervisorConfig {
            handover_sinr_threshold: 5.0,
            ..Default::default()
        });

        let mut found_warning = false;
        for i in 0..30 {
            let rf = RfMetrics {
                rsrp_dbm: -70.0,
                rsrq_db: -5.0,
                sinr_db: 25.0 - i as f64 * 2.0, // Rapidly degrading
                cqi: 12,
            };
            let events = sup.update_rf(0, &rf);
            if events
                .iter()
                .any(|e| matches!(e, SupervisorEvent::HandoverWarning { .. }))
            {
                found_warning = true;
                break;
            }
        }
        assert!(
            found_warning,
            "should emit HandoverWarning on rapidly degrading SINR"
        );
    }

    // ─── Unknown Link ───────────────────────────────────────────────────

    #[test]
    fn unknown_link_returns_none() {
        let sup = ModemSupervisor::default();
        assert!(sup.link_score(999).is_none());
        assert!(!sup.is_degraded(999));
    }
}
