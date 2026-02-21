# Strata Web Interface Plan

This document outlines the requirements for making the `strata-dashboard` and `strata-portal` completely production-ready, bulletproof, and feature-rich. It exposes the deep physical-layer intelligence and transport-layer scheduling provided by the Rust backend.

## 1. Dashboard & Real-Time Monitoring (The "Glass-to-Glass" View)
The main dashboard must provide instant situational awareness for a broadcast operator.
*   [x] **Big Red "Go Live" Button:** Foolproof, single-click stream initiation and termination.
*   [x] **Aggregate Bandwidth Graph:** Real-time stacked area chart showing Tx/Rx throughput contributed by each individual link.
*   [x] **Glass-to-Glass Health:** Estimated end-to-end latency, overall packet loss (pre-FEC vs. post-FEC), and jitter buffer depth at the Cloud Gateway.
*   [x] **Stream Metadata:** Active resolution, framerate, codec (H.264/H.265), and current adaptive bitrate.
*   [x] **Degradation State Indicator:** Visual alerts showing if the system has entered graceful degradation (e.g., "Dropping B-Frames", "Encoder Bitrate Reduced", "Emergency Keyframe-Only Mode").

## 2. Link & Modem Management (The "Bonding" Core)
This is where Strata differentiates itself from standard SRT/RIST by exposing modem intelligence.
*   [x] **Per-Link RF Telemetry:** Real-time gauges for RSRP (Signal Power), RSRQ (Signal Quality), SINR (Interference), and CQI (Channel Quality Indicator).
*   [x] **Carrier & Connection Info:** Display ISP name, connection type (LTE, 5G NSA, 5G SA), active Band, and Cell ID.
*   [x] **Band Locking Controls:** UI to force specific modems onto specific bands (e.g., Modem A on Band 71, Modem B on Band 41) to prevent tower sector contention.
*   [x] **Link Hotplugging & State:** Visual indicators for link states (Probation → Alive → Dead) and udev hotplug events.
*   [x] **Data Cap Management:** Set monthly/daily data limits per SIM card with auto-disable thresholds to prevent cellular overage charges.
*   [x] **Link Prioritization:** Ability to set base weights or mark links as "Backup Only" (e.g., use expensive satellite/roaming data only if cellular/Wi-Fi fails).
*   [x] **APN & SIM Settings:** UI to configure custom APNs, SIM PINs, and roaming toggles.

## 3. Transport & Protocol Tuning (The "Strata" Engine)
Advanced controls for broadcast engineers to tune the Rust transport layer.
*   [x] **TAROT FEC Controls:** 
    *   [x] View the current dynamic FEC overhead ratio.
    *   [x] Sliders to tune the TAROT cost function weights (α for loss, β for overhead, γ for latency). (Implemented FEC overhead target)
    *   [x] Toggle Layer 1 (Sliding-Window RLNC) vs. Layer 1b (Unequal Error Protection / RaptorQ).
*   [x] **Biscay Congestion Control:** View BBRv3 state, estimated bottleneck bandwidth (BtlBw), and minimum RTT (RTprop) per link.
*   [x] **Scheduler Tuning (IoDS/BLEST):**
    *   [x] Adjust the BLEST Head-of-Line blocking threshold (e.g., 50ms).
    *   [x] Toggle Shared Bottleneck Detection (RFC 8382) to group links sharing the same tower backhaul.
    *   [x] View Thompson Sampling link preference scores in real-time.
    *   [x] Scheduler Mode and Capacity Floor controls.

## 4. Media & Stream Configuration (The "Video" Pipeline)
Controls for the `strata-gst` (GStreamer) integration and media awareness.
*   [x] **Input Selection:** Choose video source (SDI, HDMI, USB Camera, Test Pattern, or UDP ingest).
*   [x] **Encoder Settings:** Target bitrate, minimum/maximum bitrate bounds (for the BITRATE_CMD feedback loop), resolution, framerate, and GOP size/Keyframe interval.
*   [x] **Media Awareness Stats:** Counters showing NAL unit classification (Critical vs. Reference vs. Standard vs. Disposable packets sent/dropped).

## 5. Cloud Gateway & Destination Routing
Managing where the bonded stream is reassembled and sent.
*   [x] **Destination Management:** Add, edit, and remove output endpoints (YouTube Live RTMP, Twitch, Studio SRT decoder, NDI).
*   [x] **Multi-Destination Routing:** Toggles to fan-out the stream to multiple destinations simultaneously from the cloud gateway.
*   [x] **Receiver Jitter Buffer:** Controls to set the target jitter buffer size (static) or toggle the adaptive jitter buffer sizing.

## 6. Device & Fleet Management (Hardware Ops)
Essential for managing remote edge nodes (RPi5, Jetson) in the field.
*   [x] **System Health:** CPU usage, core temperatures (critical for enclosed edge devices), RAM usage, and uptime.
*   [x] **Power Controls:** Remote reboot, shutdown, and restart of the `strata-agent` service.
*   [x] **OTA Updates:** Interface to push firmware/software updates to the edge node.
*   [x] **Configuration Management:** Export/Import JSON or TOML configuration profiles (e.g., "Stadium Profile", "Rural Profile").

## 7. Diagnostics, Alerting & Telemetry (The "Bulletproof" Ops)
Tools to troubleshoot issues without needing SSH access.
*   [x] **Live Log Viewer:** Scrolling, filterable WebSocket logs for `strata-bonding`, `strata-gst`, and the Modem Supervisor.
*   [x] **Network Tools:** Web-based Ping, Traceroute, and single-link speed tests.
*   [x] **PCAP Generation:** A button to trigger and download a `.pcap` packet capture of the bonding interface for Wireshark analysis.
*   [x] **Alerting Rules:** Configure thresholds for visual/audio alerts (e.g., "Alert if aggregate capacity drops below 5 Mbps" or "Alert if PRE_HANDOVER state is triggered").

## 8. Security & Access Control
*   [x] **Role-Based Access Control (RBAC):** 
    *   *Admin:* Full system and protocol tuning.
    *   *Operator:* Can start/stop streams and change destinations.
    *   *Viewer:* Read-only access to telemetry.
*   [x] **Authentication:** JWT token management, password resets, and session revocation.
*   [ ] **Local Network Security:** HTTPS/TLS certificate management for the local portal, and basic firewall/port-forwarding rules if the device acts as a network gateway.

## 9. UI/UX & Field-Readiness
*   [x] **Mobile-First / Responsive:** Camera operators will often access the local portal via a smartphone browser while in the field. Large touch targets are mandatory.
*   [x] **Dark Mode:** Essential for low-light broadcast environments (concerts, theaters).
*   [x] **Sub-second Reactivity:** Use WebSockets (already planned via `strata-control`) to ensure metrics update at 10-30fps without page reloads.
*   [x] **Graceful Disconnect UI:** If the web UI loses connection to the control plane, it must clearly indicate "Offline" rather than showing stale data, and automatically reconnect when available.
