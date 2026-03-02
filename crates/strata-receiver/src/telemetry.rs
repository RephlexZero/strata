//! Telemetry — collects receiver pipeline stats and sends to control plane.
//!
//! Each running pipeline reports stats via UDP on a unique port (9200+).
//! We drain all sockets each tick and forward to the control plane.

use std::sync::Arc;
use std::time::Duration;

use strata_common::models::LinkStats;
use strata_common::protocol::{Envelope, ReceiverStreamStatsPayload};

use crate::ReceiverState;

/// Run the telemetry loop — collects stats from all active pipelines.
pub async fn run(state: Arc<ReceiverState>) {
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    let mut sockets: std::collections::HashMap<u16, std::net::UdpSocket> =
        std::collections::HashMap::new();
    let mut recv_buf = [0u8; 8192];

    loop {
        interval.tick().await;

        if *state.shutdown.borrow() {
            return;
        }

        // Get the list of active streams and their stats ports
        let active = {
            let pipelines = state.pipelines.lock().await;
            pipelines.active_streams()
        };

        if active.is_empty() {
            // Clean up stale sockets
            sockets.clear();
            let mut stats = state.latest_stats.write().await;
            stats.clear();
            continue;
        }

        let receiver_id = state.receiver_id.lock().await.clone().unwrap_or_default();

        for (stream_id, stats_port) in &active {
            // Lazily bind sockets for new streams
            let sock = sockets.entry(*stats_port).or_insert_with(|| {
                let addr = format!("127.0.0.1:{stats_port}");
                let s = std::net::UdpSocket::bind(&addr).unwrap_or_else(|_| {
                    // Already bound or unavailable — bind to ephemeral as fallback
                    std::net::UdpSocket::bind("127.0.0.1:0").expect("failed to bind UDP socket")
                });
                s.set_nonblocking(true).ok();
                s
            });

            // Drain incoming stats, keep the latest
            let mut last_stats: Option<Vec<LinkStats>> = None;
            while let Ok((n, _)) = sock.recv_from(&mut recv_buf) {
                if let Ok(parsed) = parse_bonding_stats(&recv_buf[..n]) {
                    last_stats = Some(parsed);
                }
            }

            if let Some(links) = last_stats {
                // Update shared stats
                {
                    let mut latest = state.latest_stats.write().await;
                    latest.insert(stream_id.clone(), links.clone());
                }

                let timestamp_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64;

                let payload = ReceiverStreamStatsPayload {
                    stream_id: stream_id.clone(),
                    receiver_id: receiver_id.clone(),
                    uptime_s: 0, // Will be enriched by the pipeline entry
                    timestamp_ms,
                    links,
                };

                let envelope = Envelope::new("receiver.stream.stats", &payload);
                if let Ok(json) = serde_json::to_string(&envelope) {
                    let _ = state.control_tx.send(json).await;
                }
            }
        }

        // Remove sockets for streams that are no longer active
        let active_ports: std::collections::HashSet<u16> = active.iter().map(|(_, p)| *p).collect();
        sockets.retain(|port, _| active_ports.contains(port));
    }
}

/// Parse bonding stats JSON from strata-pipeline.
fn parse_bonding_stats(data: &[u8]) -> Result<Vec<LinkStats>, String> {
    let v: serde_json::Value =
        serde_json::from_slice(data).map_err(|e| format!("JSON parse error: {e}"))?;
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
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let received = link
            .get("received_bytes")
            .or_else(|| link.get("rx_bytes"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let observed_bps = link
            .get("observed_bps")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let iface = link
            .get("interface")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");

        stats.push(LinkStats {
            id,
            interface: iface.to_string(),
            state: "Live".to_string(),
            rtt_ms: rtt_us / 1000.0,
            loss_rate: loss,
            capacity_bps: capacity,
            sent_bytes: received, // receiver side: these are received bytes
            observed_bps,
            signal_dbm: None,
            link_kind: None,
            rsrp: None,
            rsrq: None,
            sinr: None,
            cqi: None,
            btlbw_bps: None,
            rtprop_ms: None,
        });
    }
    Ok(stats)
}
