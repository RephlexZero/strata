use crate::config::SchedulerConfig;
use crate::net::interface::LinkSender;
use crate::scheduler::dwrr::Dwrr;
use anyhow::Result;
use bytes::Bytes;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{error, warn};

/// Top-level bonding packet scheduler.
///
/// Wraps the [`Dwrr`] scheduler with higher-level bonding logic:
/// - Critical packet broadcast (keyframes sent to all alive links)
/// - Fast-failover mode (broadcasts all traffic on link instability)
/// - Adaptive redundancy (duplicates important packets when spare capacity allows)
/// - Escalating dead-link logging
///
/// Used by [`crate::runtime::BondingRuntime`] on the worker thread.
pub struct BondingScheduler<L: LinkSender + ?Sized> {
    scheduler: Dwrr<L>,
    next_seq: u64,
    /// Monotonic start time for bonding header timestamps.
    start_time: Instant,
    // Fast-failover state
    failover_until: Option<Instant>,
    prev_phases: HashMap<usize, crate::net::interface::LinkPhase>,
    prev_rtts: HashMap<usize, f64>,
    /// Counter for consecutive all-links-dead failures (for escalation)
    consecutive_dead_count: u64,
    /// Total packets dropped due to all links being dead (used for log messages)
    total_dead_drops: u64,
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
            start_time: Instant::now(),
            failover_until: None,
            prev_phases: HashMap::new(),
            prev_rtts: HashMap::new(),
            consecutive_dead_count: 0,
            total_dead_drops: 0,
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

    /// Registers a new link with the scheduler.
    pub fn add_link(&mut self, link: Arc<L>) {
        self.scheduler.add_link(link);
    }

    /// Removes a link by ID, stopping all traffic to it.
    pub fn remove_link(&mut self, id: usize) {
        self.scheduler.remove_link(id);
    }

    /// Refreshes link metrics from all links and checks for failover conditions.
    pub fn refresh_metrics(&mut self) {
        self.scheduler.refresh_metrics();
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

    /// Creates a bonding header with a monotonic send timestamp for OWD measurement.
    fn make_header(&self, seq: u64) -> crate::protocol::header::BondingHeader {
        let send_time_us = self.start_time.elapsed().as_micros() as u64;
        crate::protocol::header::BondingHeader::with_timestamp(seq, send_time_us)
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

            let header = self.make_header(seq);
            let wrapped = header.wrap(payload);

            for link in links {
                if link.send(&wrapped).is_ok() {
                    self.scheduler.record_send(link.id(), packet_len as u64);
                }
            }
            return Ok(());
        }

        // Adaptive Redundancy: Use spare capacity for important packets
        if config.redundancy_enabled {
            // Use cached spare ratio (updated by refresh_metrics) to avoid
            // cloning all LinkMetrics on the hot packet path.
            let spare_ratio = self.scheduler.cached_spare_ratio();

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

                    let header = self.make_header(seq);
                    let wrapped = header.wrap(payload);

                    for link in links {
                        if link.send(&wrapped).is_ok() {
                            self.scheduler.record_send(link.id(), packet_len as u64);
                        }
                    }
                    return Ok(());
                }
                // Fall through to standard if duplication failed
            }
        }

        // Standard Load Balancing
        if let Some(link) = self.scheduler.select_link(packet_len) {
            let seq = self.next_seq;
            self.next_seq += 1;

            let header = self.make_header(seq);
            let wrapped = header.wrap(payload);

            link.send(&wrapped)?;
            self.scheduler.record_send(link.id(), packet_len as u64);
            self.consecutive_dead_count = 0;
            return Ok(());
        }

        // All links dead — escalate logging based on consecutive failures.
        self.consecutive_dead_count += 1;
        self.total_dead_drops += 1;

        // Log at escalating severity: first occurrence is a warning,
        // sustained failures (100+ consecutive) escalate to error.
        if self.consecutive_dead_count == 1 {
            warn!("All links dead: dropped packet (seq={})", self.next_seq);
        } else if self.consecutive_dead_count == 100 {
            error!(
                "All links dead for {} consecutive packets — total drops: {}",
                self.consecutive_dead_count, self.total_dead_drops
            );
        } else if self.consecutive_dead_count.is_multiple_of(1000) {
            error!(
                "All links still dead after {} consecutive drops (total: {})",
                self.consecutive_dead_count, self.total_dead_drops
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
                    estimated_capacity_bps: 0.0,
                    owd_ms: 0.0,
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

    // ────────────────────────────────────────────────────────────────
    // Header timestamp & sequence wiring tests
    // ────────────────────────────────────────────────────────────────

    #[test]
    fn send_wraps_with_non_zero_timestamp() {
        use crate::protocol::header::BondingHeader;

        let config = SchedulerConfig {
            critical_broadcast: false,
            redundancy_enabled: false,
            failover_enabled: false,
            ..SchedulerConfig::default()
        };
        let mut sched = BondingScheduler::with_config(config);
        let link = Arc::new(MockLink::new(1, 10_000_000.0, 10.0));
        sched.add_link(link.clone());
        sched.refresh_metrics();

        // Small sleep so timestamp is non-zero
        std::thread::sleep(std::time::Duration::from_millis(1));

        let payload = Bytes::from_static(b"test-payload");
        let profile = crate::scheduler::PacketProfile {
            is_critical: false,
            can_drop: false,
            size_bytes: payload.len(),
        };
        sched.send(payload, profile).unwrap();

        let packets = link.sent_packets.lock().unwrap();
        assert_eq!(packets.len(), 1);

        let raw = Bytes::from(packets[0].clone());
        let (header, body) = BondingHeader::unwrap(raw).expect("Should have a valid header");

        assert_eq!(header.seq_id, 0, "First packet should be seq 0");
        assert!(
            header.send_time_us > 0,
            "send_time_us should be non-zero for OWD measurement, got {}",
            header.send_time_us
        );
        assert_eq!(body, Bytes::from_static(b"test-payload"));
    }

    #[test]
    fn seq_increments_monotonically() {
        use crate::protocol::header::BondingHeader;

        let config = SchedulerConfig {
            critical_broadcast: false,
            redundancy_enabled: false,
            failover_enabled: false,
            ..SchedulerConfig::default()
        };
        let mut sched = BondingScheduler::with_config(config);
        let link = Arc::new(MockLink::new(1, 10_000_000.0, 10.0));
        sched.add_link(link.clone());
        sched.refresh_metrics();

        let profile = crate::scheduler::PacketProfile {
            is_critical: false,
            can_drop: false,
            size_bytes: 100,
        };
        for _ in 0..10 {
            sched.send(Bytes::from_static(b"data"), profile).unwrap();
        }

        let packets = link.sent_packets.lock().unwrap();
        assert_eq!(packets.len(), 10);

        for (i, pkt) in packets.iter().enumerate() {
            let raw = Bytes::from(pkt.clone());
            let (hdr, _) = BondingHeader::unwrap(raw).unwrap();
            assert_eq!(
                hdr.seq_id, i as u64,
                "Seq should be monotonically increasing"
            );
        }
    }

    // ────────────────────────────────────────────────────────────────
    // Error path & wrapper-level API tests
    // ────────────────────────────────────────────────────────────────

    #[test]
    fn send_all_links_dead_returns_error() {
        let config = SchedulerConfig {
            critical_broadcast: false,
            redundancy_enabled: false,
            failover_enabled: false,
            ..SchedulerConfig::default()
        };
        let mut sched = BondingScheduler::with_config(config);
        // Add a link but set it to dead phase
        let link = Arc::new(MockLink::new(1, 10_000_000.0, 10.0));
        link.set_phase(LinkPhase::Init);
        // Make metrics show not alive
        link.metrics.lock().unwrap().alive = false;
        sched.add_link(link);
        sched.refresh_metrics();

        let profile = crate::scheduler::PacketProfile {
            is_critical: false,
            can_drop: false,
            size_bytes: 100,
        };
        let result = sched.send(Bytes::from_static(b"data"), profile);
        assert!(result.is_err(), "Send should fail when all links are dead");
    }

    #[test]
    fn remove_link_on_bonding_scheduler() {
        let mut sched = BondingScheduler::new();
        let l1 = Arc::new(MockLink::new(1, 10_000_000.0, 10.0));
        let l2 = Arc::new(MockLink::new(2, 5_000_000.0, 10.0));
        sched.add_link(l1);
        sched.add_link(l2);
        sched.refresh_metrics();

        let metrics = sched.get_all_metrics();
        assert!(metrics.contains_key(&1));
        assert!(metrics.contains_key(&2));

        sched.remove_link(1);
        sched.refresh_metrics();

        let metrics = sched.get_all_metrics();
        assert!(
            !metrics.contains_key(&1),
            "Link 1 should be gone after remove"
        );
        assert!(metrics.contains_key(&2), "Link 2 should remain");
    }

    #[test]
    fn get_all_metrics_reflects_links() {
        let mut sched = BondingScheduler::new();
        assert!(
            sched.get_all_metrics().is_empty(),
            "No links → empty metrics"
        );

        let l1 = Arc::new(MockLink::new(1, 10_000_000.0, 10.0));
        sched.add_link(l1);
        sched.refresh_metrics();

        let metrics = sched.get_all_metrics();
        assert_eq!(metrics.len(), 1);
        assert!(metrics.contains_key(&1));
        assert!(
            (metrics[&1].capacity_bps - 10_000_000.0).abs() < 1e-6,
            "Capacity should reflect link's reported value"
        );
    }
}
