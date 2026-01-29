# RIST Bonding Refactoring Plan (v2.0)

## Overview
Status: **Stage 2 (Prototyping)**
Goal: Move to **Stage 3 (Production Library)**

The current implementation proves the mathematical validity of the bonding algorithm but uses brittle configuration patterns ("string-based config"), inefficient threading (blocking locks), and ad-hoc telemetry (UDP side-channels). This plan outlines the steps to refactor the project into an idiomatic, high-performance GStreamer plugin.

---

## Phase 1: Configuration & Usability (The "GStreamer Way")
**Objective:** Replace the static `links="string"` property with dynamic Request Pads. This allows applications to add/remove links at runtime without stopping the pipeline.

### Steps
1.  **Implement Request Pads in `RsRistBondSink`**
    *   Define a new Pad subclass (e.g., `RsRistBondSinkPad`).
    *   Move the `uri` property from the Element to this Pad.
    *   Override `request_new_pad` in `sink.rs`.
    *   Override `release_pad` to handle clean teardown.
2.  **Update Scheduler Binding**
    *   When `request_new_pad` is called:
        *   Instantiate a new `Link` struct.
        *   Add it to the `BondingScheduler`.
        *   Map the Pad Index to the Link ID.
    *   When `release_pad` is called:
        *   Remove the corresponding Link from the `BondingScheduler`.
3.  **Deprecate String Property**
    *   Remove `links` property from `sink.rs`.
    *   Update `integration_node.rs` and tests to use the new pad-based API.

---

## Phase 2: The Control Loop (Real-time Feedback)
**Objective:** Close the loop. The bonding decision engine must utilize real-time network statistics (RTT, Loss, Jitter) returned by the receiver to adjust dispatch weights dynamically.

### Steps
1.  **Integrate RTCP / Receiver Feedback**
    *   Ensure the underlying transport (librist or UDP wrapper) exposes callback hooks for RTCP/NACK processing.
    *   If using purely custom UDP: implement a lightweight "Ack/Stats" packet sent from Receiver -> Sender at 1Hz-10Hz.
2.  **Wire Stats to Scheduler**
    *   Update `BondingScheduler` to accept `update_link_stats(id, metrics)`.
    *   Modify `Dwrr` (Deficit Weighted Round Robin) logic to recalculate weights immediately upon stats updates.
    *   Formula: `Weight = Capacity * (1 - LossRate)^4` (or similar aggressive backoff).
3.  **Validate with Netem**
    *   Use the `impaired_e2e.rs` harness.
    *   Assert that inducing 5% loss on Link A causes the Scheduler to shift traffic to Link B within < 2 seconds.

---

## Phase 3: Performance & Threading
**Objective:** Remove blocking locks from the hot path (`render()` function) to ensure the GStreamer pipeline never stalls due to network I/O.

### Steps
1.  **Architecture Change: Async Worker**
    *   Introduce an MPSC channel (e.g., `flume` or `crossbeam`) between the GStreamer `render()` thread and the Network I/O.
    *   `render()` pushes `bytes::Bytes` to the channel (zero-copy or shallow copy).
    *   Spawn a dedicated high-priority Worker Thread that consumes the channel and calls `sendto()`.
2.  **Lock Granularity Strategy**
    *   Replace `Mutex<BondingScheduler>` with finer-grained synchronization.
    *   Use `ArcSwap` for the "Active Links List" (Read-Heavy, Write-Rare).
    *   Use `AtomicU64` for Sequence Numbers.
3.  **Zero-Copy Optimizations**
    *   Investigate `gst::Buffer` mapping. Ensure we aren't performing deep copies of video payloads before sending.

---

## Phase 4: Observability (Standard Telemetry)
**Objective:** Remove custom UDP stats sockets and integrate proper GStreamer monitoring.

### Steps
1.  **Implement Standard Bus Messages**
    *   Create a structured `GstMessage` named `rist-stats`.
    *   Include fields: `rtt`, `loss`, `bitrate`, `retransmits` per link.
    *   Fire this message periodically (e.g., 1Hz) from the worker thread.
2.  **Remove Side-Channels**
    *   Delete the `stats_dest` UDP socket logic from `sink.rs`.
3.  **Update Test Harness**
    *   Update `impaired_e2e.rs` to capture `GstBus` messages instead of listening on a UDP socket.

---

## Dependency Updates
*   Evalute `librist-sys` usage: Are we wrapping the C library, or implementing RIST in pure Rust?
    *   *Decision needed*: If wrapping, ensure `stats_callback` is exposed. If pure Rust, implement RTCP parsing.

