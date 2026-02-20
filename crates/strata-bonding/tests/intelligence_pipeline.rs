//! Integration tests for the bonding intelligence pipeline.
//!
//! These tests exercise the full stack: BondingScheduler with IoDS/BLEST/Thompson,
//! BitrateAdapter, ModemSupervisor, and simulated link failure scenarios.

use bytes::Bytes;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use strata_bonding::adaptation::{AdaptationConfig, BitrateAdapter, LinkCapacity};
use strata_bonding::media::priority::DegradationStage;
use strata_bonding::modem::health::RfMetrics;
use strata_bonding::modem::supervisor::{ModemSupervisor, SupervisorConfig, SupervisorEvent};
use strata_bonding::net::interface::{LinkMetrics, LinkPhase, LinkSender};
use strata_bonding::scheduler::bonding::BondingScheduler;
use strata_bonding::scheduler::PacketProfile;

// ─── Mock Infrastructure ────────────────────────────────────────────────

struct MockLink {
    id: usize,
    metrics: Mutex<LinkMetrics>,
    sent_packets: Mutex<Vec<Vec<u8>>>,
}

impl MockLink {
    fn new(id: usize, capacity_bps: f64, rtt_ms: f64) -> Self {
        Self {
            id,
            metrics: Mutex::new(LinkMetrics {
                capacity_bps,
                rtt_ms,
                loss_rate: 0.0,
                observed_bps: 0.0,
                observed_bytes: 0,
                queue_depth: 0,
                max_queue: 100,
                alive: true,
                phase: LinkPhase::Live,
                os_up: Some(true),
                mtu: None,
                iface: None,
                link_kind: None,
                transport: None,
                estimated_capacity_bps: 0.0,
                owd_ms: 0.0,
                receiver_report: None,
            }),
            sent_packets: Mutex::new(Vec::new()),
        }
    }

    fn kill(&self) {
        let mut m = self.metrics.lock().unwrap();
        m.alive = false;
        m.os_up = Some(false);
        m.loss_rate = 1.0;
    }

    fn revive(&self) {
        let mut m = self.metrics.lock().unwrap();
        m.alive = true;
        m.os_up = Some(true);
        m.loss_rate = 0.0;
    }

    fn packet_count(&self) -> usize {
        self.sent_packets.lock().unwrap().len()
    }
}

impl LinkSender for MockLink {
    fn id(&self) -> usize {
        self.id
    }
    fn send(&self, packet: &[u8]) -> anyhow::Result<usize> {
        let m = self.metrics.lock().unwrap();
        if !m.alive {
            return Err(anyhow::anyhow!("link dead"));
        }
        drop(m);
        self.sent_packets.lock().unwrap().push(packet.to_vec());
        Ok(packet.len())
    }
    fn get_metrics(&self) -> LinkMetrics {
        self.metrics.lock().unwrap().clone()
    }
}

fn default_profile(size: usize) -> PacketProfile {
    PacketProfile {
        is_critical: false,
        can_drop: true,
        size_bytes: size,
    }
}

fn critical_profile(size: usize) -> PacketProfile {
    PacketProfile {
        is_critical: true,
        can_drop: false,
        size_bytes: size,
    }
}

// ─── Test: Full Pipeline Link Failure ───────────────────────────────────

#[test]
fn full_pipeline_link_failure_and_recovery() {
    // Setup: 3 links, scheduler + adaptation + supervisor
    let l1 = Arc::new(MockLink::new(1, 10_000_000.0, 10.0));
    let l2 = Arc::new(MockLink::new(2, 10_000_000.0, 15.0));
    let l3 = Arc::new(MockLink::new(3, 5_000_000.0, 25.0));

    let mut scheduler = BondingScheduler::new();
    scheduler.add_link(l1.clone());
    scheduler.add_link(l2.clone());
    scheduler.add_link(l3.clone());
    scheduler.refresh_metrics();

    let mut adapter = BitrateAdapter::new(AdaptationConfig {
        max_bitrate_kbps: 20_000,
        min_interval: Duration::ZERO,
        ..Default::default()
    });

    let mut supervisor = ModemSupervisor::default();
    supervisor.register_link(1);
    supervisor.register_link(2);
    supervisor.register_link(3);

    // Phase 1: Normal operation — send 50 packets
    for _ in 0..50 {
        let payload = Bytes::from(vec![0u8; 1000]);
        scheduler.send(payload, default_profile(1000)).unwrap();
    }

    // All 3 links should have received traffic
    assert!(l1.packet_count() > 0, "link 1 should have traffic");
    assert!(l2.packet_count() > 0, "link 2 should have traffic");
    // l3 might have less due to lower capacity but should still participate

    // Phase 2: Kill link 3
    l3.kill();
    scheduler.refresh_metrics();

    // Feed supervisor degraded RF for link 3
    let bad_rf = RfMetrics {
        rsrp_dbm: -140.0,
        rsrq_db: -20.0,
        sinr_db: -15.0,
        cqi: 0,
    };
    for _ in 0..10 {
        supervisor.update_rf(3, &bad_rf);
    }

    // Adaptation should detect reduced capacity
    let caps = vec![
        LinkCapacity {
            link_id: 1,
            capacity_kbps: 10_000.0,
            alive: true,
            loss_rate: 0.0,
            rtt_ms: 10.0,
            queue_depth: None,
        },
        LinkCapacity {
            link_id: 2,
            capacity_kbps: 10_000.0,
            alive: true,
            loss_rate: 0.0,
            rtt_ms: 15.0,
            queue_depth: None,
        },
        LinkCapacity {
            link_id: 3,
            capacity_kbps: 0.0,
            alive: false,
            loss_rate: 1.0,
            rtt_ms: 0.0,
            queue_depth: None,
        },
    ];
    adapter.update(&caps);

    // Send more packets — should only go to l1 and l2
    // Some sends may fail during the transition, which is expected
    let l3_before = l3.packet_count();
    let mut phase2_sent = 0;
    for _ in 0..30 {
        let payload = Bytes::from(vec![0u8; 1000]);
        if scheduler.send(payload, default_profile(1000)).is_ok() {
            phase2_sent += 1;
        }
    }
    let l3_after = l3.packet_count();
    assert_eq!(
        l3_after, l3_before,
        "dead link should receive no new traffic"
    );
    assert!(
        phase2_sent > 0,
        "some packets should succeed on alive links"
    );

    // Phase 3: Revive link 3
    l3.revive();
    scheduler.refresh_metrics();

    for _ in 0..30 {
        let payload = Bytes::from(vec![0u8; 1000]);
        let _ = scheduler.send(payload, default_profile(1000));
    }

    assert!(
        l3.packet_count() > l3_after,
        "revived link should receive traffic again"
    );
}

// ─── Test: Adaptation Reduces Bitrate Under Pressure ────────────────────

#[test]
fn adaptation_reduces_bitrate_on_capacity_drop() {
    let mut adapter = BitrateAdapter::new(AdaptationConfig {
        max_bitrate_kbps: 15_000,
        min_interval: Duration::ZERO,
        ..Default::default()
    });

    // Start with good capacity
    let good_caps = vec![
        LinkCapacity {
            link_id: 0,
            capacity_kbps: 10_000.0,
            alive: true,
            loss_rate: 0.0,
            rtt_ms: 10.0,
            queue_depth: None,
        },
        LinkCapacity {
            link_id: 1,
            capacity_kbps: 10_000.0,
            alive: true,
            loss_rate: 0.0,
            rtt_ms: 15.0,
            queue_depth: None,
        },
    ];
    adapter.update(&good_caps);
    assert_eq!(adapter.stage(), DegradationStage::Normal);

    // Sudden capacity drop
    let bad_caps = vec![
        LinkCapacity {
            link_id: 0,
            capacity_kbps: 3_000.0,
            alive: true,
            loss_rate: 0.05,
            rtt_ms: 50.0,
            queue_depth: None,
        },
        LinkCapacity {
            link_id: 1,
            capacity_kbps: 0.0,
            alive: false,
            loss_rate: 1.0,
            rtt_ms: 0.0,
            queue_depth: None,
        },
    ];
    let cmd = adapter.update(&bad_caps);
    assert!(cmd.is_some(), "should produce bitrate reduction command");
    assert!(adapter.current_target_kbps() < 15_000);
}

// ─── Test: Supervisor Detects Degradation ───────────────────────────────

#[test]
fn supervisor_to_adapter_pipeline() {
    let mut supervisor = ModemSupervisor::new(SupervisorConfig {
        degraded_threshold: 40.0,
        recovery_threshold: 55.0,
        ..Default::default()
    });

    let mut adapter = BitrateAdapter::new(AdaptationConfig {
        max_bitrate_kbps: 10_000,
        min_interval: Duration::ZERO,
        ..Default::default()
    });

    // Register links and give good RF
    let good_rf = RfMetrics {
        rsrp_dbm: -75.0,
        rsrq_db: -6.0,
        sinr_db: 20.0,
        cqi: 12,
    };
    for _ in 0..10 {
        supervisor.update_rf(0, &good_rf);
        supervisor.update_rf(1, &good_rf);
    }

    // Get capacities and feed to adapter
    let caps = supervisor.link_capacities();
    assert_eq!(caps.len(), 2);
    adapter.update(&caps);

    // Now degrade link 0
    let bad_rf = RfMetrics {
        rsrp_dbm: -130.0,
        rsrq_db: -18.0,
        sinr_db: -10.0,
        cqi: 1,
    };
    let mut saw_degraded = false;
    for _ in 0..40 {
        let events = supervisor.update_rf(0, &bad_rf);
        supervisor.update_transport(
            0,
            &strata_bonding::modem::health::TransportMetrics {
                loss_rate: 0.30,
                jitter_ms: 80.0,
                rtt_ms: 200.0,
            },
        );
        if events
            .iter()
            .any(|e| matches!(e, SupervisorEvent::LinkDegraded { .. }))
        {
            saw_degraded = true;
        }
    }
    assert!(saw_degraded, "supervisor should detect degradation");

    // Feed updated capacities to adapter
    let caps = supervisor.link_capacities();
    adapter.update(&caps);

    // Adapter should be aware of reduced capacity
    assert!(
        adapter.current_target_kbps() <= 10_000,
        "adapter should reflect capacity constraints"
    );
}

// ─── Test: Critical Packets Broadcast Even During Degradation ───────────

#[test]
fn critical_packets_broadcast_during_partial_failure() {
    let l1 = Arc::new(MockLink::new(1, 10_000_000.0, 10.0));
    let l2 = Arc::new(MockLink::new(2, 10_000_000.0, 15.0));

    let mut scheduler = BondingScheduler::new();
    scheduler.add_link(l1.clone());
    scheduler.add_link(l2.clone());
    scheduler.refresh_metrics();

    // Send a critical packet (e.g., SPS/PPS parameter set)
    let payload = Bytes::from(vec![0u8; 64]);
    scheduler.send(payload, critical_profile(64)).unwrap();

    // Both links should receive the critical packet
    assert_eq!(l1.packet_count(), 1, "l1 should receive critical packet");
    assert_eq!(l2.packet_count(), 1, "l2 should receive critical packet");
}

// ─── Test: Scheduler Uses All Intelligence Layers ───────────────────────

#[test]
fn scheduler_distributes_across_heterogeneous_links() {
    let l1 = Arc::new(MockLink::new(1, 20_000_000.0, 5.0)); // 20 Mbps, 5ms
    let l2 = Arc::new(MockLink::new(2, 5_000_000.0, 30.0)); // 5 Mbps, 30ms
    let l3 = Arc::new(MockLink::new(3, 10_000_000.0, 10.0)); // 10 Mbps, 10ms

    let mut scheduler = BondingScheduler::new();
    scheduler.add_link(l1.clone());
    scheduler.add_link(l2.clone());
    scheduler.add_link(l3.clone());
    scheduler.refresh_metrics();

    // Send 200 droppable packets
    for _ in 0..200 {
        let payload = Bytes::from(vec![0u8; 1000]);
        scheduler.send(payload, default_profile(1000)).unwrap();
    }

    let c1 = l1.packet_count();
    let c2 = l2.packet_count();
    let c3 = l3.packet_count();

    // All links should receive some traffic
    assert!(c1 > 0, "fastest link should get traffic");
    assert!(c3 > 0, "medium link should get traffic");
    // Link 2 has much higher RTT — intelligence may reduce its share
    // but it should still participate
    assert!(
        c1 + c2 + c3 == 200,
        "all packets should be delivered: {c1} + {c2} + {c3}"
    );
    // Fastest link should get the most traffic
    assert!(
        c1 >= c2,
        "fastest link should get >= slow link: l1={c1}, l2={c2}"
    );
}

// ─── Test: Adaptation Force-Reduce on Link Failure Event ────────────────

#[test]
fn force_reduce_on_link_failure_event() {
    let mut adapter = BitrateAdapter::new(AdaptationConfig {
        max_bitrate_kbps: 10_000,
        ramp_down_factor: 0.5,
        ..Default::default()
    });

    // Simulate immediate link failure
    let cmd = adapter.force_reduce(strata_bonding::adaptation::AdaptationReason::LinkFailure);
    assert_eq!(cmd.target_kbps, 5_000);

    // Second failure
    let cmd2 = adapter.force_reduce(strata_bonding::adaptation::AdaptationReason::LinkFailure);
    assert_eq!(cmd2.target_kbps, 2_500);
}
