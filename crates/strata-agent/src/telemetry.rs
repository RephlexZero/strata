//! Telemetry — collects pipeline stats and sends them to the control plane.
//!
//! In simulation mode, generates synthetic link stats that look realistic.
//! In production mode, reads stats from integration_node's UDP relay on
//! 127.0.0.1:9100 (bonding stats JSON forwarded from the GStreamer bus).

use std::sync::Arc;
use std::time::Duration;

use strata_common::models::LinkStats;
use strata_common::protocol::{Envelope, StreamStatsPayload};

use crate::pipeline;
use crate::AgentState;

/// Run the telemetry loop — sends stream.stats every second while streaming.
pub async fn run(state: Arc<AgentState>) {
    let mut interval = tokio::time::interval(Duration::from_secs(1));

    // Set up a non-blocking UDP socket to receive stats from integration_node.
    // The socket is bound once and reused across the lifetime of the agent.
    let stats_rx = std::net::UdpSocket::bind(pipeline::STATS_LISTEN_ADDR).ok();
    if let Some(ref sock) = stats_rx {
        sock.set_nonblocking(true).ok();
        tracing::info!(
            addr = pipeline::STATS_LISTEN_ADDR,
            "stats UDP listener bound"
        );
    }

    // Buffer for incoming stats JSON from integration_node
    let mut last_real_stats: Option<Vec<LinkStats>> = None;
    let mut recv_buf = [0u8; 8192];

    loop {
        interval.tick().await;

        // Check shutdown
        if *state.shutdown.borrow() {
            return;
        }

        // Only send stats if we have a pipeline running
        let pipeline = state.pipeline.lock().await;
        if !pipeline.is_running() {
            last_real_stats = None;
            tracing::trace!("telemetry: pipeline not running");
            continue;
        }

        let stream_id = match pipeline.stream_id() {
            Some(id) => id.to_string(),
            None => {
                tracing::trace!("telemetry: no stream_id");
                continue;
            }
        };

        let elapsed_s = pipeline.elapsed_s();
        drop(pipeline); // Release lock before doing I/O

        // Drain any pending stats from integration_node's UDP relay.
        // We take the most recent one (in case multiple arrived in 1s).
        let mut udp_count = 0u32;
        if let Some(ref sock) = stats_rx {
            while let Ok((n, _)) = sock.recv_from(&mut recv_buf) {
                udp_count += 1;
                if let Ok(parsed) = parse_bonding_stats(&recv_buf[..n]) {
                    last_real_stats = Some(parsed);
                }
            }
        }

        // Build stats
        let links = if state.simulate {
            generate_simulated_stats()
        } else {
            last_real_stats.clone().unwrap_or_default()
        };

        let encoder_kbps = links.iter().map(|l| l.capacity_bps).sum::<u64>() / 1000;

        tracing::debug!(
            udp_count,
            link_count = links.len(),
            encoder_kbps,
            "telemetry tick"
        );

        let stats = StreamStatsPayload {
            stream_id,
            uptime_s: elapsed_s,
            encoder_bitrate_kbps: encoder_kbps.max(1) as u32,
            links,
        };

        let envelope = Envelope::new("stream.stats", &stats);
        if let Ok(json) = serde_json::to_string(&envelope) {
            if let Err(e) = state.control_tx.send(json).await {
                tracing::warn!(error = %e, "failed to send stats to control channel");
            }
        }
    }
}

/// Parse the bonding stats JSON relayed by integration_node.
///
/// The JSON comes from the `rist-bonding-stats` GStreamer bus message
/// and has the shape: `{"links": [{"id": 0, "rtt_us": ..., ...}, ...]}`.
fn parse_bonding_stats(data: &[u8]) -> Result<Vec<LinkStats>, ()> {
    let v: serde_json::Value = serde_json::from_slice(data).map_err(|_| ())?;
    let links_arr = v.get("links").and_then(|v| v.as_array()).ok_or(())?;

    let mut stats = Vec::with_capacity(links_arr.len());
    for link in links_arr {
        let id = link.get("id").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        let rtt_us = link.get("rtt_us").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let loss = link
            .get("loss_rate")
            .or_else(|| link.get("loss_percent"))
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        let capacity = link
            .get("capacity_bps")
            .or_else(|| link.get("bandwidth_bps"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let sent = link
            .get("sent_bytes")
            .or_else(|| link.get("tx_bytes"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let iface = link
            .get("interface")
            .or_else(|| link.get("iface"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");

        stats.push(LinkStats {
            id,
            interface: iface.to_string(),
            state: "Live".into(),
            rtt_ms: rtt_us / 1000.0,
            loss_rate: loss,
            capacity_bps: capacity,
            sent_bytes: sent,
            signal_dbm: None,
        });
    }
    Ok(stats)
}

/// Generate realistic simulated link stats.
fn generate_simulated_stats() -> Vec<LinkStats> {
    use rand::Rng;
    let mut rng = rand::rng();

    vec![
        LinkStats {
            id: 0,
            interface: "wwan0".into(),
            state: "Live".into(),
            rtt_ms: 35.0 + rng.random_range(0.0..20.0_f64),
            loss_rate: rng.random_range(0.0..0.005_f64),
            capacity_bps: 8_000_000 + rng.random_range(0..2_000_000),
            sent_bytes: rng.random_range(10_000_000..500_000_000),
            signal_dbm: Some(-65 - rng.random_range(0..15)),
        },
        LinkStats {
            id: 1,
            interface: "wwan1".into(),
            state: "Live".into(),
            rtt_ms: 28.0 + rng.random_range(0.0..15.0_f64),
            loss_rate: rng.random_range(0.0..0.003_f64),
            capacity_bps: 12_000_000 + rng.random_range(0..3_000_000),
            sent_bytes: rng.random_range(10_000_000..500_000_000),
            signal_dbm: Some(-58 - rng.random_range(0..12)),
        },
    ]
}
