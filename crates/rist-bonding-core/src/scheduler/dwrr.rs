use crate::net::interface::{LinkMetrics, LinkPhase, LinkSender};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

pub(crate) struct LinkState<L: ?Sized> {
    pub link: Arc<L>,
    pub credits: f64,
    pub last_update: Instant,
    pub metrics: LinkMetrics,
    pub prev_capacity_bps: f64,
    pub prev_rtt_ms: f64,
    pub prev_loss_rate: f64,
    pub bw_slope_bps_s: f64,
    pub rtt_slope_ms_s: f64,
    pub loss_slope_per_s: f64,
    pub last_metrics_update: Instant,
    pub penalty_factor: f64,
}

pub struct Dwrr<L: LinkSender + ?Sized> {
    links: HashMap<usize, LinkState<L>>,
    sorted_ids: Vec<usize>,
    current_rr_idx: usize,
}

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
        Self {
            links: HashMap::new(),
            sorted_ids: Vec::new(),
            current_rr_idx: 0,
        }
    }

    pub fn add_link(&mut self, link: Arc<L>) {
        let id = link.id();
        let metrics = link.get_metrics();
        self.links.insert(
            id,
            LinkState {
                metrics: metrics.clone(),
                link,
                credits: 0.0,
                last_update: Instant::now(),
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
        for state in self.links.values_mut() {
            let now = Instant::now();
            state.metrics = state.link.get_metrics();
            let prev_capacity = state.prev_capacity_bps;
            let curr_capacity = state.metrics.capacity_bps;
            if prev_capacity > 0.0 && curr_capacity < prev_capacity * 0.5 {
                state.penalty_factor = (state.penalty_factor * 0.7).max(0.3);
            } else {
                state.penalty_factor = (state.penalty_factor + 0.05).min(1.0);
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

    /// Returns all alive links and deducts the cost from their credits.
    /// This is used for broadcasting critical packets.
    pub fn broadcast_links(&mut self, packet_len: usize) -> Vec<Arc<L>> {
        let packet_cost = packet_len as f64;
        let mut alive_links = Vec::new();

        for state in self.links.values_mut() {
            if state.metrics.alive {
                state.credits -= packet_cost;
                alive_links.push(state.link.clone());
            }
        }
        alive_links
    }

    pub fn select_link(&mut self, packet_len: usize) -> Option<Arc<L>> {
        if self.sorted_ids.is_empty() {
            return None;
        }

        let packet_cost = packet_len as f64;
        let now = Instant::now();

        // 1. Update Credits
        for state in self.links.values_mut() {
            let metrics = state.metrics.clone();
            if metrics.alive {
                let elapsed = now.duration_since(state.last_update).as_secs_f64();

                // Calculate Effective Capacity (Quality Aware)
                // Penalty for loss: (1.0 - loss_rate)^4 to aggressively penalize bad links.
                let horizon_s = 0.5;
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
                if state.metrics.alive {
                    if state.credits > max_creds {
                        max_creds = state.credits;
                        best_id = Some(id);
                    }
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
        let link = Arc::new(MockLink::new(1, 1_000_000.0, LinkPhase::Live));
        let mut dwrr = Dwrr::new();
        dwrr.add_link(link.clone());

        dwrr.refresh_metrics();
        let penalty = dwrr.links.get(&1).unwrap().penalty_factor;
        assert!((penalty - 1.0).abs() < 1e-6);

        link.set_capacity(400_000.0);
        dwrr.refresh_metrics();
        let penalty = dwrr.links.get(&1).unwrap().penalty_factor;
        assert!((penalty - 0.7).abs() < 1e-6);
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
            state.last_update = state.last_update - Duration::from_secs(1);
        }
        if let Some(state) = dwrr.links.get_mut(&2) {
            state.last_update = state.last_update - Duration::from_secs(1);
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
            state.last_update = state.last_update - Duration::from_secs(1);
        }
        if let Some(state) = dwrr.links.get_mut(&2) {
            state.last_update = state.last_update - Duration::from_secs(1);
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
}
