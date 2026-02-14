use crate::config::SchedulerConfig;
use crate::net::interface::LinkSender;
use crate::scheduler::dwrr::Dwrr;
use crate::scheduler::QueueClass;
use anyhow::Result;
use bytes::Bytes;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{error, warn};

/// Entry in a STORM dual-queue.
struct QueuedPacket {
    payload: Bytes,
    profile: crate::scheduler::PacketProfile,
    enqueue_time: Instant,
}

/// Top-level bonding packet scheduler.
///
/// Wraps the [`Dwrr`] scheduler with higher-level bonding logic:
/// - Critical packet broadcast (keyframes sent to all alive links)
/// - Fast-failover mode (broadcasts all traffic on link instability)
/// - Adaptive redundancy (duplicates important packets when spare capacity allows)
/// - Escalating dead-link logging
/// - STORM dual-queue: reliable vs unreliable queues with weighted scheduling
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
    /// Total packets discarded by the sender-side deadline primitive.
    discarded_deadline: u64,
    // ── STORM dual-queue state ──
    /// Reliable queue (critical / non-droppable packets).
    reliable_queue: VecDeque<QueuedPacket>,
    /// Unreliable queue (droppable, latency-sensitive packets).
    unreliable_queue: VecDeque<QueuedPacket>,
    /// DWRR deficit counter for the reliable queue (bytes).
    reliable_deficit: f64,
    /// DWRR deficit counter for the unreliable queue (bytes).
    unreliable_deficit: f64,
    /// Total packets aged-out from the unreliable queue.
    storm_aged_out: u64,
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
            discarded_deadline: 0,
            reliable_queue: VecDeque::new(),
            unreliable_queue: VecDeque::new(),
            reliable_deficit: 0.0,
            unreliable_deficit: 0.0,
            storm_aged_out: 0,
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
    /// When STORM dual-queue is **disabled** (default) the packet is sent
    /// immediately using the existing broadcast / redundancy / DWRR path.
    ///
    /// When STORM is **enabled** the packet is first enqueued into the
    /// appropriate class queue (reliable or unreliable) and then both
    /// queues are drained using weighted deficit round-robin.
    pub fn send(&mut self, payload: Bytes, profile: crate::scheduler::PacketProfile) -> Result<()> {
        if !self.scheduler.config().storm_enabled {
            return self.send_immediate(payload, profile);
        }

        // ── STORM enqueue ──
        let class = profile.queue_class();
        let cap = self.scheduler.config().storm_queue_capacity;
        let entry = QueuedPacket {
            payload,
            profile,
            enqueue_time: Instant::now(),
        };
        match class {
            QueueClass::Reliable => {
                if self.reliable_queue.len() >= cap {
                    // Reliable never drops — evict oldest unreliable instead
                    // to make room in total memory budget.
                    self.unreliable_queue.pop_front();
                }
                self.reliable_queue.push_back(entry);
            }
            QueueClass::Unreliable => {
                if self.unreliable_queue.len() >= cap {
                    // Drop oldest unreliable to make room.
                    self.unreliable_queue.pop_front();
                    self.storm_aged_out += 1;
                }
                self.unreliable_queue.push_back(entry);
            }
        }

        // ── Drain both queues ──
        self.drain_queues()
    }

    /// Drain the STORM dual-queues using weighted deficit round-robin.
    ///
    /// The reliable queue receives `storm_reliable_weight` share of the
    /// bandwidth and the unreliable queue receives the remainder.
    /// Unreliable entries older than `storm_unreliable_max_age_ms` are
    /// silently discarded.
    fn drain_queues(&mut self) -> Result<()> {
        let config = self.scheduler.config().clone();
        let max_age = Duration::from_millis(config.storm_unreliable_max_age_ms);
        let now = Instant::now();

        // Age-discard stale unreliable entries.
        let before = self.unreliable_queue.len();
        self.unreliable_queue
            .retain(|e| now.duration_since(e.enqueue_time) < max_age);
        self.storm_aged_out += (before - self.unreliable_queue.len()) as u64;

        // Weighted DWRR across the two queues.
        // Each round adds weight-proportional credits (in bytes). We use a
        // quantum of 1500 bytes (≈ 1 MTU) scaled by weight.
        const QUANTUM: f64 = 1500.0;
        let r_quantum = QUANTUM * config.storm_reliable_weight;
        let u_quantum = QUANTUM * (1.0 - config.storm_reliable_weight);

        // Process up to a bounded number of packets per call to avoid
        // unbounded latency in `send()`.
        let max_drain = 64;
        let mut drained = 0;
        let mut succeeded = 0;
        let mut last_err: Option<anyhow::Error> = None;

        while drained < max_drain
            && (!self.reliable_queue.is_empty() || !self.unreliable_queue.is_empty())
        {
            // Replenish deficits.
            if !self.reliable_queue.is_empty() {
                self.reliable_deficit += r_quantum;
            }
            if !self.unreliable_queue.is_empty() {
                self.unreliable_deficit += u_quantum;
            }

            // Service reliable queue first (higher priority).
            while let Some(front) = self.reliable_queue.front() {
                let cost = front.profile.size_bytes.max(1) as f64;
                if self.reliable_deficit < cost {
                    break;
                }
                let pkt = self.reliable_queue.pop_front().unwrap();
                self.reliable_deficit -= cost;
                drained += 1;
                match self.send_immediate(pkt.payload, pkt.profile) {
                    Ok(()) => succeeded += 1,
                    Err(e) => last_err = Some(e),
                }
            }

            // Service unreliable queue.
            while let Some(front) = self.unreliable_queue.front() {
                let cost = front.profile.size_bytes.max(1) as f64;
                if self.unreliable_deficit < cost {
                    break;
                }
                let pkt = self.unreliable_queue.pop_front().unwrap();
                self.unreliable_deficit -= cost;
                drained += 1;
                match self.send_immediate(pkt.payload, pkt.profile) {
                    Ok(()) => succeeded += 1,
                    Err(e) => last_err = Some(e),
                }
            }

            // If neither queue could make progress, break to avoid spin.
            if self.reliable_queue.is_empty() && self.unreliable_queue.is_empty() {
                break;
            }
            if drained >= max_drain {
                break;
            }
        }

        match last_err {
            Some(e) if succeeded == 0 => Err(e),
            _ => Ok(()),
        }
    }

    /// Immediately send a single packet (bypass STORM queues).
    ///
    /// This is the original scheduling path: discard primitive → broadcast
    /// → redundancy → DWRR single-link selection.
    fn send_immediate(
        &mut self,
        payload: Bytes,
        profile: crate::scheduler::PacketProfile,
    ) -> Result<()> {
        let packet_len = payload.len();
        let config = self.scheduler.config();

        // ── Discard primitive ──
        // If the packet has a deadline and is droppable, check the
        // estimated minimum OWD across alive links.  When even the
        // *best* link would deliver the packet after its deadline,
        // drop it here to save bandwidth for timely data.
        let deadline = if profile.deadline_ms > 0 {
            profile.deadline_ms
        } else {
            config.discard_deadline_ms
        };
        if deadline > 0 && profile.can_drop && !profile.is_critical {
            let min_owd = self.scheduler.min_alive_owd_ms();
            if min_owd > deadline as f64 {
                self.discarded_deadline += 1;
                self.log_discard(deadline, min_owd);
                return Ok(());
            }
        }

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
        self.log_dead_links();
        self.consecutive_dead_count += 1;
        self.total_dead_drops += 1;

        Err(anyhow::anyhow!("Link selection failed (all links dead)"))
    }

    /// Cold helper for dead-link logging — separated so the compiler can
    /// inline the hot path of `send()` without bloating it with log
    /// formatting code (#9).
    #[cold]
    #[inline(never)]
    fn log_dead_links(&self) {
        // Escalating severity: first occurrence is a warning,
        // sustained failures (100+) escalate to error.
        if self.consecutive_dead_count == 0 {
            warn!("All links dead: dropped packet (seq={})", self.next_seq);
        } else if self.consecutive_dead_count == 99 {
            error!(
                "All links dead for {} consecutive packets — total drops: {}",
                self.consecutive_dead_count + 1,
                self.total_dead_drops + 1
            );
        } else if (self.consecutive_dead_count + 1).is_multiple_of(1000) {
            error!(
                "All links still dead after {} consecutive drops (total: {})",
                self.consecutive_dead_count + 1,
                self.total_dead_drops + 1
            );
        }
    }

    /// Cold helper for deadline-discard logging.
    #[cold]
    #[inline(never)]
    fn log_discard(&self, deadline_ms: u64, min_owd_ms: f64) {
        if self.discarded_deadline.is_multiple_of(100) || self.discarded_deadline == 1 {
            tracing::debug!(
                "Discard primitive: OWD {:.1}ms > deadline {}ms — {} discarded total",
                min_owd_ms,
                deadline_ms,
                self.discarded_deadline,
            );
        }
    }

    /// Returns the number of packets discarded by the sender-side deadline.
    pub fn discarded_deadline(&self) -> u64 {
        self.discarded_deadline
    }

    /// Returns the number of unreliable packets aged-out by the STORM queue.
    pub fn storm_aged_out(&self) -> u64 {
        self.storm_aged_out
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
                    signal_dbm: None,
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
            ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
        };

        scheduler.send(payload, profile).unwrap();

        let l1_count = l1.sent_packets.lock().unwrap().len();
        let l2_count = l2.sent_packets.lock().unwrap().len();

        // Should be sent to single link, not broadcast
        assert_eq!(l1_count + l2_count, 1);
    }

    // ────────────────────────────────────────────────────────────────
    // Discard primitive tests
    // ────────────────────────────────────────────────────────────────

    #[test]
    fn discard_drops_when_owd_exceeds_deadline() {
        use crate::config::SchedulerConfig;
        let config = SchedulerConfig {
            discard_deadline_ms: 100,
            critical_broadcast: false,
            redundancy_enabled: false,
            failover_enabled: false,
            ..SchedulerConfig::default()
        };
        let mut sched = BondingScheduler::with_config(config);
        let link = Arc::new(MockLink::new(1, 10_000_000.0, 10.0));
        // Set OWD above deadline
        link.metrics.lock().unwrap().owd_ms = 150.0;
        sched.add_link(link.clone());
        sched.refresh_metrics();

        let profile = crate::scheduler::PacketProfile {
            is_critical: false,
            can_drop: true,
            size_bytes: 100,
            deadline_ms: 0, // use config default
        };
        sched.send(Bytes::from_static(b"data"), profile).unwrap();

        assert_eq!(
            link.sent_packets.lock().unwrap().len(),
            0,
            "Packet should be discarded — OWD 150 > deadline 100"
        );
        assert_eq!(sched.discarded_deadline(), 1);
    }

    #[test]
    fn discard_sends_when_owd_within_deadline() {
        use crate::config::SchedulerConfig;
        let config = SchedulerConfig {
            discard_deadline_ms: 100,
            critical_broadcast: false,
            redundancy_enabled: false,
            failover_enabled: false,
            ..SchedulerConfig::default()
        };
        let mut sched = BondingScheduler::with_config(config);
        let link = Arc::new(MockLink::new(1, 10_000_000.0, 10.0));
        link.metrics.lock().unwrap().owd_ms = 50.0;
        sched.add_link(link.clone());
        sched.refresh_metrics();

        let profile = crate::scheduler::PacketProfile {
            is_critical: false,
            can_drop: true,
            size_bytes: 100,
            deadline_ms: 0,
        };
        sched.send(Bytes::from_static(b"data"), profile).unwrap();

        assert_eq!(
            link.sent_packets.lock().unwrap().len(),
            1,
            "Packet should be sent — OWD 50 < deadline 100"
        );
        assert_eq!(sched.discarded_deadline(), 0);
    }

    #[test]
    fn discard_never_drops_critical() {
        use crate::config::SchedulerConfig;
        let config = SchedulerConfig {
            discard_deadline_ms: 10,
            critical_broadcast: false,
            redundancy_enabled: false,
            failover_enabled: false,
            ..SchedulerConfig::default()
        };
        let mut sched = BondingScheduler::with_config(config);
        let link = Arc::new(MockLink::new(1, 10_000_000.0, 10.0));
        link.metrics.lock().unwrap().owd_ms = 200.0;
        sched.add_link(link.clone());
        sched.refresh_metrics();

        let profile = crate::scheduler::PacketProfile {
            is_critical: true,
            can_drop: false,
            size_bytes: 100,
            deadline_ms: 0,
        };
        sched
            .send(Bytes::from_static(b"keyframe"), profile)
            .unwrap();

        assert_eq!(
            link.sent_packets.lock().unwrap().len(),
            1,
            "Critical packets must never be discarded"
        );
    }

    #[test]
    fn discard_uses_per_packet_deadline() {
        use crate::config::SchedulerConfig;
        let config = SchedulerConfig {
            discard_deadline_ms: 0, // config disabled
            critical_broadcast: false,
            redundancy_enabled: false,
            failover_enabled: false,
            ..SchedulerConfig::default()
        };
        let mut sched = BondingScheduler::with_config(config);
        let link = Arc::new(MockLink::new(1, 10_000_000.0, 10.0));
        link.metrics.lock().unwrap().owd_ms = 80.0;
        sched.add_link(link.clone());
        sched.refresh_metrics();

        let profile = crate::scheduler::PacketProfile {
            is_critical: false,
            can_drop: true,
            size_bytes: 100,
            deadline_ms: 50, // per-packet deadline
        };
        sched.send(Bytes::from_static(b"late"), profile).unwrap();

        assert_eq!(
            link.sent_packets.lock().unwrap().len(),
            0,
            "Per-packet deadline should take effect: OWD 80 > deadline 50"
        );
        assert_eq!(sched.discarded_deadline(), 1);
    }

    #[test]
    fn discard_disabled_when_zero() {
        use crate::config::SchedulerConfig;
        let config = SchedulerConfig {
            discard_deadline_ms: 0,
            critical_broadcast: false,
            redundancy_enabled: false,
            failover_enabled: false,
            ..SchedulerConfig::default()
        };
        let mut sched = BondingScheduler::with_config(config);
        let link = Arc::new(MockLink::new(1, 10_000_000.0, 10.0));
        link.metrics.lock().unwrap().owd_ms = 999.0;
        sched.add_link(link.clone());
        sched.refresh_metrics();

        let profile = crate::scheduler::PacketProfile {
            is_critical: false,
            can_drop: true,
            size_bytes: 100,
            deadline_ms: 0,
        };
        sched.send(Bytes::from_static(b"data"), profile).unwrap();

        assert_eq!(
            link.sent_packets.lock().unwrap().len(),
            1,
            "Discard disabled (deadline=0) — always send"
        );
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
            ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
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

    // ────────────────────────────────────────────────────────────────
    // STORM dual-queue tests
    // ────────────────────────────────────────────────────────────────

    fn storm_config() -> SchedulerConfig {
        SchedulerConfig {
            storm_enabled: true,
            storm_reliable_weight: 0.7,
            storm_unreliable_max_age_ms: 200,
            storm_queue_capacity: 64,
            critical_broadcast: false,
            redundancy_enabled: false,
            failover_enabled: false,
            ..SchedulerConfig::default()
        }
    }

    #[test]
    fn storm_reliable_packet_is_delivered() {
        let mut sched = BondingScheduler::with_config(storm_config());
        let link = Arc::new(MockLink::new(1, 10_000_000.0, 10.0));
        sched.add_link(link.clone());
        sched.refresh_metrics();

        let profile = crate::scheduler::PacketProfile {
            is_critical: true,
            can_drop: false,
            size_bytes: 100,
            deadline_ms: 0,
        };
        sched
            .send(Bytes::from_static(b"keyframe"), profile)
            .unwrap();

        assert_eq!(
            link.sent_packets.lock().unwrap().len(),
            1,
            "Reliable packet must be delivered through STORM queue"
        );
    }

    #[test]
    fn storm_unreliable_packet_is_delivered() {
        let mut sched = BondingScheduler::with_config(storm_config());
        let link = Arc::new(MockLink::new(1, 10_000_000.0, 10.0));
        sched.add_link(link.clone());
        sched.refresh_metrics();

        let profile = crate::scheduler::PacketProfile {
            is_critical: false,
            can_drop: true,
            size_bytes: 100,
            deadline_ms: 0,
        };
        sched.send(Bytes::from_static(b"bframe"), profile).unwrap();

        assert_eq!(
            link.sent_packets.lock().unwrap().len(),
            1,
            "Unreliable packet should be delivered when links are healthy"
        );
    }

    #[test]
    fn storm_age_discards_stale_unreliable() {
        let mut config = storm_config();
        config.storm_unreliable_max_age_ms = 1; // 1ms age limit
        let mut sched = BondingScheduler::with_config(config);
        let link = Arc::new(MockLink::new(1, 10_000_000.0, 10.0));
        sched.add_link(link.clone());
        sched.refresh_metrics();

        // Enqueue directly into unreliable queue with old timestamp
        sched.unreliable_queue.push_back(QueuedPacket {
            payload: Bytes::from_static(b"old"),
            profile: crate::scheduler::PacketProfile {
                is_critical: false,
                can_drop: true,
                size_bytes: 100,
                deadline_ms: 0,
            },
            enqueue_time: Instant::now() - Duration::from_millis(50),
        });

        // Now send a fresh reliable packet — drain should age-discard the stale one
        let profile = crate::scheduler::PacketProfile {
            is_critical: true,
            can_drop: false,
            size_bytes: 100,
            deadline_ms: 0,
        };
        sched.send(Bytes::from_static(b"fresh"), profile).unwrap();

        assert_eq!(
            link.sent_packets.lock().unwrap().len(),
            1,
            "Only the fresh reliable packet should be sent"
        );
        assert!(
            sched.storm_aged_out() >= 1,
            "Stale unreliable entry should be aged out"
        );
    }

    #[test]
    fn storm_reliable_weight_prioritizes_reliable() {
        let mut sched = BondingScheduler::with_config(storm_config());
        let link = Arc::new(MockLink::new(1, 10_000_000.0, 10.0));
        sched.add_link(link.clone());
        sched.refresh_metrics();

        // Send a mix: 5 unreliable then 5 reliable
        for _ in 0..5 {
            let p = crate::scheduler::PacketProfile {
                is_critical: false,
                can_drop: true,
                size_bytes: 100,
                deadline_ms: 0,
            };
            sched.send(Bytes::from_static(b"U"), p).unwrap();
        }
        for _ in 0..5 {
            let p = crate::scheduler::PacketProfile {
                is_critical: true,
                can_drop: false,
                size_bytes: 100,
                deadline_ms: 0,
            };
            sched.send(Bytes::from_static(b"R"), p).unwrap();
        }

        // All 10 should be delivered (no loss, healthy link)
        assert_eq!(
            link.sent_packets.lock().unwrap().len(),
            10,
            "STORM should deliver all packets on healthy link"
        );
    }

    #[test]
    fn storm_queue_overflow_drops_unreliable() {
        let mut config = storm_config();
        config.storm_queue_capacity = 2;
        let mut sched = BondingScheduler::with_config(config);
        let link = Arc::new(MockLink::new(1, 10_000_000.0, 10.0));
        sched.add_link(link.clone());
        sched.refresh_metrics();

        // Pre-fill the unreliable queue to capacity via direct manipulation
        // (drain would immediately send them on the healthy link).
        for i in 0..2 {
            sched.unreliable_queue.push_back(QueuedPacket {
                payload: Bytes::from(format!("fill-{}", i)),
                profile: crate::scheduler::PacketProfile {
                    is_critical: false,
                    can_drop: true,
                    size_bytes: 100,
                    deadline_ms: 0,
                },
                enqueue_time: Instant::now(),
            });
        }
        assert_eq!(sched.unreliable_queue.len(), 2);

        // Now send another unreliable — should evict oldest
        let p = crate::scheduler::PacketProfile {
            is_critical: false,
            can_drop: true,
            size_bytes: 100,
            deadline_ms: 0,
        };
        sched.send(Bytes::from_static(b"new"), p).unwrap();

        // One overflow eviction should have occurred
        assert!(
            sched.storm_aged_out() >= 1,
            "At least 1 packet should be shed from overflow, got {}",
            sched.storm_aged_out()
        );
    }

    #[test]
    fn storm_queue_class_classification() {
        use crate::scheduler::QueueClass;

        // Critical → Reliable
        let p = crate::scheduler::PacketProfile {
            is_critical: true,
            can_drop: false,
            size_bytes: 100,
            deadline_ms: 0,
        };
        assert_eq!(p.queue_class(), QueueClass::Reliable);

        // Non-droppable → Reliable
        let p = crate::scheduler::PacketProfile {
            is_critical: false,
            can_drop: false,
            size_bytes: 100,
            deadline_ms: 0,
        };
        assert_eq!(p.queue_class(), QueueClass::Reliable);

        // Droppable, not critical → Unreliable
        let p = crate::scheduler::PacketProfile {
            is_critical: false,
            can_drop: true,
            size_bytes: 100,
            deadline_ms: 0,
        };
        assert_eq!(p.queue_class(), QueueClass::Unreliable);
    }
}
