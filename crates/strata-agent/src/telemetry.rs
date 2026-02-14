//! Telemetry — collects pipeline stats and sends them to the control plane.
//!
//! In simulation mode, generates synthetic link stats that look realistic.
//! In production mode, reads from the pipeline's stats output.

use std::sync::Arc;
use std::time::Duration;

use strata_common::models::LinkStats;
use strata_common::protocol::{Envelope, StreamStatsPayload};

use crate::AgentState;

/// Run the telemetry loop — sends stream.stats every second while streaming.
pub async fn run(state: Arc<AgentState>) {
    let mut interval = tokio::time::interval(Duration::from_secs(1));

    loop {
        interval.tick().await;

        // Check shutdown
        if *state.shutdown.borrow() {
            return;
        }

        // Only send stats if we have a pipeline running
        let pipeline = state.pipeline.lock().await;
        if !pipeline.is_running() {
            continue;
        }

        let stream_id = match pipeline.stream_id() {
            Some(id) => id.to_string(),
            None => continue,
        };

        let elapsed_s = pipeline.elapsed_s();
        drop(pipeline); // Release lock before doing I/O

        // Build stats
        let links = if state.simulate {
            generate_simulated_stats()
        } else {
            collect_real_stats()
        };

        let stats = StreamStatsPayload {
            stream_id,
            uptime_s: elapsed_s,
            encoder_bitrate_kbps: 5000,
            links,
        };

        let envelope = Envelope::new("stream.stats", &stats);
        if let Ok(json) = serde_json::to_string(&envelope) {
            let _ = state.control_tx.send(json).await;
        }
    }
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

/// Collect real stats from the running pipeline.
/// TODO: read from integration_node's --stats-dest UDP output.
fn collect_real_stats() -> Vec<LinkStats> {
    // Placeholder — will read from 127.0.0.1:9100 UDP
    vec![]
}
