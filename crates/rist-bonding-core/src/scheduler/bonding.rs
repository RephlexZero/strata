use crate::net::interface::LinkSender;
use crate::scheduler::dwrr::Dwrr;
use anyhow::Result;
use bytes::Bytes;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub struct BondingScheduler<L: LinkSender + ?Sized> {
    scheduler: Dwrr<L>,
    next_seq: u64,
    // Fast-failover state
    failover_until: Option<Instant>,
    prev_phases: HashMap<usize, crate::net::interface::LinkPhase>,
    prev_rtts: HashMap<usize, f64>,
}

impl<L: LinkSender + ?Sized> BondingScheduler<L> {
    pub fn new() -> Self {
        Self {
            scheduler: Dwrr::new(),
            next_seq: 0,
            failover_until: None,
            prev_phases: HashMap::new(),
            prev_rtts: HashMap::new(),
        }
    }

    pub fn add_link(&mut self, link: Arc<L>) {
        self.scheduler.add_link(link);
    }

    pub fn remove_link(&mut self, id: usize) {
        self.scheduler.remove_link(id);
    }

    pub fn refresh_metrics(&mut self) {
        self.scheduler.refresh_metrics();
        self.check_failover_conditions();
    }

    /// Detects link instability (phase degradation or RTT spikes) and triggers fast-failover mode.
    fn check_failover_conditions(&mut self) {
        use crate::net::interface::LinkPhase;
        
        let metrics = self.scheduler.get_active_links();
        let mut trigger_failover = false;

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

            // Check for RTT spike (>3x previous smoothed value)
            if let Some(prev_rtt) = self.prev_rtts.get(id) {
                if m.rtt_ms > prev_rtt * 3.0 && *prev_rtt > 0.0 {
                    trigger_failover = true;
                }
            }

            self.prev_phases.insert(*id, m.phase);
            self.prev_rtts.insert(*id, m.rtt_ms);
        }

        if trigger_failover {
            // Enter failover mode for 3 seconds, then exponentially decay
            let failover_duration = Duration::from_secs(3);
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

    pub fn get_all_metrics(&self) -> HashMap<usize, crate::net::interface::LinkMetrics> {
        self.scheduler.get_active_links().into_iter().collect()
    }

    pub fn send(&mut self, payload: Bytes, profile: crate::scheduler::PacketProfile) -> Result<()> {
        let packet_len = payload.len();

        // Fast-failover: Broadcast during link instability
        let should_broadcast = profile.is_critical || self.in_failover_mode();

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
                if link.send(&wrapped).is_ok() {
                    self.scheduler.record_send(link.id(), packet_len as u64);
                }
            }
            return Ok(());
        }

        // Adaptive Redundancy: Use spare capacity for important packets
        let spare_capacity = self.scheduler.total_spare_capacity();
        let total_capacity: f64 = self
            .scheduler
            .get_active_links()
            .iter()
            .filter(|(_, m)| {
                m.alive && matches!(m.phase, crate::net::interface::LinkPhase::Live | crate::net::interface::LinkPhase::Warm)
            })
            .map(|(_, m)| m.capacity_bps)
            .sum();

        // Calculate spare capacity ratio
        let spare_ratio = if total_capacity > 0.0 {
            spare_capacity / total_capacity
        } else {
            0.0
        };

        // Decide duplication level based on spare capacity and packet characteristics
        let should_duplicate = spare_ratio > 0.5  // At least 50% spare capacity
            && !profile.can_drop  // Important reference frames
            && profile.size_bytes < 10_000;  // Avoid duplicating very large packets

        if should_duplicate {
            // Duplicate to 2 best diverse links
            let links = self.scheduler.select_best_n_links(packet_len, 2);
            if !links.is_empty() {
                let seq = self.next_seq;
                self.next_seq += 1;

                let header = crate::protocol::header::BondingHeader::new(seq);
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

        // Standard Load Balancing
        if let Some(link) = self.scheduler.select_link(packet_len) {
            let seq = self.next_seq;
            self.next_seq += 1;

            let header = crate::protocol::header::BondingHeader::new(seq);
            let wrapped = header.wrap(payload);

            link.send(&wrapped)?;
            self.scheduler.record_send(link.id(), packet_len as u64);
            return Ok(());
        }

        Err(anyhow::anyhow!("Link selection failed (all dead?)"))
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
        let l2 = Arc::new(MockLink::new(2, 10_000_000.0, 10.0));
        
        scheduler.add_link(l1.clone());
        scheduler.add_link(l2.clone());
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
        let l2 = Arc::new(MockLink::new(2, 10_000_000.0, 10.0));
        
        scheduler.add_link(l1.clone());
        scheduler.add_link(l2.clone());
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
}
