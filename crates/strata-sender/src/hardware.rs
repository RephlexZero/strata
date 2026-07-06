//! Hardware scanner — enumerates network interfaces, media inputs, and system stats.
//!
//! Reads from /sys, /proc, v4l2 ioctls, `ip -j`, and (for HiLink modems)
//! the gateway's HTTP API. A synthetic "GStreamer Test Source" input is
//! always available regardless of whether real capture devices are present.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use strata_protocol::models::{
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

/// Where the admin enable/disable map persists across daemon restarts.
fn interface_state_file() -> String {
    std::env::var("STRATA_INTERFACE_STATE_FILE")
        .unwrap_or_else(|_| "/var/lib/strata/interface-admin.json".into())
}

/// How long a HiLink probe result (success or failure) stays fresh before
/// the next heartbeat scan re-probes the gateway.
const MODEM_PROBE_TTL: Duration = Duration::from_secs(15);

/// Scans hardware state.
pub struct HardwareScanner {
    /// Tracks enabled/disabled state per interface name. Loaded from and
    /// persisted to `interface_state_file()` so operator toggles survive
    /// daemon restarts.
    interface_enabled: std::sync::Mutex<HashMap<String, bool>>,
    /// Per-gateway HiLink probe cache — `None` marks a gateway that didn't
    /// answer the HiLink API so we don't hammer it every heartbeat.
    modem_cache: tokio::sync::Mutex<HashMap<String, (Instant, Option<crate::hilink::ModemInfo>)>>,
}

impl HardwareScanner {
    pub fn new() -> Self {
        let map = std::fs::read_to_string(interface_state_file())
            .ok()
            .and_then(|s| serde_json::from_str::<HashMap<String, bool>>(&s).ok())
            .unwrap_or_default();
        if !map.is_empty() {
            tracing::info!(count = map.len(), "loaded persisted interface admin state");
        }
        Self {
            interface_enabled: std::sync::Mutex::new(map),
            modem_cache: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Perform a hardware scan.
    pub async fn scan(&self) -> HardwareScan {
        self.scan_real().await
    }

    /// Real hardware scan — reads from system interfaces.
    async fn scan_real(&self) -> HardwareScan {
        let mut interfaces = scan_network_interfaces();
        // Apply admin enabled state
        {
            let enabled_map = self.interface_enabled.lock().unwrap();
            for iface in &mut interfaces {
                iface.enabled = *enabled_map.get(&iface.name).unwrap_or(&true);
                if !iface.enabled {
                    iface.state = InterfaceState::Disconnected;
                    iface.ip = None;
                }
            }
        }

        // Enrich cellular interfaces with live modem status (HiLink API).
        for iface in &mut interfaces {
            if iface.iface_type != InterfaceType::Cellular || !iface.enabled {
                continue;
            }
            let Some(gateway) = iface.gateway.clone() else {
                continue;
            };
            if let Some(info) = self.probe_modem(&gateway).await {
                iface.carrier = info.carrier;
                iface.technology = info.technology;
                iface.band = info.band;
                iface.cell_id = info.cell_id;
                iface.signal_dbm = info.signal_dbm;
            }
        }

        let inputs = scan_media_inputs();
        let (cpu, mem) = scan_system_stats();

        HardwareScan {
            interfaces,
            inputs,
            cpu_percent: cpu,
            mem_used_mb: mem,
            uptime_s: get_uptime_s(),
        }
    }

    async fn probe_modem(&self, gateway: &str) -> Option<crate::hilink::ModemInfo> {
        let mut cache = self.modem_cache.lock().await;
        if let Some((at, info)) = cache.get(gateway)
            && at.elapsed() < MODEM_PROBE_TTL
        {
            return info.clone();
        }
        let info = crate::hilink::probe(gateway).await;
        cache.insert(gateway.to_string(), (Instant::now(), info.clone()));
        info
    }

    /// Enable or disable a network interface by name.
    ///
    /// This only updates the admin state (persisted to disk) — it does
    /// **not** bring the OS interface down.  The caller is responsible for
    /// telling the running pipeline to exclude/include the corresponding
    /// link so that disabling an interface only removes it from the bonding
    /// transport without killing connectivity used by other services.
    /// `eligible_interfaces()` honors this state for the next stream start.
    pub fn set_interface_enabled(&self, name: &str, enabled: bool) -> bool {
        let snapshot = {
            let mut map = self.interface_enabled.lock().unwrap();
            map.insert(name.to_string(), enabled);
            map.clone()
        };
        let path = interface_state_file();
        if let Err(e) = serde_json::to_string_pretty(&snapshot)
            .map_err(std::io::Error::other)
            .and_then(|json| std::fs::write(&path, json))
        {
            tracing::warn!(error = %e, path = %path, "failed to persist interface admin state");
        }
        true
    }

    /// Interfaces eligible to carry bonded links for the NEXT stream start:
    /// admin-enabled, OS-connected, and holding a default route. Sorted by
    /// name for deterministic link ordering.
    pub fn eligible_interfaces(&self) -> Vec<String> {
        let enabled_map = self.interface_enabled.lock().unwrap();
        let mut names: Vec<String> = scan_network_interfaces()
            .into_iter()
            .filter(|i| {
                i.state == InterfaceState::Connected
                    && i.has_default_route
                    && *enabled_map.get(&i.name).unwrap_or(&true)
            })
            .map(|i| i.name)
            .collect();
        names.sort();
        names
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

// ── Real hardware scanning helpers ──────────────────────────────────

/// Drivers that mark a netdev as a USB modem regardless of its name —
/// HiLink sticks enumerate as `eth0`/`enx…` with these.
const MODEM_DRIVERS: &[&str] = &[
    "cdc_ether",
    "cdc_ncm",
    "huawei_cdc_ncm",
    "rndis_host",
    "qmi_wwan",
    "cdc_mbim",
    "sierra_net",
];

pub(crate) fn scan_network_interfaces() -> Vec<NetworkInterface> {
    let mut interfaces = Vec::new();

    // Read /sys/class/net/ for interface enumeration
    let net_dir = match std::fs::read_dir("/sys/class/net") {
        Ok(d) => d,
        Err(_) => return interfaces,
    };

    let default_routes = read_default_routes();

    for entry in net_dir.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name == "lo" {
            continue;
        }

        // Name-prefix guess, refined below by driver/product.
        let mut iface_type = if name.starts_with("wwan") {
            InterfaceType::Cellular
        } else if name.starts_with("wlp") || name.starts_with("wlan") {
            InterfaceType::Wifi
        } else if name.starts_with("eth") || name.starts_with("en") {
            InterfaceType::Ethernet
        } else {
            continue; // skip docker, veth, etc.
        };

        let driver = read_driver(&name);
        let bus = read_bus(&name);
        let product = read_usb_product(&name);

        // A USB NIC with a CDC/modem-class driver is a cellular modem no
        // matter what the kernel named it (HiLink sticks come up as eth0).
        let looks_like_modem = driver
            .as_deref()
            .map(|d| MODEM_DRIVERS.contains(&d))
            .unwrap_or(false)
            || product
                .as_deref()
                .map(|p| {
                    let p = p.to_lowercase();
                    p.contains("huawei") || p.contains("modem") || p.contains("mobile")
                })
                .unwrap_or(false);
        if looks_like_modem {
            iface_type = InterfaceType::Cellular;
        }

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

        let (ip, subnet) = read_interface_ip(&name);
        let gateway = default_routes.get(&name).cloned();

        interfaces.push(NetworkInterface {
            name,
            iface_type,
            state,
            enabled: true, // will be overridden by scan_real
            ip,
            carrier: None, // filled by the HiLink probe in scan_real
            signal_dbm: None,
            technology: None,
            band: None,
            cell_id: None,
            data_cap_mb: None,
            data_used_mb: None,
            priority: 1,
            apn: None,
            sim_pin: None,
            roaming: false,
            driver,
            bus,
            product,
            subnet,
            has_default_route: gateway.is_some(),
            gateway,
        });
    }

    // Sort by name for deterministic ordering (eth0, eth1, eth2, ...)
    interfaces.sort_by(|a, b| a.name.cmp(&b.name));
    interfaces
}

/// Kernel driver bound to the interface's device (e.g. "cdc_ether", "r8169").
fn read_driver(name: &str) -> Option<String> {
    std::fs::read_link(format!("/sys/class/net/{name}/device/driver"))
        .ok()?
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
}

/// Which bus the device hangs off: "usb", "pci", or "platform".
fn read_bus(name: &str) -> Option<String> {
    let real = std::fs::canonicalize(format!("/sys/class/net/{name}/device")).ok()?;
    let path = real.to_string_lossy();
    // USB devices sit under a PCI/platform host controller, so check usb first.
    if path.contains("/usb") {
        Some("usb".into())
    } else if path.contains("/pci") {
        Some("pci".into())
    } else {
        Some("platform".into())
    }
}

/// USB manufacturer + product strings, when the netdev is a USB function
/// (its sysfs device dir is the interface; the parent holds the strings).
fn read_usb_product(name: &str) -> Option<String> {
    let device = std::fs::canonicalize(format!("/sys/class/net/{name}/device")).ok()?;
    let parent = device.parent()?;
    let read = |file: &str| {
        std::fs::read_to_string(parent.join(file))
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    match (read("manufacturer"), read("product")) {
        (Some(m), Some(p)) => Some(format!("{m} {p}")),
        (None, Some(p)) => Some(p),
        (Some(m), None) => Some(m),
        (None, None) => None,
    }
}

/// Map of interface name → gateway for every default route on the box.
fn read_default_routes() -> HashMap<String, String> {
    let mut routes = HashMap::new();
    let Ok(output) = std::process::Command::new("ip")
        .args(["-j", "route", "show", "default"])
        .output()
    else {
        return routes;
    };
    if !output.status.success() {
        return routes;
    }
    let Ok(json) = serde_json::from_slice::<serde_json::Value>(&output.stdout) else {
        return routes;
    };
    for route in json.as_array().into_iter().flatten() {
        if let (Some(dev), Some(gw)) = (
            route.get("dev").and_then(|v| v.as_str()),
            route.get("gateway").and_then(|v| v.as_str()),
        ) {
            routes.insert(dev.to_string(), gw.to_string());
        }
    }
    routes
}

/// Read the first IPv4 address (and its network in CIDR form) assigned to
/// a network interface. Uses `ip -j addr show dev <name>` for reliable parsing.
fn read_interface_ip(name: &str) -> (Option<String>, Option<String>) {
    let Ok(output) = std::process::Command::new("ip")
        .args(["-j", "addr", "show", "dev", name])
        .output()
    else {
        return (None, None);
    };
    if !output.status.success() {
        return (None, None);
    }

    let parsed: Option<(String, Option<String>)> = (|| {
        let json: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
        // ip -j returns an array of interface objects
        let addr_info = json.as_array()?.first()?.get("addr_info")?.as_array()?;

        // Find the first "inet" (IPv4) entry
        for addr in addr_info {
            if addr.get("family")?.as_str()? == "inet" {
                let local = addr.get("local")?.as_str()?.to_string();
                let subnet = addr
                    .get("prefixlen")
                    .and_then(|p| p.as_u64())
                    .and_then(|prefix| ipv4_network(&local, prefix as u32));
                return Some((local, subnet));
            }
        }
        None
    })();

    match parsed {
        Some((ip, subnet)) => (Some(ip), subnet),
        None => (None, None),
    }
}

/// Compute the IPv4 network in CIDR form, e.g. ("192.168.8.100", 24) →
/// "192.168.8.0/24".
fn ipv4_network(ip: &str, prefix: u32) -> Option<String> {
    if prefix > 32 {
        return None;
    }
    let addr: std::net::Ipv4Addr = ip.parse().ok()?;
    let mask = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    };
    let network = std::net::Ipv4Addr::from(u32::from(addr) & mask);
    Some(format!("{network}/{prefix}"))
}

// ── V4L2 capture-device detection ───────────────────────────────────

const VIDIOC_QUERYCAP: libc::c_ulong = 0x8068_5600; // _IOR('V', 0, struct v4l2_capability)
const V4L2_CAP_VIDEO_CAPTURE: u32 = 0x0000_0001;
const V4L2_CAP_DEVICE_CAPS: u32 = 0x8000_0000;

/// Whether a /dev/video* node is an actual video *capture* device.
///
/// USB cameras expose extra non-capture nodes (metadata on this rig's
/// FHD camera; RK3588 also has video-enc0/dec0 codec nodes) — offering
/// those in a picker produces "Device is not a capture device" pipeline
/// crashes. Checks V4L2_CAP_VIDEO_CAPTURE via VIDIOC_QUERYCAP.
pub(crate) fn is_capture_device(path: &str) -> bool {
    use std::os::fd::AsRawFd;
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    // struct v4l2_capability is 104 bytes:
    // driver[16] card[32] bus_info[32] version:u32 capabilities:u32
    // device_caps:u32 reserved[3]:u32
    let mut caps = [0u8; 104];
    // SAFETY: VIDIOC_QUERYCAP fills the passed buffer, sized to the full
    // v4l2_capability struct; the fd is owned and open for the call.
    let ret = unsafe { libc::ioctl(file.as_raw_fd(), VIDIOC_QUERYCAP, caps.as_mut_ptr()) };
    if ret != 0 {
        return false;
    }
    let capabilities = u32::from_ne_bytes(caps[84..88].try_into().unwrap());
    let device_caps = u32::from_ne_bytes(caps[88..92].try_into().unwrap());
    let effective = if capabilities & V4L2_CAP_DEVICE_CAPS != 0 {
        device_caps
    } else {
        capabilities
    };
    effective & V4L2_CAP_VIDEO_CAPTURE != 0
}

fn scan_media_inputs() -> Vec<MediaInput> {
    let mut inputs = Vec::new();

    // Always include the GStreamer test source — works everywhere,
    // no hardware required.  The pipeline uses videotestsrc + audiotestsrc.
    inputs.push(MediaInput {
        device: "test://smpte".into(),
        input_type: MediaInputType::Test,
        label: "GStreamer Test Source".into(),
        capabilities: vec![
            "1920x1080@30".into(),
            "1920x1080@60".into(),
            "1280x720@60".into(),
        ],
        status: MediaInputStatus::Available,
    });

    // Scan /dev/video* devices — capture-capable nodes only (metadata and
    // codec nodes crash the pipeline if selected as a source).
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
        if !is_capture_device(&device) {
            continue;
        }

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipv4_network_masks_correctly() {
        assert_eq!(
            ipv4_network("192.168.8.100", 24).as_deref(),
            Some("192.168.8.0/24")
        );
        assert_eq!(
            ipv4_network("10.20.30.40", 16).as_deref(),
            Some("10.20.0.0/16")
        );
        assert_eq!(ipv4_network("bogus", 24), None);
        assert_eq!(ipv4_network("1.2.3.4", 40), None);
    }

    #[test]
    fn non_video_path_is_not_capture() {
        assert!(!is_capture_device("/dev/null"));
        assert!(!is_capture_device("/nonexistent"));
    }
}
