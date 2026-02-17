# Strata Bonded Transport System Specification

**Role:** Senior Systems Architect & Transport Engineer  
**Date:** January 26, 2026  
**Status:** Approved for Implementation

---

## 1. System Overview

### High-Level Architecture
The system is a userspace bonded video transport solution designed to aggregate bandwidth across multiple heterogeneous network interfaces (LTE, 5G, WiFi, Ethernet). It leverages GStreamer for the media pipeline and Rust for all control plane, scheduling, and IO aggregation logic.

**Core Philosophy:**
- **RFC Compatibility:** The "Bonding" is non-standard and implementation-specific. The "Transport" uses a custom pure-Rust protocol based on RTP.
- **Control Inversion:** `strata-transport` handles the network I/O. All intelligence (retransmission requests, scheduling, bonding) lives in the Rust `BondingScheduler`.

### End-to-End Dataflow

1.  **Media Source (GStreamer):**
    `Video Source` -> `Encoder (H.264)` -> `MPEG-TS Muxer` -> **`stratasink`**
2.  **Bonding Sender (Rust):**
    - `stratasink` receives GStreamer Buffers.
    - **Header Extension:** Adds a custom RTP header extension or payload wrapper containing a global sequence number for reassembly.
    - **Scheduler:** Decides which Link (Interface) to use based on current metrics.
    - **Dispatch:** Pushes packet to specific `strata-transport` instance bound to that interface.
3.  **Network Transmission:**
    - Packets travel over independent UDP flows.
    - `Link A (LTE)`: High latency, Medium Jitter.
    - `Link B (5G)`: Low latency, High burstiness.
    - `Link C (WiFi)`: Low loss, prone to contention.
4.  **Bonding Receiver (Rust):**
    - Multiple `strata-transport` instances receive packets.
    - **Aggregator:** Collects packets from all links.
    - **Reordering Buffer:** Uses the global sequence number to re-order packets.
    - **De-jitter:** Holds packets for a configurable duration.
    - **Output:** Pushes ordered stream to **`stratasrc`**.
5.  **Media Sink (GStreamer):**
    **`stratasrc`** -> `TS Demux` -> `Decoder` -> `Display`

---

## 2. Runtime Contexts

The system operates in a Hybrid Sync/Async model.

### 1. GStreamer Context (Sync)
- **Role:** Data ingest and output.
- **Thread:** GStreamer Streaming Thread.
- **Constraints:** Real-time sensitive. `chain()` functions must not block indefinitely.
- **Bridge:** Uses crossbeam channels or Tokio `mpsc` channels to hand off data to the Rust runtime.

### 2. Rust Connectivity Context (Async / Tokio)
- **Role:** Network I/O, Timer management, Stats collection.
- **Thread:** Dedicated `tokio` runtime thread pool (lazy_static or instance-owned).
- **Components:**
    - `SchedulerTask`: Event loop processing incoming packets and tick events.
    - `LinkTask(s)`: One per interface. Manages transport polling (via `poll` or async wrapper).

---

## 3. Component Breakdown

### A. `stratasink` (GStreamer Element)
- **Type:** `GstBin` or `GstBaseSink`
- **Inputs:** `video/mpegts` or `application/x-rtp` (System Stream).
- **Properties:**
    - `links`: List of interface binding configs (e.g., `eth0:192.168.1.5,wlan0:10.0.0.5`).
    - `destinations`: Target Peers.
- **Responsibility:**
    - Initialize the Rust Runtime.
    - Encapsulate incoming buffers with "Bonding Header" (Sequence ID).
    - Push to `Dispatch Channel`.

### B. `BondingScheduler` (Rust Crate)
- **Purpose:** The brain of the sender.
- **State:**
    - `link_table`: Map<InterfaceID, LinkState>.
    - `global_seq`: AtomicU64.
- **Traits:**
    - `TargetSelector`: Function to pick a link for a packet.
- **Inputs:** Raw Packets + Size.
- **Outputs:** Instructions to `LinkManager`.

### C. `LinkManager` & `TransportContext` (Rust Crate)
- **Purpose:** Transport layer for network I/O.
- **Ownership:** Owns the transport context.
- **cardinality:** 1 `LinkManager` = 1 Physical Interface = 1 transport context.
- **API:**
    - `send_data(buf)`: Non-blocking send.
    - `get_metrics()`: Return RTT, buffer bloat, lost packets.

### D. `stratasrc` (GStreamer Element)
- **Type:** `GstPushSrc`
- **Responsibility:**
    - Spawns `ReceiverAggregator`.
    - Pulls ordered buffers from `Output Channel`.
    - Handles "Gap Filling" (generating silence/null packets) if deadline is missed (optional).

### E. `ReceiverAggregator` (Rust Crate)
- **Purpose:** Re-ordering and De-jittering.
- **Data Structure:** `BTreeMap<SeqNum, Packet>` or specialized RingBuffer.
- **Logic:**
    - `input(packet)`: Insert into buffer.
    - `tick()`: Check head of line. If timestamp < `now - latency`, pop and push to GStreamer.
- **Advanced Features (Crucial):**
    - **Adaptive Reordering Window:** Dynamically scale the buffer depth based on measured jitter variance; do not rely on static latency.
    - **Discard Heuristics:** Aggressively drop "poison" packets (too late to show) to unblock the head-of-line.
    - **NACK Suppression:** Do not request retransmission for packets that will definitely arrive past the playout deadline.

> **Known Limitation — NACK Suppression:** The current implementation does not
> suppress automatic NACK (ARQ) requests for packets that have already been
> skipped past in the reassembly buffer. Because the transport layer manages
> retransmission internally per-context and does not yet expose a hook to
> filter individual NACK requests, implementing this requires manually
> managing sequence tracking at the bonding layer. Packets retransmitted
> after the playout deadline are correctly discarded by the receiver's
> late-packet detection, but the retransmission itself wastes bandwidth.
> This is tracked as a future optimization.

---

## 4. Scheduler Design

The scheduler relies on **Deficit Weighted Round Robin (DWRR)** with **Active Rate Control**.

### Packet Dispatch Algorithm
1.  **Metric Update:** Driven by continuous 100ms stats callback updates from `strata-transport`, smoothed using EWMA ($\alpha \approx 0.125$) to filter jitter.
2.  **Capacity Estimation:** Each link maintains an estimated `available_bitrate` (EWMA of measured throughput).
3.  **DWRR Mechanism:**
    - Each Link has a `credit` bucket.
    - **Quantum:** Periodically (per send loop), credits are added: $\Delta \text{credits} = \text{Capacity}_{\text{bps}} \times \Delta t_{\text{sec}}$.
    - **Selection:**
        - Iterate links in Round-Robin order.
        - If `Link.credits >= Packet.size`, send and deduct credits.
        - If packet cannot be sent, skip link (accumulate credits).
    - **Fallback:** If no link has sufficient credits, force send on the link with the highest credit balance (closest to positive) to minimize latency.
4.  **Liveness Detection:** Links are considered "Dead" if RTT is 0 (after startup grace period) or if marked down by `librist`. Dead links are skipped.

### Active Rate Control
The system implements a back-pressure loop to prevent buffer bloat.
- **Capacity Aggregation:** Sum of all *alive* link capacities.
- **Thresholding:** If $\text{TotalCapacity} < \text{TargetBitrate} \times 1.2$ (Headroom), the sink emits a `congestion-control` message.
- **Action:** The application layer catches this message and dynamically reconfigures the video encoder's bitrate (e.g., `x264enc` property `bitrate`).
### Dynamic Link Management
The scheduler must handle the runtime addition and removal of network interfaces (e.g., plugging in a USB Modem).

1.  **Link Discovery:**
    - The `BondingScheduler` exposes an API `add_link(config)` and `remove_link(id)`.
    - GStreamer Properties: `links` property can be updated at runtime.
2.  **Graceful Removal:**
    - When a link is marked for removal or detected as "Dead" (timeout), it is removed from the `candidates` pool.
    - In-flight packets on a dead link are considered lost (relying on RIST ARQ or FEC if configured, otherwise upper-layer Protocol ARQ).
3.  **Warm-up Phase:**
    - New links start with a conservative `capacity` estimate (e.g., lowest known valid bitrate) until RTT/Ack feedback normalizes.
---

## 5. RIST Integration Details

We use `librist` in **Raw Profile** or simple RTP mode.

- **Isolation:** We do NOT use `librist` groups. Each `LinkManager` creates a standalone `rist_ctx`. 
- **Binding:**
    - We use `SO_BINDTODEVICE` (on Linux) or specific IP binding to ensure traffic goes out the correct modem.
    - `librist` config `rist_peer_config.address` must be specific.
- **FFI Strategy:**
    - **`librist-sys`**: Generated via `bindgen` in the workspace.
    - Unsafe code is contained strictly within `src/net/rist_sys.rs`.
    - **Callbacks:** We register `connection_status_callback` and `stats_callback`. These callbacks bridge back to Rust channels.
    - **Hard Constraint:** `librist` callbacks operate on the protocol thread. **NO** logic is performed inside them. They must strictly perform a non-blocking channel send and return immediately.

---

## 6. Statistics & Observability

Observability is crucial for debugging bonding behavior.

**Per-Link Metrics:**
- `cwmd (Congestion Window)`
- `rtt (Round Trip Time)`
- `bitrate_sent` vs `bitrate_Acked`
- `retransmission_count`

**System Metrics:**
- `goodput`: Total unique bytes delivered.
- `bonding_overhead`: Headers + Duplicate packets.
- `late_packet_loss`: Packets arriving after playout deadline.

**Export:**
- Metrics are exposed via GStreamer "bus messages" (JSON payload) readable by the application handling the pipeline.

---

## 7. Network Simulation & Testing

### Architecture
- **Host:** Linux with Kernel > 5.10.
- **Environment:** Docker Containers with `--privileged` (required for Network Namespace manipulation) or `CAP_NET_ADMIN`.

### Simulation Topology
We simulate "Modems" programmatically using the internal **`rist-network-sim` crate**.
This crate interacts with the Linux Kernel Netlink API to create isolated Network Namespaces and `veth` pairs.

**Simulated Link Types:**
1.  **Ideal Link:** 1gbps, 0ms delay.
2.  **LTE-Like:** 20mbps limit, 50ms +- 20ms jitter, 1% packet loss (simulated via `tc-netem`).
3.  **Satellite-Like:** 5mbps limit, 600ms latency.

**Validation Strategy:**
- **Integration Test:** Rust test harness uses `rist-network-sim` to spin up environments.
- Connect via 3 programmably created `veth` pipes.
- Stream 10MB of data.
- **Assert:** Receiver hash matches Sender hash.
- **Assert:** Link Usage follows expected policy (Saturation of Link A before spilling to Link B).

---

## 10. Explicit Non-Goals

1.  **Librist Internal Bonding:** We will not use RIST's native bonding features (SMP).
2.  **Encryption:** Transport encryption is assumed to be handled by RIST encryption if enabled, but is not the focus of this scheduling implementation.
3.  **Windows Support:** The project targets Linux (Linux Network Namespaces are required for testing).
4.  **Non-MPEGTS Content:** The primary target is MPEG-TS.
