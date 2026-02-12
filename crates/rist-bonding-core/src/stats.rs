use crate::net::interface::LinkMetrics;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Per-link metrics snapshot for JSON serialization.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkStatsSnapshot {
    pub rtt: f64,
    pub capacity: f64,
    pub loss: f64,
    pub alive: bool,
    pub phase: String,
    pub observed_bps: f64,
    pub observed_bytes: u64,
    pub os_up: Option<bool>,
    pub mtu: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub iface: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// AIMD delay-gradient capacity estimate (0.0 if estimator disabled).
    pub estimated_capacity_bps: f64,
    /// One-way delay estimate in milliseconds (0.0 if not available).
    pub owd_ms: f64,
}

impl LinkStatsSnapshot {
    pub fn from_metrics(m: &LinkMetrics) -> Self {
        Self {
            rtt: m.rtt_ms,
            capacity: m.capacity_bps,
            loss: m.loss_rate,
            alive: m.alive,
            phase: m.phase.as_str().to_string(),
            observed_bps: m.observed_bps,
            observed_bytes: m.observed_bytes,
            os_up: m.os_up,
            mtu: m.mtu,
            iface: m.iface.clone(),
            kind: m.link_kind.clone(),
            estimated_capacity_bps: m.estimated_capacity_bps,
            owd_ms: m.owd_ms,
        }
    }
}

/// Hierarchical stats snapshot emitted as JSON on GStreamer element messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatsSnapshot {
    pub schema_version: i32,
    pub stats_seq: u64,
    pub heartbeat: bool,
    pub mono_time_ns: u64,
    pub wall_time_ms: u64,
    pub timestamp: f64,
    pub total_capacity: f64,
    pub alive_links: u64,
    pub links: HashMap<String, LinkStatsSnapshot>,
}
