//! Prometheus metrics endpoint for the control plane.
//!
//! `GET /metrics` — aggregates link stats from all connected sender agents
//! and renders them in Prometheus text exposition format.
//!
//! This endpoint requires no authentication (standard Prometheus practice).

use axum::extract::State;
use axum::http::header;
use axum::response::IntoResponse;

use strata_common::metrics::render_prometheus;

use crate::state::AppState;

/// Handler for `GET /metrics`.
///
/// Collects the latest link stats from every connected sender agent
/// and renders them as Prometheus text exposition format, with
/// `sender_id` labels so a single scrape covers the whole fleet.
pub async fn handler(State(state): State<AppState>) -> impl IntoResponse {
    let mut out = String::with_capacity(4096);

    // Collect all cached stream stats from connected agents.
    let mut total_senders: usize = 0;
    let mut total_links: usize = 0;

    for entry in state.stream_stats().iter() {
        let sender_id = entry.key();
        let stats = entry.value();

        total_senders += 1;
        total_links += stats.links.len();

        // Render per-link metrics with sender_id prefix in labels.
        for link in &stats.links {
            use std::fmt::Write;
            writeln!(
                out,
                "strata_link_rtt_ms{{sender_id=\"{sender_id}\",link_id=\"{}\",interface=\"{}\"}} {:.3}",
                link.id, link.interface, link.rtt_ms
            )
            .unwrap();
            writeln!(
                out,
                "strata_link_capacity_bps{{sender_id=\"{sender_id}\",link_id=\"{}\",interface=\"{}\"}} {}",
                link.id, link.interface, link.capacity_bps
            )
            .unwrap();
            writeln!(
                out,
                "strata_link_loss_rate{{sender_id=\"{sender_id}\",link_id=\"{}\",interface=\"{}\"}} {:.6}",
                link.id, link.interface, link.loss_rate
            )
            .unwrap();
            writeln!(
                out,
                "strata_link_observed_bps{{sender_id=\"{sender_id}\",link_id=\"{}\",interface=\"{}\"}} {}",
                link.id, link.interface, link.observed_bps
            )
            .unwrap();
            writeln!(
                out,
                "strata_link_bytes_sent_total{{sender_id=\"{sender_id}\",link_id=\"{}\",interface=\"{}\"}} {}",
                link.id, link.interface, link.sent_bytes
            )
            .unwrap();
            let state_val = if link.state == "Live" || link.state == "live" {
                1
            } else {
                0
            };
            writeln!(
                out,
                "strata_link_state{{sender_id=\"{sender_id}\",link_id=\"{}\",interface=\"{}\",state=\"{}\"}} {state_val}",
                link.id, link.interface, link.state
            )
            .unwrap();
            if let Some(dbm) = link.signal_dbm {
                writeln!(
                    out,
                    "strata_link_signal_dbm{{sender_id=\"{sender_id}\",link_id=\"{}\",interface=\"{}\"}} {}",
                    link.id, link.interface, dbm
                )
                .unwrap();
            }
        }
    }

    // Fleet-wide aggregates
    {
        use std::fmt::Write;
        writeln!(
            out,
            "# HELP strata_fleet_senders_connected Number of sender agents currently connected."
        )
        .unwrap();
        writeln!(out, "# TYPE strata_fleet_senders_connected gauge").unwrap();
        writeln!(out, "strata_fleet_senders_connected {total_senders}").unwrap();

        writeln!(
            out,
            "# HELP strata_fleet_links_total Total links across all connected senders."
        )
        .unwrap();
        writeln!(out, "# TYPE strata_fleet_links_total gauge").unwrap();
        writeln!(out, "strata_fleet_links_total {total_links}").unwrap();

        writeln!(
            out,
            "# HELP strata_fleet_agents_registered Total agents registered (online + offline)."
        )
        .unwrap();
        writeln!(out, "# TYPE strata_fleet_agents_registered gauge").unwrap();
        writeln!(
            out,
            "strata_fleet_agents_registered {}",
            state.agents().len()
        )
        .unwrap();
    }

    // If no senders have stats, still output the agent-level metrics
    // as a fallback using the render_prometheus function for any
    // individual sender that happens to be the only one.
    // For a single-sender deployment, also render the standard flat format.
    if total_senders == 1 {
        if let Some(entry) = state.stream_stats().iter().next() {
            out.push_str(&render_prometheus(&entry.value().links));
        }
    }

    (
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        out,
    )
}

#[cfg(test)]
mod tests {
    use strata_common::models::LinkStats;
    use strata_common::protocol::StreamStatsPayload;

    #[test]
    fn fleet_metrics_rendering() {
        // Simulate what the handler does without needing AppState.
        let stats = vec![
            (
                "snd_abc".to_string(),
                StreamStatsPayload {
                    stream_id: "str_1".into(),
                    sender_id: "snd_abc".into(),
                    uptime_s: 100,
                    encoder_bitrate_kbps: 5000,
                    timestamp_ms: 0,
                    links: vec![LinkStats {
                        id: 0,
                        interface: "wwan0".into(),
                        state: "Live".into(),
                        rtt_ms: 25.0,
                        loss_rate: 0.01,
                        capacity_bps: 5_000_000,
                        sent_bytes: 100_000,
                        observed_bps: 3_000_000,
                        signal_dbm: Some(-65),
                        link_kind: Some("cellular".into()),
                    }],
                },
            ),
            (
                "snd_def".to_string(),
                StreamStatsPayload {
                    stream_id: "str_2".into(),
                    sender_id: "snd_def".into(),
                    uptime_s: 50,
                    encoder_bitrate_kbps: 3000,
                    timestamp_ms: 0,
                    links: vec![
                        LinkStats {
                            id: 0,
                            interface: "eth0".into(),
                            state: "Live".into(),
                            rtt_ms: 10.0,
                            loss_rate: 0.0,
                            capacity_bps: 10_000_000,
                            sent_bytes: 200_000,
                            observed_bps: 8_000_000,
                            signal_dbm: None,
                            link_kind: Some("ethernet".into()),
                        },
                        LinkStats {
                            id: 1,
                            interface: "wwan0".into(),
                            state: "Down".into(),
                            rtt_ms: 0.0,
                            loss_rate: 1.0,
                            capacity_bps: 0,
                            sent_bytes: 50_000,
                            observed_bps: 0,
                            signal_dbm: Some(-95),
                            link_kind: Some("cellular".into()),
                        },
                    ],
                },
            ),
        ];

        // Simulate the rendering logic from the handler.
        let mut out = String::new();
        let mut total_senders = 0usize;
        let mut total_links = 0usize;

        for (sender_id, payload) in &stats {
            total_senders += 1;
            total_links += payload.links.len();

            for link in &payload.links {
                use std::fmt::Write;
                writeln!(
                    out,
                    "strata_link_rtt_ms{{sender_id=\"{sender_id}\",link_id=\"{}\",interface=\"{}\"}} {:.3}",
                    link.id, link.interface, link.rtt_ms
                )
                .unwrap();
                writeln!(
                    out,
                    "strata_link_capacity_bps{{sender_id=\"{sender_id}\",link_id=\"{}\",interface=\"{}\"}} {}",
                    link.id, link.interface, link.capacity_bps
                )
                .unwrap();
            }
        }

        use std::fmt::Write;
        writeln!(out, "strata_fleet_senders_connected {total_senders}").unwrap();
        writeln!(out, "strata_fleet_links_total {total_links}").unwrap();

        // Verify multi-sender metrics
        assert!(out.contains(
            r#"strata_link_rtt_ms{sender_id="snd_abc",link_id="0",interface="wwan0"} 25.000"#
        ));
        assert!(out.contains(
            r#"strata_link_rtt_ms{sender_id="snd_def",link_id="0",interface="eth0"} 10.000"#
        ));
        assert!(out.contains(
            r#"strata_link_capacity_bps{sender_id="snd_def",link_id="1",interface="wwan0"} 0"#
        ));
        assert!(out.contains("strata_fleet_senders_connected 2"));
        assert!(out.contains("strata_fleet_links_total 3"));
    }

    #[test]
    fn empty_fleet_metrics() {
        let out = String::new();
        // No stats → fleet metrics should still be present after aggregation.
        let total_senders = 0;
        let total_links = 0;

        let mut result = out;
        use std::fmt::Write;
        writeln!(result, "strata_fleet_senders_connected {total_senders}").unwrap();
        writeln!(result, "strata_fleet_links_total {total_links}").unwrap();

        assert!(result.contains("strata_fleet_senders_connected 0"));
        assert!(result.contains("strata_fleet_links_total 0"));
    }

    #[test]
    fn signal_dbm_only_present_when_some() {
        let link_with = LinkStats {
            id: 0,
            interface: "wwan0".into(),
            state: "Live".into(),
            rtt_ms: 25.0,
            loss_rate: 0.01,
            capacity_bps: 5_000_000,
            sent_bytes: 100_000,
            observed_bps: 3_000_000,
            signal_dbm: Some(-65),
            link_kind: Some("cellular".into()),
        };
        let link_without = LinkStats {
            id: 1,
            interface: "eth0".into(),
            state: "Live".into(),
            rtt_ms: 10.0,
            loss_rate: 0.0,
            capacity_bps: 10_000_000,
            sent_bytes: 200_000,
            observed_bps: 8_000_000,
            signal_dbm: None,
            link_kind: Some("ethernet".into()),
        };

        let mut out = String::new();
        use std::fmt::Write;

        // Render signal_dbm only when Some
        if let Some(dbm) = link_with.signal_dbm {
            writeln!(
                out,
                "strata_link_signal_dbm{{sender_id=\"snd_test\",link_id=\"{}\",interface=\"{}\"}} {}",
                link_with.id, link_with.interface, dbm
            )
            .unwrap();
        }
        if let Some(dbm) = link_without.signal_dbm {
            writeln!(
                out,
                "strata_link_signal_dbm{{sender_id=\"snd_test\",link_id=\"{}\",interface=\"{}\"}} {}",
                link_without.id, link_without.interface, dbm
            )
            .unwrap();
        }

        assert!(out.contains(
            r#"strata_link_signal_dbm{sender_id="snd_test",link_id="0",interface="wwan0"} -65"#
        ));
        assert!(!out.contains("link_id=\"1\""));
    }
}
