# Sender Agent Design

> **Status:** Draft spec for future implementation.

---

## 1. Overview

The sender agent (`strata-agent`) is a lightweight daemon that runs on each field
device (Orange Pi 5 Plus). It bridges the gap between the raw GStreamer bonding
pipeline and the managed cloud platform.

```
┌──────────────────────────────────────────────────────┐
│                   Orange Pi 5 Plus                    │
│                                                      │
│   ┌──────────────────────────────────────────┐       │
│   │           strata-agent daemon             │       │
│   │                                          │       │
│   │  ┌────────────┐   ┌──────────────────┐   │       │
│   │  │  Hardware   │   │  Control         │   │       │
│   │  │  Scanner    │   │  Channel (WSS)   │───┼──► Cloud
│   │  │ - V4L2      │   │                  │   │       │
│   │  │ - /proc/net │   │  - Auth          │   │       │
│   │  │ - ModemMgr  │   │  - Heartbeat     │   │       │
│   │  └──────┬─────┘   │  - Commands       │   │       │
│   │         │          └──────────────────┘   │       │
│   │         │                                 │       │
│   │  ┌──────┴─────────────────────────────┐   │       │
│   │  │  Pipeline Manager                   │   │       │
│   │  │                                     │   │       │
│   │  │  Spawns/stops GStreamer pipelines    │   │       │
│   │  │  via integration_node or direct API  │   │       │
│   │  │                                     │   │       │
│   │  │  ┌───────────────────────────────┐  │   │       │
│   │  │  │ integration_node sender       │  │   │       │
│   │  │  │  v4l2src → x264enc → mpegtsmux│  │   │       │
│   │  │  │  → rsristbondsink             │──┼───┼──► RIST UDP
│   │  │  └───────────────────────────────┘  │   │       │
│   │  └─────────────────────────────────────┘   │       │
│   └──────────────────────────────────────────┘       │
└──────────────────────────────────────────────────────┘
```

---

## 2. Responsibilities

| Function | Description |
|---|---|
| **Hardware discovery** | Enumerate V4L2 devices, network interfaces, saved media files |
| **Modem management** | Read signal strength, carrier info, connection state via ModemManager D-Bus |
| **Control channel** | Maintain persistent WSS connection to cloud control plane |
| **Pipeline lifecycle** | Spawn, monitor, and kill GStreamer pipelines on command |
| **Telemetry relay** | Forward bonding stats from the pipeline to the control plane |
| **Self-update** | Download new binary, verify checksum, swap and restart |
| **Watchdog** | Auto-restart pipeline on crash; report failures to control plane |

---

## 3. Hardware Scanner

### Network Interfaces

Scans every 10 seconds (or on netlink event):

```rust
// Pseudocode
fn scan_interfaces() -> Vec<NetworkInterface> {
    // 1. Read /sys/class/net/*/type to find all interfaces
    // 2. For each wwan* interface, query ModemManager via D-Bus:
    //    - mmcli -m <n> --output-json → carrier, signal, technology
    // 3. For each interface, read operstate, IP address
    // 4. Read /proc/net/wireless for signal_dbm (Wi-Fi)
    // 5. Return structured list
}
```

### Media Inputs

Scans on startup and on USB hotplug (via udev):

```rust
fn scan_media_inputs() -> Vec<MediaInput> {
    // 1. Enumerate /dev/video* devices
    // 2. For each, use v4l2-ctl --list-formats-ext to get capabilities
    // 3. Read device name from /sys/class/video4linux/videoN/name
    // 4. Scan configured media directory for saved files
    // 5. Return structured list
}
```

### Saved Media

A configured directory (default: `/home/strata/media/`) is scanned for media
files. The agent reports filename, size, and duration (via `gst-discoverer`).

---

## 4. Pipeline Manager

The agent spawns the GStreamer pipeline as a **child process** using the existing
`integration_node` binary. This provides clean process isolation — if the pipeline
crashes, the agent daemon survives and can restart it.

```rust
struct PipelineManager {
    child: Option<Child>,      // integration_node child process
    stream_id: Option<String>,
    config: StreamConfig,
}

impl PipelineManager {
    fn start(&mut self, config: StreamConfig) -> Result<()> {
        // 1. Write bonding config to /tmp/strata-stream-<id>.toml
        // 2. Spawn: integration_node sender
        //      --source <mode> --device <dev>
        //      --dest <uris>
        //      --bitrate <kbps>
        //      --config /tmp/strata-stream-<id>.toml
        //      --stats-dest 127.0.0.1:9100  (local UDP for agent to collect)
        // 3. Monitor child process health
    }

    fn stop(&mut self) -> Result<()> {
        // 1. Send SIGINT to child (triggers EOS → graceful shutdown)
        // 2. Wait up to 5s for clean exit
        // 3. If still alive, SIGKILL
        // 4. Clean up temp config file
    }
}
```

### Stats Collection

The agent listens on a local UDP port (127.0.0.1:9100) for JSON stats from the
pipeline's `--stats-dest` flag. It parses, enriches (adds interface signal info),
and forwards to the control plane over WSS.

### Auto-Restart

If the pipeline process exits unexpectedly:
1. Wait 2 seconds
2. Report `stream.ended` with `reason: "crash"` to control plane
3. Check if control plane wants a restart (via `stream.start` response)
4. If yes, restart with same config (up to 3 retries with backoff)

---

## 5. Configuration

Agent configuration file: `/etc/strata/agent.conf`

```toml
# Control plane endpoint
control_url = "wss://platform.example.com/agent/ws"

# Device identity (written during enrollment)
device_key_path = "/etc/strata/device.key"
sender_id = "snd_abc123"

# Media directory for saved files
media_dir = "/home/strata/media"

# Hardware scanning
scan_interval_s = 10
modem_query_enabled = true

# Telemetry
stats_interval_s = 1
stats_local_port = 9100

# Self-update
auto_update = true
update_check_interval_s = 3600

# Logging
log_level = "info"
log_file = "/var/log/strata-agent.log"
```

---

## 6. systemd Integration

The agent runs as a systemd service for reliability:

```ini
# /etc/systemd/system/strata-agent.service
[Unit]
Description=Strata Sender Agent
After=network-online.target ModemManager.service
Wants=network-online.target

[Service]
Type=simple
User=strata
ExecStart=/usr/local/bin/strata-agent --config /etc/strata/agent.conf
Restart=always
RestartSec=5
Environment=RUST_LOG=info

# Security hardening
ProtectSystem=strict
ReadWritePaths=/tmp /var/log/strata-agent.log /home/strata/media
AmbientCapabilities=CAP_NET_RAW
NoNewPrivileges=true

[Install]
WantedBy=multi-user.target
```

`CAP_NET_RAW` is needed for `SO_BINDTODEVICE` (interface binding). The service
runs as a dedicated `strata` user with minimal privileges.

---

## 7. State Machine

```
                    ┌──────┐
                    │ Boot │
                    └──┬───┘
                       │
                       ▼
                ┌──────────────┐
           ┌───►│  Connecting  │◄──── WSS reconnect
           │    └──────┬───────┘      (expo backoff)
           │           │ connected
           │           ▼
           │    ┌──────────────┐
           │    │ Authenticated│
           │    └──────┬───────┘
           │           │
           │           ▼
    WSS    │    ┌──────────────┐  stream.start   ┌───────────┐
    lost   ├────│    Idle      │────────────────►│ Streaming │
           │    └──────────────┘                 └─────┬─────┘
           │           ▲                               │
           │           │ stream.stop / stream ended     │
           │           └───────────────────────────────┘
           │
           │    (from any state except Boot)
           └────────────────────────────────
```

---

## 8. Dependencies

| Dependency | Purpose | Notes |
|---|---|---|
| `tokio` | Async runtime | WebSocket, timers, process management |
| `tokio-tungstenite` | WSS client | Control channel |
| `serde` / `serde_json` | Message serialisation | Already in workspace |
| `v4l2-sys` or `v4l` | V4L2 device enumeration | Or shell out to `v4l2-ctl` |
| `zbus` | D-Bus client for ModemManager | Optional — can shell out to `mmcli` instead |
| `ed25519-dalek` | Device key signing | Auth |
| `reqwest` | HTTPS for self-update download | |
| `sha2` | Binary verification | |

### Build Target

The agent must cross-compile to `aarch64-unknown-linux-gnu` using the existing
Docker cross-compile infrastructure.

---

## 9. AP Wi-Fi Onboarding (First-Time Setup)

A brand-new sender device has no internet connectivity and no cloud credentials.
The user needs a way to:

1. Connect to the device locally
2. Configure cellular modems (APN, etc.)
3. Link the device to their cloud account
4. Verify connectivity

The solution is a **captive-portal Wi-Fi access point** that the device broadcasts
on first boot (and whenever it has no cloud connection).

### Flow

```
┌─────────────┐              ┌──────────────────────┐
│ User's Phone│              │  Sender Device       │
│ or Laptop   │              │  (ROCK 5B+ / OPi5+)  │
│             │              │                      │
│  1. Connect to             │  AP: "Strata-XXXX"   │
│     Wi-Fi AP ──────────►   │  (hostapd, open or   │
│                            │   WPA2 w/ label PSK) │
│  2. Browser opens          │                      │
│     captive portal ◄────── │  dnsmasq + HTTP      │
│                            │  (redirect all DNS   │
│  3. See setup wizard:      │   to 10.42.0.1)      │
│     - Enter enrollment     │                      │
│       token (from cloud    │                      │
│       dashboard)           │                      │
│     - Configure APNs       │                      │
│     - See detected modems  │                      │
│       + signal strength    │                      │
│     - See HDMI input       │                      │
│     - Test connectivity    │                      │
│                            │                      │
│  4. Submit → device        │                      │
│     connects to cloud  ────┼──► WSS to platform   │
│                            │                      │
│  5. Success screen →       │  AP shuts down       │
│     "Device online in      │  (or stays up as     │
│      your dashboard"       │   management iface)  │
└─────────────┘              └──────────────────────┘
```

### Implementation

The onboarding portal is a **built-in HTTP server** in the agent (not a separate
service). It serves a small set of static HTML/JS pages and a REST API:

```
GET  /               → Setup wizard SPA (embedded in binary)
GET  /api/status     → { modems: [...], inputs: [...], connectivity: bool }
POST /api/enroll     → { enrollment_token: "enr_..." }  → triggers cloud enrollment
POST /api/modem/:id  → { apn: "...", pin: "..." }       → configure modem
POST /api/wifi       → { ssid: "...", password: "..." } → connect to external Wi-Fi
GET  /api/test       → { cloud_reachable: bool, latency_ms: 42 }
```

### AP Mode Management

The agent manages `hostapd` and `dnsmasq` via systemd or direct process control:

```rust
struct ApManager {
    hostapd_running: bool,
    interface: String,    // e.g. "wlan0"
}

impl ApManager {
    fn start_ap(&mut self) {
        // 1. Write /tmp/hostapd-strata.conf:
        //    interface=wlan0
        //    ssid=Strata-{last4_of_mac}
        //    channel=6
        //    wpa=2
        //    wpa_passphrase={printed_on_device_label}
        // 2. Assign static IP: ip addr add 10.42.0.1/24 dev wlan0
        // 3. Start hostapd
        // 4. Start dnsmasq (DHCP 10.42.0.10-50, DNS redirect to 10.42.0.1)
        // 5. Start captive portal HTTP server on 10.42.0.1:80
    }

    fn stop_ap(&mut self) {
        // Kill hostapd + dnsmasq, release IP
    }
}
```

### When AP Mode is Active

| Condition | AP State |
|---|---|
| First boot (no `/etc/strata/device.key`) | **ON** — waiting for enrollment |
| Enrolled but no internet connectivity | **ON** — fallback management access |
| Enrolled + cloud connected | **OFF** (default) or ON if `ap_always_on = true` in config |
| Cloud connection lost for >5 minutes | **ON** — allow local troubleshooting |

The AP SSID is derived from the device's MAC address: `Strata-A1B2` (last 4 hex
digits). The WPA2 passphrase is printed on a label on the device.

### Captive Portal Detection

iOS, Android, Windows, and macOS all detect captive portals by probing specific
URLs. The agent's DNS (dnsmasq) redirects ALL DNS queries to 10.42.0.1, and the
HTTP server responds to known captive portal probe URLs:

```
# Apple:   GET /hotspot-detect.html → 302 to http://10.42.0.1/
# Android: GET /generate_204        → 302 to http://10.42.0.1/
# Windows: GET /connecttest.txt     → 302 to http://10.42.0.1/
```

This causes the phone/laptop to automatically pop up the captive portal browser,
showing the setup wizard without the user needing to know the device's IP.

### Security

- The AP uses WPA2 with a device-specific passphrase (printed on label)
- The onboarding HTTP server only binds to the AP interface (10.42.0.1), never to
  cellular or ethernet interfaces
- Enrollment tokens are one-time-use, generated in the cloud dashboard
- After enrollment, the onboarding endpoint requires local auth (device passphrase)
  to prevent drive-by configuration changes

---

## 10. Future Extensions

| Feature | Description | Priority |
|---|---|---|
| **USB hotplug** | Detect modem/camera plug/unplug via udev, auto-update status | Medium |
| **Preview stream** | Low-bitrate MJPEG preview via HTTP for the dashboard | Medium |
| **Local recording** | Record to SD card simultaneously while streaming | Low |
| **Multi-stream** | Run multiple pipelines to different receivers | Low |
| **GPS location** | Read GPS from modem for field mapping in dashboard | Low |
| **Audio-only mode** | Stream audio without video (radio/podcast use case) | Low |
