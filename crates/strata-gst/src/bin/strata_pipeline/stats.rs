//! Stats serialization, interface resolution, and config conversion helpers.

/// Convert a serde_json::Value to TOML string for stratasink's config property.
pub(crate) fn json_to_toml(value: &serde_json::Value) -> Result<String, String> {
    // serde_json::Value → toml::Value via intermediate serialization
    let toml_value: toml::Value =
        serde_json::from_value(value.clone()).map_err(|e| format!("json→toml conversion: {e}"))?;
    toml::to_string(&toml_value).map_err(|e| format!("toml serialization: {e}"))
}

// ── Interface resolution ────────────────────────────────────────────

/// Resolve which OS network interface routes to the host in an address.
///
/// Parses the host from `host:port` and runs
/// `ip route get <host>` to determine the outgoing interface.
pub(crate) fn resolve_interface_for_uri(uri: &str) -> Option<String> {
    // Strip strata:// or legacy rist:// prefix for backwards compat
    let stripped = uri
        .strip_prefix("strata://")
        .or_else(|| uri.strip_prefix("strata://@"))
        .or_else(|| uri.strip_prefix("rist://"))
        .or_else(|| uri.strip_prefix("rist://@"))
        .unwrap_or(uri);
    let host = stripped.split(':').next()?;
    if host.is_empty() {
        return None;
    }

    let output = std::process::Command::new("ip")
        .args(["route", "get", host])
        .output()
        .ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Parse: "172.30.0.20 dev eth2 src 172.30.0.10 ..."
    for part in stdout.split_whitespace().collect::<Vec<_>>().windows(2) {
        if part[0] == "dev" {
            return Some(part[1].to_string());
        }
    }
    None
}

// ── Stats serialization ─────────────────────────────────────────────

/// Serialize the `strata-stats` GStreamer structure into JSON
/// that the agent telemetry module can parse.
///
/// Includes ALL links (alive and dead) with full metadata so the
/// dashboard can show link state transitions and technology type.
pub(crate) fn serialize_bonding_stats(s: &gst::StructureRef) -> serde_json::Value {
    let alive_links = s.get::<u64>("alive_links").unwrap_or(0);
    let wall_time_ms = s.get::<u64>("wall_time_ms").unwrap_or(0);
    let mut links = Vec::new();

    // Probe link IDs — not necessarily 0..N contiguous
    let max_probe = alive_links.max(8) as u32;
    for id in 0..max_probe {
        let rtt_key = format!("link_{}_rtt", id);
        if let Ok(rtt_ms) = s.get::<f64>(&rtt_key) {
            let capacity = s
                .get::<f64>(&format!("link_{}_capacity", id))
                .unwrap_or(0.0);
            let loss = s.get::<f64>(&format!("link_{}_loss", id)).unwrap_or(0.0);
            let observed_bytes = s
                .get::<u64>(&format!("link_{}_observed_bytes", id))
                .unwrap_or(0);
            let observed_bps = s
                .get::<f64>(&format!("link_{}_observed_bps", id))
                .unwrap_or(0.0);
            let iface = s
                .get::<&str>(&format!("link_{}_iface", id))
                .unwrap_or("unknown");
            let alive = s
                .get::<bool>(&format!("link_{}_alive", id))
                .unwrap_or(false);
            let phase = s
                .get::<&str>(&format!("link_{}_phase", id))
                .unwrap_or("unknown");
            let os_up = s.get::<i32>(&format!("link_{}_os_up", id)).unwrap_or(-1);
            let kind = s.get::<&str>(&format!("link_{}_kind", id)).unwrap_or("");

            links.push({
                let mut obj = serde_json::json!({
                    "id": id,
                    "rtt_us": (rtt_ms * 1000.0) as u64,
                    "loss_rate": loss,
                    "capacity_bps": capacity.round() as u64,
                    "sent_bytes": observed_bytes,
                    "observed_bps": observed_bps.round() as u64,
                    "interface": iface,
                    "alive": alive,
                    "phase": phase,
                    "os_up": os_up,
                    "link_kind": kind,
                });
                if let Ok(bw) = s.get::<f64>(&format!("link_{}_btlbw_bps", id)) {
                    obj["btlbw_bps"] = serde_json::json!(bw.round() as u64);
                }
                if let Ok(rtp) = s.get::<f64>(&format!("link_{}_rtprop_ms", id)) {
                    obj["rtprop_ms"] = serde_json::json!(rtp);
                }
                obj
            });
        }
    }

    serde_json::json!({
        "links": links,
        "timestamp_ms": wall_time_ms,
    })
}
