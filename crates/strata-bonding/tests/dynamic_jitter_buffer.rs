//! Dynamic jitter buffer tests — validates adaptive latency sizing
//! under realistic cellular network patterns.
//!
//! Covers: burst reordering, jitter spike recovery, latency floor/ceiling,
//! and the adaptive sizing algorithm that replaces static buffers.

use bytes::Bytes;
use quanta::Instant;
use std::time::Duration;

use strata_bonding::receiver::aggregator::{ReassemblyBuffer, ReassemblyConfig};

// ────────────────────────────────────────────────────────────────
// 1. Cellular burst pattern: steady → spike → recovery
// ────────────────────────────────────────────────────────────────

#[test]
fn adaptive_latency_spike_and_recovery() {
    let config = ReassemblyConfig {
        start_latency: Duration::from_millis(20),
        buffer_capacity: 512,
        skip_after: Some(Duration::from_millis(300)),
        jitter_latency_multiplier: 4.0,
        max_latency_ms: 200,
    };
    let mut buf = ReassemblyBuffer::with_config(0, config);
    let start = Instant::now();

    // Phase 1: Steady (10ms IAT, minimal jitter)
    for i in 0u64..50 {
        let t = start + Duration::from_millis(i * 10);
        buf.push(i, Bytes::from(vec![i as u8]), t);
    }
    let latency_steady = buf.get_stats().current_latency_ms;

    // Phase 2: Jitter spike (alternating 2ms and 40ms IATs)
    let spike_base = start + Duration::from_millis(500);
    for i in 50u64..100 {
        let offset_ms = (i - 50) * if (i - 50) % 2 == 0 { 2 } else { 40 };
        let t = spike_base + Duration::from_millis(offset_ms);
        buf.push(i, Bytes::from(vec![i as u8]), t);
    }
    let latency_spike = buf.get_stats().current_latency_ms;

    // Latency should have increased during the spike
    assert!(
        latency_spike > latency_steady,
        "latency should increase during spike: steady={latency_steady}ms, spike={latency_spike}ms"
    );

    // Phase 3: Recovery (steady 10ms IAT again)
    // The jitter samples window (128 samples) will gradually flush the old spiky values.
    let recovery_base = spike_base + Duration::from_secs(2);
    for i in 100u64..250 {
        let t = recovery_base + Duration::from_millis((i - 100) * 10);
        buf.push(i, Bytes::from(vec![i as u8]), t);
    }
    let latency_recovered = buf.get_stats().current_latency_ms;

    // After 150 stable samples (window is 128), jitter should decrease
    assert!(
        latency_recovered < latency_spike,
        "latency should decrease after recovery: spike={latency_spike}ms, recovered={latency_recovered}ms"
    );
}

// ────────────────────────────────────────────────────────────────
// 2. Max latency ceiling is enforced under extreme jitter
// ────────────────────────────────────────────────────────────────

#[test]
fn latency_ceiling_under_extreme_jitter() {
    let config = ReassemblyConfig {
        start_latency: Duration::from_millis(10),
        buffer_capacity: 256,
        skip_after: None,
        jitter_latency_multiplier: 10.0, // Aggressive multiplier
        max_latency_ms: 150,
    };
    let mut buf = ReassemblyBuffer::with_config(0, config);
    let start = Instant::now();

    // Extreme jitter: 1ms then 200ms alternating
    for i in 0u64..100 {
        let offset = if i % 2 == 0 { i } else { i * 200 };
        let t = start + Duration::from_millis(offset);
        buf.push(i, Bytes::from(vec![0u8]), t);
    }

    let latency = buf.get_stats().current_latency_ms;
    assert!(
        latency <= 150,
        "latency must not exceed max_latency_ms=150, got {latency}ms"
    );
}

// ────────────────────────────────────────────────────────────────
// 3. Burst reordering: receiver handles out-of-order arrivals
// ────────────────────────────────────────────────────────────────

#[test]
fn handles_burst_reordering_pattern() {
    let config = ReassemblyConfig {
        start_latency: Duration::from_millis(50),
        buffer_capacity: 64,
        skip_after: Some(Duration::from_millis(100)),
        jitter_latency_multiplier: 4.0,
        max_latency_ms: 300,
    };
    let mut buf = ReassemblyBuffer::with_config(0, config);
    let start = Instant::now();

    // Simulate cellular burst reordering:
    // Packets 0-4 arrive in-order, then 5-9 arrive reversed
    for i in 0u64..5 {
        buf.push(i, Bytes::from(format!("p{i}")), start);
    }
    // Reversed burst: 9, 8, 7, 6, 5
    for i in (5u64..10).rev() {
        buf.push(i, Bytes::from(format!("p{i}")), start);
    }

    // Tick after latency — all should come out in order
    let out = buf.tick(start + Duration::from_millis(51));
    assert_eq!(out.len(), 10, "all 10 packets should be delivered");
    for (i, data) in out.iter().enumerate() {
        assert_eq!(data, &Bytes::from(format!("p{i}")), "packet {i} mismatch");
    }
    assert_eq!(buf.lost_packets, 0, "no packets should be lost");
}

// ────────────────────────────────────────────────────────────────
// 4. Skip-after policy prevents HOL blocking
// ────────────────────────────────────────────────────────────────

#[test]
fn skip_after_prevents_hol_blocking() {
    let config = ReassemblyConfig {
        start_latency: Duration::from_millis(50),
        buffer_capacity: 64,
        skip_after: Some(Duration::from_millis(30)),
        jitter_latency_multiplier: 4.0,
        max_latency_ms: 500,
    };
    let mut buf = ReassemblyBuffer::with_config(0, config);
    let start = Instant::now();

    // Seq 0 is permanently lost; seq 1-5 arrive
    for i in 1u64..6 {
        buf.push(i, Bytes::from(format!("p{i}")), start);
    }

    // At 20ms: skip_after not reached yet — nothing released (waiting for seq 0)
    let out = buf.tick(start + Duration::from_millis(20));
    assert_eq!(out.len(), 0, "should wait for seq 0 initially");

    // At 30ms: skip_after reached — should skip seq 0 and release 1-5
    let out = buf.tick(start + Duration::from_millis(30));
    assert_eq!(out.len(), 5, "should release 1-5 after skip_after");
    assert_eq!(buf.lost_packets, 1, "seq 0 should be counted as lost");
}

// ────────────────────────────────────────────────────────────────
// 5. Latency floor: never drops below start_latency
// ────────────────────────────────────────────────────────────────

#[test]
fn latency_never_drops_below_start() {
    let config = ReassemblyConfig {
        start_latency: Duration::from_millis(30),
        buffer_capacity: 256,
        skip_after: None,
        jitter_latency_multiplier: 4.0,
        max_latency_ms: 500,
    };
    let mut buf = ReassemblyBuffer::with_config(0, config);
    let start = Instant::now();

    // Perfectly constant IAT — zero jitter
    for i in 0u64..200 {
        let t = start + Duration::from_millis(i * 10);
        buf.push(i, Bytes::from(vec![0u8]), t);
    }

    let latency = buf.get_stats().current_latency_ms;
    assert!(
        latency >= 30,
        "latency should never drop below start_latency=30ms, got {latency}ms"
    );
}

// ────────────────────────────────────────────────────────────────
// 6. Large buffer capacity: stress test with 2048 concurrent packets
// ────────────────────────────────────────────────────────────────

#[test]
fn large_buffer_capacity_stress() {
    let config = ReassemblyConfig {
        start_latency: Duration::from_millis(5),
        buffer_capacity: 4096,
        skip_after: None,
        jitter_latency_multiplier: 4.0,
        max_latency_ms: 500,
    };
    let mut buf = ReassemblyBuffer::with_config(0, config);
    let start = Instant::now();

    let count = 3000u64;
    for i in 0..count {
        buf.push(i, Bytes::from(vec![0u8; 1400]), start);
    }

    let out = buf.tick(start + Duration::from_millis(5));
    assert_eq!(
        out.len(),
        count as usize,
        "all {count} packets should be delivered"
    );
    assert_eq!(buf.lost_packets, 0);
    assert_eq!(buf.duplicate_packets, 0);
}
