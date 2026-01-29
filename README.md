# Strata: Reliable High-Performance RIST Bonding for GStreamer

**Strata** is a high-performance, bonded network transport solution built on the **Reliable Internet Stream Transport (RIST)** protocol. It is implemented as a set of **GStreamer plugins** in Rust, enabling resilient video transmission over multiple unreliable network links (e.g., Cellular/5G, Public Internet, Satellite).

This project goes beyond simple packet duplication by implementing an intelligent **Bonding Scheduler** capable of load balancing, active rate control, and seamless failover.

## 🚀 Key Features

*   **Dynamic Link Configuration**: Add or remove network links at runtime using GStreamer **Request Pads** (`link_0`, `link_1`, etc.).
*   **Async Threading Architecture**:
    *   Network I/O is decoupled from the GStreamer pipeline thread.
    *   Dedicated background workers handle packet scheduling, retransmission, and telemetry.
    *   Zero-copy packet passing using shared memory where possible.
*   **Intelligent Bonding**:
    *   **Round-Robin / Load Balance**: Distribute bitrate across multiple links.
    *   **Broadcast / Redundancy**: Duplicate critical packets for minimal latency.
*   **Comprehensive Telemetry**:
    *   Real-time statistics published to the GStreamer Bus.
    *   Metrics: RTT, Capacity, Packet Loss, Queue Depth per link.
*   **Simulation & Testing**:
    *   Includes a `rist-network-sim` crate for simulating network impairments (jitter, drop, latency) to validate bonding robustness.

## 📦 Project Structure

*   **`crates/gst-rist-bonding`**: The core GStreamer plugin (`rsristbondsink`, `rsristbondsrc`).
*   **`crates/rist-bonding-core`**: The bonding logic, scheduler, and protocol handling (agnostic of GStreamer).
*   **`crates/rist-network-sim`**: Network namespace-based simulation tools for integration testing.
*   **`crates/librist-sys`**: Low-level FFI bindings to `librist`.

## 🛠️ Building & Installation

### Prerequisites
*   Rust (stable)
*   GStreamer development libraries (`libgstreamer1.0-dev`, `libgstreamer-plugins-base1.0-dev`)
*   `librist` (will be built or linked)

### Build
```bash
cargo build --release -p gst-rist-bonding
```

### Run Tests
```bash
# Run unit and integration tests
cargo test -p gst-rist-bonding

# Run specific E2E visualization test with network impairment
cargo test -p gst-rist-bonding test_impaired_bonding_visualization
```

## 🔌 Usage

### Sender (Sink)
The sink element is `rsristbondsink`. Links are configured by requesting pads with the naming pattern `link_%u`.

**Example (CLI):**
```bash
# Configure two links: one to localhost:5000, one to localhost:6000
gst-launch-1.0 videotestsrc is-live=true ! x264enc tune=zerolatency ! mpegtsmux ! \
  rsristbondsink name=sink \
  sink.link_0::uri="rist://127.0.0.1:5000" \
  sink.link_1::uri="rist://127.0.0.1:6000"
```

### Receiver (Source)
The source element is `rsristbondsrc`.

**Example (CLI):**
```bash
# Receiver binding on port 5000 and 6000
gst-launch-1.0 rsristbondsrc links="rist://@0.0.0.0:5000,rist://@0.0.0.0:6000" ! \
  tsdemux ! h264parse ! avdec_h264 ! autovideosink
```

*(Note: The Source element is currently being updated to match the Sink's request pad architecture).*

## 📊 Telemetry

The sink element emits `rist-bonding-stats` messages on the GStreamer bus at 1Hz intervals:

```javascript
{
  "link_0_rtt": 45.0,        // ms
  "link_0_capacity": 5000000, // bps
  "link_0_loss": 0.01,       // 1%
  "link_0_alive": true
}
```

## 🔮 Roadmap

1.  **Phase 1 (Complete)**: Basic implementation, Request Pads for config, Async Worker.
2.  **Phase 2**: Adaptive Bitrate logic (sending `congestion-control` messages upstream).
3.  **Phase 3**: Advanced Scheduler (Weighted Round Robin based on RTT/Capacity).
4.  **Phase 4**: Production Hardening (Encryption, SRT interop).

See [EXECUTION_PLAN.md](EXECUTION_PLAN.md) for details.
