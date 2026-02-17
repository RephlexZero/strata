//! Prometheus metrics rendering for link stats.
//!
//! Renders `LinkStats` in Prometheus text exposition format, suitable
//! for scraping by Prometheus or compatible collectors.

use crate::models::LinkStats;
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
}
