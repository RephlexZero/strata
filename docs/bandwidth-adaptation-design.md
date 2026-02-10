# Bandwidth Adaptation Design: Delay-Gradient AIMD for Cellular Links

**Status:** Proposal  
**Date:** 2025-01-23  
**Scope:** `rist-bonding-core` scheduler + `gst-rist-bonding` sink + `integration_node`

---

## 1. Problem Statement

### 1.1 The Circular Dependency Bug (Fixed)

The system had a positive feedback loop in congestion control:

- **`capacity_bps`** was set to librist's `bandwidth` field — an EWMA of observed
  wire-rate throughput (bytes actually sent on the wire).
- **`observed_bps`** in `link.rs` `get_metrics()` was set to *the same value* as
  `capacity_bps`.
- The congestion recommendation formula (`observed / capacity > trigger_ratio`)
  always yielded ~1.0, permanently triggering congestion.
- The recommended bitrate was `capacity * headroom (0.85)`, but since the encoder
  would increase to fill the recommendation, `capacity` tracked upward → positive
  feedback → bitrate spiralled from 3 Mbps to 26+ Mbps → sender queue overflow.

**Fix applied:** `observed_bps` is now computed independently from
`bytes_written` deltas in `Link::get_metrics()`, breaking the circular
dependency. The integration_node congestion handler now only *reduces* bitrate
(never increases beyond the configured ceiling).

### 1.2 The Deeper Problem: No True Capacity Estimation

Even with the fix, there is a fundamental gap:

- **`capacity_bps`** from librist is *observed throughput*, not *available path
  capacity*. On an unconstrained link (e.g. veth pair, Gigabit LAN), it tracks
  the send rate, not the link ceiling. On a constrained link (cellular), it lags
  behind true capacity changes.
- The DWRR scheduler in `refresh_metrics()` further overwrites `observed_bps` with
  its own `measured_bps` (line ~170 of `dwrr.rs`), partially negating the link.rs
  fix.
- There is **no upward probing mechanism** — once bitrate is reduced due to
  congestion, it never recovers.
- The congestion recommendation (`capacity * 0.85`) has no awareness of whether
  the bottleneck is actually the network or the encoder simply being set too high.

### 1.3 The Goal

Design a capacity estimation and bitrate adaptation system that:

1. Works well on **cellular links** with rapid bandwidth fluctuation (100ms–1s).
2. Uses **delay (RTT gradient)** as the primary congestion signal — loss-based
   signals arrive too late for live video.
3. Operates at the **100ms DWRR refresh cycle** for fast reaction.
4. Surfaces a **per-link estimated capacity** distinct from observed throughput.
5. Provides **additive increase** for upward probing when headroom exists.
6. Integrates cleanly with existing DWRR credit computation, penalty factors,
   and lifecycle state machine.

---

## 2. Existing Control Loop Architecture

Three nested control loops currently exist:

### 2.1 Loop 1 — librist Stats Callback (100ms)

**File:** `net/wrapper.rs` → `stats_cb()`  
**Period:** 100ms (configured via `ctx.register_stats(stats, 100)`)  
**Data flow:** librist C library → `LinkStats` atomics

Updates:
- `rtt` (raw, microseconds from librist)
- `sent` / `retransmitted` (per-interval counters, zeroed by librist each callback)
- `bandwidth` (EWMA of wire-rate bytes — NOT per-interval, NOT zeroed)
- EWMA state: smoothed RTT, loss, bandwidth → written to atomics
  (`smoothed_rtt_us`, `smoothed_loss_permille`, `smoothed_bw_bps`)

**Key insight:** librist's `bandwidth` field is computed via
`rist_calculate_bitrate()` — a fast EWMA updated on every `sendto()`. It
represents "how fast we're currently pushing data", not "how fast could we push".

### 2.2 Loop 2 — DWRR `refresh_metrics()` (100ms)

**File:** `scheduler/dwrr.rs` → `refresh_metrics()`  
**Called by:** `runtime.rs` worker loop every 100ms  
**Data flow:** `Link::get_metrics()` → `LinkState` fields → credit computation

Actions per link:
1. Reads `LinkMetrics` from the link (calls `get_metrics()`)
2. Computes `measured_bps` from `sent_bytes` delta (application-level throughput)
3. **Overwrites** `state.metrics.observed_bps = state.measured_bps` (line ~170)
4. **Overwrites** `state.metrics.observed_bytes = state.sent_bytes`
5. Computes `spare_capacity_bps = capacity_bps - measured_bps`
6. Applies bootstrap floor if `capacity_bps < 1 Mbps`
7. Computes penalty factor (capacity drop detection)
8. Computes trend slopes: `bw_slope_bps_s`, `rtt_slope_ms_s`, `loss_slope_per_s`
9. Updates `cached_spare_ratio` and `cached_total_capacity`

**Problem:** Step 3 means the DWRR's view of `observed_bps` is always its own
`measured_bps`, regardless of what `Link::get_metrics()` computed. And
`capacity_bps` is still the librist throughput EWMA.

### 2.3 Loop 3 — GStreamer Stats Thread (1000ms default)

**File:** `sink.rs` → stats thread spawned in `BaseSinkImpl::start()`  
**Period:** `stats_interval_ms` (default 1000ms, configurable, min 100ms)  
**Data flow:** `metrics_handle` → GStreamer bus messages

Actions:
1. Reads the shared `HashMap<usize, LinkMetrics>` snapshot
2. Computes aggregate `total_capacity`, `total_observed_bps`, `alive_links`
3. Emits `rist-bonding-stats` bus message (JSON payload, schema v2)
4. Evaluates congestion: if `observed > capacity * trigger_ratio` →
   emits `congestion-control` with `recommended-bitrate = capacity * headroom`
5. Evaluates headroom: if `observed < capacity * headroom` →
   emits `bandwidth-available` with `max-bitrate = capacity * headroom`

**Problem:** This loop is too slow (1s) for cellular adaptation. The congestion
formula compares observed throughput to the librist throughput EWMA — both track
the same underlying send rate. The trigger conditions are fragile.

---

## 3. Proposed Design: Delay-Gradient AIMD Capacity Estimator

### 3.1 Core Idea

Add a **per-link capacity estimator** inside the DWRR `refresh_metrics()` call
(100ms cadence). The estimator maintains a `estimated_capacity_bps` field that:

- **Decreases multiplicatively** when RTT gradient indicates congestion
- **Increases additively** when RTT is stable and below the baseline
- Is **independent** of the librist throughput metric

This is inspired by delay-based congestion control (LEDBAT, Copa, BBR's probe
phases) adapted for the bonding use case.

### 3.2 RTT Gradient as Congestion Signal

Why RTT, not loss?

| Signal | Latency to detect | Cellular suitability | False positive rate |
|--------|-------------------|---------------------|-------------------|
| Packet loss | 200–500ms (retransmit) | Poor (random loss) | High on wireless |
| RTT increase | 100ms (next stats) | Good (queuing) | Moderate |
| RTT gradient | 100ms (derivative) | Excellent | Low |

**RTT gradient** = `(current_rtt - baseline_rtt) / baseline_rtt`

- If gradient > `rtt_congestion_ratio` (e.g. 2.5×) → **congestion detected**
  → multiplicative decrease
- If gradient < `rtt_headroom_ratio` (e.g. 1.3×) → **headroom available**
  → additive increase
- In between → **hold** (no change)

### 3.3 RTT Baseline Tracking

The baseline RTT (`rtt_min`) must track the *uncongested* path delay:

- Maintained as a **windowed minimum** over the last N seconds (e.g. 10s)
- Reset when the link transitions to `Probe` or `Warm` phase (lifecycle event)
- Smoothed via the existing EWMA to reduce noise

This is critical for cellular: the *propagation* RTT changes as the UE hands off
between cells, so the baseline must adapt, but slowly enough to not chase
congestion-induced RTT inflation.

**Proposed:** Use a sliding window of 10s (100 samples at 100ms) tracking the
minimum smoothed RTT. The window resets on lifecycle phase transitions.

### 3.4 AIMD Parameters

Per-link state added to `LinkState` in DWRR:

```rust
pub estimated_capacity_bps: f64,  // The AIMD estimate
pub rtt_min_window: VecDeque<f64>, // Sliding window for min RTT
pub rtt_min_bps: f64,             // Current min RTT from window
pub last_decrease_at: Instant,     // Cooldown for MD
pub capacity_hold: bool,          // Suppress AI during holds
```

**Parameters (new config knobs):**

| Parameter | Default | Description |
|-----------|---------|-------------|
| `rtt_congestion_ratio` | 2.5 | RTT / baseline ratio triggering MD |
| `rtt_headroom_ratio` | 1.3 | RTT / baseline ratio allowing AI |
| `md_factor` | 0.7 | Multiplicative decrease factor |
| `ai_step_ratio` | 0.05 | Additive increase as fraction of estimated capacity |
| `decrease_cooldown_ms` | 500 | Minimum time between consecutive decreases |
| `rtt_min_window_s` | 10.0 | Sliding window for baseline RTT (seconds) |
| `capacity_estimate_enabled` | true | Master toggle |

### 3.5 AIMD Algorithm (per refresh_metrics cycle, per link)

```
let rtt_ratio = current_smoothed_rtt / rtt_min_baseline;

if rtt_ratio > rtt_congestion_ratio
    && now - last_decrease_at > decrease_cooldown:
    // Multiplicative Decrease
    estimated_capacity *= md_factor;
    last_decrease_at = now;

else if rtt_ratio < rtt_headroom_ratio
    && has_traffic
    && measured_bps > estimated_capacity * 0.5:
    // Additive Increase (only if we're actually using a good fraction
    // of the current estimate — prevents runaway increase on idle links)
    estimated_capacity += estimated_capacity * ai_step_ratio;

// Clamp: never below capacity_floor, never above 10× measured throughput
estimated_capacity = estimated_capacity
    .clamp(capacity_floor_bps, measured_bps.max(capacity_bps) * 10.0);
```

### 3.6 Integration with DWRR Credit Computation

Currently, `select_link()` uses `capacity_bps` for credit accrual:

```rust
let predicted_bw = (state.metrics.capacity_bps + state.bw_slope_bps_s * horizon_s).max(0.0);
```

**Proposed change:** Use `estimated_capacity_bps` instead when the capacity
estimator is enabled:

```rust
let base_capacity = if capacity_estimate_enabled {
    state.estimated_capacity_bps
} else {
    state.metrics.capacity_bps
};
let predicted_bw = (base_capacity + state.bw_slope_bps_s * horizon_s).max(0.0);
```

This means the DWRR will naturally allocate more traffic to links with higher
estimated capacity, and less to links showing congestion — without any changes
to the credit/penalty/burst-window machinery.

### 3.7 Aggregate Capacity & Congestion Signal

The stats thread (sink.rs) currently uses `total_capacity` (sum of per-link
`capacity_bps`) for congestion recommendations. This should change to use the
sum of per-link `estimated_capacity_bps`.

**Changes to `LinkMetrics`:**

```rust
pub struct LinkMetrics {
    // ... existing fields ...
    pub estimated_capacity_bps: f64,  // NEW: AIMD estimate (0.0 if disabled)
}
```

**Changes to congestion recommendation (sink.rs):**

```rust
let effective_capacity = if m.estimated_capacity_bps > 0.0 {
    m.estimated_capacity_bps
} else {
    m.capacity_bps
};
```

This propagates the delay-gradient signal all the way to the GStreamer bus
message, providing the application (integration_node or any GStreamer user) with
a capacity-aware bitrate recommendation.

### 3.8 Interaction with Existing Mechanisms

| Existing mechanism | Interaction | Changes needed |
|---|---|---|
| **Penalty factor** | Orthogonal — penalty reacts to capacity *drops* (>50%), AIMD reacts to RTT gradient. Both affect credit accrual multiplicatively. | None |
| **Trend prediction** | `bw_slope_bps_s` can be computed from `estimated_capacity_bps` deltas instead of librist `capacity_bps` deltas, giving better trend signals. | Update slope source |
| **Lifecycle phases** | Reset `rtt_min_window` on phase transitions (Probe, Warm). Suppress AIMD increase during Probe/Cooldown/Init/Reset. | Phase-aware guards |
| **Failover** | Failover triggers on RTT spike factor (3×). This is compatible — failover is a broadcast-mode safety net, AIMD is a rate-shaping signal. | None |
| **Adaptive redundancy** | `spare_capacity_bps` uses `capacity_bps - measured_bps`. Should use `estimated_capacity_bps - measured_bps` instead. | Update spare calc |
| **Bootstrap floor** | The `capacity_floor_bps` (1 Mbps default) serves as the lower bound for AIMD. | Already integrated |

---

## 4. Semantic Clarification: capacity_bps vs. estimated_capacity_bps

Currently `capacity_bps` in `LinkMetrics` is misleading — it's not capacity,
it's the librist wire-rate throughput EWMA. This creates confusion throughout
the codebase.

**Proposed rename (non-breaking, internal only):**

| Current name | Proposed name | Meaning |
|---|---|---|
| `capacity_bps` | `wire_rate_bps` | librist's EWMA of observed wire-rate throughput |
| *(new)* | `estimated_capacity_bps` | AIMD delay-gradient capacity estimate |

This rename makes the semantics clear and prevents future developers from
assuming `capacity_bps` represents available bandwidth.

However, since `capacity_bps` is used extensively (~50+ references), this rename
should be done as a **separate, mechanical refactor** after the AIMD feature
lands and is validated.

---

## 5. Integration Node Changes

The `integration_node.rs` binary currently handles:

- `congestion-control` → reduce encoder bitrate (only downward)
- `bandwidth-available` → increase encoder bitrate (additive, up to ceiling)

With the AIMD estimator, these handlers become simpler and more accurate:

- The `congestion-control` recommended bitrate will already reflect the AIMD
  estimate (not just `capacity * 0.85`), so the handler just applies it.
- The `bandwidth-available` signal will only fire when the AIMD shows genuine
  headroom, reducing false-positive increases.

No structural changes needed — the existing AIMD pattern in integration_node
(ramp_step_kbps) is correct for the outer application-level loop.

---

## 6. Implementation Plan

### Phase 1: AIMD Core in DWRR (smallest shippable change)

**Files:** `scheduler/dwrr.rs`, `config.rs`

1. Add `rtt_min_window`, `estimated_capacity_bps`, `last_decrease_at` to
   `LinkState`.
2. Add AIMD config knobs to `SchedulerConfig` / `SchedulerConfigInput`.
3. Implement the AIMD algorithm in `refresh_metrics()` after existing metric
   updates.
4. Initialize `estimated_capacity_bps` from `capacity_bps` when first traffic
   is observed.
5. Add unit tests with MockLink varying RTT to verify MD/AI/hold transitions.

### Phase 2: Propagate to Credit Computation

**Files:** `scheduler/dwrr.rs`

6. Replace `capacity_bps` with `estimated_capacity_bps` in `select_link()`
   credit accrual when estimator is enabled.
7. Update `spare_capacity_bps` to use `estimated_capacity_bps`.
8. Update `bw_slope_bps_s` to track `estimated_capacity_bps` changes.

### Phase 3: Surface via Metrics & Stats

**Files:** `net/interface.rs`, `stats.rs`, `sink.rs`

9. Add `estimated_capacity_bps` to `LinkMetrics`.
10. Include in `LinkStatsSnapshot` and `StatsSnapshot` (schema v3).
11. Use aggregate estimated capacity in congestion/bandwidth-available signals.

### Phase 4: Validation

12. Run `end_to_end.rs` and `impaired_e2e.rs` tests — verify bitrate adaptation.
13. Add a new impaired test with RTT-varying scenario (simulated cellular).
14. Validate with `rist-network-sim` topology using `tc netem` delay variation.

### Phase 5: Semantic Rename (optional, later)

15. Rename `capacity_bps` → `wire_rate_bps` across the codebase.

---

## 7. Open Questions

### 7.1 RTT Min Window Duration

10 seconds is a reasonable default, but cellular handovers can shift propagation
delay by 50–100ms in <1 second. Should the window be shorter (5s)? Or
alternatively, should we detect step-changes in RTT and reset the window?

### 7.2 Hard Capacity Ceiling

Should there be a user-configured hard ceiling (`max_capacity_bps`) per link?
This would prevent the AIMD from estimating capacity higher than a known
physical limit (e.g., 50 Mbps for a 5G NR link). The current approach uses
`10 × max(measured, wire_rate)` which may be too generous.

### 7.3 Loss Integration

Pure delay-gradient ignores loss. On cellular, random loss from fading is
common. Should loss above a threshold (e.g., 5% sustained) trigger an
independent MD, even if RTT is stable? This would catch scenarios where queues
are not building but packets are being dropped at the radio layer.

### 7.4 EWMA Alpha for Cellular

The default `ewma_alpha` is 0.125 (slow EWMA). Cellular links may benefit from
a faster alpha (0.3–0.5) to track rapid changes. Should this be per-link
configurable based on `link_kind`, or should the AIMD estimator use its own
separate smoothing?

### 7.5 Interaction with librist's ARQ

librist performs its own retransmission (ARQ). When the AIMD reduces estimated
capacity, librist may still be retransmitting at the old rate via its
`recovery_maxbitrate` setting. Should the AIMD estimate feed back into librist's
recovery rate limit? This would require extending the `RecoveryConfig` to be
dynamic, which is a larger change.

### 7.6 Multiple Estimator Algorithms

The AIMD approach is simple and well-understood. For future work, consider:
- **BBR-style probe phases** — alternate between probing bandwidth and probing
  RTT (drain phase). More aggressive throughput but complex.
- **Kalman filter** — model bandwidth as a hidden state with RTT/loss as
  observations. Better statistical properties but harder to tune.
- **GCC (Google Congestion Control)** — delay-gradient with inter-arrival time
  model. Designed for WebRTC, well-suited for real-time video.

The config toggle (`capacity_estimate_enabled`) allows swapping algorithms later
without changing the integration points.

---

## 8. File Change Summary

| File | Changes |
|------|---------|
| `scheduler/dwrr.rs` | Add AIMD state to LinkState, implement algorithm in refresh_metrics, update credit accrual |
| `config.rs` | Add AIMD config knobs (7 new fields) |
| `net/interface.rs` | Add `estimated_capacity_bps` to LinkMetrics |
| `net/link.rs` | Initialize `estimated_capacity_bps` to 0.0 in get_metrics |
| `stats.rs` | Add field to LinkStatsSnapshot (schema v3) |
| `sink.rs` | Use estimated capacity in congestion calculation |
| `scheduler/bonding.rs` | No changes needed (uses DWRR interface) |
| `runtime.rs` | No changes needed (calls refresh_metrics as before) |
| `integration_node.rs` | No structural changes (existing AIMD pattern works) |

---

## 9. Test Strategy

1. **Unit tests (dwrr.rs):** MockLink with controllable RTT — verify MD triggers
   at threshold, AI ramps up, cooldown prevents rapid oscillation, hold zone
   prevents changes in the band between thresholds.

2. **Unit tests (config.rs):** Verify new knobs parse, clamp, and default
   correctly.

3. **Integration test (impaired_e2e.rs):** Use `tc netem` to add 50ms delay
   spikes (simulating cellular congestion) and verify bitrate reduces and
   recovers within 2–5 seconds.

4. **Regression test (end_to_end.rs):** Verify existing clean-link behavior is
   unchanged (AIMD should hold steady on uncongested links).

5. **Stats validation:** Verify `estimated_capacity_bps` appears in JSON stats
   output with reasonable values.
