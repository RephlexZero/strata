//! # EDPF — Earliest Delivery Path First
//!
//! Delay-based packet scheduler for bonded transport links.
//!
//! Replaces DWRR with a scheduler that routes each packet to the link
//! predicted to deliver it first:
//!
//! ```text
//! Predicted_Arrival(link) = In_Flight_Bytes / (Capacity_bps / 8) + Base_RTT
//! Selected = argmin(Predicted_Arrival) over all alive links
//! ```
//!
//! For transport-backed links, the in-flight estimate is derived from the
//! actual queue depth (paced_queue + sender output queue) each refresh cycle.
//! The transport layer's own congestion control (BBR/Biscay) and paced_queue
//! cap handle rate limiting and backpressure.

use crate::config::SchedulerConfig;
use crate::net::interface::{LinkMetrics, LinkPhase, LinkSender};
use quanta::Instant;
use std::collections::HashMap;
use std::sync::Arc;

/// Per-link state tracked by the EDPF scheduler.
pub(crate) struct LinkState<L: ?Sized> {
    pub link: Arc<L>,
    pub metrics: LinkMetrics,
    /// Bytes currently in-flight (sent but not yet acknowledged).
    pub in_flight_bytes: u64,
    /// Bytes that the scheduler attempted to send but the link rejected.
    pub failed_bytes: u64,
    pub last_failed_bytes: u64,
    /// Total bytes sent through this link (for throughput measurement).
    pub sent_bytes: u64,
    pub last_sent_bytes: u64,
    pub last_sent_at: Instant,
    /// Measured send rate (bps).
    pub measured_bps: f64,
    /// Spare capacity above measured throughput.
    pub spare_capacity_bps: f64,
    /// Whether we've observed any traffic on this link.
    pub has_traffic: bool,
    /// Previous capacity for penalty detection.
    pub prev_capacity_bps: f64,
    /// Penalty factor applied on sudden capacity drops (0.3–1.0).
    pub penalty_factor: f64,
    /// Previous link phase (for detecting transitions).
    pub prev_phase: LinkPhase,
    /// Stop signal for the feedback thread.
    pub stop_tx: Option<crossbeam_channel::Sender<()>>,
}

impl<L: ?Sized> LinkState<L> {
    /// Base RTT in seconds (uses rtprop if available, else rtt_ms/2 as proxy).
    ///
    /// Used for predicted-arrival calculation — deliberately includes any
    /// queuing signal so we avoid routing more packets into an already-queued
    /// link when it has lower clean-path RTT than others.
    fn base_rtt_secs(&self) -> f64 {
        if let Some(rtprop) = self.metrics.rtprop_ms
            && rtprop > 0.0
        {
            return rtprop / 1000.0;
        }
        // Fall back to smoothed RTT (which includes queuing) — conservative.
        (self.metrics.rtt_ms / 1000.0).max(0.001)
    }

    /// Estimated capacity in bytes per second, discounted by observed loss rate.
    ///
    /// A link with 80% loss is only delivering 20% of its nominal capacity.
    /// Applying the discount here ensures predicted-arrival calculations
    /// route fewer packets to high-loss links, preventing the ARQ queue
    /// from building up unboundedly when oracle estimates are stale.
    fn capacity_bytes_per_sec(&self) -> f64 {
        // Clamp to 0.99 so a link never appears to have exactly 0 capacity;
        // the fallback routing path can still use it as a last resort.
        let loss = self.metrics.loss_rate.clamp(0.0, 0.99);
        (self.metrics.capacity_bps / 8.0 * (1.0 - loss)).max(1.0)
    }

    /// Predicted arrival time (seconds from now) for a packet of `size_bytes`.
    ///
    /// `arrival = in_flight_bytes / capacity_Bps + base_rtt`
    fn predicted_arrival(&self, size_bytes: usize) -> f64 {
        let queue_drain =
            (self.in_flight_bytes as f64 + size_bytes as f64) / self.capacity_bytes_per_sec();
        queue_drain + self.base_rtt_secs()
    }
}

/// Earliest Delivery Path First (EDPF) packet scheduler.
///
/// Routes each packet to the link predicted to deliver it earliest.
/// The transport layer's congestion control (BBR/Biscay) and paced_queue
/// cap handle rate limiting and backpressure.
pub struct Edpf<L: LinkSender + ?Sized + 'static> {
    links: HashMap<usize, LinkState<L>>,
    sorted_ids: Vec<usize>,
    config: SchedulerConfig,
    /// When set, routes all traffic to this link for saturation probing.
    probe_boost_link: Option<usize>,
}

impl<L: LinkSender + ?Sized + 'static> Drop for Edpf<L> {
    fn drop(&mut self) {
        for state in self.links.values_mut() {
            if let Some(tx) = state.stop_tx.take() {
                let _ = tx.send(());
            }
        }
    }
}

impl<L: LinkSender + ?Sized + 'static> Edpf<L> {
    pub fn new() -> Self {
        Self::with_config(SchedulerConfig::default())
    }

    pub fn with_config(config: SchedulerConfig) -> Self {
        Self {
            links: HashMap::new(),
            sorted_ids: Vec::new(),
            config,
            probe_boost_link: None,
        }
    }

    pub fn config(&self) -> &SchedulerConfig {
        &self.config
    }

    pub fn update_config(&mut self, config: SchedulerConfig) {
        self.config = config;
    }

    /// Set the link to receive all traffic for saturation probing.
    pub fn set_probe_boost_link(&mut self, id: Option<usize>) {
        self.probe_boost_link = id;
    }

    pub fn add_link(&mut self, link: Arc<L>) {
        let id = link.id();
        let metrics = link.get_metrics();
        let now = Instant::now();

        let (stop_tx, stop_rx) = crossbeam_channel::bounded(1);
        let link_clone = link.clone();
        std::thread::spawn(move || {
            loop {
                if stop_rx.try_recv().is_ok() {
                    break;
                }
                link_clone.recv_feedback();
                link_clone.flush_paced();
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        });

        self.links.insert(
            id,
            LinkState {
                metrics: metrics.clone(),
                link,
                in_flight_bytes: 0,
                failed_bytes: 0,
                last_failed_bytes: 0,
                sent_bytes: 0,
                last_sent_bytes: 0,
                last_sent_at: now,
                measured_bps: 0.0,
                spare_capacity_bps: 0.0,
                has_traffic: false,
                prev_capacity_bps: metrics.capacity_bps,
                penalty_factor: 1.0,
                prev_phase: LinkPhase::Init,
                stop_tx: Some(stop_tx),
            },
        );
        self.sorted_ids.push(id);
        self.sorted_ids.sort();
    }

    pub fn remove_link(&mut self, id: usize) {
        if let Some(mut state) = self.links.remove(&id)
            && let Some(tx) = state.stop_tx.take()
        {
            let _ = tx.send(());
        }
        if let Some(pos) = self.sorted_ids.iter().position(|&x| x == id) {
            self.sorted_ids.remove(pos);
        }
    }

    pub fn refresh_metrics(&mut self) {
        let capacity_floor = self.config.capacity_floor_bps;
        let penalty_decay = self.config.penalty_decay;
        let penalty_recovery = self.config.penalty_recovery;

        for state in self.links.values_mut() {
            let now = Instant::now();
            state.metrics = state.link.get_metrics();

            // Update in-flight estimate for predicted-arrival routing.
            //
            // For transport-backed links: use the actual queue depth as the
            // in-flight estimate. This replaces the old ACK-delta counter
            // which leaked upward permanently when queue-capped packets
            // (dropped from paced_queue front) were never ACKed.
            //
            // BDP hard-capping is disabled for transport links because the
            // transport layer's own congestion control (BBR/Biscay) handles
            // rate limiting and the paced_queue cap provides backpressure.
            if state.metrics.transport.is_some() {
                // Use queue_depth * estimated packet size for predicted_arrival.
                // queue_depth = paced_queue.len() + sender_output_queue.len()
                const EST_PKT_SIZE: u64 = 1400;
                state.in_flight_bytes = (state.metrics.queue_depth as u64) * EST_PKT_SIZE;
            } else {
                // Estimate bytes drained since last refresh based on capacity.
                let dt = now
                    .duration_since(state.last_sent_at)
                    .as_secs_f64()
                    .max(0.001);
                let drained = (state.capacity_bytes_per_sec() * dt) as u64;
                state.in_flight_bytes = state.in_flight_bytes.saturating_sub(drained);
            }

            let dt_sent = now.duration_since(state.last_sent_at).as_secs_f64();

            // Transport links report socket-level observed_bps.
            if state.metrics.transport.is_some() {
                if dt_sent > 0.0 {
                    state.last_sent_bytes = state.sent_bytes;
                    state.last_failed_bytes = state.failed_bytes;
                    state.last_sent_at = now;
                }
                state.has_traffic = state.metrics.observed_bps > 0.0;
                state.measured_bps = state.metrics.observed_bps;
            } else {
                if dt_sent > 0.0 {
                    let delta_bytes = state.sent_bytes.saturating_sub(state.last_sent_bytes);
                    let delta_failed = state.failed_bytes.saturating_sub(state.last_failed_bytes);
                    let delta_total = delta_bytes + delta_failed;
                    if delta_total > 0 {
                        state.measured_bps = (delta_total as f64 * 8.0) / dt_sent;
                        state.has_traffic = true;
                    }
                    state.last_sent_bytes = state.sent_bytes;
                    state.last_failed_bytes = state.failed_bytes;
                    state.last_sent_at = now;
                }
                state.metrics.observed_bps = state.measured_bps;
                state.metrics.observed_bytes = state.sent_bytes.saturating_add(state.failed_bytes);
            }

            // Spare capacity
            if state.has_traffic {
                state.spare_capacity_bps =
                    (state.metrics.capacity_bps - state.measured_bps).max(0.0);
            } else {
                state.spare_capacity_bps = 0.0;
            }

            // Bootstrap floor for uncalibrated links
            if state.metrics.capacity_bps < 1_000_000.0
                && matches!(state.metrics.phase, LinkPhase::Probe | LinkPhase::Warm)
            {
                state.metrics.capacity_bps = capacity_floor;
                state.metrics.estimated_capacity_bps = capacity_floor;
            }

            state.prev_phase = state.metrics.phase;

            // Penalty factor for sudden capacity drops
            let prev_capacity = state.prev_capacity_bps;
            let curr_capacity = state.metrics.capacity_bps;
            if prev_capacity > 0.0 && curr_capacity < prev_capacity * 0.5 {
                state.penalty_factor = (state.penalty_factor * penalty_decay).max(0.3);
            } else {
                state.penalty_factor = (state.penalty_factor + penalty_recovery).min(1.0);
            }

            state.prev_capacity_bps = curr_capacity;
        }
    }

    pub fn get_active_links(&self) -> Vec<(usize, LinkMetrics)> {
        self.links
            .iter()
            .map(|(id, l)| (*id, l.metrics.clone()))
            .collect()
    }

    /// Get a link reference by ID.
    pub fn get_link(&self, id: usize) -> Option<Arc<L>> {
        self.links.get(&id).map(|s| s.link.clone())
    }

    /// Record a successful send (updates in-flight and sent counters).
    pub fn record_send(&mut self, id: usize, bytes: u64) {
        if let Some(state) = self.links.get_mut(&id) {
            state.sent_bytes = state.sent_bytes.saturating_add(bytes);
            state.in_flight_bytes = state.in_flight_bytes.saturating_add(bytes);
            state.has_traffic = true;
        }
    }

    /// Records a failed send attempt.
    pub fn record_send_failed(&mut self, id: usize, bytes: u64) {
        if let Some(state) = self.links.get_mut(&id) {
            state.failed_bytes = state.failed_bytes.saturating_add(bytes);
            state.has_traffic = true;
        }
    }

    /// Mark a link as having traffic (for testing).
    pub fn mark_has_traffic(&mut self, id: usize) {
        if let Some(state) = self.links.get_mut(&id) {
            state.has_traffic = true;
        }
    }

    /// Returns the total spare capacity across all Live/Warm links.
    pub fn total_spare_capacity(&self) -> f64 {
        self.links
            .values()
            .filter(|state| {
                matches!(state.metrics.phase, LinkPhase::Live | LinkPhase::Warm)
                    && state.metrics.alive
            })
            .map(|state| state.spare_capacity_bps)
            .sum()
    }

    /// Returns all alive links (for broadcasting critical packets).
    pub fn broadcast_links(&mut self, _packet_len: usize) -> Vec<Arc<L>> {
        let any_alive = self.links.values().any(|state| state.metrics.alive);
        self.links
            .values()
            .filter(|state| state.metrics.alive || !any_alive)
            .map(|state| state.link.clone())
            .collect()
    }

    /// Selects the best N links with diversity preference (for redundancy).
    pub fn select_best_n_links(&mut self, packet_len: usize, n: usize) -> Vec<Arc<L>> {
        let mut selected = Vec::new();
        let mut used_kinds: std::collections::HashSet<String> = std::collections::HashSet::new();

        // Score by predicted arrival time (lower = better)
        let mut scored_links: Vec<_> = self
            .links
            .iter()
            .filter(|(_, state)| state.metrics.alive)
            .map(|(id, state)| {
                let arrival = state.predicted_arrival(packet_len);
                let phase_weight = match state.metrics.phase {
                    LinkPhase::Live => 1.0,
                    LinkPhase::Warm => 0.8,
                    LinkPhase::Degrade => 0.5,
                    LinkPhase::Probe => 0.3,
                    _ => 0.1,
                };
                // Invert arrival for scoring (lower arrival = higher score)
                let score = phase_weight / (arrival + 0.001);
                (*id, score, state.metrics.link_kind.clone())
            })
            .collect();

        scored_links.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        // First pass: diverse links
        for (id, _score, link_kind) in &scored_links {
            if selected.len() >= n {
                break;
            }
            let is_diverse = match link_kind {
                None => true,
                Some(kind) => !used_kinds.contains(kind.as_str()),
            };
            if is_diverse && let Some(state) = self.links.get(id) {
                selected.push(state.link.clone());
                if let Some(kind) = link_kind {
                    used_kinds.insert(kind.clone());
                }
            }
        }

        // Fill remaining slots
        if selected.len() < n {
            let selected_ids: std::collections::HashSet<usize> =
                selected.iter().map(|l| l.id()).collect();
            for (id, _score, _) in &scored_links {
                if selected.len() >= n {
                    break;
                }
                if !selected_ids.contains(id)
                    && let Some(state) = self.links.get(id)
                {
                    selected.push(state.link.clone());
                }
            }
        }

        selected
    }

    /// EDPF link selection: pick the link with the lowest predicted arrival time.
    ///
    /// BDP-blocked links are excluded unless all links are blocked (graceful
    /// degradation: pick the least-loaded blocked link).
    pub fn select_link(&mut self, packet_len: usize) -> Option<Arc<L>> {
        let candidates = self.sorted_ids.clone();
        self.select_from_links(packet_len, &candidates)
    }

    /// EDPF selection from a subset of candidate link IDs.
    pub fn select_from_links(&mut self, packet_len: usize, candidates: &[usize]) -> Option<Arc<L>> {
        if candidates.is_empty() {
            return None;
        }

        let any_alive = self.links.values().any(|state| state.metrics.alive);
        // Collect (link_id, predicted_arrival) for alive candidates
        let mut scored: Vec<(usize, f64)> = Vec::new();
        for &id in candidates {
            if let Some(state) = self.links.get(&id)
                && (state.metrics.alive || !any_alive)
            {
                let phase_ok =
                    !matches!(state.metrics.phase, LinkPhase::Cooldown | LinkPhase::Reset);
                let os_ok = !matches!(state.metrics.os_up, Some(false));
                if phase_ok && os_ok {
                    let arrival = state.predicted_arrival(packet_len);
                    scored.push((id, arrival));
                }
            }
        }

        if scored.is_empty() {
            // Last resort: any alive link
            for &id in candidates {
                if let Some(state) = self.links.get(&id)
                    && (state.metrics.alive || !any_alive)
                {
                    return Some(state.link.clone());
                }
            }
            return None;
        }

        // Pick the link with lowest predicted arrival time.
        // BDP hard-capping has been removed: transport links have their own
        // congestion control (BBR/Biscay) and paced_queue cap for backpressure.
        // EDPF's predicted_arrival naturally routes away from loaded links
        // because higher queue depth → longer drain time → higher arrival.
        let best = scored
            .iter()
            .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

        if let Some(&(id, _arrival)) = best {
            return self.links.get(&id).map(|s| s.link.clone());
        }

        None
    }

    /// Returns in-flight bytes for a link.
    pub fn link_in_flight(&self, id: usize) -> Option<u64> {
        self.links.get(&id).map(|s| s.in_flight_bytes)
    }
}

impl<L: LinkSender + ?Sized> Default for Edpf<L> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::interface::{LinkMetrics, TransportMetrics};
    use std::sync::Arc;
    use std::sync::Mutex;

    struct MockLink {
        id: usize,
        metrics: Mutex<LinkMetrics>,
    }

    impl MockLink {
        fn new(id: usize, capacity_bps: f64, rtt_ms: f64, phase: LinkPhase) -> Self {
            Self {
                id,
                metrics: Mutex::new(LinkMetrics {
                    rtt_ms,
                    capacity_bps,
                    loss_rate: 0.0,
                    observed_bps: 0.0,
                    observed_bytes: 0,
                    queue_depth: 0,
                    max_queue: 100,
                    alive: true,
                    phase,
                    os_up: None,
                    mtu: None,
                    iface: None,
                    link_kind: None,
                    transport: None,
                    btlbw_bps: None,
                    rtprop_ms: Some(rtt_ms),
                    ack_delivery_bps: 0.0,
                    ack_bytes: 0,
                    estimated_capacity_bps: 0.0,
                    owd_ms: 0.0,
                    receiver_report: None,
                }),
            }
        }

        /// Create a mock link with transport metrics (enables BDP blocking).
        fn with_transport(id: usize, capacity_bps: f64, rtt_ms: f64, phase: LinkPhase) -> Self {
            let link = Self::new(id, capacity_bps, rtt_ms, phase);
            link.metrics.lock().unwrap().transport = Some(TransportMetrics::default());
            link
        }
    }

    impl LinkSender for MockLink {
        fn id(&self) -> usize {
            self.id
        }
        fn send(&self, _packet: &[u8]) -> anyhow::Result<usize> {
            Ok(0)
        }
        fn get_metrics(&self) -> LinkMetrics {
            self.metrics.lock().unwrap().clone()
        }
    }

    #[test]
    fn edpf_selects_fastest_link() {
        let mut edpf = Edpf::new();
        let l1 = Arc::new(MockLink::new(1, 10_000_000.0, 10.0, LinkPhase::Live));
        let l2 = Arc::new(MockLink::new(2, 3_000_000.0, 10.0, LinkPhase::Live));

        edpf.add_link(l1.clone());
        edpf.add_link(l2.clone());
        edpf.refresh_metrics();

        // Both empty — L1 should win (lower arrival = higher capacity)
        let selected = edpf.select_link(1400).unwrap();
        assert_eq!(selected.id(), 1);
    }

    #[test]
    fn edpf_shifts_to_slower_link_as_fast_fills() {
        let mut edpf = Edpf::new();
        let l1 = Arc::new(MockLink::new(1, 10_000_000.0, 10.0, LinkPhase::Live));
        let l2 = Arc::new(MockLink::new(2, 5_000_000.0, 10.0, LinkPhase::Live));

        edpf.add_link(l1.clone());
        edpf.add_link(l2.clone());
        edpf.refresh_metrics();

        // Load L1 with enough in-flight to raise its predicted arrival
        // above L2's predicted arrival
        edpf.record_send(1, 50_000); // 50KB in-flight on L1

        let selected = edpf.select_link(1400).unwrap();
        // L1 predicted: 50000/1250000 + 0.01 = 0.05s
        // L2 predicted: 0/625000 + 0.01 = 0.01s → L2 wins
        assert_eq!(selected.id(), 2);
    }

    #[test]
    fn transport_link_routes_to_least_loaded() {
        // Transport link with high in-flight → EDPF routes to the other
        let mut edpf = Edpf::new();
        let l1 = Arc::new(MockLink::with_transport(
            1,
            10_000_000.0,
            10.0,
            LinkPhase::Live,
        ));
        let l2 = Arc::new(MockLink::with_transport(
            2,
            10_000_000.0,
            10.0,
            LinkPhase::Live,
        ));

        edpf.add_link(l1.clone());
        edpf.add_link(l2.clone());
        edpf.refresh_metrics();

        // Load L1 heavily — predicted arrival becomes worse
        edpf.record_send(1, 100_000);

        // EDPF should pick L2 (lower predicted arrival)
        let selected = edpf.select_link(1400).unwrap();
        assert_eq!(selected.id(), 2);
    }

    #[test]
    fn transport_link_never_returns_none() {
        // With BDP blocking removed, transport links always return Some
        let mut edpf = Edpf::new();
        let l1 = Arc::new(MockLink::with_transport(
            1,
            10_000_000.0,
            10.0,
            LinkPhase::Live,
        ));
        edpf.add_link(l1.clone());
        edpf.refresh_metrics();

        // Even with massive in-flight, select_link returns Some
        edpf.record_send(1, 10_000_000);
        let selected = edpf.select_link(1400);
        assert!(
            selected.is_some(),
            "transport links should never return None (CC handles congestion)"
        );
    }

    #[test]
    fn transport_in_flight_resets_from_queue_depth() {
        // After refresh_metrics, in_flight should reflect queue_depth
        let mut edpf = Edpf::new();
        let l1 = Arc::new(MockLink::with_transport(
            1,
            10_000_000.0,
            10.0,
            LinkPhase::Live,
        ));
        edpf.add_link(l1.clone());
        edpf.refresh_metrics();

        // Send a lot — in_flight grows via record_send
        edpf.record_send(1, 500_000);
        assert_eq!(edpf.link_in_flight(1), Some(500_000));

        // Set queue_depth to 100 packets and refresh
        l1.metrics.lock().unwrap().queue_depth = 100;
        edpf.refresh_metrics();

        // in_flight should now be queue_depth * 1400, NOT 500_000
        assert_eq!(
            edpf.link_in_flight(1),
            Some(100 * 1400),
            "transport in_flight should reset to queue_depth * pkt_size"
        );
    }

    #[test]
    fn non_transport_in_flight_drains_with_time() {
        let mut edpf = Edpf::new();
        // Non-transport link — uses time-based drain
        let l1 = Arc::new(MockLink::new(1, 10_000_000.0, 10.0, LinkPhase::Live));
        edpf.add_link(l1.clone());
        edpf.refresh_metrics();

        edpf.record_send(1, 50_000);
        let before = edpf.link_in_flight(1).unwrap();
        assert_eq!(before, 50_000);

        // After refresh, time-based drain should reduce in_flight
        std::thread::sleep(std::time::Duration::from_millis(10));
        edpf.refresh_metrics();
        let after = edpf.link_in_flight(1).unwrap();
        assert!(
            after < before,
            "non-transport in_flight should drain over time ({} < {})",
            after,
            before
        );
    }

    #[test]
    fn rapid_sends_always_route_to_best_link() {
        // Verifies that rapid sends never deadlock — always returns Some
        let mut edpf = Edpf::new();
        let l1 = Arc::new(MockLink::with_transport(
            1,
            5_000_000.0,
            80.0,
            LinkPhase::Live,
        ));
        let l2 = Arc::new(MockLink::with_transport(
            2,
            5_000_000.0,
            160.0,
            LinkPhase::Live,
        ));

        edpf.add_link(l1.clone());
        edpf.add_link(l2.clone());
        edpf.refresh_metrics();

        // 10,000 rapid sends — should never return None
        for _ in 0..10_000 {
            let selected = edpf.select_link(1400);
            assert!(selected.is_some(), "should never deadlock");
            edpf.record_send(selected.unwrap().id(), 1400);
        }
    }

    #[test]
    fn queue_depth_refresh_prevents_in_flight_leak() {
        // Simulates the field-test scenario: sends + queue drops → leak
        // Verify refresh_metrics resets in_flight from queue_depth
        let mut edpf = Edpf::new();
        let l1 = Arc::new(MockLink::with_transport(
            1,
            5_000_000.0,
            80.0,
            LinkPhase::Live,
        ));
        edpf.add_link(l1.clone());
        edpf.refresh_metrics();

        // Simulate lots of sends that would leak the old counter
        for _ in 0..1000 {
            edpf.record_send(1, 1400);
        }
        let leaked = edpf.link_in_flight(1).unwrap();
        assert_eq!(leaked, 1000 * 1400); // 1.4MB leaked in-flight

        // Now simulate that the actual queue only has 50 packets
        // (the rest were dropped by queue cap or paced out)
        l1.metrics.lock().unwrap().queue_depth = 50;
        edpf.refresh_metrics();

        let actual = edpf.link_in_flight(1).unwrap();
        assert_eq!(
            actual,
            50 * 1400,
            "refresh should reset in_flight to queue_depth ({} vs {})",
            actual,
            50 * 1400
        );
    }
}
