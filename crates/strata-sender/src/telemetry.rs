//! Telemetry — collects pipeline stats and sends them to the control plane.
//!
//! Reads stats from strata-node's UDP relay on 127.0.0.1:9100
//! (bonding stats JSON forwarded from the GStreamer bus).

use std::sync::Arc;
use std::time::Duration;

use strata_protocol::models::LinkStats;
use strata_protocol::{AgentMessage, Envelope, StreamStatsPayload};

use crate::AgentState;
use crate::pipeline;

/// Run the telemetry loop — sends stream.stats every second while streaming.
pub async fn run(state: Arc<AgentState>) {
    let mut interval = tokio::time::interval(Duration::from_secs(1));

    // Set up a non-blocking UDP socket to receive stats from strata-node.
    // The socket is bound once and reused across the lifetime of the agent.
    let stats_rx = std::net::UdpSocket::bind(pipeline::STATS_LISTEN_ADDR).ok();
    if let Some(ref sock) = stats_rx {
        sock.set_nonblocking(true).ok();
        tracing::info!(
            addr = pipeline::STATS_LISTEN_ADDR,
            "stats UDP listener bound"
        );
    }

    // Buffer for incoming stats JSON from strata-node
    let mut last_real_stats: Option<(Vec<LinkStats>, Option<u64>)> = None;
    let mut recv_buf = [0u8; 8192];

    loop {
        interval.tick().await;

        // Check shutdown
        if *state.shutdown.borrow() {
            return;
        }

        // Only send stats if we have a pipeline running
        let mut pipeline = state.pipeline.lock().await;
        if !pipeline.is_running() {
            last_real_stats = None;
            continue;
        }

        let stream_id = match pipeline.stream_id() {
            Some(id) => id.to_string(),
            None => continue,
        };

        let elapsed_s = pipeline.elapsed_s();
        let link_ifaces = pipeline.link_interfaces();
        drop(pipeline); // Release lock before doing I/O

        // Drain any pending stats from strata-node's UDP relay.
        // We take the most recent one (in case multiple arrived in 1s).
        if let Some(ref sock) = stats_rx {
            while let Ok((n, _)) = sock.recv_from(&mut recv_buf) {
                match parse_bonding_stats(&recv_buf[..n]) {
                    Ok(parsed) => {
                        last_real_stats = Some(parsed);
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "failed to parse bonding stats from strata-node");
                    }
                }
            }
        }

        let (mut links, commanded_bitrate_bps) = last_real_stats.clone().unwrap_or_default();

        // Overlay the spawn-time link→interface pinning onto stats whose
        // interface the pipeline didn't name — the dashboard needs every
        // link row mappable to a physical interface.
        for link in &mut links {
            if (link.interface.is_empty() || link.interface == "unknown")
                && let Some(iface) = link_ifaces.get(link.id as usize).filter(|s| !s.is_empty())
            {
                link.interface = iface.clone();
            }
        }

        // Keep the stream's cumulative byte count current so stream.ended
        // reports a real total.
        let sent_total: u64 = links.iter().map(|l| l.sent_bytes).sum();
        if sent_total > 0 {
            let mut pipeline = state.pipeline.lock().await;
            pipeline.set_total_bytes(sent_total);
        }

        // Update shared link stats for Prometheus /metrics endpoint
        {
            let mut latest = state.latest_link_stats.write().await;
            *latest = links.clone();
        }

        // The encoder bitrate is the adapter's *commanded* target
        // (top-level `current_bitrate_bps`), NOT summed on-the-wire
        // `observed_bps` — those are different quantities and conflating
        // them sends diagnosis to the wrong layer (the encoder can be
        // commanding 2.6 Mbps while observed throughput reads 0.5 Mbps
        // during a loss burst). Fall back to the observed sum only if the
        // node didn't report a commanded target.
        let encoder_kbps: u64 = commanded_bitrate_bps
            .map(|b| b / 1000)
            .unwrap_or_else(|| links.iter().map(|l| l.observed_bps).sum::<u64>() / 1000);

        let timestamp_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        let stats = StreamStatsPayload {
            stream_id,
            sender_id: String::new(), // Set by control plane at trust boundary
            uptime_s: elapsed_s,
            encoder_bitrate_kbps: encoder_kbps.max(1) as u32,
            timestamp_ms,
            links,
            sender_metrics: None,
            receiver_metrics: None,
        };

        let envelope = Envelope::from_message(&AgentMessage::StreamStats(stats));
        if let Ok(json) = envelope.and_then(|e| serde_json::to_string(&e))
            && let Err(e) = state.control_tx.send(json).await
        {
            tracing::warn!(error = %e, "failed to send stats to control channel");
        }
    }
}

/// Parse the bonding stats JSON relayed by strata-node.
///
/// The JSON comes from the `strata-stats` GStreamer bus message
/// and has the shape: `{"links": [{"id": 0, "rtt_us": ..., ...}, ...]}`.
/// Parsed bonding stats: the per-link array plus the adapter's *commanded*
/// encoder target (top-level `current_bitrate_bps`). The latter is the
/// real encoder bitrate; summed `observed_bps` is on-the-wire throughput
/// (a different quantity) and must not masquerade as the encoder rate.
fn parse_bonding_stats(data: &[u8]) -> Result<(Vec<LinkStats>, Option<u64>), String> {
    let v: serde_json::Value =
        serde_json::from_slice(data).map_err(|e| format!("JSON parse error: {e}"))?;
    let current_bitrate_bps = v
        .get("current_bitrate_bps")
        .and_then(|x| x.as_u64())
        .filter(|&b| b > 0);
    let links_arr = v
        .get("links")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "missing 'links' array".to_string())?;

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
        let observed_bps = link
            .get("observed_bps")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let iface = link
            .get("interface")
            .or_else(|| link.get("iface"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let alive = link.get("alive").and_then(|v| v.as_bool()).unwrap_or(true);
        let phase = link
            .get("phase")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let os_up = link.get("os_up").and_then(|v| v.as_i64()).unwrap_or(-1);
        let link_kind = link
            .get("link_kind")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        let btlbw_bps = link.get("btlbw_bps").and_then(|v| v.as_u64());
        let rtprop_ms = link.get("rtprop_ms").and_then(|v| v.as_f64());

        // Derive human-readable state from alive/phase/os_up
        let state = if !alive {
            if os_up == 0 {
                "OS Down".to_string()
            } else {
                "Down".to_string()
            }
        } else {
            match phase {
                "probing" => "Probing".to_string(),
                "stable" => "Live".to_string(),
                _ => "Live".to_string(),
            }
        };

        stats.push(LinkStats {
            id,
            interface: iface.to_string(),
            state,
            rtt_ms: rtt_us / 1000.0,
            loss_rate: loss,
            capacity_bps: capacity,
            sent_bytes: sent,
            observed_bps,
            signal_dbm: None,
            link_kind,
            rsrp: None,
            rsrq: None,
            sinr: None,
            cqi: None,
            btlbw_bps,
            rtprop_ms,
        });
    }
    Ok((stats, current_bitrate_bps))
}
