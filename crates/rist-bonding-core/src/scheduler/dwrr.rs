use crate::config::SchedulerConfig;
use crate::net::interface::{LinkMetrics, LinkPhase, LinkSender};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

/// Per-link state tracked by the DWRR scheduler.
///
/// Holds the link's credit balance, throughput measurements, trend slopes,
/// and penalty factors used to compute effective capacity during link selection.
pub(crate) struct LinkState<L: ?Sized> {
    pub link: Arc<L>,
    pub credits: f64,
    pub last_update: Instant,
    pub metrics: LinkMetrics,
    pub sent_bytes: u64,
    pub last_sent_bytes: u64,
    pub last_sent_at: Instant,
    pub measured_bps: f64,
    pub spare_capacity_bps: f64,
    pub has_traffic: bool,
    pub prev_capacity_bps: f64,
    pub prev_rtt_ms: f64,
    pub prev_loss_rate: f64,
    pub bw_slope_bps_s: f64,
    pub rtt_slope_ms_s: f64,
    pub loss_slope_per_s: f64,
    pub last_metrics_update: Instant,
    pub penalty_factor: f64,
}

/// Deficit Weighted Round Robin (DWRR) packet scheduler.
///
/// Distributes packets across links proportional to their effective capacity.
/// Each link accumulates byte "credits" at a rate proportional to its
/// quality-adjusted bandwidth. A link is selected when it has enough credits
/// to cover the packet cost. Credits are capped to a burst window that
/// scales with the link's lifecycle phase and loss rate.
///
/// The scheduler also provides broadcast, best-N selection (for redundancy),
/// and spare-capacity queries used by higher-level bonding logic.
pub struct Dwrr<L: LinkSender + ?Sized> {
    links: HashMap<usize, LinkState<L>>,
    sorted_ids: Vec<usize>,
    current_rr_idx: usize,
    config: SchedulerConfig,
}

/// Computes the burst window (in seconds) for credit capping.
///
/// Links in healthier phases (Live) get larger burst windows, allowing
/// them to absorb short traffic spikes. Degraded or probing links are
/// tightly limited. Loss further reduces the window.
fn compute_burst_window_s(phase: LinkPhase, loss_rate: f64) -> f64 {
    let base = match phase {
        LinkPhase::Probe => 0.02,
        LinkPhase::Warm => 0.05,
        LinkPhase::Live => 0.1,
        LinkPhase::Degrade => 0.04,
        LinkPhase::Cooldown | LinkPhase::Reset | LinkPhase::Init => 0.01,
    };

    let loss_factor = (1.0 - loss_rate).clamp(0.1, 1.0);
    (base * loss_factor).clamp(0.01, 0.1)
}

impl<L: LinkSender + ?Sized> Dwrr<L> {
    pub fn new() -> Self {
        Self::with_config(SchedulerConfig::default())
    }

    pub fn with_config(config: SchedulerConfig) -> Self {
        Self {
            links: HashMap::new(),
            sorted_ids: Vec::new(),
            current_rr_idx: 0,
            config,
        }
    }

    pub fn config(&self) -> &SchedulerConfig {
        &self.config
    }

    pub fn update_config(&mut self, config: SchedulerConfig) {
        self.config = config;
    }

    pub fn add_link(&mut self, link: Arc<L>) {
        let id = link.id();
        let metrics = link.get_metrics();
        let now = Instant::now();
        self.links.insert(
            id,
            LinkState {
                metrics: metrics.clone(),
                link,
                credits: 0.0,
                last_update: now,
                sent_bytes: 0,
                last_sent_bytes: 0,
                last_sent_at: now,
                measured_bps: 0.0,
                spare_capacity_bps: 0.0,
                has_traffic: false,
                prev_capacity_bps: metrics.capacity_bps,
                prev_rtt_ms: metrics.rtt_ms,
                prev_loss_rate: metrics.loss_rate,
                bw_slope_bps_s: 0.0,
                rtt_slope_ms_s: 0.0,
                loss_slope_per_s: 0.0,
                last_metrics_update: Instant::now(),
                penalty_factor: 1.0,
            },
        );
        self.sorted_ids.push(id);
        self.sorted_ids.sort();
    }

    pub fn refresh_metrics(&mut self) {
        let capacity_floor = self.config.capacity_floor_bps;
        let penalty_decay = self.config.penalty_decay;
        let penalty_recovery = self.config.penalty_recovery;

        for state in self.links.values_mut() {
            let now = Instant::now();
            state.metrics = state.link.get_metrics();
            let dt_sent = now.duration_since(state.last_sent_at).as_secs_f64();
            if dt_sent > 0.0 {
                let delta_bytes = state.sent_bytes.saturating_sub(state.last_sent_bytes);
                if delta_bytes > 0 {
                    state.measured_bps = (delta_bytes as f64 * 8.0) / dt_sent;
                    state.has_traffic = true;
                }
                state.last_sent_bytes = state.sent_bytes;
                state.last_sent_at = now;
            }

            state.metrics.observed_bps = state.measured_bps;
            state.metrics.observed_bytes = state.sent_bytes;

            // Calculate spare capacity: only when we have observed traffic,
            // otherwise spare is 0 to prevent premature duplication at startup
            if state.has_traffic {
                state.spare_capacity_bps =
                    (state.metrics.capacity_bps - state.measured_bps).max(0.0);
            } else {
                state.spare_capacity_bps = 0.0;
            }

            if state.metrics.observed_bps > 0.0 {
                state.metrics.alive = true;
            }

            if state.metrics.capacity_bps < 1_000_000.0 {
                // Use configured capacity floor for bootstrap
                if state.measured_bps > (capacity_floor * 0.3) {
                    state.metrics.capacity_bps = state.measured_bps * 2.0;
                } else {
                    state.metrics.capacity_bps = capacity_floor;
                }
            }
            let prev_capacity = state.prev_capacity_bps;
            let curr_capacity = state.metrics.capacity_bps;
            if prev_capacity > 0.0 && curr_capacity < prev_capacity * 0.5 {
                state.penalty_factor = (state.penalty_factor * penalty_decay).max(0.3);
            } else {
                state.penalty_factor = (state.penalty_factor + penalty_recovery).min(1.0);
            }

            let dt = now.duration_since(state.last_metrics_update).as_secs_f64();
            if dt > 0.0 {
                state.bw_slope_bps_s = (curr_capacity - state.prev_capacity_bps) / dt;
                state.rtt_slope_ms_s = (state.metrics.rtt_ms - state.prev_rtt_ms) / dt;
                state.loss_slope_per_s = (state.metrics.loss_rate - state.prev_loss_rate) / dt;
            }

            state.prev_capacity_bps = curr_capacity;
            state.prev_rtt_ms = state.metrics.rtt_ms;
            state.prev_loss_rate = state.metrics.loss_rate;
            state.last_metrics_update = now;
        }
    }

    pub fn remove_link(&mut self, id: usize) {
        self.links.remove(&id);
        if let Some(pos) = self.sorted_ids.iter().position(|&x| x == id) {
            self.sorted_ids.remove(pos);
        }
        // Reset RR index if out of bounds
        if self.current_rr_idx >= self.sorted_ids.len() {
            self.current_rr_idx = 0;
        }
    }

    pub fn get_active_links(&self) -> Vec<(usize, crate::net::interface::LinkMetrics)> {
        self.links
            .iter()
            .map(|(id, l)| (*id, l.metrics.clone()))
            .collect()
    }

    pub fn record_send(&mut self, id: usize, bytes: u64) {
        if let Some(state) = self.links.get_mut(&id) {
            state.sent_bytes = state.sent_bytes.saturating_add(bytes);
            state.has_traffic = true;
        }
    }

    /// Mark a link as having traffic (for testing). In production, this is set
    /// automatically when bytes are sent or observed.
    pub fn mark_has_traffic(&mut self, id: usize) {
        if let Some(state) = self.links.get_mut(&id) {
            state.has_traffic = true;
        }
    }

    /// Returns the total spare capacity (unused bandwidth) across all Live/Warm links.
    /// This is used for calculating the redundancy budget.
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

    /// Returns all alive links and deducts the cost from their credits.
    /// This is used for broadcasting critical packets.
    pub fn broadcast_links(&mut self, packet_len: usize) -> Vec<Arc<L>> {
        let packet_cost = packet_len as f64;
        let mut alive_links = Vec::new();

        let any_alive = self.links.values().any(|state| state.metrics.alive);

        for state in self.links.values_mut() {
            if state.metrics.alive || !any_alive {
                state.credits -= packet_cost;
                alive_links.push(state.link.clone());
            }
        }
        alive_links
    }

    /// Selects the best N links with diversity preference.
    /// Prefers links from different carriers/interfaces (link_kind) to maximize path independence.
    pub fn select_best_n_links(&mut self, packet_len: usize, n: usize) -> Vec<Arc<L>> {
        let packet_cost = packet_len as f64;
        let mut selected = Vec::new();
        let mut used_kinds: std::collections::HashSet<String> = std::collections::HashSet::new();

        // Score all alive links
        let mut scored_links: Vec<_> = self
            .links
            .iter()
            .filter(|(_, state)| state.metrics.alive)
            .map(|(id, state)| {
                // Quality score: capacity * (loss_quality * 0.5 + rtt_quality * 0.3 + phase * 0.2)
                let loss_quality = (1.0 - state.metrics.loss_rate).max(0.0);
                let rtt_quality = 1.0 / (1.0 + state.metrics.rtt_ms / 100.0);
                let phase_weight = match state.metrics.phase {
                    LinkPhase::Live => 1.0,
                    LinkPhase::Warm => 0.8,
                    LinkPhase::Degrade => 0.5,
                    LinkPhase::Probe => 0.3,
                    _ => 0.1,
                };

                let quality_score = state.metrics.capacity_bps
                    * (loss_quality * 0.5 + rtt_quality * 0.3 + phase_weight * 0.2);

                (*id, quality_score, state.metrics.link_kind.clone())
            })
            .collect();

        // Sort by quality score descending
        scored_links.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        // Select up to N links, preferring diversity
        for (id, _score, link_kind) in &scored_links {
            if selected.len() >= n {
                break;
            }

            // Diversity preference: prefer new link_kind if we have multiple links
            let is_diverse = match link_kind {
                None => true, // Unknown kind is always considered diverse
                Some(kind) => !used_kinds.contains(kind.as_str()),
            };

            // Always select if we haven't reached N, but prefer diverse links first
            if is_diverse || selected.len() < n {
                if let Some(state) = self.links.get_mut(id) {
                    state.credits -= packet_cost;
                    selected.push(state.link.clone());
                    if let Some(kind) = link_kind {
                        used_kinds.insert(kind.clone());
                    }
                }
            }
        }

        // If we couldn't get N diverse links, fill with remaining best quality links
        if selected.len() < n {
            for (id, _score, _) in &scored_links {
                if selected.len() >= n {
                    break;
                }
                if !selected.iter().any(|l| l.id() == *id) {
                    if let Some(state) = self.links.get_mut(id) {
                        state.credits -= packet_cost;
                        selected.push(state.link.clone());
                    }
                }
            }
        }

        selected
    }

    pub fn select_link(&mut self, packet_len: usize) -> Option<Arc<L>> {
        if self.sorted_ids.is_empty() {
            return None;
        }

        let packet_cost = packet_len as f64;
        let now = Instant::now();
        let horizon_s = self.config.prediction_horizon_s;

        // 1. Update Credits
        let any_alive = self.links.values().any(|state| state.metrics.alive);
        for state in self.links.values_mut() {
            let metrics = state.metrics.clone();
            if metrics.alive || !any_alive {
                let elapsed = now.duration_since(state.last_update).as_secs_f64();

                // Calculate Effective Capacity (Quality Aware)
                let predicted_bw =
                    (metrics.capacity_bps + state.bw_slope_bps_s * horizon_s).max(0.0);
                let predicted_loss =
                    (metrics.loss_rate + state.loss_slope_per_s * horizon_s).clamp(0.0, 1.0);
                let predicted_rtt = (metrics.rtt_ms + state.rtt_slope_ms_s * horizon_s).max(0.0);

                let quality_factor = (1.0 - predicted_loss).powi(4);
                let rtt_factor = 1.0 / (1.0 + predicted_rtt / 200.0);

                let phase_factor = match metrics.phase {
                    LinkPhase::Probe => 0.2,
                    LinkPhase::Warm => 0.6,
                    LinkPhase::Live => 1.0,
                    LinkPhase::Degrade => 0.7,
                    LinkPhase::Cooldown | LinkPhase::Reset | LinkPhase::Init => 0.1,
                };

                let os_up_factor = if matches!(metrics.os_up, Some(false)) {
                    0.2
                } else {
                    1.0
                };

                let effective_bps = predicted_bw
                    * quality_factor
                    * state.penalty_factor
                    * phase_factor
                    * os_up_factor
                    * rtt_factor;
                // Capacity is in bps (bits per sec). Convert to bytes per sec.
                let bytes_per_sec = effective_bps / 8.0;

                // Add credits
                state.credits += bytes_per_sec * elapsed;

                // Cap credits (adaptive burst window based on phase/loss)
                let burst_window_s = compute_burst_window_s(metrics.phase, predicted_loss);
                let max_credits = bytes_per_sec * burst_window_s;
                if state.credits > max_credits {
                    state.credits = max_credits;
                }
            }
            state.last_update = now;
        }

        // 2. Select Link (DWRR)
        let start_idx = self.current_rr_idx;
        let count = self.sorted_ids.len();

        for i in 0..count {
            let idx = (start_idx + i) % count;
            let id = self.sorted_ids[idx];

            if let Some(state) = self.links.get_mut(&id) {
                let metrics = state.metrics.clone();
                if !metrics.alive {
                    continue;
                }

                if state.credits >= packet_cost {
                    state.credits -= packet_cost;
                    self.current_rr_idx = (idx + 1) % count;
                    return Some(state.link.clone());
                }
            }
        }

        // Fallback: Pick link with max credits (best effort)
        let mut best_id = None;
        let mut max_creds = f64::MIN;

        for &id in &self.sorted_ids {
            if let Some(state) = self.links.get(&id) {
                if state.metrics.alive && state.credits > max_creds {
                    max_creds = state.credits;
                    best_id = Some(id);
                }
            }
        }

        if let Some(id) = best_id {
            if let Some(state) = self.links.get_mut(&id) {
                state.credits -= packet_cost; // Goes negative
                return Some(state.link.clone());
            }
        }

        None
    }
}

impl<L: LinkSender + ?Sized> Default for Dwrr<L> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::interface::LinkMetrics;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    struct MockLink {
        id: usize,
        metrics: Mutex<LinkMetrics>,
    }

    impl MockLink {
        fn new(id: usize, capacity_bps: f64, phase: LinkPhase) -> Self {
            Self {
                id,
                metrics: Mutex::new(LinkMetrics {
                    rtt_ms: 10.0,
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
                }),
            }
        }

        fn set_capacity(&self, capacity_bps: f64) {
            if let Ok(mut m) = self.metrics.lock() {
                m.capacity_bps = capacity_bps;
            }
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
    fn penalty_factor_reacts_to_capacity_drop() {
        let link = Arc::new(MockLink::new(1, 10_000_000.0, LinkPhase::Live)); // Start at 10M
        let mut dwrr = Dwrr::new();
        dwrr.add_link(link.clone());

        dwrr.refresh_metrics();
        let penalty = dwrr.links.get(&1).unwrap().penalty_factor;
        assert!((penalty - 1.0).abs() < 1e-6);

        // Drop capacity to 4M (< 50% of 10M, but > 1M so not bootstrapped)
        link.set_capacity(4_000_000.0);
        dwrr.refresh_metrics();
        let penalty = dwrr.links.get(&1).unwrap().penalty_factor;
        assert!((penalty - 0.7).abs() < 0.01); // Penalty should be 1.0 * 0.7 = 0.7
    }

    #[test]
    fn warmup_phase_reduces_credit_growth() {
        let live = Arc::new(MockLink::new(1, 1_000_000.0, LinkPhase::Live));
        let probe = Arc::new(MockLink::new(2, 1_000_000.0, LinkPhase::Probe));

        let mut dwrr = Dwrr::new();
        dwrr.add_link(live.clone());
        dwrr.add_link(probe.clone());

        // Force metrics refresh and provide elapsed time for credit accrual
        dwrr.refresh_metrics();
        if let Some(state) = dwrr.links.get_mut(&1) {
            state.last_update -= Duration::from_secs(1);
        }
        if let Some(state) = dwrr.links.get_mut(&2) {
            state.last_update -= Duration::from_secs(1);
        }

        let _ = dwrr.select_link(1200);
        let live_credits = dwrr.links.get(&1).unwrap().credits;
        let probe_credits = dwrr.links.get(&2).unwrap().credits;

        assert!(live_credits >= probe_credits);
    }

    #[test]
    fn burst_window_scales_by_phase() {
        let live = Arc::new(MockLink::new(1, 1_000_000.0, LinkPhase::Live));
        let probe = Arc::new(MockLink::new(2, 1_000_000.0, LinkPhase::Probe));

        let mut dwrr = Dwrr::new();
        dwrr.add_link(live.clone());
        dwrr.add_link(probe.clone());

        if let Some(state) = dwrr.links.get_mut(&1) {
            state.last_update -= Duration::from_secs(1);
        }
        if let Some(state) = dwrr.links.get_mut(&2) {
            state.last_update -= Duration::from_secs(1);
        }

        let _ = dwrr.select_link(1);

        let live_state = dwrr.links.get(&1).unwrap();
        let probe_state = dwrr.links.get(&2).unwrap();

        let live_burst = compute_burst_window_s(LinkPhase::Live, 0.0);
        let probe_burst = compute_burst_window_s(LinkPhase::Probe, 0.0);

        let rtt_factor = 1.0 / (1.0 + 10.0 / 200.0);
        let live_max = (1_000_000.0 * rtt_factor / 8.0) * live_burst;
        let probe_max = (1_000_000.0 * rtt_factor * 0.2 / 8.0) * probe_burst;

        assert!(live_state.credits <= live_max + 1.0);
        assert!(probe_state.credits <= probe_max + 1.0);
    }

    #[test]
    fn test_predictive_scoring_prefers_positive_bw_trend() {
        let link1 = Arc::new(MockLink::new(1, 1_000_000.0, LinkPhase::Live));
        let link2 = Arc::new(MockLink::new(2, 1_000_000.0, LinkPhase::Live));

        let mut dwrr = Dwrr::new();
        dwrr.add_link(link1.clone());
        dwrr.add_link(link2.clone());

        let now = Instant::now();
        if let Some(state) = dwrr.links.get_mut(&1) {
            state.prev_capacity_bps = 700_000.0;
            state.prev_rtt_ms = state.metrics.rtt_ms;
            state.prev_loss_rate = state.metrics.loss_rate;
            state.last_metrics_update = now - Duration::from_secs(1);
        }
        if let Some(state) = dwrr.links.get_mut(&2) {
            state.prev_capacity_bps = 1_300_000.0;
            state.prev_rtt_ms = state.metrics.rtt_ms;
            state.prev_loss_rate = state.metrics.loss_rate;
            state.last_metrics_update = now - Duration::from_secs(1);
        }

        dwrr.refresh_metrics();

        if let Some(state) = dwrr.links.get_mut(&1) {
            state.last_update = Instant::now() - Duration::from_secs(1);
        }
        if let Some(state) = dwrr.links.get_mut(&2) {
            state.last_update = Instant::now() - Duration::from_secs(1);
        }

        let _ = dwrr.select_link(0);
        let link1_credits = dwrr.links.get(&1).unwrap().credits;
        let link2_credits = dwrr.links.get(&2).unwrap().credits;

        assert!(link1_credits > link2_credits);
    }

    #[test]
    fn test_spare_capacity_calculation() {
        let link = Arc::new(MockLink::new(1, 10_000_000.0, LinkPhase::Live));
        let mut dwrr = Dwrr::new();
        dwrr.add_link(link.clone());

        // Initial state - no traffic yet, spare should be 0 (not full capacity)
        dwrr.refresh_metrics();
        let state = dwrr.links.get(&1).unwrap();
        assert_eq!(
            state.spare_capacity_bps, 0.0,
            "spare should be 0 before any traffic"
        );

        // Simulate observed traffic at 6 Mbps (mark has_traffic)
        if let Some(state) = dwrr.links.get_mut(&1) {
            state.measured_bps = 6_000_000.0;
            state.has_traffic = true;
        }
        dwrr.refresh_metrics();

        let state = dwrr.links.get(&1).unwrap();
        assert_eq!(state.spare_capacity_bps, 4_000_000.0); // 10M - 6M
    }

    #[test]
    fn test_total_spare_capacity() {
        let link1 = Arc::new(MockLink::new(1, 10_000_000.0, LinkPhase::Live));
        let link2 = Arc::new(MockLink::new(2, 5_000_000.0, LinkPhase::Live));
        let link3 = Arc::new(MockLink::new(3, 8_000_000.0, LinkPhase::Probe));

        let mut dwrr = Dwrr::new();
        dwrr.add_link(link1.clone());
        dwrr.add_link(link2.clone());
        dwrr.add_link(link3.clone());

        // Set observed traffic and mark as having traffic
        if let Some(state) = dwrr.links.get_mut(&1) {
            state.measured_bps = 7_000_000.0; // 3M spare
            state.has_traffic = true;
        }
        if let Some(state) = dwrr.links.get_mut(&2) {
            state.measured_bps = 3_000_000.0; // 2M spare
            state.has_traffic = true;
        }
        if let Some(state) = dwrr.links.get_mut(&3) {
            state.measured_bps = 1_000_000.0; // 7M spare but link is Probe
            state.has_traffic = true;
        }

        dwrr.refresh_metrics();

        let total_spare = dwrr.total_spare_capacity();
        // Only Link1 (3M) + Link2 (2M) = 5M (Link3 excluded as Probe phase)
        assert_eq!(total_spare, 5_000_000.0);
    }

    #[test]
    fn test_diversity_aware_link_selection() {
        // Create links with different kinds
        let wifi_link = Arc::new(MockLink::new(1, 10_000_000.0, LinkPhase::Live));
        let cellular_link = Arc::new(MockLink::new(2, 8_000_000.0, LinkPhase::Live));
        let wired_link = Arc::new(MockLink::new(3, 12_000_000.0, LinkPhase::Live));

        // Set link_kind for diversity
        if let Ok(mut m) = wifi_link.metrics.lock() {
            m.link_kind = Some("wifi".to_string());
        }
        if let Ok(mut m) = cellular_link.metrics.lock() {
            m.link_kind = Some("cellular".to_string());
        }
        if let Ok(mut m) = wired_link.metrics.lock() {
            m.link_kind = Some("wired".to_string());
        }

        let mut dwrr = Dwrr::new();
        dwrr.add_link(wifi_link.clone());
        dwrr.add_link(cellular_link.clone());
        dwrr.add_link(wired_link.clone());

        dwrr.refresh_metrics();

        // Select best 2 links - should prefer diverse kinds
        let selected = dwrr.select_best_n_links(1000, 2);
        assert_eq!(selected.len(), 2);

        // Should get wired (highest capacity) and wifi (second highest)
        let ids: Vec<usize> = selected.iter().map(|l| l.id()).collect();
        assert!(ids.contains(&3)); // Wired should be selected
        assert!(ids.len() == 2); // Got 2 links
    }

    #[test]
    fn select_link_with_zero_links() {
        let mut dwrr: Dwrr<MockLink> = Dwrr::new();
        assert!(dwrr.select_link(1200).is_none());
    }

    #[test]
    fn select_link_with_all_dead_links() {
        let link1 = Arc::new(MockLink::new(1, 10_000_000.0, LinkPhase::Live));
        let link2 = Arc::new(MockLink::new(2, 5_000_000.0, LinkPhase::Live));

        if let Ok(mut m) = link1.metrics.lock() {
            m.alive = false;
        }
        if let Ok(mut m) = link2.metrics.lock() {
            m.alive = false;
        }

        let mut dwrr = Dwrr::new();
        dwrr.add_link(link1);
        dwrr.add_link(link2);
        dwrr.refresh_metrics();

        // Dead links are skipped in both main loop and fallback
        let result = dwrr.select_link(1200);
        assert!(result.is_none(), "All dead links should return None");
    }

    #[test]
    fn broadcast_links_with_no_alive() {
        let link = Arc::new(MockLink::new(1, 10_000_000.0, LinkPhase::Live));
        if let Ok(mut m) = link.metrics.lock() {
            m.alive = false;
        }

        let mut dwrr = Dwrr::new();
        dwrr.add_link(link);
        dwrr.refresh_metrics();

        let links = dwrr.broadcast_links(100);
        assert_eq!(links.len(), 1);
    }

    #[test]
    fn broadcast_links_empty_scheduler() {
        let mut dwrr: Dwrr<MockLink> = Dwrr::new();
        let links = dwrr.broadcast_links(100);
        assert!(links.is_empty());
    }

    #[test]
    fn record_send_tracks_bytes() {
        let link = Arc::new(MockLink::new(1, 10_000_000.0, LinkPhase::Live));
        let mut dwrr = Dwrr::new();
        dwrr.add_link(link);

        dwrr.record_send(1, 1500);
        assert_eq!(dwrr.links.get(&1).unwrap().sent_bytes, 1500);

        dwrr.record_send(1, 1000);
        assert_eq!(dwrr.links.get(&1).unwrap().sent_bytes, 2500);
    }

    #[test]
    fn record_send_nonexistent_link() {
        let mut dwrr: Dwrr<MockLink> = Dwrr::new();
        dwrr.record_send(999, 1500); // Should not panic
    }

    #[test]
    fn remove_link_resets_rr_index() {
        let link1 = Arc::new(MockLink::new(1, 10_000_000.0, LinkPhase::Live));
        let link2 = Arc::new(MockLink::new(2, 5_000_000.0, LinkPhase::Live));
        let link3 = Arc::new(MockLink::new(3, 8_000_000.0, LinkPhase::Live));

        let mut dwrr = Dwrr::new();
        dwrr.add_link(link1);
        dwrr.add_link(link2);
        dwrr.add_link(link3);

        assert_eq!(dwrr.sorted_ids.len(), 3);

        dwrr.remove_link(2);
        assert_eq!(dwrr.sorted_ids.len(), 2);
        assert!(!dwrr.sorted_ids.contains(&2));
        assert!(dwrr.current_rr_idx < dwrr.sorted_ids.len());
    }

    #[test]
    fn remove_nonexistent_link() {
        let mut dwrr: Dwrr<MockLink> = Dwrr::new();
        dwrr.remove_link(999); // Should not panic
    }

    #[test]
    fn os_down_reduces_credits() {
        let link1 = Arc::new(MockLink::new(1, 10_000_000.0, LinkPhase::Live));
        let link2 = Arc::new(MockLink::new(2, 10_000_000.0, LinkPhase::Live));

        if let Ok(mut m) = link1.metrics.lock() {
            m.os_up = Some(false);
        }

        let mut dwrr = Dwrr::new();
        dwrr.add_link(link1.clone());
        dwrr.add_link(link2.clone());
        dwrr.refresh_metrics();

        for state in dwrr.links.values_mut() {
            state.last_update -= Duration::from_secs(1);
        }

        let _ = dwrr.select_link(0);
        let link1_credits = dwrr.links.get(&1).unwrap().credits;
        let link2_credits = dwrr.links.get(&2).unwrap().credits;

        assert!(
            link2_credits > link1_credits,
            "Link with os_up=false should have fewer credits ({} vs {})",
            link1_credits,
            link2_credits
        );
    }

    #[test]
    fn get_active_links_returns_all() {
        let link = Arc::new(MockLink::new(1, 10_000_000.0, LinkPhase::Live));
        let mut dwrr = Dwrr::new();
        dwrr.add_link(link);

        let links = dwrr.get_active_links();
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].0, 1);
    }

    #[test]
    fn update_config_applies() {
        let mut dwrr: Dwrr<MockLink> = Dwrr::new();
        assert!(dwrr.config().redundancy_enabled);

        let new_cfg = SchedulerConfig {
            redundancy_enabled: false,
            ..SchedulerConfig::default()
        };
        dwrr.update_config(new_cfg);
        assert!(!dwrr.config().redundancy_enabled);
    }
}
