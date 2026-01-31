use crate::net::interface::LinkSender;
use crate::scheduler::dwrr::Dwrr;
use anyhow::Result;
use bytes::Bytes;
use std::collections::HashMap;
use std::sync::Arc;

pub struct BondingScheduler<L: LinkSender + ?Sized> {
    scheduler: Dwrr<L>,
    next_seq: u64,
}

impl<L: LinkSender + ?Sized> BondingScheduler<L> {
    pub fn new() -> Self {
        Self {
            scheduler: Dwrr::new(),
            next_seq: 0,
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
    }

    pub fn get_all_metrics(&self) -> HashMap<usize, crate::net::interface::LinkMetrics> {
        self.scheduler.get_active_links().into_iter().collect()
    }

    pub fn send(&mut self, payload: Bytes, profile: crate::scheduler::PacketProfile) -> Result<()> {
        let packet_len = payload.len();

        // Critical Strategy: Broadcast to all alive links
        if profile.is_critical {
            let links = self.scheduler.broadcast_links(packet_len);
            if links.is_empty() {
                return Err(anyhow::anyhow!("No active links for critical packet"));
            }

            let seq = self.next_seq;
            self.next_seq += 1;

            let header = crate::protocol::header::BondingHeader::new(seq);
            let wrapped = header.wrap(payload);

            for link in links {
                // Ignore individual errors during broadcast, as long as one succeeds?
                // Or bail? For bonding, best effort broadcast.
                if link.send(&wrapped).is_ok() {
                    self.scheduler.record_send(link.id(), packet_len as u64);
                }
            }
            return Ok(());
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
        metrics: LinkMetrics,
        sent_packets: Mutex<Vec<Vec<u8>>>,
    }

    impl MockLink {
        fn new(id: usize, capacity: f64, rtt: f64) -> Self {
            Self {
                id,
                metrics: LinkMetrics {
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
                },
                sent_packets: Mutex::new(Vec::new()),
            }
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
            self.metrics.clone()
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

        let payload = Bytes::from_static(b"Data");
        scheduler
            .send(payload, crate::scheduler::PacketProfile::default())
            .unwrap();

        // L1 should be selected
        assert_eq!(l1.sent_packets.lock().unwrap().len(), 1);
        assert_eq!(l2.sent_packets.lock().unwrap().len(), 0);
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
        };

        scheduler.send(payload, profile).unwrap();

        assert_eq!(l1.sent_packets.lock().unwrap().len(), 1);
        assert_eq!(l2.sent_packets.lock().unwrap().len(), 1);
    }
}
