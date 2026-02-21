//! Prometheus metrics rendering for link stats.
//!
//! Renders `LinkStats` in Prometheus text exposition format, suitable
//! for scraping by Prometheus or compatible collectors.

use crate::models::{LinkStats, TransportReceiverMetrics, TransportSenderMetrics};
use std::fmt::Write;

/// Render a slice of `LinkStats` as Prometheus text exposition format.
pub fn render_prometheus(links: &[LinkStats]) -> String {
    let mut out = String::with_capacity(2048);

    // ── Per-link gauges ─────────────────────────────────────────

    writeln!(
        out,
        "# HELP strata_link_rtt_ms Smoothed RTT in milliseconds."
    )
    .unwrap();
    writeln!(out, "# TYPE strata_link_rtt_ms gauge").unwrap();
    for l in links {
        writeln!(
            out,
            "strata_link_rtt_ms{{link_id=\"{}\",interface=\"{}\"}} {:.3}",
            l.id, l.interface, l.rtt_ms
        )
        .unwrap();
    }

    writeln!(
        out,
        "# HELP strata_link_capacity_bps Estimated link capacity in bits per second."
    )
    .unwrap();
    writeln!(out, "# TYPE strata_link_capacity_bps gauge").unwrap();
    for l in links {
        writeln!(
            out,
            "strata_link_capacity_bps{{link_id=\"{}\",interface=\"{}\"}} {}",
            l.id, l.interface, l.capacity_bps
        )
        .unwrap();
    }

    writeln!(
        out,
        "# HELP strata_link_loss_rate Observed packet loss rate (0.0-1.0)."
    )
    .unwrap();
    writeln!(out, "# TYPE strata_link_loss_rate gauge").unwrap();
    for l in links {
        writeln!(
            out,
            "strata_link_loss_rate{{link_id=\"{}\",interface=\"{}\"}} {:.6}",
            l.id, l.interface, l.loss_rate
        )
        .unwrap();
    }

    writeln!(
        out,
        "# HELP strata_link_observed_bps Actual throughput in bits per second."
    )
    .unwrap();
    writeln!(out, "# TYPE strata_link_observed_bps gauge").unwrap();
    for l in links {
        writeln!(
            out,
            "strata_link_observed_bps{{link_id=\"{}\",interface=\"{}\"}} {}",
            l.id, l.interface, l.observed_bps
        )
        .unwrap();
    }

    writeln!(
        out,
        "# HELP strata_link_bytes_sent_total Total bytes sent on this link."
    )
    .unwrap();
    writeln!(out, "# TYPE strata_link_bytes_sent_total counter").unwrap();
    for l in links {
        writeln!(
            out,
            "strata_link_bytes_sent_total{{link_id=\"{}\",interface=\"{}\"}} {}",
            l.id, l.interface, l.sent_bytes
        )
        .unwrap();
    }

    writeln!(out, "# HELP strata_link_state Link state (1=live, 0=down).").unwrap();
    writeln!(out, "# TYPE strata_link_state gauge").unwrap();
    for l in links {
        let v = if l.state == "Live" || l.state == "live" {
            1
        } else {
            0
        };
        writeln!(
            out,
            "strata_link_state{{link_id=\"{}\",interface=\"{}\",state=\"{}\"}} {v}",
            l.id, l.interface, l.state
        )
        .unwrap();
    }

    // ── Aggregate metrics ───────────────────────────────────────

    let alive_count = links
        .iter()
        .filter(|l| l.state == "Live" || l.state == "live")
        .count();
    let total_capacity: u64 = links
        .iter()
        .filter(|l| l.state == "Live" || l.state == "live")
        .map(|l| l.capacity_bps)
        .sum();
    let total_observed: u64 = links
        .iter()
        .filter(|l| l.state == "Live" || l.state == "live")
        .map(|l| l.observed_bps)
        .sum();

    writeln!(
        out,
        "# HELP strata_links_total Total number of configured links."
    )
    .unwrap();
    writeln!(out, "# TYPE strata_links_total gauge").unwrap();
    writeln!(out, "strata_links_total {}", links.len()).unwrap();

    writeln!(
        out,
        "# HELP strata_links_alive Number of links currently alive."
    )
    .unwrap();
    writeln!(out, "# TYPE strata_links_alive gauge").unwrap();
    writeln!(out, "strata_links_alive {alive_count}").unwrap();

    writeln!(
        out,
        "# HELP strata_total_capacity_bps Aggregate capacity of alive links."
    )
    .unwrap();
    writeln!(out, "# TYPE strata_total_capacity_bps gauge").unwrap();
    writeln!(out, "strata_total_capacity_bps {total_capacity}").unwrap();

    writeln!(
        out,
        "# HELP strata_total_observed_bps Aggregate observed throughput."
    )
    .unwrap();
    writeln!(out, "# TYPE strata_total_observed_bps gauge").unwrap();
    writeln!(out, "strata_total_observed_bps {total_observed}").unwrap();

    out
}

/// Render sender-side transport stats in Prometheus text exposition format.
pub fn render_sender_prometheus(stats: &TransportSenderMetrics) -> String {
    let mut out = String::with_capacity(1024);

    writeln!(
        out,
        "# HELP strata_tx_packets_sent_total Total packets sent (including retransmissions)."
    )
    .unwrap();
    writeln!(out, "# TYPE strata_tx_packets_sent_total counter").unwrap();
    writeln!(out, "strata_tx_packets_sent_total {}", stats.packets_sent).unwrap();

    writeln!(
        out,
        "# HELP strata_tx_bytes_sent_total Total original payload bytes sent."
    )
    .unwrap();
    writeln!(out, "# TYPE strata_tx_bytes_sent_total counter").unwrap();
    writeln!(out, "strata_tx_bytes_sent_total {}", stats.bytes_sent).unwrap();

    writeln!(
        out,
        "# HELP strata_tx_packets_acked_total Packets acknowledged by receiver."
    )
    .unwrap();
    writeln!(out, "# TYPE strata_tx_packets_acked_total counter").unwrap();
    writeln!(out, "strata_tx_packets_acked_total {}", stats.packets_acked).unwrap();

    writeln!(
        out,
        "# HELP strata_tx_retransmissions_total NACK-triggered retransmissions."
    )
    .unwrap();
    writeln!(out, "# TYPE strata_tx_retransmissions_total counter").unwrap();
    writeln!(
        out,
        "strata_tx_retransmissions_total {}",
        stats.retransmissions
    )
    .unwrap();

    writeln!(
        out,
        "# HELP strata_tx_packets_expired_total Packets expired from send buffer without ACK."
    )
    .unwrap();
    writeln!(out, "# TYPE strata_tx_packets_expired_total counter").unwrap();
    writeln!(
        out,
        "strata_tx_packets_expired_total {}",
        stats.packets_expired
    )
    .unwrap();

    writeln!(
        out,
        "# HELP strata_tx_fec_repairs_sent_total FEC repair packets sent."
    )
    .unwrap();
    writeln!(out, "# TYPE strata_tx_fec_repairs_sent_total counter").unwrap();
    writeln!(
        out,
        "strata_tx_fec_repairs_sent_total {}",
        stats.fec_repairs_sent
    )
    .unwrap();

    writeln!(
        out,
        "# HELP strata_tx_rtt_us Last measured round-trip time in microseconds."
    )
    .unwrap();
    writeln!(out, "# TYPE strata_tx_rtt_us gauge").unwrap();
    writeln!(out, "strata_tx_rtt_us {}", stats.last_rtt_us).unwrap();

    // Derived metrics
    let loss_rate = if stats.packets_sent > 0 {
        let unacked = stats.packets_sent.saturating_sub(stats.packets_acked);
        unacked as f64 / stats.packets_sent as f64
    } else {
        0.0
    };
    writeln!(
        out,
        "# HELP strata_tx_loss_rate Estimated loss rate (unacked/sent)."
    )
    .unwrap();
    writeln!(out, "# TYPE strata_tx_loss_rate gauge").unwrap();
    writeln!(out, "strata_tx_loss_rate {loss_rate:.6}").unwrap();

    out
}

/// Render receiver-side transport stats in Prometheus text exposition format.
pub fn render_receiver_prometheus(stats: &TransportReceiverMetrics) -> String {
    let mut out = String::with_capacity(1024);

    writeln!(
        out,
        "# HELP strata_rx_packets_received_total Total packets received (including duplicates)."
    )
    .unwrap();
    writeln!(out, "# TYPE strata_rx_packets_received_total counter").unwrap();
    writeln!(
        out,
        "strata_rx_packets_received_total {}",
        stats.packets_received
    )
    .unwrap();

    writeln!(
        out,
        "# HELP strata_rx_bytes_received_total Total payload bytes received."
    )
    .unwrap();
    writeln!(out, "# TYPE strata_rx_bytes_received_total counter").unwrap();
    writeln!(
        out,
        "strata_rx_bytes_received_total {}",
        stats.bytes_received
    )
    .unwrap();

    writeln!(
        out,
        "# HELP strata_rx_packets_delivered_total Packets delivered to the application."
    )
    .unwrap();
    writeln!(out, "# TYPE strata_rx_packets_delivered_total counter").unwrap();
    writeln!(
        out,
        "strata_rx_packets_delivered_total {}",
        stats.packets_delivered
    )
    .unwrap();

    writeln!(
        out,
        "# HELP strata_rx_duplicates_total Duplicate packets received."
    )
    .unwrap();
    writeln!(out, "# TYPE strata_rx_duplicates_total counter").unwrap();
    writeln!(out, "strata_rx_duplicates_total {}", stats.duplicates).unwrap();

    writeln!(
        out,
        "# HELP strata_rx_late_packets_total Packets received after playout deadline."
    )
    .unwrap();
    writeln!(out, "# TYPE strata_rx_late_packets_total counter").unwrap();
    writeln!(out, "strata_rx_late_packets_total {}", stats.late_packets).unwrap();

    writeln!(
        out,
        "# HELP strata_rx_fec_recoveries_total Packets recovered via FEC decoding."
    )
    .unwrap();
    writeln!(out, "# TYPE strata_rx_fec_recoveries_total counter").unwrap();
    writeln!(
        out,
        "strata_rx_fec_recoveries_total {}",
        stats.fec_recoveries
    )
    .unwrap();

    writeln!(
        out,
        "# HELP strata_rx_nacks_sent_total NACKs sent to request retransmission."
    )
    .unwrap();
    writeln!(out, "# TYPE strata_rx_nacks_sent_total counter").unwrap();
    writeln!(out, "strata_rx_nacks_sent_total {}", stats.nacks_sent).unwrap();

    writeln!(
        out,
        "# HELP strata_rx_jitter_buffer_depth Current jitter buffer depth in packets."
    )
    .unwrap();
    writeln!(out, "# TYPE strata_rx_jitter_buffer_depth gauge").unwrap();
    writeln!(
        out,
        "strata_rx_jitter_buffer_depth {}",
        stats.jitter_buffer_depth
    )
    .unwrap();

    // Derived: goodput ratio
    let goodput = if stats.packets_received > 0 {
        stats.packets_delivered as f64 / stats.packets_received as f64
    } else {
        0.0
    };
    writeln!(
        out,
        "# HELP strata_rx_goodput_ratio Effective goodput (delivered/received)."
    )
    .unwrap();
    writeln!(out, "# TYPE strata_rx_goodput_ratio gauge").unwrap();
    writeln!(out, "strata_rx_goodput_ratio {goodput:.6}").unwrap();

    out
}

/// Render all available metrics in a single Prometheus scrape response.
///
/// Combines link stats, optional sender transport stats, and optional
/// receiver transport stats into one text block.
pub fn render_all_prometheus(
    links: &[LinkStats],
    sender: Option<&TransportSenderMetrics>,
    receiver: Option<&TransportReceiverMetrics>,
) -> String {
    let mut out = render_prometheus(links);
    if let Some(s) = sender {
        out.push_str(&render_sender_prometheus(s));
    }
    if let Some(r) = receiver {
        out.push_str(&render_receiver_prometheus(r));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_stats() -> Vec<LinkStats> {
        vec![
            LinkStats {
                id: 0,
                interface: "wwan0".into(),
                state: "Live".into(),
                rtt_ms: 25.5,
                loss_rate: 0.02,
                capacity_bps: 5_000_000,
                sent_bytes: 100_000,
                observed_bps: 3_000_000,
                signal_dbm: Some(-65),
                link_kind: Some("cellular".into()),
                rsrp: None,
                rsrq: None,
                sinr: None,
                cqi: None,
                btlbw_bps: Some(4_500_000),
                rtprop_ms: Some(20.0),
            },
            LinkStats {
                id: 1,
                interface: "wwan1".into(),
                state: "Live".into(),
                rtt_ms: 50.0,
                loss_rate: 0.05,
                capacity_bps: 2_000_000,
                sent_bytes: 50_000,
                observed_bps: 1_500_000,
                signal_dbm: Some(-72),
                link_kind: Some("cellular".into()),
                rsrp: None,
                rsrq: None,
                sinr: None,
                cqi: None,
                btlbw_bps: Some(1_800_000),
                rtprop_ms: Some(45.0),
            },
        ]
    }

    #[test]
    fn render_contains_help_and_type_lines() {
        let out = render_prometheus(&sample_stats());
        assert!(out.contains("# HELP strata_link_rtt_ms"));
        assert!(out.contains("# TYPE strata_link_rtt_ms gauge"));
        assert!(out.contains("# HELP strata_link_capacity_bps"));
        assert!(out.contains("# HELP strata_link_loss_rate"));
        assert!(out.contains("# HELP strata_link_state"));
        assert!(out.contains("# HELP strata_links_alive"));
        assert!(out.contains("# HELP strata_total_capacity_bps"));
    }

    #[test]
    fn render_per_link_values() {
        let out = render_prometheus(&sample_stats());
        assert!(out.contains(r#"strata_link_rtt_ms{link_id="0",interface="wwan0"} 25.500"#));
        assert!(out.contains(r#"strata_link_capacity_bps{link_id="0",interface="wwan0"} 5000000"#));
        assert!(out.contains(r#"strata_link_loss_rate{link_id="0",interface="wwan0"} 0.020000"#));
        assert!(out.contains(r#"strata_link_rtt_ms{link_id="1",interface="wwan1"} 50.000"#));
    }

    #[test]
    fn render_aggregate_values() {
        let out = render_prometheus(&sample_stats());
        assert!(out.contains("strata_links_total 2"));
        assert!(out.contains("strata_links_alive 2"));
        assert!(out.contains("strata_total_capacity_bps 7000000"));
        assert!(out.contains("strata_total_observed_bps 4500000"));
    }

    #[test]
    fn render_dead_link_excluded_from_alive() {
        let mut stats = sample_stats();
        stats[1].state = "Down".into();
        let out = render_prometheus(&stats);
        assert!(out.contains("strata_links_alive 1"));
        // Total capacity should only include alive link
        assert!(out.contains("strata_total_capacity_bps 5000000"));
    }

    #[test]
    fn render_empty_links() {
        let out = render_prometheus(&[]);
        assert!(out.contains("strata_links_total 0"));
        assert!(out.contains("strata_links_alive 0"));
        assert!(out.contains("strata_total_capacity_bps 0"));
    }

    #[test]
    fn render_state_label() {
        let out = render_prometheus(&sample_stats());
        assert!(out.contains(r#"strata_link_state{link_id="0",interface="wwan0",state="Live"} 1"#));
    }

    // ── Transport Sender Metrics Tests ──────────────────────────

    fn sample_sender_metrics() -> TransportSenderMetrics {
        TransportSenderMetrics {
            packets_sent: 10_000,
            bytes_sent: 14_000_000,
            packets_acked: 9_500,
            retransmissions: 200,
            packets_expired: 50,
            fec_repairs_sent: 300,
            last_rtt_us: 25_000,
        }
    }

    #[test]
    fn render_sender_contains_all_counters() {
        let out = render_sender_prometheus(&sample_sender_metrics());
        assert!(out.contains("# HELP strata_tx_packets_sent_total"));
        assert!(out.contains("# TYPE strata_tx_packets_sent_total counter"));
        assert!(out.contains("strata_tx_packets_sent_total 10000"));
        assert!(out.contains("strata_tx_bytes_sent_total 14000000"));
        assert!(out.contains("strata_tx_packets_acked_total 9500"));
        assert!(out.contains("strata_tx_retransmissions_total 200"));
        assert!(out.contains("strata_tx_packets_expired_total 50"));
        assert!(out.contains("strata_tx_fec_repairs_sent_total 300"));
    }

    #[test]
    fn render_sender_rtt_gauge() {
        let out = render_sender_prometheus(&sample_sender_metrics());
        assert!(out.contains("# TYPE strata_tx_rtt_us gauge"));
        assert!(out.contains("strata_tx_rtt_us 25000"));
    }

    #[test]
    fn render_sender_loss_rate() {
        let out = render_sender_prometheus(&sample_sender_metrics());
        // 500 unacked out of 10000 = 0.05
        assert!(out.contains("strata_tx_loss_rate 0.050000"));
    }

    #[test]
    fn render_sender_zero_packets_no_nan() {
        let stats = TransportSenderMetrics::default();
        let out = render_sender_prometheus(&stats);
        assert!(out.contains("strata_tx_loss_rate 0.000000"));
        assert!(!out.contains("NaN"));
    }

    // ── Transport Receiver Metrics Tests ────────────────────────

    fn sample_receiver_metrics() -> TransportReceiverMetrics {
        TransportReceiverMetrics {
            packets_received: 11_000,
            bytes_received: 15_000_000,
            packets_delivered: 10_000,
            duplicates: 500,
            late_packets: 200,
            fec_recoveries: 150,
            nacks_sent: 80,
            highest_delivered_seq: 9_999,
            jitter_buffer_depth: 12,
        }
    }

    #[test]
    fn render_receiver_contains_all_counters() {
        let out = render_receiver_prometheus(&sample_receiver_metrics());
        assert!(out.contains("# HELP strata_rx_packets_received_total"));
        assert!(out.contains("strata_rx_packets_received_total 11000"));
        assert!(out.contains("strata_rx_bytes_received_total 15000000"));
        assert!(out.contains("strata_rx_packets_delivered_total 10000"));
        assert!(out.contains("strata_rx_duplicates_total 500"));
        assert!(out.contains("strata_rx_late_packets_total 200"));
        assert!(out.contains("strata_rx_fec_recoveries_total 150"));
        assert!(out.contains("strata_rx_nacks_sent_total 80"));
    }

    #[test]
    fn render_receiver_jitter_buffer() {
        let out = render_receiver_prometheus(&sample_receiver_metrics());
        assert!(out.contains("# TYPE strata_rx_jitter_buffer_depth gauge"));
        assert!(out.contains("strata_rx_jitter_buffer_depth 12"));
    }

    #[test]
    fn render_receiver_goodput_ratio() {
        let out = render_receiver_prometheus(&sample_receiver_metrics());
        // 10000/11000 ≈ 0.909091
        assert!(out.contains("strata_rx_goodput_ratio 0.90909"));
    }

    #[test]
    fn render_receiver_zero_packets_no_nan() {
        let stats = TransportReceiverMetrics::default();
        let out = render_receiver_prometheus(&stats);
        assert!(out.contains("strata_rx_goodput_ratio 0.000000"));
        assert!(!out.contains("NaN"));
    }

    // ── Combined render_all Tests ──────────────────────────────

    #[test]
    fn render_all_links_only() {
        let out = render_all_prometheus(&sample_stats(), None, None);
        assert!(out.contains("strata_link_rtt_ms"));
        assert!(out.contains("strata_links_total 2"));
        assert!(!out.contains("strata_tx_"));
        assert!(!out.contains("strata_rx_"));
    }

    #[test]
    fn render_all_with_sender() {
        let out = render_all_prometheus(&sample_stats(), Some(&sample_sender_metrics()), None);
        assert!(out.contains("strata_link_rtt_ms"));
        assert!(out.contains("strata_tx_packets_sent_total 10000"));
        assert!(!out.contains("strata_rx_"));
    }

    #[test]
    fn render_all_with_everything() {
        let out = render_all_prometheus(
            &sample_stats(),
            Some(&sample_sender_metrics()),
            Some(&sample_receiver_metrics()),
        );
        assert!(out.contains("strata_links_total 2"));
        assert!(out.contains("strata_tx_fec_repairs_sent_total 300"));
        assert!(out.contains("strata_rx_fec_recoveries_total 150"));
        assert!(out.contains("strata_rx_jitter_buffer_depth 12"));
    }
}
