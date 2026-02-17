//! # Integration tests: Sender ↔ Receiver through the wire format
//!
//! These tests verify the full vertical stack:
//! Sender → wire encode → Receiver → deliver
//!
//! No actual network I/O — the "network" is simulated by passing Bytes
//! directly. Impairment (loss, reorder, duplication) is applied in the middle.

use bytes::Bytes;
use std::time::Duration;
use strata_transport::pool::Priority;
use strata_transport::receiver::{DeliveredPacket, Receiver, ReceiverConfig, ReceiverEvent};
use strata_transport::sender::{OutputPacket, Sender, SenderConfig};

// ─── Helpers ────────────────────────────────────────────────────────────────

/// Simulate a perfect network: sender → receiver with zero loss.
fn perfect_transfer(sender: &mut Sender, receiver: &mut Receiver) {
    let output: Vec<OutputPacket> = sender.drain_output().collect();
    for pkt in output {
        if !pkt.is_fec_repair {
            receiver.receive(pkt.data);
        }
    }
}

/// Collect all delivered packets from receiver events.
fn collect_deliveries(receiver: &mut Receiver) -> Vec<DeliveredPacket> {
    receiver
        .drain_events()
        .filter_map(|e| match e {
            ReceiverEvent::Deliver(d) => Some(d),
            _ => None,
        })
        .collect()
}

fn test_sender() -> Sender {
    Sender::new(SenderConfig {
        max_payload_size: 1200,
        pool_capacity: 512,
        fec_k: 8,
        fec_r: 2,
        packet_ttl: Duration::from_secs(5),
        max_retries: 3,
    })
}

fn test_receiver() -> Receiver {
    Receiver::new(ReceiverConfig {
        reorder_capacity: 512,
        max_fec_generations: 32,
        nack_rearm_ms: 0, // instant for tests
        max_nack_retries: 3,
    })
}

// ─── Perfect Network (Zero Loss) ───────────────────────────────────────────

#[test]
fn end_to_end_single_packet() {
    let mut tx = test_sender();
    let mut rx = test_receiver();

    tx.send(Bytes::from_static(b"hello world"), Priority::Standard);
    perfect_transfer(&mut tx, &mut rx);

    let delivered = collect_deliveries(&mut rx);
    assert_eq!(delivered.len(), 1);
    assert_eq!(delivered[0].payload, &b"hello world"[..]);
}

#[test]
fn end_to_end_sequence_100_packets() {
    let mut tx = test_sender();
    let mut rx = test_receiver();

    for i in 0u32..100 {
        let data = format!("packet-{i}");
        tx.send(Bytes::from(data), Priority::Standard);
    }
    perfect_transfer(&mut tx, &mut rx);

    let delivered = collect_deliveries(&mut rx);
    assert_eq!(delivered.len(), 100, "should deliver all 100 packets");

    // Verify ordering
    for (i, d) in delivered.iter().enumerate() {
        let expected = format!("packet-{i}");
        assert_eq!(
            d.payload,
            expected.as_bytes(),
            "packet {i} payload mismatch"
        );
    }
}

#[test]
fn end_to_end_preserves_priority_flags() {
    let mut tx = test_sender();
    let mut rx = test_receiver();

    // Critical → sets both keyframe + config
    tx.send(Bytes::from_static(b"sps-pps"), Priority::Critical);
    // Reference → sets keyframe
    tx.send(Bytes::from_static(b"idr-frame"), Priority::Reference);
    // Standard → no flags
    tx.send(Bytes::from_static(b"p-frame"), Priority::Standard);

    perfect_transfer(&mut tx, &mut rx);
    let delivered = collect_deliveries(&mut rx);
    assert_eq!(delivered.len(), 3);

    assert!(delivered[0].is_config, "Critical should set config flag");
    assert!(
        delivered[0].is_keyframe,
        "Critical should set keyframe flag"
    );
    assert!(
        delivered[1].is_keyframe,
        "Reference should set keyframe flag"
    );
    assert!(
        !delivered[1].is_config,
        "Reference should NOT set config flag"
    );
    assert!(
        !delivered[2].is_keyframe,
        "Standard should NOT set keyframe flag"
    );
}

// ─── Simulated Loss + ARQ Recovery ─────────────────────────────────────────

#[test]
fn loss_recovery_via_nack_retransmit() {
    let mut tx = test_sender();
    let mut rx = test_receiver();

    // Send 5 packets
    for i in 0..5u8 {
        tx.send(Bytes::from(vec![i; 50]), Priority::Standard);
    }

    // Simulate loss of packet seq=2
    let output: Vec<OutputPacket> = tx.drain_output().collect();
    for pkt in &output {
        if !pkt.is_fec_repair && pkt.sequence != 2 {
            rx.receive(pkt.data.clone());
        }
    }

    // Receiver should have delivered 0, 1 then stalled at gap
    let delivered = collect_deliveries(&mut rx);
    assert_eq!(delivered.len(), 2, "should deliver 0 and 1 before gap");

    // Receiver generates NACK for seq=2
    let nack = rx.generate_nacks();
    assert!(nack.is_some(), "should generate NACK for gap at seq=2");
    let nack = nack.unwrap();

    // Verify NACK covers seq=2
    let nacked_seqs: Vec<u64> = nack
        .ranges
        .iter()
        .flat_map(|r| {
            let s = r.start.value();
            let c = r.count.value();
            s..s + c
        })
        .collect();
    assert!(nacked_seqs.contains(&2), "NACK should include seq=2");

    // Feed NACK back to sender → retransmit
    let retransmitted = tx.process_nack(&nack);
    assert!(
        retransmitted >= 1,
        "sender should retransmit at least 1 pkt"
    );

    // Feed retransmitted packets to receiver
    perfect_transfer(&mut tx, &mut rx);

    // Now packets 2, 3, 4 should deliver
    let delivered = collect_deliveries(&mut rx);
    assert_eq!(
        delivered.len(),
        3,
        "should deliver remaining 3 packets after retransmit"
    );
}

// ─── ACK Feedback Loop ─────────────────────────────────────────────────────

#[test]
fn ack_feedback_frees_sender_pool() {
    let mut tx = test_sender();
    let mut rx = test_receiver();

    // Send 10 packets
    for i in 0..10u8 {
        tx.send(Bytes::from(vec![i; 10]), Priority::Standard);
    }
    perfect_transfer(&mut tx, &mut rx);
    collect_deliveries(&mut rx); // drain but don't inspect

    assert_eq!(tx.in_flight(), 10, "10 packets should be in flight");

    // Generate ACK from receiver and feed to sender
    let ack = rx.generate_ack();
    let acked = tx.process_ack(&ack);
    assert_eq!(acked, 10, "all 10 should be acknowledged");
    assert_eq!(tx.in_flight(), 0, "pool should be empty after ACK");
}

#[test]
fn partial_ack_frees_subset() {
    let mut tx = test_sender();
    let mut rx = test_receiver();

    // Send 10 packets, deliver only first 5 in order
    for i in 0..10u8 {
        tx.send(Bytes::from(vec![i; 10]), Priority::Standard);
    }
    let output: Vec<OutputPacket> = tx.drain_output().collect();

    // Only deliver seqs 0-4 (skip 5-9)
    for pkt in &output {
        if !pkt.is_fec_repair && pkt.sequence < 5 {
            rx.receive(pkt.data.clone());
        }
    }
    collect_deliveries(&mut rx);

    let ack = rx.generate_ack();
    assert_eq!(ack.cumulative_seq.value(), 4);

    let acked = tx.process_ack(&ack);
    assert_eq!(acked, 5); // seqs 0-4
    assert_eq!(tx.in_flight(), 5); // seqs 5-9 still in flight
}

// ─── Reordering ─────────────────────────────────────────────────────────────

#[test]
fn out_of_order_packets_delivered_correctly() {
    let mut tx = test_sender();
    let mut rx = test_receiver();

    for i in 0..5u8 {
        tx.send(Bytes::from(vec![i; 20]), Priority::Standard);
    }
    let output: Vec<OutputPacket> = tx.drain_output().collect();

    // Deliver in wrong order: 0, 2, 4, 1, 3
    let data_pkts: Vec<&OutputPacket> = output.iter().filter(|p| !p.is_fec_repair).collect();
    for &seq in &[0u64, 2, 4, 1, 3] {
        let pkt = data_pkts.iter().find(|p| p.sequence == seq).unwrap();
        rx.receive(pkt.data.clone());
    }

    let delivered = collect_deliveries(&mut rx);
    assert_eq!(delivered.len(), 5, "all 5 should eventually deliver");

    // Verify in-order delivery
    for (i, d) in delivered.iter().enumerate() {
        assert_eq!(d.sequence, i as u64, "delivery order should be sequential");
    }
}

// ─── Duplicate Handling ─────────────────────────────────────────────────────

#[test]
fn duplicates_not_delivered_twice() {
    let mut tx = test_sender();
    let mut rx = test_receiver();

    tx.send(Bytes::from_static(b"data"), Priority::Standard);
    let output: Vec<OutputPacket> = tx.drain_output().collect();

    // Send same packet twice
    let data_pkt = output.iter().find(|p| !p.is_fec_repair).unwrap();
    rx.receive(data_pkt.data.clone());
    rx.receive(data_pkt.data.clone());

    let delivered = collect_deliveries(&mut rx);
    assert_eq!(delivered.len(), 1, "duplicate should not deliver twice");
    assert_eq!(rx.stats().duplicates, 1);
}

// ─── Fragmentation E2E ─────────────────────────────────────────────────────

#[test]
fn fragmented_packet_reassembled() {
    let mut tx = Sender::new(SenderConfig {
        max_payload_size: 100, // Force fragmentation
        pool_capacity: 256,
        fec_k: 16,
        fec_r: 2,
        packet_ttl: Duration::from_secs(5),
        max_retries: 3,
    });
    let mut rx = test_receiver();

    // 300 bytes → 3 fragments (100 + 100 + 100)
    let payload = Bytes::from(vec![0xAB; 300]);
    tx.send(payload.clone(), Priority::Standard);
    perfect_transfer(&mut tx, &mut rx);

    let delivered = collect_deliveries(&mut rx);
    assert_eq!(
        delivered.len(),
        1,
        "fragments should reassemble into 1 delivery"
    );
    assert_eq!(delivered[0].payload.len(), 300);
    assert!(delivered[0].payload.iter().all(|&b| b == 0xAB));
}

// ─── Statistics Consistency ─────────────────────────────────────────────────

#[test]
fn stats_consistency_after_transfer() {
    let mut tx = test_sender();
    let mut rx = test_receiver();

    for i in 0..50u8 {
        tx.send(Bytes::from(vec![i; 100]), Priority::Standard);
    }
    perfect_transfer(&mut tx, &mut rx);
    collect_deliveries(&mut rx);

    assert_eq!(tx.stats().packets_sent, 50);
    assert_eq!(tx.stats().bytes_sent, 50 * 100);
    assert_eq!(rx.stats().packets_received, 50);
    assert_eq!(rx.stats().packets_delivered, 50);
    assert_eq!(rx.stats().duplicates, 0);
}

// ─── Burst Loss + ARQ ──────────────────────────────────────────────────────

#[test]
fn burst_loss_recovery() {
    let mut tx = test_sender();
    let mut rx = test_receiver();

    // Send 20 packets
    for i in 0..20u8 {
        tx.send(Bytes::from(vec![i; 50]), Priority::Standard);
    }

    let output: Vec<OutputPacket> = tx.drain_output().collect();

    // Simulate burst loss of seqs 5-9 (5 consecutive packets)
    for pkt in &output {
        if !pkt.is_fec_repair && !(5..=9).contains(&pkt.sequence) {
            rx.receive(pkt.data.clone());
        }
    }

    // Should deliver 0-4
    let delivered = collect_deliveries(&mut rx);
    assert_eq!(
        delivered.len(),
        5,
        "should deliver packets 0-4 before burst gap"
    );

    // NACK → retransmit → deliver the rest
    let nack = rx.generate_nacks().expect("should NACK burst gap");
    let retransmitted = tx.process_nack(&nack);
    assert!(retransmitted >= 5, "should retransmit all 5 lost packets");

    perfect_transfer(&mut tx, &mut rx);
    let delivered = collect_deliveries(&mut rx);
    assert_eq!(delivered.len(), 15, "should deliver remaining 15 packets");
}

// ─── SACK Bitmap ────────────────────────────────────────────────────────────

#[test]
fn sack_ack_with_gaps() {
    let mut tx = test_sender();
    let mut rx = test_receiver();

    for i in 0..10u8 {
        tx.send(Bytes::from(vec![i; 10]), Priority::Standard);
    }
    let output: Vec<OutputPacket> = tx.drain_output().collect();

    // Deliver: 0, 1, 3, 5, 7 (gaps at 2, 4, 6, 8, 9)
    for pkt in &output {
        if !pkt.is_fec_repair && [0u64, 1, 3, 5, 7].contains(&pkt.sequence) {
            rx.receive(pkt.data.clone());
        }
    }
    collect_deliveries(&mut rx);

    let ack = rx.generate_ack();
    // Cumulative should be 1 (last fully contiguous)
    assert_eq!(ack.cumulative_seq.value(), 1);
    // SACK bitmap should mark 3, 5, 7 as received (bits 1, 3, 5)
    assert!(ack.sack_bitmap & (1 << 1) != 0, "seq 3 should be in SACK");
    assert!(ack.sack_bitmap & (1 << 3) != 0, "seq 5 should be in SACK");
    assert!(ack.sack_bitmap & (1 << 5) != 0, "seq 7 should be in SACK");

    // Feed SACK back to sender
    let acked = tx.process_ack(&ack);
    // seqs 0, 1 (cumulative) + 3, 5, 7 (SACK) = 5
    assert_eq!(acked, 5);
    assert_eq!(tx.in_flight(), 5); // seqs 2, 4, 6, 8, 9 still in flight
}

// ─── Deterministic Simulation Tests ─────────────────────────────────────
//
// These replace turmoil-based tests (turmoil is tokio-only; we use monoio).
// They deterministically simulate network conditions with seeded RNG for
// reproducibility, matching the master plan's DST intent.

use rand::rngs::SmallRng;
use rand::{RngExt, SeedableRng};

/// Simulate a lossy network: sender → receiver with seeded random packet loss.
/// Protects seq 0 from loss (simulates session handshake ensuring first packet arrives).
fn lossy_transfer(
    sender: &mut Sender,
    receiver: &mut Receiver,
    rng: &mut SmallRng,
    loss_rate: f64,
) -> Vec<u64> {
    let output: Vec<OutputPacket> = sender.drain_output().collect();
    let mut dropped = Vec::new();
    for pkt in output {
        if pkt.is_fec_repair {
            receiver.receive(pkt.data);
            continue;
        }
        // Never drop seq 0 — receiver needs it to initialize correctly
        if pkt.sequence > 0 && rng.random_bool(loss_rate) {
            dropped.push(pkt.sequence);
        } else {
            receiver.receive(pkt.data);
        }
    }
    dropped
}

/// Run full ARQ recovery loop: NACK → retransmit → deliver, up to `max_rounds`.
fn arq_recovery(sender: &mut Sender, receiver: &mut Receiver, max_rounds: usize) {
    for _ in 0..max_rounds {
        if let Some(nack) = receiver.generate_nacks() {
            let retransmitted = sender.process_nack(&nack);
            if retransmitted == 0 {
                break;
            }
            perfect_transfer(sender, receiver);
        } else {
            break;
        }
    }
}

// ─── Phase 0: 10,000 packet delivery ───────────────────────────────────

#[test]
fn simulation_10k_packets_perfect_delivery() {
    let mut tx = Sender::new(SenderConfig {
        max_payload_size: 1200,
        pool_capacity: 16384,
        fec_k: 32,
        fec_r: 4,
        packet_ttl: Duration::from_secs(30),
        max_retries: 5,
    });
    let mut rx = Receiver::new(ReceiverConfig {
        reorder_capacity: 16384,
        max_fec_generations: 512,
        nack_rearm_ms: 0,
        max_nack_retries: 5,
    });

    let count = 10_000;
    for i in 0u32..count {
        let data = format!("pkt-{i:05}");
        tx.send(Bytes::from(data), Priority::Standard);
    }
    perfect_transfer(&mut tx, &mut rx);

    let delivered = collect_deliveries(&mut rx);
    assert_eq!(
        delivered.len(),
        count as usize,
        "all 10,000 packets should be delivered"
    );

    // Verify ordering
    for (i, d) in delivered.iter().enumerate() {
        let expected = format!("pkt-{i:05}");
        assert_eq!(d.payload, expected.as_bytes(), "packet {i} mismatch");
    }

    // Verify stats
    assert_eq!(tx.stats().packets_sent, count as u64);
    assert_eq!(rx.stats().packets_delivered, count as u64);
    assert_eq!(rx.stats().duplicates, 0);
}

// ─── Phase 1: Random loss at 5%, 10%, 20% ──────────────────────────────

fn run_loss_recovery_test(loss_rate: f64, seed: u64) {
    let mut tx = Sender::new(SenderConfig {
        max_payload_size: 1200,
        pool_capacity: 4096,
        fec_k: 16,
        fec_r: 4,
        packet_ttl: Duration::from_secs(30),
        max_retries: 50,
    });
    let mut rx = Receiver::new(ReceiverConfig {
        reorder_capacity: 4096,
        max_fec_generations: 256,
        nack_rearm_ms: 0,
        max_nack_retries: 50,
    });
    let mut rng = SmallRng::seed_from_u64(seed);

    let count = 1_000u32;
    let mut delivered_seqs = std::collections::HashSet::new();

    // Send all main packets
    for i in 0..count {
        let data = format!("L{i:04}");
        tx.send(Bytes::from(data), Priority::Standard);
    }

    // Initial transfer with loss (seq 0 protected to ensure receiver initialization)
    lossy_transfer(&mut tx, &mut rx, &mut rng, loss_rate);
    for d in collect_deliveries(&mut rx) {
        delivered_seqs.insert(d.sequence);
    }

    // Send probe packets to trigger NACK detection for any tail losses.
    // In production, the continuous stream provides this naturally.
    for _ in 0..3u32 {
        tx.send(Bytes::from("probe"), Priority::Standard);
    }
    perfect_transfer(&mut tx, &mut rx);
    for d in collect_deliveries(&mut rx) {
        delivered_seqs.insert(d.sequence);
    }

    // Iterative ARQ recovery
    for _ in 0..100 {
        // Check if all main packet seqs (0..count) have been delivered
        let main_delivered = (0..count as u64)
            .filter(|s| delivered_seqs.contains(s))
            .count();
        if main_delivered >= count as usize {
            break;
        }
        arq_recovery(&mut tx, &mut rx, 1);
        for d in collect_deliveries(&mut rx) {
            delivered_seqs.insert(d.sequence);
        }
    }

    let main_delivered = (0..count as u64)
        .filter(|s| delivered_seqs.contains(s))
        .count();
    assert_eq!(
        main_delivered, count as usize,
        "{loss_rate:.0}% loss (seed={seed}): expected {count} deliveries, got {main_delivered}"
    );
}

#[test]
fn simulation_5_percent_random_loss_zero_app_loss() {
    run_loss_recovery_test(0.05, 0xDEAD_0005);
}

#[test]
fn simulation_10_percent_random_loss_zero_app_loss() {
    run_loss_recovery_test(0.10, 0xDEAD_0010);
}

#[test]
fn simulation_20_percent_random_loss_zero_app_loss() {
    run_loss_recovery_test(0.20, 0xDEAD_0020);
}

// ─── Phase 1: Burst loss (Gilbert-Elliott model) ────────────────────────

/// Gilbert-Elliott 2-state Markov chain loss model.
struct GilbertElliottLoss {
    /// Probability of transitioning from Good to Bad state.
    p_good_to_bad: f64,
    /// Probability of transitioning from Bad to Good state.
    p_bad_to_good: f64,
    /// Loss probability in the Bad state.
    p_loss_in_bad: f64,
    /// Current state: true = Good, false = Bad.
    in_good_state: bool,
}

impl GilbertElliottLoss {
    fn new(p_good_to_bad: f64, p_bad_to_good: f64, p_loss_in_bad: f64) -> Self {
        Self {
            p_good_to_bad,
            p_bad_to_good,
            p_loss_in_bad,
            in_good_state: true,
        }
    }

    /// Returns true if this packet should be dropped.
    fn should_drop(&mut self, rng: &mut SmallRng) -> bool {
        // State transition
        if self.in_good_state {
            if rng.random_bool(self.p_good_to_bad) {
                self.in_good_state = false;
            }
        } else if rng.random_bool(self.p_bad_to_good) {
            self.in_good_state = true;
        }

        // Loss decision
        if !self.in_good_state {
            rng.random_bool(self.p_loss_in_bad)
        } else {
            false
        }
    }
}

#[test]
fn simulation_burst_loss_gilbert_elliott_recovery() {
    let mut tx = Sender::new(SenderConfig {
        max_payload_size: 1200,
        pool_capacity: 4096,
        fec_k: 16,
        fec_r: 4,
        packet_ttl: Duration::from_secs(30),
        max_retries: 10,
    });
    let mut rx = Receiver::new(ReceiverConfig {
        reorder_capacity: 4096,
        max_fec_generations: 256,
        nack_rearm_ms: 0,
        max_nack_retries: 10,
    });
    let mut rng = SmallRng::seed_from_u64(0xB0857);
    // p(G→B)=5%, p(B→G)=30%, p(loss|B)=80% — produces bursty loss patterns
    let mut ge_model = GilbertElliottLoss::new(0.05, 0.30, 0.80);

    let count = 1_000u32;
    let batch_size = 50;
    let mut total_delivered = 0usize;
    let mut total_dropped = 0usize;

    for batch_start in (0..count).step_by(batch_size) {
        let batch_end = (batch_start + batch_size as u32).min(count);
        for i in batch_start..batch_end {
            let data = format!("GE-{i:04}");
            tx.send(Bytes::from(data), Priority::Standard);
        }

        // Transfer with Gilbert-Elliott loss
        let output: Vec<OutputPacket> = tx.drain_output().collect();
        for pkt in output {
            if pkt.is_fec_repair {
                rx.receive(pkt.data);
                continue;
            }
            if ge_model.should_drop(&mut rng) {
                total_dropped += 1;
            } else {
                rx.receive(pkt.data);
            }
        }

        total_delivered += collect_deliveries(&mut rx).len();

        // ARQ recovery
        arq_recovery(&mut tx, &mut rx, 5);
        total_delivered += collect_deliveries(&mut rx).len();
    }

    // Final sweep
    for _ in 0..20 {
        arq_recovery(&mut tx, &mut rx, 1);
        let d = collect_deliveries(&mut rx);
        if d.is_empty() {
            break;
        }
        total_delivered += d.len();
    }

    assert!(
        total_dropped > 0,
        "Gilbert-Elliott model should drop some packets (got 0 drops)"
    );
    assert_eq!(
        total_delivered, count as usize,
        "burst loss: expected {count} deliveries, got {total_delivered} (dropped {total_dropped})"
    );
}
