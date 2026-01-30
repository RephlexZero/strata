use std::collections::HashSet;
use std::time::Duration;

use serde::Deserialize;

pub const CONFIG_VERSION: u32 = 1;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct BondingConfigInput {
    pub version: u32,
    pub links: Vec<LinkConfigInput>,
    pub receiver: ReceiverConfigInput,
    pub lifecycle: LinkLifecycleConfigInput,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct LinkConfigInput {
    pub id: Option<usize>,
    pub uri: String,
    pub interface: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ReceiverConfigInput {
    pub start_latency_ms: Option<u64>,
    pub buffer_capacity: Option<usize>,
    pub skip_after_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct LinkLifecycleConfigInput {
    pub good_loss_rate_max: Option<f64>,
    pub good_rtt_ms_min: Option<f64>,
    pub good_capacity_bps_min: Option<f64>,
    pub stats_fresh_ms: Option<u64>,
    pub stats_stale_ms: Option<u64>,
    pub probe_to_warm_good: Option<u32>,
    pub warm_to_live_good: Option<u32>,
    pub warm_to_degrade_bad: Option<u32>,
    pub live_to_degrade_bad: Option<u32>,
    pub degrade_to_warm_good: Option<u32>,
    pub degrade_to_cooldown_bad: Option<u32>,
    pub cooldown_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkConfig {
    pub id: usize,
    pub uri: String,
    pub interface: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiverConfig {
    pub start_latency: Duration,
    pub buffer_capacity: usize,
    pub skip_after: Option<Duration>,
}

#[derive(Debug, Clone)]
pub struct LinkLifecycleConfig {
    pub good_loss_rate_max: f64,
    pub good_rtt_ms_min: f64,
    pub good_capacity_bps_min: f64,
    pub stats_fresh_ms: u64,
    pub stats_stale_ms: u64,
    pub probe_to_warm_good: u32,
    pub warm_to_live_good: u32,
    pub warm_to_degrade_bad: u32,
    pub live_to_degrade_bad: u32,
    pub degrade_to_warm_good: u32,
    pub degrade_to_cooldown_bad: u32,
    pub cooldown_ms: u64,
}

impl Default for ReceiverConfig {
    fn default() -> Self {
        Self {
            start_latency: Duration::from_millis(50),
            buffer_capacity: 2048,
            skip_after: None,
        }
    }
}

impl Default for LinkLifecycleConfig {
    fn default() -> Self {
        Self {
            good_loss_rate_max: 0.2,
            good_rtt_ms_min: 1.0,
            good_capacity_bps_min: 1.0,
            stats_fresh_ms: 1500,
            stats_stale_ms: 3000,
            probe_to_warm_good: 3,
            warm_to_live_good: 10,
            warm_to_degrade_bad: 3,
            live_to_degrade_bad: 3,
            degrade_to_warm_good: 5,
            degrade_to_cooldown_bad: 10,
            cooldown_ms: 2000,
        }
    }
}

#[derive(Debug, Clone)]
pub struct BondingConfig {
    pub version: u32,
    pub links: Vec<LinkConfig>,
    pub receiver: ReceiverConfig,
    pub lifecycle: LinkLifecycleConfig,
}

impl Default for BondingConfig {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            links: Vec::new(),
            receiver: ReceiverConfig::default(),
            lifecycle: LinkLifecycleConfig::default(),
        }
    }
}

impl LinkLifecycleConfigInput {
    pub fn resolve(self) -> LinkLifecycleConfig {
        let defaults = LinkLifecycleConfig::default();
        LinkLifecycleConfig {
            good_loss_rate_max: self
                .good_loss_rate_max
                .unwrap_or(defaults.good_loss_rate_max),
            good_rtt_ms_min: self.good_rtt_ms_min.unwrap_or(defaults.good_rtt_ms_min),
            good_capacity_bps_min: self
                .good_capacity_bps_min
                .unwrap_or(defaults.good_capacity_bps_min),
            stats_fresh_ms: self.stats_fresh_ms.unwrap_or(defaults.stats_fresh_ms),
            stats_stale_ms: self.stats_stale_ms.unwrap_or(defaults.stats_stale_ms),
            probe_to_warm_good: self
                .probe_to_warm_good
                .unwrap_or(defaults.probe_to_warm_good),
            warm_to_live_good: self.warm_to_live_good.unwrap_or(defaults.warm_to_live_good),
            warm_to_degrade_bad: self
                .warm_to_degrade_bad
                .unwrap_or(defaults.warm_to_degrade_bad),
            live_to_degrade_bad: self
                .live_to_degrade_bad
                .unwrap_or(defaults.live_to_degrade_bad),
            degrade_to_warm_good: self
                .degrade_to_warm_good
                .unwrap_or(defaults.degrade_to_warm_good),
            degrade_to_cooldown_bad: self
                .degrade_to_cooldown_bad
                .unwrap_or(defaults.degrade_to_cooldown_bad),
            cooldown_ms: self.cooldown_ms.unwrap_or(defaults.cooldown_ms),
        }
    }
}

impl BondingConfigInput {
    pub fn resolve(self) -> Result<BondingConfig, String> {
        let version = if self.version == 0 {
            CONFIG_VERSION
        } else {
            self.version
        };
        if version != CONFIG_VERSION {
            return Err(format!("Unsupported config version {}", version));
        }

        let receiver = ReceiverConfig {
            start_latency: Duration::from_millis(
                self.receiver
                    .start_latency_ms
                    .unwrap_or(ReceiverConfig::default().start_latency.as_millis() as u64),
            ),
            buffer_capacity: self
                .receiver
                .buffer_capacity
                .unwrap_or(ReceiverConfig::default().buffer_capacity)
                .max(16),
            skip_after: self.receiver.skip_after_ms.map(Duration::from_millis),
        };

        let lifecycle = self.lifecycle.resolve();

        let mut used = HashSet::new();
        let mut out = Vec::new();
        for (idx, link) in self.links.into_iter().enumerate() {
            let id = link.id.unwrap_or(idx);
            if !used.insert(id) {
                continue;
            }
            let uri = link.uri.trim().to_string();
            if uri.is_empty() {
                continue;
            }
            let iface = link.interface.and_then(|iface| {
                let trimmed = iface.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_string())
                }
            });
            out.push(LinkConfig {
                id,
                uri,
                interface: iface,
            });
        }

        Ok(BondingConfig {
            version,
            links: out,
            receiver,
            lifecycle,
        })
    }
}

impl BondingConfig {
    pub fn from_toml_str(input: &str) -> Result<Self, String> {
        if input.trim().is_empty() {
            return Ok(BondingConfig::default());
        }
        let parsed: BondingConfigInput =
            toml::from_str(input).map_err(|e| format!("Invalid config TOML: {}", e))?;
        parsed.resolve()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_toml_config_basic() {
        let toml = r#"
            version = 1

            [[links]]
            id = 10
            uri = "rist://1.2.3.4:5000"

            [[links]]
            uri = "rist://5.6.7.8:5000"
            interface = "eth0"

            [receiver]
            start_latency_ms = 80
            buffer_capacity = 1024
            skip_after_ms = 40

            [lifecycle]
            good_loss_rate_max = 0.15
            stats_fresh_ms = 1200
            cooldown_ms = 1500
        "#;

        let cfg = BondingConfig::from_toml_str(toml).unwrap();
        assert_eq!(cfg.version, CONFIG_VERSION);
        assert_eq!(cfg.links.len(), 2);
        assert_eq!(cfg.links[0].id, 10);
        assert_eq!(cfg.links[0].uri, "rist://1.2.3.4:5000");
        assert!(cfg.links[0].interface.is_none());
        assert_eq!(cfg.links[1].id, 1);
        assert_eq!(cfg.links[1].interface.as_deref(), Some("eth0"));
        assert_eq!(cfg.receiver.start_latency, Duration::from_millis(80));
        assert_eq!(cfg.receiver.buffer_capacity, 1024);
        assert_eq!(cfg.receiver.skip_after, Some(Duration::from_millis(40)));
        assert_eq!(cfg.lifecycle.good_loss_rate_max, 0.15);
        assert_eq!(cfg.lifecycle.stats_fresh_ms, 1200);
        assert_eq!(cfg.lifecycle.cooldown_ms, 1500);
    }

    #[test]
    fn parse_toml_config_dedup() {
        let toml = r#"
            version = 1
            [[links]]
            id = 1
            uri = "a"
            [[links]]
            id = 1
            uri = "b"
        "#;
        let cfg = BondingConfig::from_toml_str(toml).unwrap();
        assert_eq!(cfg.links.len(), 1);
        assert_eq!(cfg.links[0].uri, "a");
    }

    #[test]
    fn parse_toml_config_version_zero_defaults() {
        let toml = r#"
            version = 0
            [[links]]
            uri = "rist://1.2.3.4:5000"
        "#;

        let cfg = BondingConfig::from_toml_str(toml).unwrap();
        assert_eq!(cfg.version, CONFIG_VERSION);
        assert_eq!(cfg.links.len(), 1);
        assert_eq!(cfg.links[0].uri, "rist://1.2.3.4:5000");
    }

    #[test]
    fn parse_toml_config_empty_defaults() {
        let cfg = BondingConfig::from_toml_str("").unwrap();
        assert_eq!(cfg.version, CONFIG_VERSION);
        assert!(cfg.links.is_empty());
        assert_eq!(cfg.receiver, ReceiverConfig::default());
        assert_eq!(
            cfg.lifecycle.good_loss_rate_max,
            LinkLifecycleConfig::default().good_loss_rate_max
        );
    }
}
