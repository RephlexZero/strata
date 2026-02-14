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
            iface: m.iface.as_ref().map(|s| s.to_string()),
            kind: m.link_kind.as_ref().map(|s| s.to_string()),
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
    /// Aggregate NADA reference rate: sum of per-link `estimated_capacity_bps`
    /// for alive links.  This is the `r_ref` used by RFC 8698 §5.2.2 sender-
    /// side rate derivation to compute encoder target bitrate (`r_vin`).
    pub aggregate_nada_ref_bps: f64,
    pub alive_links: u64,
    /// Total packets dropped because all links were dead.
    pub total_dead_drops: u64,
    pub links: HashMap<String, LinkStatsSnapshot>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::interface::{LinkMetrics, LinkPhase};

    /// Build a fully-populated LinkMetrics for testing.
    fn sample_metrics() -> LinkMetrics {
        LinkMetrics {
            rtt_ms: 12.5,
            capacity_bps: 10_000_000.0,
            loss_rate: 0.02,
            observed_bps: 5_000_000.0,
            observed_bytes: 123_456,
            queue_depth: 10,
            max_queue: 100,
            alive: true,
            phase: LinkPhase::Live,
            os_up: Some(true),
            mtu: Some(1500),
            iface: Some("eth0".into()),
            link_kind: Some("wired".into()),
            estimated_capacity_bps: 8_000_000.0,
            owd_ms: 6.25,
        }
    }

    #[test]
    fn from_metrics_maps_all_fields() {
        let m = sample_metrics();
        let s = LinkStatsSnapshot::from_metrics(&m);

        assert!((s.rtt - 12.5).abs() < 1e-6);
        assert!((s.capacity - 10_000_000.0).abs() < 1e-6);
        assert!((s.loss - 0.02).abs() < 1e-6);
        assert!(s.alive);
        assert_eq!(s.phase, "live");
        assert!((s.observed_bps - 5_000_000.0).abs() < 1e-6);
        assert_eq!(s.observed_bytes, 123_456);
        assert_eq!(s.os_up, Some(true));
        assert_eq!(s.mtu, Some(1500));
        assert_eq!(s.iface.as_deref(), Some("eth0"));
        assert_eq!(s.kind.as_deref(), Some("wired"));
        assert!((s.estimated_capacity_bps - 8_000_000.0).abs() < 1e-6);
        assert!((s.owd_ms - 6.25).abs() < 1e-6);
    }

    #[test]
    fn from_metrics_phase_string_all_variants() {
        let phases = [
            (LinkPhase::Init, "init"),
            (LinkPhase::Probe, "probe"),
            (LinkPhase::Warm, "warm"),
            (LinkPhase::Live, "live"),
            (LinkPhase::Degrade, "degrade"),
            (LinkPhase::Cooldown, "cooldown"),
            (LinkPhase::Reset, "reset"),
        ];
        for (phase, expected_str) in phases {
            let m = LinkMetrics {
                phase,
                ..LinkMetrics::default()
            };
            let s = LinkStatsSnapshot::from_metrics(&m);
            assert_eq!(
                s.phase, expected_str,
                "Phase {:?} should serialize as {:?}",
                phase, expected_str
            );
        }
    }

    #[test]
    fn from_metrics_optional_fields_none() {
        let m = LinkMetrics {
            os_up: None,
            mtu: None,
            iface: None,
            link_kind: None,
            ..LinkMetrics::default()
        };
        let s = LinkStatsSnapshot::from_metrics(&m);
        assert_eq!(s.os_up, None);
        assert_eq!(s.mtu, None);
        assert_eq!(s.iface, None);
        assert_eq!(s.kind, None);
    }

    #[test]
    fn snapshot_json_roundtrip() {
        let m = sample_metrics();
        let s = LinkStatsSnapshot::from_metrics(&m);

        let json = serde_json::to_string(&s).expect("serialize failed");
        let deserialized: LinkStatsSnapshot =
            serde_json::from_str(&json).expect("deserialize failed");

        assert!((deserialized.rtt - s.rtt).abs() < 1e-6);
        assert!((deserialized.capacity - s.capacity).abs() < 1e-6);
        assert!((deserialized.loss - s.loss).abs() < 1e-6);
        assert_eq!(deserialized.alive, s.alive);
        assert_eq!(deserialized.phase, s.phase);
        assert_eq!(deserialized.os_up, s.os_up);
        assert_eq!(deserialized.mtu, s.mtu);
        assert_eq!(deserialized.iface, s.iface);
        assert_eq!(deserialized.kind, s.kind);
    }

    #[test]
    fn stats_snapshot_fields() {
        let snap = StatsSnapshot {
            schema_version: 1,
            stats_seq: 42,
            heartbeat: false,
            mono_time_ns: 1_000_000_000,
            wall_time_ms: 1700000000000,
            timestamp: 1.23,
            total_capacity: 20_000_000.0,
            aggregate_nada_ref_bps: 15_000_000.0,
            alive_links: 2,
            total_dead_drops: 0,
            links: HashMap::new(),
        };
        assert_eq!(snap.schema_version, 1);
        assert_eq!(snap.stats_seq, 42);
        assert!(!snap.heartbeat);
        assert_eq!(snap.alive_links, 2);
        assert!(snap.links.is_empty());

        // JSON roundtrip of full snapshot
        let json = serde_json::to_string(&snap).expect("serialize");
        let back: StatsSnapshot = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.schema_version, snap.schema_version);
        assert_eq!(back.stats_seq, snap.stats_seq);
        assert!((back.aggregate_nada_ref_bps - snap.aggregate_nada_ref_bps).abs() < 1e-6);
    }

    // ────────────────────────────────────────────────────────────────
    // CompactString → String serialization tests (#6)
    // ────────────────────────────────────────────────────────────────

    #[test]
    fn compact_string_iface_serializes_to_json_string() {
        let m = sample_metrics();
        let s = LinkStatsSnapshot::from_metrics(&m);
        let json = serde_json::to_string(&s).unwrap();
        // Verify the JSON contains "iface":"eth0" and "kind":"wired"
        assert!(json.contains(r#""iface":"eth0""#), "json = {}", json);
        assert!(json.contains(r#""kind":"wired""#), "json = {}", json);
    }

    #[test]
    fn compact_string_none_omitted_from_json() {
        let m = LinkMetrics {
            iface: None,
            link_kind: None,
            ..LinkMetrics::default()
        };
        let s = LinkStatsSnapshot::from_metrics(&m);
        let json = serde_json::to_string(&s).unwrap();
        // With skip_serializing_if = "Option::is_none", these should be absent
        assert!(
            !json.contains("iface"),
            "json should omit null iface: {}",
            json
        );
        assert!(
            !json.contains("kind"),
            "json should omit null kind: {}",
            json
        );
    }
}
