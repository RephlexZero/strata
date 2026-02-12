# RFC-Based Improvement Proposals for rist-bonding

**Date:** 2026-02-12  
**Status:** Approved for Implementation  
**Based on:** RFC 6356 (MPTCP Coupled CC), RFC 8698 (NADA), RFC 8382 (SBD)

---

## Overview

This document proposes 8 improvements to the rist-bonding congestion control
and scheduling engine, grounded in IETF experimental RFCs for real-time media
transport. Each proposal maps specific RFC recommendations to gaps in the
current implementation with concrete code-level changes.

---

## Proposal 1: Coupled Additive Increase (RFC 6356 §3)

### Gap
Each link runs fully independent AIMD with a fixed `ai_step_ratio = 0.08`.
RFC 6356 couples additive increases across all subflows via a dynamic `alpha`
parameter so aggregate throughput does not exceed what a single TCP flow would
get on any shared bottleneck.

### Algorithm
Compute alpha per RFC 6356 Equation (2):

```
alpha = cap_total * max_i(cap_i / rtt_i²) / (sum_i(cap_i / rtt_i))²
```

Each link's AI becomes `alpha * cap_i / cap_total` instead of a flat 8%.
MD stays independent per-link (RFC 6356 §5 confirms this).

### Files Changed
- `crates/rist-bonding-core/src/scheduler/dwrr.rs` — compute alpha, apply coupled AI

---

## Proposal 2: Unified Aggregate Congestion Signal (RFC 8698 §4.2)

### Gap
Current system uses two independent triggers — delay-gradient MD and loss-based
MD — with no interaction. NADA combines delay, loss, and (optionally) ECN into
a single scalar congestion signal:

```
x_curr = d_tilde + DLOSS * (p_loss / PLRREF)² + DMARK * (p_mark / PMRREF)²
```

### Algorithm
Compute per-link `x_curr` from queuing delay estimate + loss penalty. Use
`x_curr` as the unified input for rate adjustments instead of separate
delay/loss thresholds.

### Files Changed
- `crates/rist-bonding-core/src/scheduler/dwrr.rs` — compute x_curr per link
- `crates/rist-bonding-core/src/config.rs` — add `dloss_ref_ms`, `plr_ref`

---

## Proposal 3: Dual-Mode Operation — Accelerated Ramp-Up (RFC 8698 §4.3)

### Gap
Current AIMD has only one increase mode: linear additive increase of 8% per
cycle. NADA defines two modes:
- **Accelerated ramp-up** (multiplicative): when path is clearly underutilized
- **Gradual update** (PI-controller): when congestion present

### Algorithm
Mode detection: no loss within observation window AND no queuing delay buildup.
In accelerated mode:
```
gamma = min(GAMMA_MAX, QBOUND / (rtt + DELTA + DFILT))
r_ref = max(r_ref, (1 + gamma) * r_recv)
```

### Files Changed
- `crates/rist-bonding-core/src/scheduler/dwrr.rs` — mode detection + multiplicative increase
- `crates/rist-bonding-core/src/config.rs` — add `gamma_max`, `qbound_ms`

---

## Proposal 4: Minimum Filter for Delay Samples (RFC 8698 §5.1.1)

### Gap
Current system feeds raw RTT values directly into the min-window baseline.
NADA recommends "a minimum filter with a window size of 15 samples" to reject
processing hiccups and non-congestion-induced jitter.

### Algorithm
Before using RTT for baseline and ratio calculation, push into a 15-sample
circular buffer and extract the minimum.

### Files Changed
- `crates/rist-bonding-core/src/scheduler/dwrr.rs` — add `rtt_sample_filter` to LinkState

---

## Proposal 5: RMAX from Application (RFC 8698 §4.3)

### Gap
`max_capacity_bps` defaults to 0.0 (disabled). NADA specifies that RMAX should
come from the media encoder's maximum supported rate, clipping the reference
rate to `[RMIN, RMAX]`.

### Algorithm
Expose `max-bitrate` property on the GStreamer sink element. When set, propagate
to `SchedulerConfig::max_capacity_bps` so the clamp becomes application-aware.

### Files Changed
- `crates/gst-rist-bonding/src/sink.rs` — add `max-bitrate` GStreamer property

---

## Proposal 6: Shared Bottleneck Detection (RFC 8382 §3)

### Gap
No awareness of whether multiple links share a physical bottleneck. When
links share an upstream router, treating them independently leads to
over-aggressive aggregate behavior.

### Algorithm
Compute per-link OWD distribution statistics every interval T:
- Skewness estimate (bottleneck detection)
- Mean Absolute Deviation (variability)
- Oscillation estimate (mean-crossing frequency)
- Packet loss (supplementary)

Run the 5-step grouping algorithm to cluster links sharing a bottleneck.
Grouped links have their AI coupling tightened.

### Files Changed
- `crates/rist-bonding-core/src/scheduler/sbd.rs` — new module
- `crates/rist-bonding-core/src/scheduler/mod.rs` — export sbd
- `crates/rist-bonding-core/src/scheduler/dwrr.rs` — integrate SBD groups
- `crates/rist-bonding-core/src/config.rs` — SBD config knobs

---

## Proposal 7: PI-Controller Gradual Update (RFC 8698 §4.3 Eq. 5-7)

### Gap
Current AIMD makes binary decisions: multiplicative decrease OR additive
increase. NADA's gradual mode is a PI-controller:

```
x_offset = x_curr - PRIO * XREF * RMAX / r_ref
x_diff = x_curr - x_prev
r_ref -= KAPPA * (delta/TAU) * (x_offset/TAU) * r_ref
r_ref -= KAPPA * ETA * (x_diff/TAU) * r_ref
```

### Algorithm
Replace the binary AIMD in gradual mode with the PI equations. The
proportional term (x_diff) reacts to changes; the integral term (x_offset)
steers toward equilibrium.

### Files Changed
- `crates/rist-bonding-core/src/scheduler/dwrr.rs` — PI-controller equations
- `crates/rist-bonding-core/src/config.rs` — add `nada_kappa`, `nada_eta`, `nada_tau_ms`, `nada_xref_ms`

---

## Proposal 8: One-Way Delay Timestamps (RFC 8698 §6.1, RFC 8382 §1.2.2)

### Gap
System uses RTT for all delay-based decisions. OWD is recommended because it
avoids reverse-path noise.

### Algorithm
Embed a sender timestamp (microsecond precision) in the bonding header. The
receiver computes `d_fwd = t_recv - t_sent` and tracks `d_base = min(d_fwd)`.
Queuing delay is `d_queue = d_fwd - d_base`. No clock sync needed — relative
OWD cancels the fixed offset.

### Files Changed
- `crates/rist-bonding-core/src/protocol/header.rs` — add `send_time_us` field
- `crates/rist-bonding-core/src/receiver/bonding.rs` — compute OWD per-packet
- `crates/rist-bonding-core/src/net/interface.rs` — add OWD metrics fields

---

## Implementation Order

| Priority | Proposal | Impact | Effort |
|----------|----------|--------|--------|
| 1 | #4 Min Filter | Medium | Low |
| 2 | #5 RMAX Property | Medium | Low |
| 3 | #3 Dual-Mode Ramp-Up | High | Low-Mod |
| 4 | #1 Coupled AI | High | Moderate |
| 5 | #2 Unified Signal | High | Moderate |
| 6 | #7 PI-Controller | Medium | Moderate |
| 7 | #8 OWD Timestamps | Medium | High |
| 8 | #6 SBD Module | High | High |
