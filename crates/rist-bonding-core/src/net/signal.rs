//! Signal-strength watermark for wireless link classification.
//!
//! Reads the wireless signal level (RSSI / RSRP) for an interface from
//! the Linux `/proc/net/wireless` pseudo-file (available for Wi-Fi) or,
//! for cellular modems via NetworkManager, from sysfs.
//!
//! When the signal drops below a configurable **watermark threshold**
//! (e.g. RSRP < −115 dBm), the link is classified as *unreliable* for
//! scheduling purposes: droppable (P/B-frame) traffic is steered away
//! from it and FEC overhead may be raised.
//!
//! # Thread safety
//!
//! The [`read_signal_dbm`] function performs a blocking file I/O read.
//! It is intended to be called within the scheduler's `refresh_metrics`
//! cycle (every ~100 ms), NOT on the hot packet-send path.

use std::fs;

/// Attempt to read the wireless signal level for `iface` from
/// `/proc/net/wireless`.
///
/// Returns `Some(dbm)` on success, `None` if the interface is not
/// wireless or the file is unavailable.
pub fn read_signal_dbm(iface: &str) -> Option<f64> {
    let contents = fs::read_to_string("/proc/net/wireless").ok()?;

    // /proc/net/wireless format (after two header lines):
    //   iface: 0000   level.  noise.  nwid  crypt  ...
    // Example:
    //   wlan0: 0000   -42.  -95.  0  0  0  0  0  0
    for line in contents.lines().skip(2) {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix(iface) {
            if let Some(rest) = rest.strip_prefix(':') {
                // Skip the status field(s) and parse the signal level.
                let fields: Vec<&str> = rest.split_whitespace().collect();
                if fields.len() >= 2 {
                    // The signal level field may have a trailing period.
                    let level_str = fields[1].trim_end_matches('.');
                    return level_str.parse::<f64>().ok();
                }
            }
        }
    }
    None
}

/// Evaluates the signal watermark policy.
///
/// Returns `true` if the link should be considered **unreliable**
/// (signal below threshold). Returns `false` for wired links or
/// when signal data is unavailable.
pub fn is_below_watermark(signal_dbm: Option<f64>, threshold_dbm: f64) -> bool {
    match signal_dbm {
        Some(dbm) => dbm < threshold_dbm,
        None => false,
    }
}

/// Signal watermark configuration.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct SignalWatermarkConfig {
    /// Enable signal-based link classification.
    pub enabled: bool,
    /// Signal threshold in dBm. Below this → link classified unreliable.
    /// For Wi-Fi: typical range −30 (excellent) to −90 (unusable).
    /// For LTE RSRP: typical range −80 (excellent) to −120 (out of service).
    pub threshold_dbm: f64,
    /// When unreliable, reduce the link's effective capacity by this factor
    /// (0.0 – 1.0). E.g. 0.5 → halve the capacity for scheduling credit.
    pub capacity_penalty: f64,
}

impl Default for SignalWatermarkConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            threshold_dbm: -80.0,
            capacity_penalty: 0.5,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watermark_below_threshold_is_unreliable() {
        assert!(is_below_watermark(Some(-90.0), -80.0));
    }

    #[test]
    fn watermark_above_threshold_is_reliable() {
        assert!(!is_below_watermark(Some(-60.0), -80.0));
    }

    #[test]
    fn watermark_at_threshold_is_reliable() {
        assert!(!is_below_watermark(Some(-80.0), -80.0));
    }

    #[test]
    fn watermark_none_signal_is_reliable() {
        assert!(!is_below_watermark(None, -80.0));
    }

    #[test]
    fn read_signal_returns_none_when_not_wireless() {
        // On a container without wireless interfaces, should return None
        let result = read_signal_dbm("nonexistent_iface0");
        assert!(result.is_none());
    }

    #[test]
    fn parse_proc_net_wireless_format() {
        // Simulate the parsing logic with known content
        let fake_content = "\
Inter-| sta-|   Quality        |   Discarded packets               | Missed | WE
 face | tus | link level noise |  nwid  crypt   frag  retry   misc | beacon | 22
 wlan0: 0000   -42.  -95.  0        0      0      0       0       0
 wwan0: 0000   -75.  -100.  0        0      0      0       0       0";

        // Parse wlan0 signal level
        for line in fake_content.lines().skip(2) {
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix("wlan0") {
                if let Some(rest) = rest.strip_prefix(':') {
                    let fields: Vec<&str> = rest.split_whitespace().collect();
                    assert!(fields.len() >= 2);
                    let level = fields[1].trim_end_matches('.').parse::<f64>().unwrap();
                    assert!((level - (-42.0)).abs() < 1e-6);
                    return;
                }
            }
        }
        panic!("Should have parsed wlan0 signal level");
    }

    #[test]
    fn config_defaults() {
        let cfg = SignalWatermarkConfig::default();
        assert!(!cfg.enabled);
        assert!((cfg.threshold_dbm - (-80.0)).abs() < 1e-6);
        assert!((cfg.capacity_penalty - 0.5).abs() < 1e-6);
    }
}
