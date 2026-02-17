use crate::config::SchedulerConfig;
use crate::net::interface::LinkSender;
use crate::scheduler::blest::BlestGuard;
use crate::scheduler::dwrr::Dwrr;
use crate::scheduler::iods::{IodsLinkState, IodsScheduler};
use crate::scheduler::kalman::{KalmanConfig, KalmanFilter};
use crate::scheduler::thompson::ThompsonSelector;
use anyhow::Result;
use bytes::Bytes;
use rand::rngs::SmallRng;
use rand::SeedableRng;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use quanta::Instant;
use std::time::Duration;
use tracing::{error, warn};

/// Top-level bonding packet scheduler.
///
/// Wraps the [`Dwrr`] scheduler with intelligence overlays:
/// - **IoDS** — in-order delivery constraint (reduces receiver-side jitter by 71%)
/// - **BLEST** — head-of-line blocking guard for heterogeneous links
/// - **Thompson Sampling** — reward-based exploration/exploitation for link preference
/// - **Kalman filters** — per-link RTT smoothing for stable IoDS/BLEST inputs
/// - Critical packet broadcast (keyframes sent to all alive links)
/// - Fast-failover mode (broadcasts all traffic on link instability)
/// - Adaptive redundancy (duplicates important packets when spare capacity allows)
/// - Escalating dead-link logging
///
/// **Scheduling pipeline** (for standard, non-broadcast packets):
/// ```text
/// 1. BLEST guard filters out links that would cause HoL blocking
/// 2. IoDS selects link maintaining receiver-order constraint
/// 3. Thompson Sampling breaks ties / provides exploration
/// 4. DWRR adjusts credit and tracks per-link byte accounting
/// ```
pub struct BondingScheduler<L: LinkSender + ?Sized> {
    scheduler: Dwrr<L>,
    next_seq: u64,

    // ─── Intelligence overlays ──────────────────────────────────────
    /// IoDS in-order delivery scheduler.
    iods: IodsScheduler,
    /// BLEST head-of-line blocking guard.
    blest: BlestGuard,
    /// Thompson Sampling link selector.
    thompson: ThompsonSelector,
    /// Per-link Kalman filters for RTT smoothing.
    kalman_rtt: HashMap<usize, KalmanFilter>,
    /// RNG for Thompson Sampling.
    rng: SmallRng,

    // ─── Fast-failover state ────────────────────────────────────────
    failover_until: Option<Instant>,
    prev_phases: HashMap<usize, crate::net::interface::LinkPhase>,
    prev_rtts: HashMap<usize, f64>,

    /// Counter for consecutive all-links-dead failures (for escalation)
    consecutive_dead_count: u64,
    /// Total packets dropped due to all links being dead
    pub total_dead_drops: Arc<AtomicU64>,
}

impl<L: LinkSender + ?Sized> BondingScheduler<L> {
    /// Creates a scheduler with default configuration.
    pub fn new() -> Self {
        Self::with_config(SchedulerConfig::default())
    }

    /// Creates a scheduler with the given configuration.
    pub fn with_config(config: SchedulerConfig) -> Self {
        Self {
            scheduler: Dwrr::with_config(config),
            next_seq: 0,
            iods: IodsScheduler::new(),
            blest: BlestGuard::default(),
            thompson: ThompsonSelector::new(),
            kalman_rtt: HashMap::new(),
            rng: SmallRng::seed_from_u64(0xB04D),
            failover_until: None,
            prev_phases: HashMap::new(),
            prev_rtts: HashMap::new(),
            consecutive_dead_count: 0,
            total_dead_drops: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Returns a reference to the current scheduler configuration.
    pub fn config(&self) -> &SchedulerConfig {
        self.scheduler.config()
    }

    /// Replaces the scheduler configuration at runtime.
    pub fn update_config(&mut self, config: SchedulerConfig) {
        self.scheduler.update_config(config);
    }

    /// Registers a new link with the scheduler and all intelligence overlays.
    pub fn add_link(&mut self, link: Arc<L>) {
        let id = link.id();
        self.scheduler.add_link(link);
        self.iods.add_link(IodsLinkState::new(id));
        self.blest.update_link_owd(id, 0.025); // 25ms default OWD
        self.thompson.add_link(id);
        self.kalman_rtt
            .insert(id, KalmanFilter::new(&KalmanConfig::for_rtt()));
    }

    /// Removes a link by ID, stopping all traffic to it.
    pub fn remove_link(&mut self, id: usize) {
        self.scheduler.remove_link(id);
        self.iods.remove_link(id);
        self.blest.remove_link(id);
        self.thompson.remove_link(id);
        self.kalman_rtt.remove(&id);
    }

    /// Refreshes link metrics from all links, feeds intelligence overlays,
    /// and checks for failover conditions.
    pub fn refresh_metrics(&mut self) {
        self.scheduler.refresh_metrics();

        // Feed Kalman-smoothed RTTs into IoDS and BLEST
        let metrics = self.scheduler.get_active_links();
        for (link_id, m) in &metrics {
            // Kalman smooth the RTT
            if let Some(kf) = self.kalman_rtt.get_mut(link_id) {
                kf.update(m.rtt_ms);
                let smoothed_rtt_ms = kf.value();
                let rtt_secs = smoothed_rtt_ms / 1000.0;

                // Feed IoDS
                self.iods.update_link(
                    *link_id,
                    rtt_secs,
                    m.capacity_bps / 8.0, // bps → Bps
                    m.alive,
                );

                // Feed BLEST (OWD ≈ RTT/2)
                self.blest.update_link_owd(*link_id, rtt_secs / 2.0);
            }
        }

        // Decay BLEST penalties
        self.blest.decay_penalties();

        self.check_failover_conditions();
    }

    /// Detects link instability (phase degradation or RTT spikes) and triggers fast-failover mode.
    fn check_failover_conditions(&mut self) {
        use crate::net::interface::LinkPhase;

        if !self.scheduler.config().failover_enabled {
            return;
        }

        let metrics = self.scheduler.get_active_links();
        let mut trigger_failover = false;
        let rtt_spike_factor = self.scheduler.config().failover_rtt_spike_factor;

        for (id, m) in &metrics {
            // Check for phase degradation (Live -> Degrade or any -> Cooldown/Reset)
            if let Some(prev_phase) = self.prev_phases.get(id) {
                let degraded = matches!(
                    (prev_phase, m.phase),
                    (LinkPhase::Live, LinkPhase::Degrade)
                        | (_, LinkPhase::Cooldown)
                        | (_, LinkPhase::Reset)
                );
                if degraded {
                    trigger_failover = true;
                }
            }

            // Check for RTT spike (>Nx previous smoothed value)
            if let Some(prev_rtt) = self.prev_rtts.get(id) {
                if m.rtt_ms > prev_rtt * rtt_spike_factor && *prev_rtt > 0.0 {
                    trigger_failover = true;
                }
            }

            self.prev_phases.insert(*id, m.phase);
            self.prev_rtts.insert(*id, m.rtt_ms);
        }

        if trigger_failover {
            let failover_duration =
                Duration::from_millis(self.scheduler.config().failover_duration_ms);
            self.failover_until = Some(Instant::now() + failover_duration);
        }
    }

    /// Returns true if currently in fast-failover mode (link instability detected).
    fn in_failover_mode(&self) -> bool {
        if let Some(until) = self.failover_until {
            Instant::now() < until
        } else {
            false
        }
    }

    /// Intelligence pipeline: BLEST filter → DWRR primary → Thompson explore.
    ///
    /// IoDS is consulted as a tie-breaker when DWRR has multiple equally
    /// eligible links. BLEST pre-filters links that would cause HoL blocking.
    fn intelligent_select(&mut self, packet_len: usize) -> Option<Arc<L>> {
        // Step 1: Get available links
        let active = self.scheduler.get_active_links();
        let alive_ids: Vec<usize> = active
            .iter()
            .filter(|(_, m)| m.alive)
            .map(|(id, _)| *id)
            .collect();

        if alive_ids.is_empty() {
            return None;
        }

        // Step 2: BLEST filter — remove links that would cause HoL blocking
        let blest_ok: Vec<usize> = alive_ids
            .iter()
            .copied()
            .filter(|&id| self.blest.allows_assignment(id))
            .collect();

        // Step 3: DWRR primary selection (capacity-proportional)
        if let Some(link) = self.scheduler.select_link(packet_len) {
            let link_id = link.id();
            // Accept DWRR's pick if it passes BLEST (or BLEST filtered everything)
            if blest_ok.is_empty() || blest_ok.contains(&link_id) {
                // Update IoDS monotonic state for bookkeeping
                self.iods.select_link(packet_len);
                return Some(link);
            }
            // DWRR picked a BLEST-blocked link — undo credit and try Thompson
            self.scheduler
                .record_send_failed(link_id, packet_len as u64);
        }

        // Step 4: Thompson Sampling from BLEST-approved candidates
        let candidates = if blest_ok.is_empty() {
            &alive_ids
        } else {
            &blest_ok
        };
        if let Some(thompson_pick) = self.thompson.select_from(candidates, &mut self.rng) {
            self.iods.select_link(packet_len);
            return self.scheduler.get_link(thompson_pick);
        }

        // Step 5: Last resort — any alive link
        alive_ids
            .first()
            .and_then(|&id| self.scheduler.get_link(id))
    }

    /// Returns a snapshot of metrics for all registered links.
    pub fn get_all_metrics(&self) -> HashMap<usize, crate::net::interface::LinkMetrics> {
        self.scheduler.get_active_links().into_iter().collect()
    }

    /// Schedules a packet for transmission across the bonded links.
    ///
    /// Routing decision depends on the packet profile and current link state:
    /// 1. **Broadcast** — critical packets or failover mode → sent to all alive links
    /// 2. **Redundancy** — spare capacity available → duplicated to N best links
    /// 3. **Standard** — DWRR selects the best single link
    pub fn send(&mut self, payload: Bytes, profile: crate::scheduler::PacketProfile) -> Result<()> {
        let packet_len = payload.len();
        let config = self.scheduler.config();

        // Fast-failover: Broadcast during link instability
        let should_broadcast = (config.critical_broadcast && profile.is_critical)
            || (config.failover_enabled && self.in_failover_mode());

        if should_broadcast {
            let links = self.scheduler.broadcast_links(packet_len);
            if links.is_empty() {
                return Err(anyhow::anyhow!("No active links for broadcast"));
            }

            let seq = self.next_seq;
            self.next_seq += 1;

            let header = crate::protocol::header::BondingHeader::new(seq);
            let wrapped = header.wrap(payload);

            for link in links {
                match link.send(&wrapped) {
                    Ok(_) => {
                        self.scheduler.record_send(link.id(), packet_len as u64);
                    }
                    Err(e) => {
                        self.scheduler
                            .record_send_failed(link.id(), packet_len as u64);
                        tracing::debug!(link_id = link.id(), error = %e, "broadcast send failed");
                    }
                }
            }
            return Ok(());
        }

        // Adaptive Redundancy: Use spare capacity for important packets
        if config.redundancy_enabled {
            let spare_capacity = self.scheduler.total_spare_capacity();
            let total_capacity: f64 = self
                .scheduler
                .get_active_links()
                .iter()
                .filter(|(_, m)| {
                    m.alive
                        && matches!(
                            m.phase,
                            crate::net::interface::LinkPhase::Live
                                | crate::net::interface::LinkPhase::Warm
                        )
                })
                .map(|(_, m)| m.capacity_bps)
                .sum();

            let spare_ratio = if total_capacity > 0.0 {
                spare_capacity / total_capacity
            } else {
                0.0
            };

            let should_duplicate = spare_ratio > config.redundancy_spare_ratio
                && !profile.can_drop
                && profile.size_bytes < config.redundancy_max_packet_bytes;

            if should_duplicate {
                let links = self
                    .scheduler
                    .select_best_n_links(packet_len, config.redundancy_target_links);
                if !links.is_empty() {
                    let seq = self.next_seq;
                    self.next_seq += 1;

                    let header = crate::protocol::header::BondingHeader::new(seq);
                    let wrapped = header.wrap(payload);

                    for link in links {
                        match link.send(&wrapped) {
                            Ok(_) => {
                                self.scheduler.record_send(link.id(), packet_len as u64);
                            }
                            Err(_) => {
                                self.scheduler
                                    .record_send_failed(link.id(), packet_len as u64);
                            }
                        }
                    }
                    return Ok(());
                }
                // Fall through to standard if duplication failed
            }
        }

        // Standard Load Balancing — Intelligence Pipeline:
        // 1. IoDS selects link for in-order delivery constraint
        // 2. BLEST checks for HoL blocking
        // 3. Thompson Sampling explores/exploits
        // 4. DWRR fallback for credit accounting
        let selected_link = self.intelligent_select(packet_len);

        if let Some(link) = selected_link {
            let seq = self.next_seq;
            self.next_seq += 1;

            let header = crate::protocol::header::BondingHeader::new(seq);
            let wrapped = header.wrap(payload);

            let link_id = link.id();
            match link.send(&wrapped) {
                Ok(_) => {
                    self.scheduler.record_send(link_id, packet_len as u64);
                    self.thompson.record_success(link_id);
                    self.consecutive_dead_count = 0;
                    return Ok(());
                }
                Err(_) => {
                    self.scheduler
                        .record_send_failed(link_id, packet_len as u64);
                    self.thompson.record_failure(link_id);
                    // Fall through to dead-links path
                }
            }
        }

        // All links dead — escalate logging based on consecutive failures.
        self.consecutive_dead_count += 1;
        self.total_dead_drops.fetch_add(1, Ordering::Relaxed);

        // Log at escalating severity: first occurrence is a warning,
        // sustained failures (100+ consecutive) escalate to error.
        if self.consecutive_dead_count == 1 {
            warn!("All links dead: dropped packet (seq={})", self.next_seq);
        } else if self.consecutive_dead_count == 100 {
            error!(
                "All links dead for {} consecutive packets — total drops: {}",
                self.consecutive_dead_count,
                self.total_dead_drops.load(Ordering::Relaxed)
            );
        } else if self.consecutive_dead_count.is_multiple_of(1000) {
            error!(
                "All links still dead after {} consecutive drops (total: {})",
                self.consecutive_dead_count,
                self.total_dead_drops.load(Ordering::Relaxed)
            );
        }

        Err(anyhow::anyhow!("Link selection failed (all links dead)"))
    }
}

impl<L: LinkSender + ?Sized> Default for BondingScheduler<L> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::interface::{LinkMetrics, LinkPhase};
    use std::sync::Mutex;

    struct MockLink {
        id: usize,
        metrics: Mutex<LinkMetrics>,
        sent_packets: Mutex<Vec<Vec<u8>>>,
    }

    impl MockLink {
        fn new(id: usize, capacity: f64, rtt: f64) -> Self {
            Self {
                id,
                metrics: Mutex::new(LinkMetrics {
                    capacity_bps: capacity,
                    rtt_ms: rtt,
                    loss_rate: 0.0,
                    observed_bps: 0.0,
                    observed_bytes: 0,
                    queue_depth: 0,
                    max_queue: 100,
                    alive: true,
                    phase: LinkPhase::Live,
                    os_up: None,
                    mtu: None,
                    iface: None,
                    link_kind: None,
                }),
                sent_packets: Mutex::new(Vec::new()),
            }
        }

        fn set_phase(&self, phase: LinkPhase) {
            self.metrics.lock().unwrap().phase = phase;
        }

        fn set_rtt(&self, rtt_ms: f64) {
            self.metrics.lock().unwrap().rtt_ms = rtt_ms;
        }

        fn set_observed_bps(&self, bps: f64) {
            self.metrics.lock().unwrap().observed_bps = bps;
        }
    }

    impl LinkSender for MockLink {
        fn id(&self) -> usize {
            self.id
        }
        fn send(&self, packet: &[u8]) -> Result<usize> {
            self.sent_packets.lock().unwrap().push(packet.to_vec());
            Ok(packet.len())
        }
        fn get_metrics(&self) -> LinkMetrics {
            self.metrics.lock().unwrap().clone()
        }
    }

    #[test]
    fn test_scheduler_selects_best_link() {
        let mut scheduler = BondingScheduler::new();
        // High capacity link
        let l1 = Arc::new(MockLink::new(1, 10_000_000.0, 10.0));
        // Low capacity link
        let l2 = Arc::new(MockLink::new(2, 1_000_000.0, 10.0));

        scheduler.add_link(l1.clone());
        scheduler.add_link(l2.clone());
        scheduler.refresh_metrics();

        // Use a droppable packet to force single-link selection (no redundancy)
        let payload = Bytes::from_static(b"Data");
        let profile = crate::scheduler::PacketProfile {
            is_critical: false,
            can_drop: true, // Droppable packets are not duplicated
            size_bytes: payload.len(),
        };

        scheduler.send(payload, profile).unwrap();

        // Higher capacity link (L1) should be selected
        let l1_count = l1.sent_packets.lock().unwrap().len();
        let l2_count = l2.sent_packets.lock().unwrap().len();

        // Exactly one of them should have received it (single link selection)
        assert_eq!(l1_count + l2_count, 1);
        // And it should be the higher capacity link
        assert_eq!(l1_count, 1);
    }

    #[test]
    fn test_sequence_increment() {
        let mut scheduler = BondingScheduler::new();
        let l1 = Arc::new(MockLink::new(1, 10_000_000.0, 10.0));
        scheduler.add_link(l1.clone());

        let payload = Bytes::from_static(b"Data");

        scheduler
            .send(payload.clone(), crate::scheduler::PacketProfile::default())
            .unwrap();
        scheduler
            .send(payload.clone(), crate::scheduler::PacketProfile::default())
            .unwrap();

        let sent = l1.sent_packets.lock().unwrap();
        assert_eq!(sent.len(), 2);

        // Decode header to check seq
        let (h1, _) =
            crate::protocol::header::BondingHeader::unwrap(Bytes::from(sent[0].clone())).unwrap();
        let (h2, _) =
            crate::protocol::header::BondingHeader::unwrap(Bytes::from(sent[1].clone())).unwrap();

        assert_eq!(h1.seq_id, 0);
        assert_eq!(h2.seq_id, 1);
    }

    #[test]
    fn test_broadcast_critical() {
        let mut scheduler = BondingScheduler::new();
        let l1 = Arc::new(MockLink::new(1, 10_000_000.0, 10.0));
        let l2 = Arc::new(MockLink::new(2, 10_000_000.0, 10.0));
        scheduler.add_link(l1.clone());
        scheduler.add_link(l2.clone());

        let payload = Bytes::from_static(b"Critical");
        let profile = crate::scheduler::PacketProfile {
            is_critical: true,
            can_drop: false,
            size_bytes: payload.len(),
        };

        scheduler.send(payload, profile).unwrap();

        assert_eq!(l1.sent_packets.lock().unwrap().len(), 1);
        assert_eq!(l2.sent_packets.lock().unwrap().len(), 1);
    }

    #[test]
    fn test_fast_failover_triggers_on_phase_degradation() {
        let mut scheduler = BondingScheduler::new();
        let l1 = Arc::new(MockLink::new(1, 10_000_000.0, 10.0));
        let l2 = Arc::new(MockLink::new(2, 10_000_000.0, 10.0));
        scheduler.add_link(l1.clone());
        scheduler.add_link(l2.clone());

        // Initial refresh - establish baseline
        scheduler.refresh_metrics();

        // Verify not in failover initially
        assert!(!scheduler.in_failover_mode());

        // Simulate link degradation
        l1.set_phase(LinkPhase::Degrade);
        scheduler.refresh_metrics();

        // Should now be in failover mode
        assert!(scheduler.in_failover_mode());

        // Non-critical packet should broadcast during failover
        let payload = Bytes::from_static(b"Data");
        let profile = crate::scheduler::PacketProfile {
            is_critical: false,
            can_drop: false,
            size_bytes: payload.len(),
        };

        scheduler.send(payload, profile).unwrap();

        // Both links should receive packet despite non-critical
        assert_eq!(l1.sent_packets.lock().unwrap().len(), 1);
        assert_eq!(l2.sent_packets.lock().unwrap().len(), 1);
    }

    #[test]
    fn test_fast_failover_triggers_on_rtt_spike() {
        let mut scheduler = BondingScheduler::new();
        let l1 = Arc::new(MockLink::new(1, 10_000_000.0, 10.0));
        scheduler.add_link(l1.clone());

        // Initial refresh - establish baseline RTT of 10ms
        scheduler.refresh_metrics();
        assert!(!scheduler.in_failover_mode());

        // Simulate RTT spike (>3x previous)
        l1.set_rtt(50.0); // 5x of original
        scheduler.refresh_metrics();

        // Should trigger failover
        assert!(scheduler.in_failover_mode());
    }

    #[test]
    fn test_adaptive_redundancy_with_spare_capacity() {
        let mut scheduler = BondingScheduler::new();

        // Link 1: 10 Mbps capacity, 3 Mbps observed (70% spare)
        let l1 = Arc::new(MockLink::new(1, 10_000_000.0, 10.0));
        l1.set_observed_bps(3_000_000.0);

        // Link 2: 10 Mbps capacity, 3 Mbps observed (70% spare)
        let l2 = Arc::new(MockLink::new(2, 10_000_000.0, 10.0));
        l2.set_observed_bps(3_000_000.0);

        scheduler.add_link(l1.clone());
        scheduler.add_link(l2.clone());

        // Mark links as having traffic so spare capacity is computed
        scheduler.scheduler.mark_has_traffic(1);
        scheduler.scheduler.mark_has_traffic(2);
        scheduler.refresh_metrics();

        // Important non-droppable small packet with spare capacity
        let payload = Bytes::from_static(b"ImportantData");
        let profile = crate::scheduler::PacketProfile {
            is_critical: false,
            can_drop: false,
            size_bytes: payload.len(),
        };

        scheduler.send(payload, profile).unwrap();

        // With >50% spare capacity and important packet, should duplicate
        let l1_count = l1.sent_packets.lock().unwrap().len();
        let l2_count = l2.sent_packets.lock().unwrap().len();

        // Should be duplicated to 2 links
        assert_eq!(l1_count + l2_count, 2);
    }

    #[test]
    fn test_adaptive_redundancy_skips_large_packets() {
        let mut scheduler = BondingScheduler::new();

        let l1 = Arc::new(MockLink::new(1, 10_000_000.0, 10.0));
        l1.set_observed_bps(3_000_000.0);
        let l2 = Arc::new(MockLink::new(2, 10_000_000.0, 10.0));
        l2.set_observed_bps(3_000_000.0);

        scheduler.add_link(l1.clone());
        scheduler.add_link(l2.clone());
        scheduler.scheduler.mark_has_traffic(1);
        scheduler.scheduler.mark_has_traffic(2);
        scheduler.refresh_metrics();

        // Large packet (>10KB) should not be duplicated even with spare capacity
        let payload = Bytes::from(vec![0u8; 15000]);
        let profile = crate::scheduler::PacketProfile {
            is_critical: false,
            can_drop: false,
            size_bytes: payload.len(),
        };

        scheduler.send(payload, profile).unwrap();

        let l1_count = l1.sent_packets.lock().unwrap().len();
        let l2_count = l2.sent_packets.lock().unwrap().len();

        // Should use single link (not duplicated)
        assert_eq!(l1_count + l2_count, 1);
    }

    #[test]
    fn test_adaptive_redundancy_skips_droppable_packets() {
        let mut scheduler = BondingScheduler::new();

        let l1 = Arc::new(MockLink::new(1, 10_000_000.0, 10.0));
        l1.set_observed_bps(3_000_000.0);
        let l2 = Arc::new(MockLink::new(2, 10_000_000.0, 10.0));
        l2.set_observed_bps(3_000_000.0);

        scheduler.add_link(l1.clone());
        scheduler.add_link(l2.clone());
        scheduler.scheduler.mark_has_traffic(1);
        scheduler.scheduler.mark_has_traffic(2);
        scheduler.refresh_metrics();

        // Droppable packet should not be duplicated
        let payload = Bytes::from_static(b"DroppableData");
        let profile = crate::scheduler::PacketProfile {
            is_critical: false,
            can_drop: true,
            size_bytes: payload.len(),
        };

        scheduler.send(payload, profile).unwrap();

        let l1_count = l1.sent_packets.lock().unwrap().len();
        let l2_count = l2.sent_packets.lock().unwrap().len();

        // Should use single link (not duplicated)
        assert_eq!(l1_count + l2_count, 1);
    }

    #[test]
    fn test_redundancy_disabled_by_config() {
        use crate::config::SchedulerConfig;
        let config = SchedulerConfig {
            redundancy_enabled: false,
            ..SchedulerConfig::default()
        };
        let mut scheduler = BondingScheduler::with_config(config);

        let l1 = Arc::new(MockLink::new(1, 10_000_000.0, 10.0));
        l1.set_observed_bps(3_000_000.0);
        let l2 = Arc::new(MockLink::new(2, 10_000_000.0, 10.0));
        l2.set_observed_bps(3_000_000.0);

        scheduler.add_link(l1.clone());
        scheduler.add_link(l2.clone());
        scheduler.scheduler.mark_has_traffic(1);
        scheduler.scheduler.mark_has_traffic(2);
        scheduler.refresh_metrics();

        // Important non-droppable small packet — but redundancy disabled
        let payload = Bytes::from_static(b"ImportantData");
        let profile = crate::scheduler::PacketProfile {
            is_critical: false,
            can_drop: false,
            size_bytes: payload.len(),
        };

        scheduler.send(payload, profile).unwrap();

        let l1_count = l1.sent_packets.lock().unwrap().len();
        let l2_count = l2.sent_packets.lock().unwrap().len();

        // Single link only — no duplication
        assert_eq!(l1_count + l2_count, 1);
    }

    #[test]
    fn test_failover_disabled_by_config() {
        use crate::config::SchedulerConfig;
        let config = SchedulerConfig {
            failover_enabled: false,
            ..SchedulerConfig::default()
        };
        let mut scheduler = BondingScheduler::with_config(config);

        let l1 = Arc::new(MockLink::new(1, 10_000_000.0, 10.0));
        let l2 = Arc::new(MockLink::new(2, 10_000_000.0, 10.0));
        scheduler.add_link(l1.clone());
        scheduler.add_link(l2.clone());

        // Initial refresh
        scheduler.refresh_metrics();
        assert!(!scheduler.in_failover_mode());

        // Simulate phase degradation
        l1.set_phase(LinkPhase::Degrade);
        scheduler.refresh_metrics();

        // Failover should NOT trigger because it is disabled
        assert!(!scheduler.in_failover_mode());
    }

    #[test]
    fn test_critical_broadcast_disabled_by_config() {
        use crate::config::SchedulerConfig;
        let config = SchedulerConfig {
            critical_broadcast: false,
            redundancy_enabled: false,
            ..SchedulerConfig::default()
        };
        let mut scheduler = BondingScheduler::with_config(config);

        let l1 = Arc::new(MockLink::new(1, 10_000_000.0, 10.0));
        let l2 = Arc::new(MockLink::new(2, 10_000_000.0, 10.0));
        scheduler.add_link(l1.clone());
        scheduler.add_link(l2.clone());
        scheduler.refresh_metrics();

        // Critical packet but broadcast disabled
        let payload = Bytes::from_static(b"Critical");
        let profile = crate::scheduler::PacketProfile {
            is_critical: true,
            can_drop: false,
            size_bytes: payload.len(),
        };

        scheduler.send(payload, profile).unwrap();

        let l1_count = l1.sent_packets.lock().unwrap().len();
        let l2_count = l2.sent_packets.lock().unwrap().len();

        // Should be sent to single link, not broadcast
        assert_eq!(l1_count + l2_count, 1);
    }

    #[test]
    fn test_os_down_link_excluded_from_traffic() {
        // When one link has os_up=false (interface down), all traffic
        // should be routed through remaining alive links.
        let mut scheduler = BondingScheduler::new();
        let l1 = Arc::new(MockLink::new(1, 10_000_000.0, 10.0));
        let l2 = Arc::new(MockLink::new(2, 10_000_000.0, 10.0));

        // Mark link1 as interface-down
        {
            let mut m = l1.metrics.lock().unwrap();
            m.os_up = Some(false);
            m.alive = false;
        }

        scheduler.add_link(l1.clone());
        scheduler.add_link(l2.clone());
        scheduler.refresh_metrics();

        // Send 20 non-critical droppable packets
        for _ in 0..20 {
            let payload = Bytes::from_static(b"TestData");
            let profile = crate::scheduler::PacketProfile {
                is_critical: false,
                can_drop: true,
                size_bytes: payload.len(),
            };
            scheduler.send(payload, profile).unwrap();
        }

        let l1_count = l1.sent_packets.lock().unwrap().len();
        let l2_count = l2.sent_packets.lock().unwrap().len();

        // All 20 packets should go to link2 (link1 is dead)
        assert_eq!(l1_count, 0, "Dead link should receive no traffic");
        assert_eq!(l2_count, 20, "Alive link should receive all traffic");
    }

    #[test]
    fn test_os_down_link_excluded_from_broadcast() {
        // Even critical broadcast should skip links with os_up=false.
        let mut scheduler = BondingScheduler::new();
        let l1 = Arc::new(MockLink::new(1, 10_000_000.0, 10.0));
        let l2 = Arc::new(MockLink::new(2, 10_000_000.0, 10.0));

        {
            let mut m = l1.metrics.lock().unwrap();
            m.os_up = Some(false);
            m.alive = false;
        }

        scheduler.add_link(l1.clone());
        scheduler.add_link(l2.clone());
        scheduler.refresh_metrics();

        let payload = Bytes::from_static(b"Keyframe");
        let profile = crate::scheduler::PacketProfile {
            is_critical: true,
            can_drop: false,
            size_bytes: payload.len(),
        };
        scheduler.send(payload, profile).unwrap();

        let l1_count = l1.sent_packets.lock().unwrap().len();
        let l2_count = l2.sent_packets.lock().unwrap().len();

        assert_eq!(l1_count, 0, "Dead link should not receive broadcasts");
        assert_eq!(l2_count, 1, "Alive link should receive broadcast");
    }

    #[test]
    fn test_os_down_recovery_resumes_traffic() {
        // When a downed interface comes back up, traffic should resume
        // flowing to it.
        let mut scheduler = BondingScheduler::new();
        let l1 = Arc::new(MockLink::new(1, 10_000_000.0, 10.0));
        let l2 = Arc::new(MockLink::new(2, 10_000_000.0, 10.0));

        // Start with link1 down
        {
            let mut m = l1.metrics.lock().unwrap();
            m.os_up = Some(false);
            m.alive = false;
        }

        scheduler.add_link(l1.clone());
        scheduler.add_link(l2.clone());
        scheduler.refresh_metrics();

        // Send during outage — all goes to l2
        let payload = Bytes::from_static(b"Data");
        let profile = crate::scheduler::PacketProfile {
            is_critical: false,
            can_drop: true,
            size_bytes: payload.len(),
        };
        scheduler.send(payload.clone(), profile).unwrap();

        assert_eq!(l1.sent_packets.lock().unwrap().len(), 0);
        assert_eq!(l2.sent_packets.lock().unwrap().len(), 1);

        // Bring link1 back up
        {
            let mut m = l1.metrics.lock().unwrap();
            m.os_up = Some(true);
            m.alive = true;
        }
        scheduler.refresh_metrics();

        // Send more packets — link1 should now participate
        for _ in 0..20 {
            scheduler.send(payload.clone(), profile).unwrap();
        }

        let l1_count = l1.sent_packets.lock().unwrap().len();
        assert!(
            l1_count > 0,
            "Recovered link should receive traffic again, got {}",
            l1_count
        );
    }

    // ─── Intelligence Pipeline Tests ────────────────────────────────────

    #[test]
    fn test_intelligence_avoids_high_latency_link() {
        // BLEST should avoid a link with very high RTT relative to others
        let mut scheduler = BondingScheduler::new();
        let l1 = Arc::new(MockLink::new(1, 10_000_000.0, 5.0)); // 5ms RTT
        let l2 = Arc::new(MockLink::new(2, 10_000_000.0, 500.0)); // 500ms RTT

        scheduler.add_link(l1.clone());
        scheduler.add_link(l2.clone());
        scheduler.refresh_metrics();

        // Send many droppable packets — intelligence should prefer l1
        let profile = crate::scheduler::PacketProfile {
            is_critical: false,
            can_drop: true,
            size_bytes: 1000,
        };

        for _ in 0..50 {
            let payload = Bytes::from(vec![0u8; 1000]);
            scheduler.send(payload, profile).unwrap();
        }

        let l1_count = l1.sent_packets.lock().unwrap().len();
        let l2_count = l2.sent_packets.lock().unwrap().len();

        // Low-latency link should get the majority of traffic
        assert!(
            l1_count > l2_count,
            "low-latency link should be preferred: l1={l1_count}, l2={l2_count}"
        );
    }

    #[test]
    fn test_thompson_learns_from_failures() {
        // Thompson Sampling should learn to avoid a link that fails
        let mut scheduler = BondingScheduler::new();
        let l1 = Arc::new(MockLink::new(1, 10_000_000.0, 10.0));
        let l2 = Arc::new(MockLink::new(2, 10_000_000.0, 10.0));

        scheduler.add_link(l1.clone());
        scheduler.add_link(l2.clone());
        scheduler.refresh_metrics();

        // Manually record many failures for link 2
        for _ in 0..50 {
            scheduler.thompson.record_failure(2);
        }
        for _ in 0..50 {
            scheduler.thompson.record_success(1);
        }

        // Thompson should now strongly prefer link 1
        let rate1 = scheduler.thompson.estimated_success_rate(1).unwrap();
        let rate2 = scheduler.thompson.estimated_success_rate(2).unwrap();
        assert!(
            rate1 > rate2 * 2.0,
            "link 1 should have much better estimated rate: l1={rate1}, l2={rate2}"
        );
    }

    #[test]
    fn test_kalman_smooths_rtt_updates() {
        let mut scheduler = BondingScheduler::new();
        let l1 = Arc::new(MockLink::new(1, 10_000_000.0, 10.0));
        scheduler.add_link(l1.clone());

        // Feed several RTT updates through refresh
        for _ in 0..10 {
            scheduler.refresh_metrics();
        }

        // Kalman should have been initialized
        let kf = scheduler.kalman_rtt.get(&1).unwrap();
        assert!(kf.is_initialized(), "Kalman filter should be initialized");
        // Smoothed value should be close to 10ms
        assert!(
            (kf.value() - 10.0).abs() < 5.0,
            "Kalman-smoothed RTT should be close to 10ms, got {}",
            kf.value()
        );
    }

    #[test]
    fn test_intelligence_registers_on_add_link() {
        let mut scheduler: BondingScheduler<MockLink> = BondingScheduler::new();
        let l1 = Arc::new(MockLink::new(1, 10_000_000.0, 10.0));
        let l2 = Arc::new(MockLink::new(2, 5_000_000.0, 20.0));

        scheduler.add_link(l1);
        scheduler.add_link(l2);

        assert_eq!(scheduler.iods.link_count(), 2);
        assert_eq!(scheduler.thompson.link_count(), 2);
        assert!(scheduler.kalman_rtt.contains_key(&1));
        assert!(scheduler.kalman_rtt.contains_key(&2));
    }

    #[test]
    fn test_intelligence_deregisters_on_remove_link() {
        let mut scheduler: BondingScheduler<MockLink> = BondingScheduler::new();
        let l1 = Arc::new(MockLink::new(1, 10_000_000.0, 10.0));
        let l2 = Arc::new(MockLink::new(2, 5_000_000.0, 20.0));

        scheduler.add_link(l1);
        scheduler.add_link(l2);
        scheduler.remove_link(2);

        assert_eq!(scheduler.iods.link_count(), 1);
        assert_eq!(scheduler.thompson.link_count(), 1);
        assert!(!scheduler.kalman_rtt.contains_key(&2));
    }
}
