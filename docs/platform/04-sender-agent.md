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

## 9. Future Extensions

| Feature | Description | Priority |
|---|---|---|
| **USB hotplug** | Detect modem/camera plug/unplug via udev, auto-update status | Medium |
| **Preview stream** | Low-bitrate MJPEG preview via HTTP for the dashboard | Medium |
| **Local recording** | Record to SD card simultaneously while streaming | Low |
| **Multi-stream** | Run multiple pipelines to different receivers | Low |
| **GPS location** | Read GPS from modem for field mapping in dashboard | Low |
| **Audio-only mode** | Stream audio without video (radio/podcast use case) | Low |
