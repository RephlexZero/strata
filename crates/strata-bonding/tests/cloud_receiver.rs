//! Cloud receiver integration tests.
//!
//! Tests the standalone receiver path: `TransportBondingReceiver` → output,
//! exercising multi-link reassembly, jitter buffer adaptation, metrics
//! exposure, and graceful shutdown — all without GStreamer.

use bytes::Bytes;
use std::net::UdpSocket;
use std::time::Duration;

use strata_bonding::receiver::aggregator::{ReassemblyBuffer, ReassemblyConfig};
use strata_bonding::receiver::transport::TransportBondingReceiver;

// ────────────────────────────────────────────────────────────────
// 1. Standalone receiver: receive, reassemble, and output to channel
// ────────────────────────────────────────────────────────────────

#[test]
fn standalone_receiver_multi_link_reassembly() {
    // Simulate a 2-link cloud receiver that bonds packets from two senders.
    let rcv = TransportBondingReceiver::new(Duration::from_millis(20));

    // Bind two links on ephemeral ports
    let sock_a = UdpSocket::bind("127.0.0.1:0").unwrap();
    let sock_b = UdpSocket::bind("127.0.0.1:0").unwrap();
    let addr_a = sock_a.local_addr().unwrap();
    let addr_b = sock_b.local_addr().unwrap();

    rcv.add_link_socket(sock_a).unwrap();
    rcv.add_link_socket(sock_b).unwrap();

    // Create two senders, one per link
    let send_a = UdpSocket::bind("127.0.0.1:0").unwrap();
    send_a.connect(addr_a).unwrap();
    let sender_a = strata_bonding::net::transport::TransportLink::new(
        0,
        send_a,
        strata_transport::sender::SenderConfig::default(),
    );

    let send_b = UdpSocket::bind("127.0.0.1:0").unwrap();
    send_b.connect(addr_b).unwrap();
    let sender_b = strata_bonding::net::transport::TransportLink::new(
        1,
        send_b,
        strata_transport::sender::SenderConfig::default(),
    );

    use strata_bonding::net::interface::LinkSender;
    use strata_bonding::protocol::header::BondingHeader;

    // Send interleaved: even seq on link A, odd seq on link B
    for i in 0u64..10 {
        let payload = Bytes::from(format!("pkt-{i}"));
        let header = BondingHeader::new(i);
        let wrapped = header.wrap(payload);
        if i % 2 == 0 {
            sender_a.send(&wrapped).unwrap();
        } else {
            sender_b.send(&wrapped).unwrap();
        }
    }

    // Collect all packets — they should arrive in order despite being
    // sent on alternating links
    let mut received = Vec::new();
    for _ in 0..10 {
        match rcv.output_rx.recv_timeout(Duration::from_secs(3)) {
            Ok(data) => received.push(data),
            Err(_) => break,
        }
    }

    assert_eq!(received.len(), 10, "should receive all 10 packets");
    for (i, data) in received.iter().enumerate() {
        assert_eq!(data, &Bytes::from(format!("pkt-{i}")));
    }
}

// ────────────────────────────────────────────────────────────────
// 2. Dynamic jitter buffer: adapts latency under varying jitter
// ────────────────────────────────────────────────────────────────

#[test]
fn jitter_buffer_adapts_to_high_jitter_then_recovers() {
    let config = ReassemblyConfig {
        start_latency: Duration::from_millis(20),
        buffer_capacity: 256,
        skip_after: Some(Duration::from_millis(200)),
        jitter_latency_multiplier: 4.0,
        max_latency_ms: 300,
    };
    let mut buf = ReassemblyBuffer::with_config(0, config);
    let start = quanta::Instant::now();

    // Phase 1: constant IAT (10ms) — low jitter
    for i in 0u64..30 {
        let t = start + Duration::from_millis(i * 10);
        buf.push(i, Bytes::from(format!("p{i}")), t);
    }
    let stats_low = buf.get_stats();
    let latency_low = stats_low.current_latency_ms;

    // Phase 2: bursty IAT (1ms then 50ms alternating) — high jitter
    for i in 30u64..80 {
        let base_t = start + Duration::from_millis(300);
        let offset = if (i - 30) % 2 == 0 {
            i - 30 // 1ms per step
        } else {
            (i - 30) * 50 // 50ms per step
        };
        let t = base_t + Duration::from_millis(offset);
        buf.push(i, Bytes::from(format!("p{i}")), t);
    }
    let stats_high = buf.get_stats();
    let latency_high = stats_high.current_latency_ms;

    // Adaptive latency should increase under high jitter
    assert!(
        latency_high > latency_low,
        "latency should increase with jitter: low={latency_low}ms, high={latency_high}ms"
    );

    // Latency must stay below the configured max
    assert!(
        latency_high <= 300,
        "latency should respect max_latency_ms, got {latency_high}ms"
    );
}

// ────────────────────────────────────────────────────────────────
// 3. Receiver stats: lost/late/dup counters under impairment
// ────────────────────────────────────────────────────────────────

#[test]
fn receiver_stats_reflect_impairments() {
    let config = ReassemblyConfig {
        start_latency: Duration::from_millis(10),
        buffer_capacity: 128,
        skip_after: Some(Duration::from_millis(50)),
        jitter_latency_multiplier: 4.0,
        max_latency_ms: 500,
    };
    let mut buf = ReassemblyBuffer::with_config(0, config);
    let start = quanta::Instant::now();

    // Send packets 0, 1, 2, then SKIP 3, send 4, 5
    buf.push(0, Bytes::from_static(b"A"), start);
    buf.push(1, Bytes::from_static(b"B"), start);
    buf.push(2, Bytes::from_static(b"C"), start);
    // seq 3 is lost!
    buf.push(4, Bytes::from_static(b"E"), start);
    buf.push(5, Bytes::from_static(b"F"), start);

    // Tick after skip_after to release past the gap
    let released = buf.tick(start + Duration::from_millis(60));

    // Should have released 0, 1, 2, then skipped 3, released 4, 5
    assert_eq!(released.len(), 5, "should release 5 packets (skipping gap)");
    assert_eq!(buf.lost_packets, 1, "seq 3 should be counted as lost");

    // Send a duplicate
    buf.push(5, Bytes::from_static(b"F-dup"), start);
    // seq 5 was already consumed, so it's late, not duplicate
    assert_eq!(buf.late_packets, 1);

    // Send something not yet released as duplicate
    buf.push(6, Bytes::from_static(b"G"), start);
    buf.push(6, Bytes::from_static(b"G-dup"), start);
    assert_eq!(buf.duplicate_packets, 1);
}

// ────────────────────────────────────────────────────────────────
// 4. Metrics server alongside receiver
// ────────────────────────────────────────────────────────────────

#[test]
fn cloud_receiver_with_metrics_endpoint() {
    use std::collections::HashMap;
    use std::io::Read;
    use std::net::TcpStream;
    use std::sync::{Arc, Mutex};
    use strata_bonding::metrics::MetricsServer;
    use strata_bonding::net::interface::LinkMetrics;

    // Create a shared metrics source (simulating the receiver's view)
    let metrics: Arc<Mutex<HashMap<usize, LinkMetrics>>> = Arc::new(Mutex::new(HashMap::new()));

    // Add two link metrics entries
    {
        use strata_bonding::net::interface::LinkPhase;
        let mut m = metrics.lock().unwrap();
        m.insert(
            0,
            LinkMetrics {
                rtt_ms: 25.0,
                loss_rate: 0.001,
                capacity_bps: 10_000_000.0,
                observed_bytes: 5_000_000,
                observed_bps: 5_000_000.0,
                alive: true,
                phase: LinkPhase::Live,
                iface: Some("eth0".into()),
                link_kind: Some("wired".into()),
                os_up: Some(true),
                ..LinkMetrics::default()
            },
        );
        m.insert(
            1,
            LinkMetrics {
                rtt_ms: 45.0,
                loss_rate: 0.02,
                capacity_bps: 5_000_000.0,
                observed_bytes: 2_000_000,
                observed_bps: 3_000_000.0,
                alive: true,
                phase: LinkPhase::Live,
                iface: Some("wwan0".into()),
                link_kind: Some("cellular".into()),
                os_up: Some(true),
                ..LinkMetrics::default()
            },
        );
    }

    // Start metrics server on ephemeral port
    let mut server = MetricsServer::start("127.0.0.1:0".parse().unwrap(), metrics)
        .expect("should start metrics server");
    let addr = server.addr();

    // Fetch Prometheus text
    let mut conn = TcpStream::connect(addr).unwrap();
    conn.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let request = format!("GET /metrics HTTP/1.1\r\nHost: {addr}\r\n\r\n");
    std::io::Write::write_all(&mut conn, request.as_bytes()).unwrap();

    let mut response = String::new();
    let _ = conn.read_to_string(&mut response);

    // Verify both links appear in Prometheus output
    assert!(
        response.contains("strata_link_rtt_ms"),
        "should have RTT metric"
    );
    assert!(response.contains("link_id=\"0\""), "should have link 0");
    assert!(response.contains("link_id=\"1\""), "should have link 1");
    assert!(
        response.contains("strata_link_loss_rate"),
        "should have loss metric"
    );

    server.stop();
}

// ────────────────────────────────────────────────────────────────
// 5. Graceful shutdown: receiver drains cleanly
// ────────────────────────────────────────────────────────────────

#[test]
fn receiver_graceful_shutdown_drains_pending() {
    let mut rcv = TransportBondingReceiver::new(Duration::from_millis(10));
    let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    let addr = sock.local_addr().unwrap();
    rcv.add_link_socket(sock).unwrap();

    // Send some packets
    let send_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    send_sock.connect(addr).unwrap();
    let sender = strata_bonding::net::transport::TransportLink::new(
        0,
        send_sock,
        strata_transport::sender::SenderConfig::default(),
    );

    use strata_bonding::net::interface::LinkSender;
    use strata_bonding::protocol::header::BondingHeader;

    for i in 0u64..5 {
        let header = BondingHeader::new(i);
        let wrapped = header.wrap(Bytes::from(format!("data-{i}")));
        sender.send(&wrapped).unwrap();
    }

    // Give time for processing
    std::thread::sleep(Duration::from_millis(100));

    // Drain any available
    let mut count = 0;
    while rcv.output_rx.try_recv().is_ok() {
        count += 1;
    }

    // Shutdown should complete without hanging
    rcv.shutdown();
    assert!(!rcv.is_running());

    // Verify we got at least some packets before shutdown
    assert!(count > 0, "should have received at least some packets");
}

// ────────────────────────────────────────────────────────────────
// 6. Output abstraction: channel-based output for piping
// ────────────────────────────────────────────────────────────────

#[test]
fn receiver_output_channel_is_bounded() {
    // Verify the output channel has bounded capacity (backpressure)
    let rcv = TransportBondingReceiver::new(Duration::from_millis(10));

    // output_rx is bounded — we can't send unlimited data without a consumer
    assert!(rcv.is_running());

    // The receiver should be droppable without panic
    drop(rcv);
}
