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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkConfig {
    pub id: usize,
    pub uri: String,
    pub interface: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ReceiverConfig {
    pub start_latency: Duration,
    pub buffer_capacity: usize,
    pub skip_after: Option<Duration>,
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

#[derive(Debug, Clone)]
pub struct BondingConfig {
    pub version: u32,
    pub links: Vec<LinkConfig>,
    pub receiver: ReceiverConfig,
}

impl Default for BondingConfig {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            links: Vec::new(),
            receiver: ReceiverConfig::default(),
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
}
