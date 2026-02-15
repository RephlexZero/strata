//! Hardware scanner — enumerates network interfaces, media inputs, and system stats.
//!
//! In simulation mode, generates realistic fake data for local development.
//! In production mode, reads from /sys, /proc, v4l2, and ModemManager.

use serde::{Deserialize, Serialize};

use strata_common::models::{
    InterfaceState, InterfaceType, MediaInput, MediaInputStatus, MediaInputType, NetworkInterface,
};

/// Result of a hardware scan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HardwareScan {
    pub interfaces: Vec<NetworkInterface>,
    pub inputs: Vec<MediaInput>,
    pub cpu_percent: f32,
    pub mem_used_mb: u32,
    pub uptime_s: u64,
}

/// Scans hardware state — real or simulated.
pub struct HardwareScanner {
    simulate: bool,
    /// When true, inject simulated cellular interfaces and a test media input
    /// alongside real system stats.  Used by the dev Docker container which has
    /// no real wwan devices or capture cards but needs to look like a field sender.
    dev_interfaces: bool,
    /// Tracks enabled/disabled state per interface name.
    interface_enabled: std::sync::Mutex<std::collections::HashMap<String, bool>>,
}

impl HardwareScanner {
    pub fn new(simulate: bool) -> Self {
        let dev_interfaces = std::env::var("DEV_INTERFACES").is_ok();
        Self {
            simulate,
            dev_interfaces,
            interface_enabled: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Perform a hardware scan.
    pub async fn scan(&self) -> HardwareScan {
        if self.simulate {
            self.scan_simulated().await
        } else {
            self.scan_real().await
        }
    }

    /// Simulated hardware scan — generates fake but realistic data.
    async fn scan_simulated(&self) -> HardwareScan {
        use rand::Rng;
        let mut rng = rand::rng();
        let enabled_map = self.interface_enabled.lock().unwrap();

        let mut interfaces = vec![
            NetworkInterface {
                name: "wwan0".into(),
                iface_type: InterfaceType::Cellular,
                state: InterfaceState::Connected,
                enabled: *enabled_map.get("wwan0").unwrap_or(&true),
                ip: Some("10.45.0.2".into()),
                carrier: Some("T-Mobile".into()),
                signal_dbm: Some(-65 - rng.random_range(0..20)),
                technology: Some("LTE".into()),
            },
            NetworkInterface {
                name: "wwan1".into(),
                iface_type: InterfaceType::Cellular,
                state: InterfaceState::Connected,
                enabled: *enabled_map.get("wwan1").unwrap_or(&true),
                ip: Some("10.46.0.3".into()),
                carrier: Some("Vodafone".into()),
                signal_dbm: Some(-60 - rng.random_range(0..15)),
                technology: Some("5G-NSA".into()),
            },
            NetworkInterface {
                name: "eth0".into(),
                iface_type: InterfaceType::Ethernet,
                state: InterfaceState::Disconnected,
                enabled: *enabled_map.get("eth0").unwrap_or(&true),
                ip: None,
                carrier: None,
                signal_dbm: None,
                technology: None,
            },
        ];

        // Apply enabled state: if disabled, force state to Disconnected
        for iface in &mut interfaces {
            if !iface.enabled {
                iface.state = InterfaceState::Disconnected;
                iface.ip = None;
            }
        }

        let inputs = vec![MediaInput {
            device: "/dev/video0".into(),
            input_type: MediaInputType::Test,
            label: "Simulated HDMI Capture".into(),
            capabilities: vec!["1920x1080@30".into(), "1280x720@60".into()],
            status: MediaInputStatus::Available,
        }];

        HardwareScan {
            interfaces,
            inputs,
            cpu_percent: 8.0 + rng.random_range(0.0..15.0_f32),
            mem_used_mb: 180 + rng.random_range(0..50),
            uptime_s: get_uptime_s(),
        }
    }

    /// Real hardware scan — reads from system interfaces.
    ///
    /// When `dev_interfaces` is set, simulated cellular interfaces and a test
    /// media input are injected so the dashboard looks realistic even inside a
    /// plain Docker container that only has eth0.
    async fn scan_real(&self) -> HardwareScan {
        let enabled_map = self.interface_enabled.lock().unwrap();
        let mut interfaces = if self.dev_interfaces {
            dev_simulated_interfaces(&enabled_map)
        } else {
            scan_network_interfaces()
        };
        // Apply enabled state
        for iface in &mut interfaces {
            iface.enabled = *enabled_map.get(&iface.name).unwrap_or(&true);
            if !iface.enabled {
                iface.state = InterfaceState::Disconnected;
                iface.ip = None;
            }
        }
        drop(enabled_map);

        let inputs = if self.dev_interfaces {
            dev_simulated_inputs()
        } else {
            scan_media_inputs()
        };
        let (cpu, mem) = scan_system_stats();

        HardwareScan {
            interfaces,
            inputs,
            cpu_percent: cpu,
            mem_used_mb: mem,
            uptime_s: get_uptime_s(),
        }
    }

    /// Enable or disable a network interface by name.
    pub fn set_interface_enabled(&self, name: &str, enabled: bool) -> bool {
        let mut map = self.interface_enabled.lock().unwrap();
        map.insert(name.to_string(), enabled);

        // In real mode, actually bring the interface up/down
        if !self.simulate {
            let action = if enabled { "up" } else { "down" };
            let status = std::process::Command::new("ip")
                .args(["link", "set", name, action])
                .status();
            if let Err(e) = status {
                tracing::warn!(interface = %name, action, error = %e, "failed to set interface state");
                return false;
            }
        }
        true
    }

    /// Discover new network interfaces not previously seen.
    /// Returns the list of newly discovered interface names.
    pub async fn discover_interfaces(&self) -> Vec<String> {
        let scan = self.scan().await;
        let map = self.interface_enabled.lock().unwrap();
        let mut new_ifaces = Vec::new();
        for iface in &scan.interfaces {
            if !map.contains_key(&iface.name) {
                new_ifaces.push(iface.name.clone());
            }
        }
        new_ifaces
    }
}

// ── Dev-simulated hardware (for Docker containers without real modems) ────

/// Simulated interfaces for the dev container — three links that look realistic.
fn dev_simulated_interfaces(
    enabled_map: &std::collections::HashMap<String, bool>,
) -> Vec<NetworkInterface> {
    use rand::Rng;
    let mut rng = rand::rng();

    vec![
        NetworkInterface {
            name: "wwan0".into(),
            iface_type: InterfaceType::Cellular,
            state: InterfaceState::Connected,
            enabled: *enabled_map.get("wwan0").unwrap_or(&true),
            ip: Some("10.45.0.2".into()),
            carrier: Some("T-Mobile".into()),
            signal_dbm: Some(-65 - rng.random_range(0..20)),
            technology: Some("LTE".into()),
        },
        NetworkInterface {
            name: "wwan1".into(),
            iface_type: InterfaceType::Cellular,
            state: InterfaceState::Connected,
            enabled: *enabled_map.get("wwan1").unwrap_or(&true),
            ip: Some("10.46.0.3".into()),
            carrier: Some("Vodafone".into()),
            signal_dbm: Some(-60 - rng.random_range(0..15)),
            technology: Some("5G-NSA".into()),
        },
        NetworkInterface {
            name: "eth0".into(),
            iface_type: InterfaceType::Ethernet,
            state: InterfaceState::Connected,
            enabled: *enabled_map.get("eth0").unwrap_or(&true),
            ip: Some("172.19.0.5".into()),
            carrier: None,
            signal_dbm: None,
            technology: None,
        },
    ]
}

/// Simulated media input for the dev container (no real capture card).
fn dev_simulated_inputs() -> Vec<MediaInput> {
    vec![MediaInput {
        device: "/dev/video0".into(),
        input_type: MediaInputType::Test,
        label: "Simulated HDMI Capture".into(),
        capabilities: vec!["1920x1080@30".into(), "1280x720@60".into()],
        status: MediaInputStatus::Available,
    }]
}

// ── Real hardware scanning helpers ──────────────────────────────────

fn scan_network_interfaces() -> Vec<NetworkInterface> {
    let mut interfaces = Vec::new();

    // Read /sys/class/net/ for interface enumeration
    let net_dir = match std::fs::read_dir("/sys/class/net") {
        Ok(d) => d,
        Err(_) => return interfaces,
    };

    for entry in net_dir.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name == "lo" {
            continue;
        }

        // Determine type
        let iface_type = if name.starts_with("wwan") || name.starts_with("wlp") {
            if name.starts_with("wwan") {
                InterfaceType::Cellular
            } else {
                InterfaceType::Wifi
            }
        } else if name.starts_with("eth") || name.starts_with("en") {
            InterfaceType::Ethernet
        } else {
            continue; // skip docker, veth, etc.
        };

        // Read operstate
        let state_path = format!("/sys/class/net/{name}/operstate");
        let state = match std::fs::read_to_string(&state_path) {
            Ok(s) => match s.trim() {
                "up" => InterfaceState::Connected,
                "down" => InterfaceState::Disconnected,
                _ => InterfaceState::Disconnected,
            },
            Err(_) => InterfaceState::Disconnected,
        };

        interfaces.push(NetworkInterface {
            name,
            iface_type,
            state,
            enabled: true, // will be overridden by scan_real
            ip: None,      // TODO: read from `ip addr`
            carrier: None, // TODO: read from ModemManager
            signal_dbm: None,
            technology: None,
        });
    }

    interfaces
}

fn scan_media_inputs() -> Vec<MediaInput> {
    let mut inputs = Vec::new();

    // Scan /dev/video* devices
    let dev_dir = match std::fs::read_dir("/dev") {
        Ok(d) => d,
        Err(_) => return inputs,
    };

    for entry in dev_dir.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with("video") {
            continue;
        }

        let device = format!("/dev/{name}");

        // Try to get device name from sysfs
        let label_path = format!("/sys/class/video4linux/{name}/name");
        let label = std::fs::read_to_string(&label_path)
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| format!("Video Device {name}"));

        inputs.push(MediaInput {
            device,
            input_type: MediaInputType::V4l2,
            label,
            capabilities: vec![], // TODO: v4l2-ctl --list-formats-ext
            status: MediaInputStatus::Available,
        });
    }

    inputs
}

fn scan_system_stats() -> (f32, u32) {
    use sysinfo::System;
    let mut sys = System::new();
    sys.refresh_cpu_all();
    sys.refresh_memory();

    let cpu = sys.global_cpu_usage();
    let mem_used_mb = (sys.used_memory() / 1_048_576) as u32;
    (cpu, mem_used_mb)
}

fn get_uptime_s() -> u64 {
    std::fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|s| s.split_whitespace().next().map(|v| v.to_string()))
        .and_then(|v| v.parse::<f64>().ok())
        .map(|v| v as u64)
        .unwrap_or(0)
}
