# Strata

Bonded video transport over RIST for GStreamer, written in Rust.

Strata aggregates bandwidth across multiple unreliable network interfaces — cellular modems, WiFi, Ethernet, satellite — into a single resilient video stream. It is built as a pair of GStreamer elements (`rsristbondsink` / `rsristbondsrc`) backed by an intelligent scheduling core that handles load balancing, adaptive redundancy, congestion control, and seamless failover without requiring any changes to upstream or downstream pipeline elements.

The system is designed for field deployment on constrained hardware (e.g. Orange Pi with USB cellular modems) where link conditions are unpredictable and every bit of available bandwidth matters.

---

## Table of Contents

- [Architecture](#architecture)
- [Project Structure](#project-structure)
- [Prerequisites](#prerequisites)
- [Building](#building)
- [Quick Start](#quick-start)
- [Configuration Reference](#configuration-reference)
  - [Links](#links)
  - [Scheduler](#scheduler)
  - [Receiver](#receiver)
  - [Link Lifecycle](#link-lifecycle)
- [GStreamer Elements](#gstreamer-elements)
- [Integration Node (CLI)](#integration-node-cli)
- [Telemetry](#telemetry)
- [Testing](#testing)
- [Privileges](#privileges)
- [Performance Budgets](#performance-budgets)
- [Further Documentation](#further-documentation)
- [License](#license)

---

## Architecture

```
Sender                                                       Receiver
┌─────────────────────────────────────┐    ┌─────────────────────────────────────┐
│ GStreamer Pipeline                   │    │ GStreamer Pipeline                   │
│                                     │    │                                     │
│ videosrc -> encoder -> mpegtsmux ──>│    │<── tsdemux -> decoder -> display    │
│                    rsristbondsink    │    │    rsristbondsrc                    │
└──────────────┬──────────────────────┘    └──────────────┬──────────────────────┘
               │                                          │
       ┌───────┴───────┐                          ┌───────┴───────┐
       │ BondingRuntime │                          │ BondingReceiver│
       │  ┌───────────┐ │                          │  ┌───────────┐ │
       │  │  DWRR      │ │                          │  │ Reassembly│ │
       │  │ Scheduler  │ │                          │  │  Buffer   │ │
       │  └───────────┘ │                          │  └───────────┘ │
       └──┬─────┬───┬──┘                          └──┬─────┬───┬──┘
          │     │   │                                │     │   │
       ┌──┴─┐┌──┴─┐┌┴──┐                         ┌──┴─┐┌──┴─┐┌┴──┐
       │LTE ││WiFi││Eth│   ── UDP/RIST ──>        │LTE ││WiFi││Eth│
       └────┘└────┘└───┘                          └────┘└────┘└───┘
```

**Key design decisions:**

- **librist is demoted to a socket wrapper.** All scheduling, bonding, and retransmission intelligence lives in the Rust `BondingScheduler`. librist handles RTP framing and ARQ only.
- **Decoupled threading.** The GStreamer streaming thread never blocks on network I/O. Packets are handed off via a bounded crossbeam channel to a dedicated scheduler worker thread.
- **Content-aware dispatch.** The scheduler inspects GStreamer buffer flags to identify keyframes (broadcast to all links for reliability) and droppable frames (shed first during congestion).
- **Deficit Weighted Round Robin (DWRR).** Credit-based load balancing with predictive scoring, quality-aware credit accrual, and trend-based link selection. Not simple round-robin.

---

## Project Structure

```
crates/
  gst-rist-bonding/     GStreamer plugin (rsristbondsink, rsristbondsrc)
                         Produces libgstristbonding.so + integration_node binary
  rist-bonding-core/     Core bonding logic, scheduler, receiver, config parsing
                         No GStreamer dependency — usable standalone
  librist-sys/           FFI bindings to librist, built from vendor/librist via Meson
  rist-network-sim/      Linux netns + tc-netem network simulation for testing
vendor/
  librist/               librist source (git submodule)
docs/                    Operational documentation
```

---

## Prerequisites

**System packages** (Debian/Ubuntu):

```bash
sudo apt-get install -y \
  build-essential \
  meson ninja-build \
  libgstreamer1.0-dev \
  libgstreamer-plugins-base1.0-dev \
  gstreamer1.0-plugins-base \
  gstreamer1.0-plugins-good \
  gstreamer1.0-plugins-ugly \
  gstreamer1.0-x \
  libx264-dev \
  pkg-config
```

**Rust** (stable toolchain, 2021 edition):

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

**librist** is compiled from source automatically via the build script. The `vendor/librist` git submodule must be initialized:

```bash
git submodule update --init --recursive
```

---

## Building

```bash
# Debug build (faster compilation, includes debug symbols)
cargo build

# Release build (LTO enabled, single codegen unit, optimized)
cargo build --release

# Build only the GStreamer plugin
cargo build --release -p gst-rist-bonding
```

The GStreamer plugin shared library is produced at:
```
target/release/libgstristbonding.so
```

To make GStreamer discover the plugin, add it to the search path:
```bash
export GST_PLUGIN_PATH="$PWD/target/release:$GST_PLUGIN_PATH"

# Verify registration
gst-inspect-1.0 rsristbondsink
gst-inspect-1.0 rsristbondsrc
```

The standalone integration binary is at:
```
target/release/integration_node
```

---

## Quick Start

**Sender** — stream a test pattern over two bonded links:

```bash
gst-launch-1.0 \
  videotestsrc is-live=true ! \
  video/x-raw,width=1920,height=1080,framerate=30/1 ! \
  x264enc tune=zerolatency bitrate=3000 ! \
  mpegtsmux ! \
  rsristbondsink name=sink \
    sink.link_0::uri="rist://192.168.1.100:5000" \
    sink.link_1::uri="rist://10.0.0.100:5000"
```

**Receiver** — listen and display:

```bash
gst-launch-1.0 \
  rsristbondsrc links="rist://@0.0.0.0:5000" latency=100 ! \
  tsdemux ! h264parse ! avdec_h264 ! autovideosink
```

**Using TOML config** (recommended for production):

```bash
gst-launch-1.0 \
  videotestsrc is-live=true ! x264enc tune=zerolatency bitrate=3000 ! mpegtsmux ! \
  rsristbondsink config="$(cat config.toml)"
```

Where `config.toml` contains:

```toml
version = 1

[[links]]
id = 0
uri = "rist://192.168.1.100:5000"
interface = "wwan0"
recovery_maxbitrate = 20000
recovery_rtt_max = 800

[[links]]
id = 1
uri = "rist://10.0.0.100:5000"
interface = "wlan0"

[scheduler]
redundancy_enabled = true
critical_broadcast = true
failover_enabled = true
```

All fields except `version` and link `uri` are optional and fall back to sensible defaults.

---

## Configuration Reference

Configuration is provided as a TOML string via the `config` property on `rsristbondsink`. Every section and field is optional — omitted values resolve to the defaults shown below.

### Links

Each `[[links]]` entry defines a network path to the receiver.

```toml
[[links]]
id = 0                        # Unique link identifier (default: array index)
uri = "rist://10.0.0.1:5000"  # RIST destination URL (required)
interface = "wwan0"            # Bind to OS interface (optional)
recovery_maxbitrate = 100000   # RIST ARQ max bitrate, kbps (optional, librist default)
recovery_rtt_max = 500         # RIST ARQ max RTT, ms (optional, librist default)
recovery_reorder_buffer = 15   # RIST ARQ reorder buffer, ms (optional, librist default)
```

| Field | Type | Default | Description |
|---|---|---|---|
| `id` | integer | array index | Unique identifier for this link. Duplicate IDs are deduplicated (first wins). |
| `uri` | string | *required* | RIST URL. Sender: `rist://host:port`. Receiver: `rist://@0.0.0.0:port`. |
| `interface` | string | none | OS network interface to bind to (e.g. `wwan0`, `eth0`). Enables `SO_BINDTODEVICE`. Empty strings are treated as none. |
| `recovery_maxbitrate` | integer | librist default | Maximum bitrate in kbps that librist will use for retransmission requests. Useful for capping ARQ traffic on metered links. |
| `recovery_rtt_max` | integer | librist default | Maximum RTT in ms that librist will tolerate before considering packets unrecoverable. Increase for high-latency links (satellite, intercontinental). |
| `recovery_reorder_buffer` | integer | librist default | Reorder buffer window in ms. Increase if the link reorders packets aggressively. |

### Scheduler

The `[scheduler]` section controls the bonding brain: load balancing, redundancy, failover, congestion control, and internal tuning.

```toml
[scheduler]
# --- Feature Toggles ---
redundancy_enabled = true       # Adaptive packet duplication when spare capacity exists
critical_broadcast = true       # Broadcast keyframes/headers to ALL alive links
failover_enabled = true         # Fast failover on RTT spikes

# --- Redundancy Tuning ---
redundancy_spare_ratio = 0.5    # Spare capacity ratio to trigger duplication (0.0–1.0)
redundancy_max_packet_bytes = 10000  # Packets larger than this are never duplicated
redundancy_target_links = 2     # How many links to send duplicates to (min: 1)

# --- Failover Tuning ---
failover_duration_ms = 3000     # How long to broadcast after a failover trigger
failover_rtt_spike_factor = 3.0 # RTT must exceed baseline * this factor to trigger

# --- Congestion Control ---
congestion_headroom_ratio = 0.85  # Recommended bitrate = capacity * this
congestion_trigger_ratio = 0.90   # Trigger congestion msg when observed > capacity * this

# --- Link Quality Estimation ---
ewma_alpha = 0.125              # EWMA smoothing factor (higher = more reactive, noisier)
prediction_horizon_s = 0.5      # Seconds ahead to project link quality trends
capacity_floor_bps = 5000000.0  # Minimum assumed capacity for new/unknown links (bps)
penalty_decay = 0.7             # Multiplier applied to link capacity on quality drops
penalty_recovery = 0.05         # Recovery rate per refresh after penalty

# --- Receiver Buffer ---
jitter_latency_multiplier = 4.0 # Adaptive buffer latency = p95_jitter * this
max_latency_ms = 500            # Hard ceiling on adaptive reassembly latency

# --- Runtime ---
stats_interval_ms = 1000        # How often to emit stats on GStreamer bus (min: 100)
channel_capacity = 1000         # Bounded channel depth between GStreamer and scheduler (min: 16)
```

Full reference:

| Field | Type | Default | Range | Description |
|---|---|---|---|---|
| `redundancy_enabled` | bool | `true` | — | Master toggle for adaptive packet duplication. When spare capacity exists across links, non-critical packets are duplicated for reliability. Disable for pure load-balancing without duplication. |
| `redundancy_spare_ratio` | float | `0.5` | 0.0–1.0 | Minimum ratio of spare-to-total capacity required before duplication activates. Lower values duplicate more aggressively. |
| `redundancy_max_packet_bytes` | integer | `10000` | — | Packets larger than this (bytes) are never duplicated regardless of spare capacity. Prevents bandwidth waste on large payloads. |
| `redundancy_target_links` | integer | `2` | min 1 | Number of additional links to send duplicate packets to. |
| `critical_broadcast` | bool | `true` | — | When enabled, keyframes (IDR), stream headers, and audio packets are broadcast to all alive links for maximum reliability. Disable only if bandwidth is extremely constrained. |
| `failover_enabled` | bool | `true` | — | When a link's RTT spikes above `failover_rtt_spike_factor` times its baseline, temporarily broadcast all packets to all links for `failover_duration_ms`. |
| `failover_duration_ms` | integer | `3000` | — | Duration in ms to maintain failover broadcast mode after trigger. |
| `failover_rtt_spike_factor` | float | `3.0` | — | RTT spike detection threshold. A link's current RTT must exceed its smoothed baseline by this factor to trigger failover. |
| `congestion_headroom_ratio` | float | `0.85` | 0.0–1.0 | When congestion is detected, the recommended encoder bitrate is `total_capacity * this`. |
| `congestion_trigger_ratio` | float | `0.90` | 0.0–1.0 | Congestion is signaled when `observed_bps > total_capacity * this`. |
| `ewma_alpha` | float | `0.125` | 0.001–1.0 | Smoothing factor for Exponentially Weighted Moving Average on link stats (RTT, bandwidth, loss). Higher values react faster to changes but are noisier. |
| `prediction_horizon_s` | float | `0.5` | — | How far ahead (seconds) the scheduler projects bandwidth trends when scoring links. |
| `capacity_floor_bps` | float | `5000000` | — | Bootstrap capacity assumed for links that haven't reported stats yet (bps). |
| `penalty_decay` | float | `0.7` | 0.0–1.0 | When a link degrades, its effective capacity is multiplied by this factor. |
| `penalty_recovery` | float | `0.05` | 0.0–1.0 | Per stats-refresh recovery rate for penalized links. |
| `jitter_latency_multiplier` | float | `4.0` | — | Receiver-side adaptive buffer latency is calculated as `p95_jitter * this`. |
| `max_latency_ms` | integer | `500` | — | Hard ceiling on the adaptive reassembly buffer latency (ms). |
| `stats_interval_ms` | integer | `1000` | min 100 | How often the sink emits `rist-bonding-stats` messages on the GStreamer bus. |
| `channel_capacity` | integer | `1000` | min 16 | Depth of the bounded channel between the GStreamer thread and the scheduler worker. If the channel fills, droppable frames are shed first. |

### Receiver

The `[receiver]` section configures the reassembly buffer on the receiving side.

```toml
[receiver]
start_latency_ms = 50     # Initial playout latency before adaptive kicks in
buffer_capacity = 2048     # Max packets held in the reorder buffer
skip_after_ms = 30         # Aggressively release head-of-line after this gap (optional)
```

| Field | Type | Default | Description |
|---|---|---|---|
| `start_latency_ms` | integer | `50` | Initial reassembly buffer latency in ms. The adaptive algorithm adjusts from this baseline. |
| `buffer_capacity` | integer | `2048` (min 16) | Maximum number of packets the reorder buffer will hold. |
| `skip_after_ms` | integer | none | If set, packets blocked at the head of the reorder buffer for longer than this are released even if gaps remain. Reduces latency at the cost of potential discontinuities. |

### Link Lifecycle

The `[lifecycle]` section controls the state machine that governs how links are promoted, demoted, and recovered. Each link transitions through phases: **Init** -> **Probe** -> **Warm** -> **Live** -> **Degrade** -> **Cooldown** -> **Reset**.

```toml
[lifecycle]
good_loss_rate_max = 0.2       # Max loss rate (0.0–1.0) still considered "good"
good_rtt_ms_min = 1.0          # Minimum RTT (ms) for a stats report to count as "good"
good_capacity_bps_min = 1.0    # Minimum capacity (bps) for "good"
stats_fresh_ms = 1500          # Stats younger than this are "fresh"
stats_stale_ms = 3000          # Stats older than this trigger Reset
probe_to_warm_good = 3         # Consecutive good reports to promote Probe -> Warm
warm_to_live_good = 10         # Consecutive good reports to promote Warm -> Live
warm_to_degrade_bad = 3        # Consecutive bad reports to demote Warm -> Degrade
live_to_degrade_bad = 3        # Consecutive bad reports to demote Live -> Degrade
degrade_to_warm_good = 5       # Consecutive good reports to recover Degrade -> Warm
degrade_to_cooldown_bad = 10   # Consecutive bad reports to demote Degrade -> Cooldown
cooldown_ms = 2000             # Time spent in Cooldown before re-probing
```

**Link phase diagram:**

```
Init ──(fresh stats)──> Probe ──(3 good)──> Warm ──(10 good)──> Live
                          |                   |                    |
                       (stale)             (3 bad)              (3 bad)
                          |                   |                    |
                          v                   v                    v
                        Reset <──(cooldown)── Cooldown <──(10 bad) Degrade
                          ^                                   |
                          └────────────(stale stats)──────────┘
                                                              |
                                              (5 good)────> Warm
```

Links in **Init**, **Cooldown**, or **Reset** do not carry traffic. Links in **Probe** or **Warm** carry traffic with conservative capacity estimates. Only **Live** links are considered at full capacity.

---

## GStreamer Elements

### rsristbondsink

Sender element. Accepts any stream (typically MPEG-TS) and distributes packets across bonded RIST links.

**Properties:**

| Property | Type | Mutability | Description |
|---|---|---|---|
| `config` | string | ready | TOML configuration string (see [Configuration Reference](#configuration-reference)) |
| `links` | string | ready | *(Deprecated)* Comma-separated RIST URLs. Use `config` instead. |

**Pad templates:**

| Pad | Direction | Presence | Caps |
|---|---|---|---|
| `sink` | Sink | Always | Any |
| `link_%u` | Src | Request | `meta/x-rist-config` |

Request pads expose a `uri` property to set the RIST destination URL per-link. This is an alternative to defining links in the TOML config.

**Bus messages emitted:**
- `rist-bonding-stats` — periodic per-link telemetry (see [Telemetry](#telemetry))
- `congestion-control` — emitted when aggregate throughput approaches total capacity, carries a `recommended-bitrate` field (bps) for upstream encoder adjustment

### rsristbondsrc

Receiver element. Listens on one or more RIST bind addresses, reassembles packets, and pushes the reordered stream downstream.

**Properties:**

| Property | Type | Default | Description |
|---|---|---|---|
| `links` | string | `""` | Comma-separated RIST bind URLs (e.g. `rist://@0.0.0.0:5000`) |
| `latency` | uint | `100` | Reassembly buffer latency in ms |

**Pad templates:**

| Pad | Direction | Presence | Caps |
|---|---|---|---|
| `src` | Src | Always | Any |

---

## Integration Node (CLI)

The `integration_node` binary provides a ready-made sender/receiver for testing and production use without writing GStreamer pipeline code.

```bash
# Build
cargo build --release -p gst-rist-bonding --bin integration_node

# Run sender
./target/release/integration_node sender \
  --dest "rist://10.0.0.1:5000,rist://10.0.0.2:5000" \
  --bitrate 3000 \
  --config bonding.toml \
  --stats-dest 127.0.0.1:9000

# Run receiver
./target/release/integration_node receiver \
  --bind "rist://@0.0.0.0:5000" \
  --output recording.ts
```

**Sender flags:**

| Flag | Required | Default | Description |
|---|---|---|---|
| `--dest <urls>` | yes | — | Comma-separated RIST destination URLs |
| `--bitrate <kbps>` | no | `3000` | Encoder bitrate in kbps |
| `--config <path>` | no | — | Path to TOML config file |
| `--stats-dest <host:port>` | no | — | UDP endpoint to relay JSON stats |

The sender generates a 1080p60 SMPTE test pattern, encodes with x264 in zerolatency mode, and muxes to MPEG-TS. When a `congestion-control` message arrives, it dynamically adjusts the encoder bitrate (floor: 500 kbps).

**Receiver flags:**

| Flag | Required | Default | Description |
|---|---|---|---|
| `--bind <urls>` | yes | — | Comma-separated RIST bind URLs |
| `--output <path>` | no | — | File path for recording. `.ts` = raw TS dump; other extensions trigger remux to MP4. |
| `--config <path>` | no | — | Path to TOML config file (reserved for future use) |

The receiver supports graceful shutdown via Ctrl+C (sends EOS to flush the pipeline).

---

## Telemetry

The sink emits `rist-bonding-stats` messages on the GStreamer bus at the interval defined by `stats_interval_ms` (default: 1 second).

**Message fields:**

| Field | Type | Description |
|---|---|---|
| `schema_version` | i32 | Always `1` |
| `stats_seq` | u64 | Monotonically increasing sequence number |
| `heartbeat` | bool | Always `true` |
| `mono_time_ns` | u64 | Nanoseconds since sink start (monotonic clock) |
| `wall_time_ms` | u64 | Unix epoch milliseconds |
| `total_capacity` | f64 | Sum of alive link capacities (bps) |
| `alive_links` | u64 | Number of links in a traffic-carrying phase |

**Per-link fields** (repeated for each link N):

| Field | Type | Description |
|---|---|---|
| `link_N_rtt` | f64 | Smoothed round-trip time (ms) |
| `link_N_capacity` | f64 | Estimated capacity (bps) |
| `link_N_loss` | f64 | Smoothed loss rate (0.0–1.0) |
| `link_N_observed_bps` | f64 | Actual measured throughput (bps) |
| `link_N_observed_bytes` | u64 | Cumulative bytes sent |
| `link_N_alive` | bool | Whether the link is carrying traffic |
| `link_N_phase` | string | Lifecycle phase: Init, Probe, Warm, Live, Degrade, Cooldown, Reset |
| `link_N_os_up` | i32 | OS operstate: 1 = up, 0 = down, -1 = unknown |
| `link_N_mtu` | i32 | Interface MTU or -1 if unknown |
| `link_N_iface` | string | OS interface name (empty if not bound) |
| `link_N_kind` | string | Inferred link type: `cellular`, `wifi`, `wired`, `loopback`, or empty |

When stats are relayed via `--stats-dest` in the integration node, they are converted to JSON with a `links` object keyed by link ID.

---

## Testing

```bash
# Unit tests (no special privileges required)
cargo test -p rist-bonding-core --lib
cargo test -p gst-rist-bonding --lib

# All unit tests across workspace
cargo test --lib

# Integration tests (require CAP_NET_ADMIN or sudo for netns)
sudo cargo test -p gst-rist-bonding --test end_to_end
sudo cargo test -p gst-rist-bonding --test impaired_e2e
sudo cargo test -p gst-rist-bonding --test robustness
sudo cargo test -p gst-rist-bonding --test stats_accuracy
```

**Test suites:**

| Test | What it validates | Requires sudo |
|---|---|---|
| Unit tests (`--lib`) | Config parsing, DWRR scheduling, EWMA smoothing, lifecycle state machine, link metrics, reassembly buffer, congestion control | No |
| `end_to_end` | Full sender-to-receiver bonded transmission over netns veth pairs | Yes |
| `impaired_e2e` | Bonding under simulated impairments (jitter, loss, rate limiting via tc-netem); generates plots | Yes |
| `robustness` | Multi-link race-car scenarios with primary + backup links | Yes |
| `stats_accuracy` | Validates telemetry accuracy for cellular-like link conditions | Yes |

The `rist-network-sim` crate creates Linux network namespaces and veth pairs to simulate isolated multi-link topologies. Impairments are applied via `tc qdisc add ... netem`. Tests that require netns skip gracefully when run without sufficient privileges.

---

## Privileges

| Component | Required Privileges |
|---|---|
| `rsristbondsink` / `rsristbondsrc` | None (user-level) |
| `rist-bonding-core` | None (user-level) |
| `integration_node` | None (user-level) |
| `rist-network-sim` | CAP_NET_ADMIN or sudo |
| Integration tests (netns-based) | CAP_NET_ADMIN or sudo |

Production deployments run entirely at user level. Elevated privileges are only needed for the network simulation test infrastructure.

---

## Performance Budgets

| Metric | Target |
|---|---|
| CPU | ≤ 1 core per stream at 30 fps; ≤ 70% on a 4-core host |
| Memory | ≤ 64 MB per stream; ≤ 512 MB total |
| End-to-end latency | ≤ 200 ms |
| p95 jitter impact | ≤ 50 ms above target latency |

See [docs/perf_budgets.md](docs/perf_budgets.md) for detailed breakdowns.

---

## Further Documentation

| Document | Description |
|---|---|
| [SPECIFICATION.md](SPECIFICATION.md) | System architecture specification and design rationale |
| [docs/production_plan.md](docs/production_plan.md) | Milestone tracking and release checklist |
| [docs/ops_playbook.md](docs/ops_playbook.md) | Operational triage guide: link oscillation, low throughput, high latency |
| [docs/config_migration.md](docs/config_migration.md) | Config schema versioning and migration path |
| [docs/regression_thresholds.md](docs/regression_thresholds.md) | Regression test pass/fail thresholds |
| [docs/perf_budgets.md](docs/perf_budgets.md) | CPU, memory, and latency budgets |
| [docs/privileges.md](docs/privileges.md) | Privilege matrix per component |

---

## License

LGPL. See [vendor/librist/COPYING](vendor/librist/COPYING) for librist's license terms.
