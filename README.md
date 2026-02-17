<p align="center">
  <strong>Strata</strong><br/>
  <em>Open-source bonded cellular video transport — the $15,000 LiveU alternative, written in Rust.</em>
</p>

<p align="center">
  <a href="https://github.com/RephlexZero/strata/actions/workflows/ci.yml"><img src="https://github.com/RephlexZero/strata/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="https://github.com/RephlexZero/strata/actions/workflows/platform.yml"><img src="https://github.com/RephlexZero/strata/actions/workflows/platform.yml/badge.svg" alt="Platform CI"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-LGPL--2.1--or--later-blue.svg" alt="License"></a>
</p>

---

Strata bonds 2–6 unreliable network interfaces — USB cellular modems, WiFi, Ethernet, satellite — into a single resilient live video stream. It ships as a **GStreamer plugin** (`stratasink` / `stratasrc`), a standalone CLI (`strata-node`), and a complete **management platform** with a web dashboard, control plane API, and field-device agent.

Built for field deployment on commodity ARM64 hardware (Orange Pi 5 Plus, Raspberry Pi 5) with off-the-shelf USB modems. Pure Rust from the wire protocol up — no C transport dependencies, no vendor lock-in.

### Why Strata?

| Capability | LiveU | TVU | Dejero | SRT | RIST | **Strata** |
|---|---|---|---|---|---|---|
| N-link bonding | ✓ (6+) | ✓ (12) | ✓ (3-6) | Limited | Load share | **2-6 links** |
| Per-packet scheduling | ✓ | ✓ | ✓ | Round-robin | — | **IoDS / BLEST** |
| RF-aware routing | ✓ | ✓ | ✓ | ✗ | ✗ | **Biscay CC** |
| Predictive handover | ✓ | ✓ | ✓ | ✗ | ✗ | **Kalman + modem supervisor** |
| Adaptive FEC | Dynamic | RaptorQ | — | ✗ | ✗ | **TAROT cost function** |
| Media-aware priority | ✓ | ✓ | ✓ | ✗ | ✗ | **NAL classification** |
| Encoder feedback loop | ✓ | ✓ | ✓ | ✗ | TR-06-04 | **Built-in** |
| Fleet management | ✓ | ✓ | ✓ | ✗ | ✗ | **Web dashboard** |
| Open source | ✗ | ✗ | ✗ | ✓ | Spec only | **✓** |
| Price | $15K+ | $15K+ | $15K+ | Free | Free | **Free** |

---

## Quick Start

### Install a Release

Pre-built binaries for **x86_64** and **aarch64** Linux are on the [Releases](https://github.com/RephlexZero/strata/releases) page.

```bash
VERSION="v0.5.0"
ARCH="$(uname -m)"
curl -LO "https://github.com/RephlexZero/strata/releases/download/${VERSION}/strata-${VERSION}-${ARCH}-linux-gnu.so"
sudo cp strata-*-linux-gnu.so /usr/lib/${ARCH}-linux-gnu/gstreamer-1.0/libgststrata.so
gst-inspect-1.0 stratasink   # verify
```

Only GStreamer 1.x is needed at runtime — the transport is pure Rust with no C dependencies.

### Send and Receive

**Sender** — bonded stream over two links:

```bash
strata-node sender --source test --bitrate 3000 \
  --dest 192.168.1.100:5000,10.0.0.100:5000
```

**Receiver** — reassemble and relay to YouTube:

```bash
strata-node receiver --bind 0.0.0.0:5000,0.0.0.0:5002 \
  --relay-url "rtmp://a.rtmp.youtube.com/live2/YOUR_STREAM_KEY"
```

Or use GStreamer directly:

```bash
# Sender
gst-launch-1.0 videotestsrc is-live=true ! x264enc tune=zerolatency bitrate=3000 ! \
  mpegtsmux ! stratasink destinations="192.168.1.100:5000,10.0.0.100:5000"

# Receiver
gst-launch-1.0 stratasrc links="0.0.0.0:5000" latency=100 ! \
  tsdemux ! h264parse ! avdec_h264 ! autovideosink
```

### Run the Full Platform

```bash
docker compose up --build -d
# Dashboard:  http://localhost:3000  (dev@strata.local / development)
# Portal:     http://localhost:3001
```

This starts PostgreSQL, the control plane, a simulated sender with tc-netem cellular impairments across 3 isolated bridge networks, and a receiver — a complete end-to-end demo with realistic network conditions.

---

## Architecture

```
┌───────────────────────────────────────────────────────────────┐
│                         EDGE NODE                             │
│  ┌──────────┐   ┌────────────┐   ┌────────────┐   ┌─────────┐│
│  │ Encoder  │──▶│  Media     │──▶│  FEC       │──▶│ Network ││
│  │ (H.264/  │   │  Classifier│   │  Codec     │   │ Reactor ││
│  │  H.265/  │   │  (NAL      │   │  (XOR +    │   │ (per-   ││
│  │  AV1)    │   │   parse)   │   │   TAROT)   │   │  link)  ││
│  └──────────┘   └────────────┘   └────────────┘   └────┬────┘│
│  ┌─────────────────────────────────────────────────────┤     │
│  │              Bonding Scheduler                      │     │
│  │  ┌────────┐  ┌────────┐  ┌────────┐  ┌────────┐    │     │
│  │  │Link 1  │  │Link 2  │  │Link 3  │  │Link N  │    │     │
│  │  │DWRR Q  │  │DWRR Q  │  │DWRR Q  │  │DWRR Q  │    │     │
│  │  └───┬────┘  └───┬────┘  └───┬────┘  └───┬────┘    │     │
│  └──────┼───────────┼───────────┼───────────┼─────────┘     │
│  ┌──────▼───────────▼───────────▼───────────▼─────────┐     │
│  │           Modem Supervisor                          │     │
│  │  QMI/MBIM → RSRP, RSRQ, SINR, CQI per link        │     │
│  └─────────────────────────────────────────────────────┘     │
└───────────────────────────────────────────────────────────────┘
                              │ UDP × N links
                              ▼
┌────────────────────────────────────────────────────────────────┐
│                       CLOUD GATEWAY                            │
│  ┌──────────┐   ┌────────────┐   ┌────────────┐               │
│  │ Network  │──▶│  FEC       │──▶│  Jitter    │──▶ RTMP/SRT/ │
│  │ Receiver │   │  Decoder   │   │  Buffer    │   HLS/Record  │
│  └──────────┘   └────────────┘   └────────────┘               │
└────────────────────────────────────────────────────────────────┘
                              │
┌─────────────────────────────▼───────────────────────────────────┐
│                      CONTROL PLANE                              │
│  Web Dashboard (Leptos) · REST API (Axum) · Fleet Management    │
│  PostgreSQL · WebSocket Telemetry · Remote Config               │
└─────────────────────────────────────────────────────────────────┘
```

Strata is a **three-layer system**: a custom wire protocol (`strata-transport`), a multi-link bonding engine (`strata-bonding`), and a management platform. Each layer is a separate Rust crate.

### Transport Protocol (`strata-transport`)

A custom UDP protocol purpose-built for bonded video, replacing RIST/SRT:

- **Custom wire format** — 12-byte header with QUIC-style VarInt sequence numbers (62-bit space), media-aware flags (keyframe, codec config, fragment markers)
- **Hybrid FEC + ARQ** — systematic XOR-based FEC with NACK-triggered coded repair; TAROT cost function auto-tunes FEC rate per link
- **Biscay congestion control** — BBRv3 base with cellular radio feed-forward (SINR→capacity ceiling, CQI derivative tracking, handover detection)
- **Session management** — handshake, keepalive, link join/leave, RTT tracking (RFC 6298 SRTT/RTTVAR)

### Bonding Engine (`strata-bonding`)

Multi-link scheduling and orchestration:

- **DWRR scheduler** — per-link Deficit Weighted Round Robin queues with capacity-proportional weights
- **IoDS** — In-order Delivery Scheduler enforcing monotonic arrival constraint to minimize receiver reordering
- **BLEST** — Blocking estimation guard prevents head-of-line blocking on slow links
- **Thompson Sampling** — contextual bandit link selection with Beta distribution priors
- **Kalman filter** — smooths RTT/capacity estimates, tracks RSRP trend for handover prediction
- **Media awareness** — NAL unit parser (H.264/H.265/AV1) classifies packets by priority; keyframes broadcast to all links
- **Modem supervisor** — QMI/MBIM polling for RSRP, RSRQ, SINR, CQI; band management and link health scoring

### Management Platform

- **Control plane** (`strata-control`) — Axum REST API, WebSocket hubs for agents and dashboards, PostgreSQL, JWT auth
- **Operator dashboard** (`strata-dashboard`) — Leptos WASM SPA with live sender status, stream management, destination CRUD
- **Sender agent** (`strata-agent`) — field device daemon with hardware scanning, interface management, GStreamer pipeline lifecycle
- **Sender portal** (`strata-portal`) — local WASM UI for on-site enrollment, configuration, and diagnostics

---

## Project Structure

```
crates/
  strata-transport/      Custom wire protocol — FEC, ARQ, Biscay CC, session mgmt
  strata-bonding/        Bonding engine — DWRR/IoDS/BLEST scheduler, modem, media
  strata-gst/            GStreamer plugin (stratasink/stratasrc) + strata-node CLI
  strata-sim/            Network simulation — Linux netns + tc-netem
  strata-common/         Shared types, protocol messages, auth (JWT + ed25519)
  strata-control/        Control plane — Axum API, WebSocket, PostgreSQL
  strata-agent/          Sender agent daemon (field devices)
  strata-dashboard/      Operator dashboard — Leptos CSR WASM + Tailwind/DaisyUI
  strata-portal/         Field device portal — Leptos CSR WASM + Tailwind/DaisyUI
docker/
  Dockerfile.cross-aarch64   Cross-compile for Orange Pi / aarch64
docker-compose.yml           Full dev stack with simulated impaired networks
```

### Dependency Graph

```
strata-gst ──▶ strata-bonding ──▶ strata-transport
                     │
                     └──▶ strata-common

strata-control ──▶ strata-common
strata-agent   ──▶ strata-common
strata-dashboard ──▶ strata-common (types only)
```

---

## Development

### Dev Container (Recommended)

The fastest path — zero local setup:

1. Install the [Dev Containers](https://marketplace.visualstudio.com/items?itemName=ms-vscode-remote.remote-containers) extension (or open in Codespaces)
2. "Reopen in Container"
3. `cargo build`

Includes Rust, GStreamer dev libs, network tooling (`iproute2`, `tc`, `tcpdump`), and all build dependencies.

### Building

```bash
cargo build                          # Debug
cargo build --release                # Release (LTO)
cargo build --release -p strata-gst  # Plugin only
```

```bash
export GST_PLUGIN_PATH="$PWD/target/release:$GST_PLUGIN_PATH"
gst-inspect-1.0 stratasink
```

### Cross-compiling for aarch64

```bash
docker build -f docker/Dockerfile.cross-aarch64 -o dist/ .
# Output: dist/libgststrata.so (aarch64)
```

### Testing

```bash
cargo test --workspace --lib                          # Unit tests (no privileges)
cargo test -p strata-transport                        # Transport protocol tests
cargo test -p strata-common                           # Platform model + protocol tests
cargo test -p strata-control                          # API integration (needs PostgreSQL)
sudo cargo test -p strata-gst --test end_to_end       # Transport integration (needs NET_ADMIN)
sudo cargo test -p strata-gst --test video_output     # Produces reviewable MPEG-TS files
```

### Releasing

```bash
cargo release -p strata-gst patch --execute
```

GitHub Actions builds x86_64 + aarch64 and creates a release with `.so` assets.

---

## Documentation

Full documentation is in the **[Wiki](https://github.com/RephlexZero/strata/wiki)**.

| Page | Description |
|---|---|
| [Architecture](https://github.com/RephlexZero/strata/wiki/Architecture) | Transport protocol, bonding engine, scheduling algorithms, FEC/ARQ design |
| [Getting Started](https://github.com/RephlexZero/strata/wiki/Getting-Started) | Install, build, quick start, dev container, cross-compilation |
| [Strata Platform](https://github.com/RephlexZero/strata/wiki/Strata-Platform) | Control plane, dashboard, agent, portal — full platform guide |
| [Configuration Reference](https://github.com/RephlexZero/strata/wiki/Configuration-Reference) | Complete TOML config — links, scheduler, CC, FEC, lifecycle, receiver |
| [GStreamer Elements](https://github.com/RephlexZero/strata/wiki/GStreamer-Elements) | `stratasink` / `stratasrc` properties, pads, pipeline examples |
| [Strata Node CLI](https://github.com/RephlexZero/strata/wiki/Strata-Node) | `strata-node` sender/receiver usage, source hot-swap, RTMP relay |
| [Cellular Modem Setup](https://github.com/RephlexZero/strata/wiki/Cellular-Modem-Setup) | USB modem config, policy routing, band management |
| [Telemetry](https://github.com/RephlexZero/strata/wiki/Telemetry) | Stats schema, JSON relay, Prometheus metrics |
| [Testing](https://github.com/RephlexZero/strata/wiki/Testing) | Test matrix, simulation framework, CI workflows |
| [Deployment](https://github.com/RephlexZero/strata/wiki/Deployment) | Production setup, privileges, performance budgets, troubleshooting |

---

## License

[LGPL-2.1-or-later](LICENSE)
