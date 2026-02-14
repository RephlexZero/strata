# Cellular Video Transport: Validation & Encoder Bitrate Signaling

> **Purpose**: Research summary and implementation plan for (a) testing throughput
> aggregation and bandwidth estimation over cellular links, and (b) RFC-based
> encoder bitrate signaling.  
> **Date**: 2026-01-30  
> **Prerequisite commit**: `0989ac0` — *feat: implement RFC-based congestion control improvements*

---

## 1  Current Architecture Overview

### 1.1  Congestion Control (NADA / RFC 8698)

The DWRR scheduler in `rist-bonding-core` already implements a per-link NADA
controller (RFC 8698 §4.2–4.3):

| Component | Location | Description |
|-----------|----------|-------------|
| RTT baseline | `dwrr.rs` L330–354 | Fast/slow min-window for `rtt_baseline` |
| Unified congestion signal | `dwrr.rs` L356–365 | `x_curr = d_queuing + DLOSS_REF·(p_loss/PLR_REF)²` |
| Accelerated ramp-up | `dwrr.rs` L399–413 | Multiplicative increase bounded by `γ ≤ qbound/(rtt·r_recv)` |
| PI-controller (gradual) | `dwrr.rs` L434–458 | `r_n *= 1 − κ·(δ/τ)·(x_offset + η·x_diff)` |
| SBD shared bottleneck | `sbd.rs` | RFC 8382 skewness-based detection, coupled α scaling |

### 1.2  Rate Signaling (Current)

The GStreamer sink element posts two message types from the stats thread
(`sink.rs` L495–540):

| Message | Field | Trigger | Value |
|---------|-------|---------|-------|
| `congestion-control` | `recommended-bitrate` (u64 bps) | `observed_bps > total_capacity × 0.85` | `total_capacity × 0.90` |
| `bandwidth-available` | `max-bitrate` (u64 bps) | utilization below trigger | `total_capacity × headroom_ratio` |

These are consumed by `integration_node.rs`, which adjusts x264enc bitrate:

- **Decrease**: sets encoder to `max(500, recommended_kbps)` — never above configured max
- **Increase**: ramps up by 10% per stats interval, capped at configured bitrate

This constitutes a simple AIMD loop but is **not** derived from the NADA
controller's per-link `estimated_capacity_bps` output.

### 1.3  max-bitrate property

A `max-bitrate` GStreamer property was added to propagate RMAX into the
scheduler config, enabling tighter NADA clamping per RFC 8698 §4.1.

---

## 2  Test Coverage Gap Analysis

### 2.1  Current Test Inventory

| Category | Count | Location |
|----------|-------|----------|
| Core unit tests | 165 | `rist-bonding-core/src/` |
| GStreamer unit tests | 10 | `gst-rist-bonding/src/sink.rs` |
| Network sim tests | 4 | `rist-network-sim/src/` |
| Integration tests | 9 across 5 files | `gst-rist-bonding/tests/` |
| **Total** | **~188** | |

### 2.2  Critical Gaps for Cellular Video Transport

| # | Gap | Risk | Priority |
|---|-----|------|----------|
| 1 | **Encoder adaptation closed loop** — no test validates the full cycle: CC estimate → GStreamer message → encoder bitrate change → throughput stabilization | High — the primary user-facing feature is untested end-to-end | P0 |
| 2 | **Asymmetric delay** — no test with >100ms RTT difference between links (common in LTE vs 5G bonding) | High — NADA's rtt_baseline diverges per-link, causing asymmetric capacity estimates | P0 |
| 3 | **Bandwidth ramp-down speed** — no test measures how quickly throughput drops when link capacity decreases suddenly (cellular handover) | High — users will see frozen video if adaptation is too slow | P0 |
| 4 | **Link hotplug** — no test adds/removes links during active traffic (cellular interface up/down) | Medium — common in mobile scenarios | P1 |
| 5 | **Receiver reorder/dedup** — no receiver-side tests for packet reordering across links | Medium — critical for clean TS output | P1 |
| 6 | **Long-running stability** — no test runs for >60s to verify no drift, leak, or oscillation | Medium — cellular sessions run for hours | P1 |
| 7 | **SBD integration** — shared-bottleneck detection has unit tests but no multi-link integration test | Low | P2 |
| 8 | **Back-pressure / queue overflow** — no test for encoder burst exceeding bonded capacity | Low | P2 |

---

## 3  RFC Research: Encoder Bitrate Signaling

### 3.1  RFC 8698 — NADA Sender-Side Rate Adaptation (§5.2)

RFC 8698 prescribes exactly how a sender should derive encoder target rate from
the congestion controller's output. The key formulas are in §5.2.2:

```
r_ref    = output of NADA congestion controller (aggregate reference rate)
r_diff_v = min(0.05 × r_ref,  BETA_V × 8 × buffer_len × FPS)
r_diff_s = min(0.05 × r_ref,  BETA_S × 8 × buffer_len × FPS)
r_vin    = max(RMIN,  r_ref − r_diff_v)     ← target encoder bitrate
r_send   = min(RMAX,  r_ref + r_diff_s)     ← transport sending rate
```

Where:
- **RMIN** = 150 Kbps default minimum encoder rate
- **RMAX** = 1.5 Mbps default maximum (configurable via `max-bitrate` property)
- **BETA_V** = 0.1 — smoothing for video encoder target
- **BETA_S** = 0.1 — smoothing for sending rate adjustment
- **FPS** = encoder frame rate (30 default)
- **buffer_len** = rate-shaping buffer occupancy (bytes)

**Key insight**: The current implementation uses a simple threshold
(`observed > 85% capacity`) to trigger rate signals. RFC 8698 instead derives
`r_vin` directly from the NADA controller's `r_ref` output, which already
incorporates delay-gradient, loss, and PI-controller smoothing. This is more
responsive and more stable.

#### Applicability to our system

Our DWRR scheduler computes `estimated_capacity_bps` per-link and aggregates
via `total_capacity` across alive links. This aggregate is functionally
equivalent to `r_ref` in single-flow NADA. The path forward is:

1. Sum per-link `estimated_capacity_bps` → aggregate `r_ref`
2. Apply the §5.2.2 formulas to derive `r_vin` and `r_send`
3. Post `r_vin` as the `recommended-bitrate` in GStreamer messages
4. Optionally maintain a rate-shaping buffer to absorb encoder bursts

### 3.2  RFC 8888 — RTCP Congestion Control Feedback

RFC 8888 defines the standardized RTCP feedback format (PT=205, FMT=11) for
sender-based congestion control algorithms like NADA, SCReAM, and Google-GCC:

- **Per-packet feedback**: received bit, ECN (2 bits), arrival time offset (13 bits)
- **Report Timestamp**: 32-bit NTP-derived timestamp
- **Feedback interval**: 50–200ms recommended (once per frame as upper bound)
- **SDP signaling**: `a=rtcp-fb:* ack ccfb`

**Relevance to our system**: Our system operates below the RTP layer (bonding
RIST flows), so we already get per-packet feedback via librist's own statistics
(RTT, loss, bandwidth). RFC 8888's format is informative but not directly
applicable — we don't need to implement the RTCP extension since librist
provides equivalent information.

### 3.3  RFC 5104 — TMMBR (Temporary Maximum Media Stream Bit Rate Request)

RFC 5104 §4.2.1 defines TMMBR, a receiver-to-sender signaling mechanism for
rate limiting:

- **Purpose**: Receiver tells sender "do not exceed this bitrate" — used for
  receiver-based congestion control or local resource limits
- **Format**: RTCP RTPFB, FMT=3; tuple of (MxTBR, Measured Overhead)
- **Two-way handshake**: TMMBR → TMMBN (notification/acknowledgement)
- **Bounding set algorithm**: When multiple receivers report different limits,
  the sender computes the intersection of all constraints (feasible region)

**Key point from §3.5.4.5 (Point-to-Point)**:
> "TMMBR is useful for putting restrictions on the application and thus placing
> the congestion control mechanism in the right ballpark. However, TMMBR SHALL
> NOT be used as a substitute for congestion control."

**Relevance**: TMMBR is complementary to NADA — it sets hard upper bounds while
NADA does continuous adaptation within those bounds. In our architecture, the
`max-bitrate` property already serves the TMMBR role (hard ceiling), and the
GStreamer `bandwidth-available` message conveys equivalent information to the
encoder. No additional implementation needed; the semantics are already covered.

### 3.4  RFC 4585 — RTP/AVPF Early Feedback

RFC 4585 defines the extended RTCP feedback profile that enables sub-5-second
feedback intervals. Key message types:

| Type | FMT | Purpose |
|------|-----|---------|
| Generic NACK | RTPFB/1 | Transport-layer packet loss indication |
| PLI | PSFB/1 | Picture Loss Indication → request IDR |
| SLI | PSFB/2 | Slice Loss Indication |
| RPSI | PSFB/3 | Reference Picture Selection Indication |
| App-layer FB | PSFB/15 | Application-defined feedback |

**Relevance**: These are RTP-level mechanisms. Our system operates at the RIST
bonding layer, which handles retransmission internally. However, if the bonded
link experiences unrecoverable loss, the application layer could use PLI to
request a keyframe from the encoder. This is already standard GStreamer behavior
via `force-key-unit` events.

### 3.5  draft-ietf-rmcat-cc-codec-interactions-02

This RMCAT working group draft defines the conceptual interface between
congestion control and media codecs:

| Interaction | Direction | Description |
|-------------|-----------|-------------|
| **Allowed Rate** (mandatory) | CC → Codec | Max transmit rate for next interval; must not exceed TMMBR/REMB limits |
| Media Elasticity | Codec → CC | Codec's supported rate range and granularity |
| Startup Ramp | Bidirectional | Negotiation for initial ramp-up behavior |
| Delay Tolerance | Codec → CC | Acceptable delay bound |
| Loss Tolerance | Codec → CC | Acceptable post-FEC loss |
| Rate Stability | Codec → CC | Preference for stability vs. fast reaction |

**Key takeaway**: Only "Allowed Rate" is mandatory — all others are optional.
Our `congestion-control` and `bandwidth-available` messages already implement
the mandatory Allowed Rate interface. The optional interactions suggest
future enhancements (codec reporting its supported rate range so CC can plan
better).

---

## 4  Proposed Implementation Plan

### Phase 1: Derive encoder rate from NADA output (RFC 8698 §5.2.2)

**Goal**: Replace the threshold-based congestion detection with NADA-derived
rate signaling.

1. In `StatsSnapshot`, add `aggregate_nada_ref_bps` field — sum of per-link
   `estimated_capacity_bps` for alive links
2. In `sink.rs` stats thread, compute:
   ```
   r_ref    = stats.aggregate_nada_ref_bps
   r_vin    = max(RMIN, r_ref × (1.0 − BETA_V))  // simplified: no buffer
   headroom = min(RMAX, r_ref × (1.0 + BETA_S))
   ```
3. Always post `congestion-control` with `recommended-bitrate = r_vin`
4. Always post `bandwidth-available` with `max-bitrate = headroom`
5. Remove the threshold-based `compute_congestion_recommendation()` function

**Rationale**: This makes rate signaling continuous and smooth rather than
binary (congested vs not). The NADA PI-controller already provides the
stability/responsiveness tradeoff.

### Phase 2: New tests for cellular video transport

| Test | File | What it validates |
|------|------|-------------------|
| `test_encoder_adaptation_loop` | `end_to_end.rs` | Full cycle: reduced link capacity → CC detects → message posted → encoder adapts → throughput stabilizes within 5s |
| `test_asymmetric_rtt` | `end_to_end.rs` | Two links with 20ms vs 150ms RTT — bonded throughput should be within 10% of sum of individual capacities |
| `test_sudden_capacity_drop` | `end_to_end.rs` | Cut one link's capacity by 50% mid-stream — no packet loss burst >2%, recovery within 3s |
| `test_link_hotplug` | `end_to_end.rs` | Remove and re-add a link during traffic — clean recovery, no crash, throughput returns to pre-removal level |
| `test_long_running_stability` | `robustness.rs` | 120s run at 80% utilization — coefficient of variation in throughput <15% |
| `test_nada_rate_signal_accuracy` | `stats_accuracy.rs` | Verify `aggregate_nada_ref_bps` tracks actual bonded capacity within ±15% after convergence |

### Phase 3: Enhanced codec interaction (future)

- Report codec elasticity (min/max rate range) to CC layer
- Implement rate-shaping buffer between encoder and RIST sender
- Add `force-key-unit` forwarding on unrecoverable loss detection
- Consider temporal-spatial tradeoff signaling (RFC 5104 TSTR) for
  quality-vs-framerate adaptation

---

## 5  Summary of RFC Applicability

| RFC | Title | Applicable? | Notes |
|-----|-------|-------------|-------|
| **8698** | NADA | ✅ Already implemented (CC); §5.2.2 sender-side rate derivation pending | Core of our CC; encoder rate formulas are the missing piece |
| **8888** | RTCP CC Feedback | ℹ️ Informational | We get equivalent data from librist stats; no need for the RTCP extension |
| **5104** | Codec Control (TMMBR) | ✅ Semantically covered | `max-bitrate` property + `bandwidth-available` message serve the same role |
| **4585** | RTP/AVPF | ℹ️ Informational | PLI/FIR handled by GStreamer; RIST retransmission covers transport feedback |
| **6356** | Coupled CC | ✅ Already implemented | `coupled_alpha` in SBD module |
| **8382** | SBD | ✅ Already implemented | Skewness-based detection in `sbd.rs` |
| RMCAT CC-Codec | CC ↔ Codec Interactions | ✅ Mandatory part covered | Allowed Rate interface via GStreamer messages; optional parts deferred |

---

## 6  Risk Assessment for Cellular Deployment

| Risk | Impact | Mitigation |
|------|--------|------------|
| **Rapid capacity fluctuation** (handover, signal fade) | Frozen video, buffer underrun | NADA accelerated ramp-up + PI-controller smooth descent; test with `test_sudden_capacity_drop` |
| **Asymmetric link quality** (LTE + WiFi, different RTTs) | Suboptimal aggregation, head-of-line blocking | Per-link NADA with per-link baseline; test with `test_asymmetric_rtt` |
| **Interface disappearance** (tunnel down, airplane mode) | Crash or stall | `alive_links` tracking + scheduler fallback; test with `test_link_hotplug` |
| **Encoder not responding to rate signals** | Queue build-up → latency spike | Rate-shaping buffer (Phase 3); VBV buffer in x264enc as interim |
| **Long session drift** | Gradual quality degradation | PI-controller prevents drift; test with `test_long_running_stability` |
