# Distilled Approach: Capacity Oracle for Multi-Path Scheduling

_Synthesized from Gemini Deep Research, Gemini Response, Claude Response, and OpenAI Response — February 2026_
_Vetted against the Strata codebase — all integration points verified against actual code._

---

## 1. Root Cause (Universal Consensus)

All four sources agree on the diagnosis. The system has a **single metric (`btl_bw`) serving two incompatible roles**:

| Role | What it needs | What `btl_bw` provides |
|---|---|---|
| **CC Pacing** | The rate currently flowing safely on this link | ✅ Correct under partial load |
| **Scheduler Credits** | The physical capacity of this link | ❌ Wrong — reflects traffic allocated, not capacity |

Under multi-path with partial load, `delivery_rate = min(traffic_sent, link_capacity)`. Since `traffic_sent < link_capacity` for every link, `btl_bw ≈ traffic_sent`. The DWRR→btl_bw→DWRR feedback loop is mathematically inescapable without architectural change.

**Key insight from Gemini Deep Research:** MPTCP avoids this entirely because window-based schedulers (CWND) are self-balancing — if `inflight < CWND`, the path draws more traffic. They never need an explicit `capacity_bps`. Strata's rate-based DWRR explicitly requires a capacity number, so we must produce one correctly.

---

## 2. The Solution: Decouple Capacity Estimation from CC Pacing

All four sources converge on the same architecture:

```
┌─────────────────────────────────────────────┐
│                TransportLink                 │
│                                              │
│  ┌──────────────────┐  ┌──────────────────┐  │
│  │ BiscayController │  │  CapacityOracle  │  │
│  │   (BBR pacing)   │  │  (link capacity) │  │
│  │                  │  │                  │  │
│  │ btl_bw → pacing  │  │ est_cap → DWRR  │  │
│  │ (leave unchanged)│  │       → IoDS     │  │
│  └──────────────────┘  │       → test     │  │
│                        └──────────────────┘  │
└─────────────────────────────────────────────┘
```

- **BiscayController** keeps doing exactly what it does. If the scheduler gives it 2.6 Mbps, it paces at 2.6 Mbps. This is correct CC behavior. **Do not change it.**
- **CapacityOracle** (new, per-link) independently estimates the true physical capacity. DWRR, IoDS, and all scheduling consumers read `est_cap` instead of `btl_bw`.

### Corrected Integration Point (Verified Against Codebase)

The original research responses (Claude, OpenAI) stated that `capacity_bps` comes from `pacing_rate * 8.0`. **This is wrong.** The actual code in `transport.rs` lines 548–564 already uses `btl_bw * 8.0` (not pacing_rate), capped by a physics guard at `ack_rate_ewma × 1.5`:

```rust
// Actual code in TransportLink::get_metrics():
let btl_bw_bps = cc.btl_bw() * 8.0;
let ack_rate = *self.ack_rate_ewma_bps.lock().unwrap();
let capacity_bps = if btl_bw_bps > 0.0 {
    let capped = if ack_rate > 100_000.0 {
        btl_bw_bps.min(ack_rate * 1.5)
    } else { btl_bw_bps };
    capped.clamp(100_000.0, 50_000_000.0)
} else { 0.0 };
```

The code comment even says _"Using pacing_rate would create a feedback loop"_ — but `btl_bw` has the exact same feedback loop under multi-path partial load. The fix is the same: replace this block with `oracle.estimated_cap()`.

### What Consumes `capacity_bps` (Verified)

| Consumer | File | How it uses capacity | Impact of Oracle |
|----------|------|---------------------|------------------|
| **DWRR credit accumulation** | `dwrr.rs` `select_from_links()` ~L648 | `metrics.capacity_bps` → `effective_bps` → `bytes_per_sec * elapsed` → credits | ✅ Stable credits from Oracle |
| **DWRR penalty_factor** | `dwrr.rs` `refresh_metrics()` ~L299 | Fires when `curr_capacity < prev_capacity * 0.5` — triggers on oscillating btl_bw | ✅ Oracle is stable → penalty never triggers falsely |
| **DWRR capacity floor** | `dwrr.rs` `refresh_metrics()` ~L267 | Overrides `capacity_bps` when < 1 Mbps during Probe/Warm | ✅ Compatible — floor still applies during startup before Oracle has data |
| **IoDS serialization delay** | `iods.rs` `predicted_arrival()` L31 | `packet_size / bandwidth_bps` — capacity used in arrival time prediction | ✅ Stable Oracle → stable link selection order |
| **BondingScheduler → IoDS feed** | `bonding.rs` `refresh_metrics()` ~L149 | `m.capacity_bps / 8.0` fed to `iods.update_link()` | ✅ Automatic — reads `state.metrics.capacity_bps` |
| **BLEST filter** | `blest.rs` `allows_assignment()` | Uses OWD only (RTT/2), **not capacity directly** | ⚪ No direct impact, but OWD is more stable when queue depths stabilize |
| **Test assertion** | `three_link_convergence.rs` L515 | `estimated_capacity_bps` averaged over final window; asserts < 3× tc rate | ✅ Oracle returns true capacity → assertion passes trivially |
| **Stats reporting** | `dummy_node.rs` ~L280 | `metrics.estimated_capacity_bps` serialized to JSON | ✅ Automatic — reads from `LinkMetrics` |

**Single integration point:** Replace the `capacity_bps` computation in `TransportLink::get_metrics()` with `oracle.estimated_cap()`. Set `estimated_capacity_bps` to the same value. All seven consumers get the stable estimate automatically.

---

## 3. How to Estimate Capacity Without Saturation

The sources propose three mechanisms. After vetting against the codebase, the feasibility assessment changes significantly.

### A. Staggered Short Saturation Probes (Primary — Recommended)

**Endorsed by: Claude (strongly), OpenAI (strongly), Gemini (as supplement)**

Temporarily route 100% of DWRR credits to one link for a short window. During this window, the link is saturated by application traffic, and the peak delivery rate reflects the true physical capacity.

| Parameter | Value | Rationale |
|---|---|---|
| Probe duration | **400 ms** | Long enough for BBR to measure peak (several RTTs at 50ms cellular RTT). Short enough to avoid visible video artifacts with a jitter buffer. |
| Probe interval per link | **20 s** | With 3 links, one probe every ~7s across the system. Acceptable disruption budget. |
| Staggering | Round-robin | Only one link probed at a time. Others continue with current `est_cap` credits. |
| Measurement | Peak observed delivery rate during probe window | Take the max socket-level `observed_bps` (the EWMA in `transport.rs`), **not** `btl_bw` (which has 75th-percentile smoothing and a 10s window that makes it slow to respond to a 400ms probe). |

**Codebase fit:**  
- **Probe token rotation already exists** in `BondingScheduler::rotate_probe_token()` (`bonding.rs` L175). It cycles the BBR 1.25× probe gain across links at 1 Hz. The saturation probe mechanism can re-use this round-robin infrastructure but with different semantics (full credit pinning, not just BBR gain).
- **`refresh_metrics()` runs every ~100ms** in the runtime loop (`runtime.rs` L310). A 400ms probe window spans ~4 refresh cycles — enough to sample the peak observed_bps reliably.
- **Measurement source:** Use `socket_rate_bps` from the EWMA in `transport.rs` L491 (the raw socket-level observed rate), not `btl_bw`. The EWMA settles within ~5 samples (5 × 100ms = 500ms) with the 0.2/0.8 weighting.
- **Credit pinning:** During probe, set the probed link's `effective_bps` to a large value (e.g., 50 Mbps ceiling) and other links to a tiny trickle (e.g., 100 kbps). This routes essentially all traffic to the probe link. The DWRR `select_from_links()` already picks max-credits — this just makes one link always win.

**Why this works where calibration lock-in failed:** Lock-in was a one-time capture at startup. Saturation probes *periodically refresh* the estimate, tracking genuine capacity changes (handovers, signal variation). And because `est_cap` feeds *all* consumers (not just DWRR credits), no secondary modifier can override it.

**Latency concern** (from Gemini Deep Research): Saturation induces bufferbloat. Mitigation: 400ms is within typical jitter buffer tolerance (Strata targets 500ms–1s). The encoder doesn't change rate — the same 14 Mbps of video simply routes temporarily to one link. The other links drain their queues during the probe, so aggregate jitter stays bounded.

### B. Packet-Pair Dispersion (Deferred — Protocol Changes Required)

**Endorsed by: Gemini Deep Research (extensively), Gemini Response (as primary)**

$$C_{est} = \frac{packet\_size}{\Delta t_{receiver}}$$

**Codebase reality check — NOT currently feasible:**

The Strata ACK feedback protocol (`strata-transport`) reports **aggregate** delivery rates (delivered_bytes / interval), not per-packet timestamps. Implementing PPD requires:

1. Per-packet nanosecond timestamps in the receiver's ACK stream (protocol change)
2. Receiver-side paired-packet detection logic (new receiver code)
3. Sender-side probe injection bypassing BBR pacing (transport change)

This is ~500+ LOC across both sender and receiver including protocol changes. **Defer to Phase 2** after the Oracle with saturation probes validates the architecture.

The Oracle API should accept PPD samples for future integration:
```rust
/// Future: accept a packet-pair capacity sample from the transport layer.
pub fn observe_packet_pair(&mut self, capacity_bps: f64) { ... }
```

### C. Cross-Layer Cellular Telemetry (Deferred — Production Only)

**Not implementable in simulation** (tc/netem doesn't expose radio metrics). The `on_rf_metrics()` hook already exists on `LinkSender` and `BiscayController`. The Oracle should accept RF metrics to reset confidence on handover, but this is a production-only path.

---

## 4. Stability: Confidence + Decay Model

**From OpenAI (best formulation), confirmed by Gemini Deep Research (Kalman filtering)**

Raw probe samples are noisy. The Oracle must smooth them:

```rust
pub struct CapacityOracle {
    /// Best estimate of physical capacity (bps).  
    /// This is what get_metrics().capacity_bps returns.
    estimated_cap: f64,
    /// Conservative floor: max delivery rate ever observed on this link.
    lower_bound: f64,
    /// Peak from most recent saturation probe.
    upper_bound: f64,
    /// Confidence in current estimate (0.0–1.0).
    confidence: f64,
    /// When last saturation probe completed.
    last_probe: Instant,
}
```

**Key rules:**

1. **`lower_bound` only increases** from observed delivery rates (fed from `transport.rs`'s `observed_bps` EWMA). Never decreases except on explicit handover reset. This prevents the estimate from collapsing when a link is temporarily starved by the DWRR scheduler.
2. **`upper_bound` comes from saturation probes.** Updated every 20s per link.
3. **`estimated_cap = lerp(lower_bound, upper_bound, confidence)`**. High confidence → trust the probe. Low confidence → conservative.
4. **Confidence decays** with half-life of 30s without fresh evidence. Forces periodic re-probing.
5. **Downshift triggers**: RTT > 2× baseline OR loss > 5% → `confidence = 0`, schedule immediate re-probe. This handles handovers.
6. **Pre-probe bootstrap**: Before the first saturation probe completes (~7s for link 0, ~14s for link 1, ~21s for link 2), the Oracle has no `upper_bound`. During this phase, `estimated_cap` falls back to the Biscay `btl_bw * 8.0` value (the current behavior). The DWRR capacity floor already handles the Probe/Warm phase, so this creates a smooth handoff: floor → btl_bw → oracle.

A full Kalman filter (Gemini Deep Research's suggestion) is theoretically optimal but over-engineered for the initial implementation. The confidence+decay model achieves 90% of the benefit with 10% of the complexity. Kalman can be bolted on later when cellular telemetry (input C) is available.

---

## 5. Integration: What Changes Where (Verified Against Codebase)

### Transport Layer — `crates/strata-bonding/src/net/transport.rs`

**TransportLink struct** (add field):
```rust
/// Capacity oracle — independent of BBR btl_bw.
oracle: Mutex<CapacityOracle>,
```

**`TransportLink::get_metrics()`** (lines 548–564 — replace capacity_bps computation):
```rust
// BEFORE (current — feedback loop):
let capacity_bps = btl_bw_bps capped by ack_rate * 1.5;

// AFTER (decoupled):
let oracle_cap = self.oracle.lock().unwrap().estimated_cap();
let capacity_bps = if oracle_cap > 0.0 {
    oracle_cap
} else {
    // Fallback to btl_bw before first probe completes
    btl_bw_bps_capped  // existing logic
};
```

**Feed observed delivery rate to Oracle** (add after the EWMA calculation, ~line 505):
```rust
self.oracle.lock().unwrap().observe_delivery(observed_bps);
```

**BiscayController** (`congestion.rs`): **No changes.**

### DWRR Scheduler — `crates/strata-bonding/src/scheduler/dwrr.rs`

**Remove:**
- `calibrated_capacity_bps` from `LinkState`
- `probed_capacity_bps` from `LinkState`
- `exploring_link_id`, `explore_start`, `last_explore_end`, `explore_rr_idx`, `explore_start_acked` from `Dwrr`
- Exploration timer logic in `refresh_metrics()`
- Calibration peak tracking in `refresh_metrics()`
- The branching `if state.calibrated_capacity_bps > 0.0` in `select_from_links()`

**Simplify `select_from_links()` credit calculation** back to the standard formula:
```rust
let base_bw = metrics.capacity_bps;  // Now stable from Oracle
let predicted_bw = (base_bw + state.bw_slope_bps_s * horizon_s).max(0.0);
let quality_factor = (1.0 - predicted_loss).max(0.1);
let effective_bps = predicted_bw * quality_factor * state.penalty_factor * phase_factor * os_up_factor;
```

Because the Oracle produces stable capacity values, `bw_slope` stabilizes near zero, `penalty_factor` stays at 1.0 (no false 50% drops), and `quality_factor` reflects real loss only. The standard formula works correctly with clean inputs.

### Bonding Scheduler — `crates/strata-bonding/src/scheduler/bonding.rs`

**Add probe scheduling state** to `BondingScheduler`:
```rust
/// Link currently undergoing a saturation probe (if any).
saturation_probe_link: Option<usize>,
/// When the current saturation probe started.
saturation_probe_start: Instant,
/// Round-robin index for cycling saturation probes.
saturation_probe_rr_idx: usize,
/// When the last saturation probe ended (any link).
last_saturation_probe_end: Instant,
/// Peak observed_bps seen during current probe window.
saturation_probe_peak_bps: f64,
```

**In `refresh_metrics()`** (~L135, after `self.scheduler.refresh_metrics()`):
```rust
self.drive_saturation_probe(&metrics);
```

**New method `drive_saturation_probe()`:**
1. If no probe active and `last_probe_end.elapsed() > probe_interval / num_links`:
   - Pick next link in round-robin
   - Set `saturation_probe_link = Some(link_id)`
   - Record start time
2. If probe active and within duration:
   - Track `max(probe_peak, this_link's observed_bps)` from metrics
3. If probe active and duration elapsed:
   - Call `link.complete_saturation_probe(probe_peak_bps)` (new method on `LinkSender` trait)
   - Clear probe state

**During probe — credit pinning in `select_from_links()`:**
Rather than modifying DWRR internals, the bonding scheduler can override the candidates list:
- If `saturation_probe_link == Some(id)`, pass only `[id]` as candidates to `select_from_links()`
- This naturally routes all traffic to the probe link without touching the credit system

This is cleaner and less invasive than modifying effective_bps values.

### LinkSender Trait — `crates/strata-bonding/src/net/interface.rs`

**Add method:**
```rust
/// Report the result of a saturation probe to the link's capacity oracle.
fn complete_saturation_probe(&self, _peak_bps: f64) {}
```

### Config — `crates/strata-bonding/src/config.rs`

**Add to SchedulerConfig:**
```rust
/// Seconds between saturation probes for each link.
pub saturation_probe_interval_s: f64,  // default: 20.0
/// Duration of each saturation probe in seconds.  
pub saturation_probe_duration_s: f64,  // default: 0.4
```

**Remove:** `explore_interval_s`, `explore_duration_s`, `explore_gain` (replaced by saturation probe config).

### Bonding Scheduler Config Defaults

| Parameter | Default | Notes |
|---|---|---|
| `saturation_probe_interval_s` | 20.0 | Per-link; with 3 links → one probe every ~7s system-wide |
| `saturation_probe_duration_s` | 0.4 | 400ms — 4 refresh cycles at 100ms |

---

## 6. Probe Scheduling Protocol

```
Time: ──────────────────────────────────────────────────────►
       ┌─────┐                    ┌─────┐
Link0: │PROBE│                    │PROBE│
       └─────┘                    └─────┘
                 ┌─────┐                    ┌─────┐
Link1:           │PROBE│                    │PROBE│
                 └─────┘                    └─────┘
                           ┌─────┐                    ┌─────┐
Link2:                     │PROBE│                    │PROBE│
                           └─────┘                    └─────┘
       |←── 7s ──→|←── 7s ──→|←── 7s ──→|
       |←────────── 20s per link ────────────→|
```

During a probe window for link N:
1. **Route pinning:** Pass only `[N]` as candidates to `select_from_links()`. All video packets go to link N. Other links still receive keepalive pings (existing transport behavior), maintaining their CC state.
2. **Measure peak:** Track the maximum `observed_bps` from `LinkMetrics` across the ~4 refresh cycles during the 400ms window.
3. **Complete probe:** Call `link.complete_saturation_probe(peak_bps)`. The Oracle stores this as `upper_bound` and sets confidence high.
4. **Restore:** Clear `saturation_probe_link`. The BondingScheduler resumes calling `intelligent_select()` normally, which passes all alive links as candidates.

**First probe timing (startup):** After all links reach `LinkPhase::Live` (post-SlowStart calibration, ~10s), start the first saturation probe. This avoids probing during the bootstrapping DWRR capacity floor period.

---

## 7. What Each Source Got Right (and Wrong)

| Source | Best Contribution | Limitation |
|---|---|---|
| **Gemini Deep Research** | Definitive analysis of *why* the problem exists. Excellent PPD math. Kalman filter theory. Academic citations. | Over-engineered for initial implementation (EKF + cellular telemetry). PPD alone is too noisy on cellular to be primary. **Critical omission:** didn't check whether Strata's protocol supports per-packet timestamps (it doesn't). |
| **Gemini Response** | Clean framing of the app-limited guard concept. Identified that BBR should not decay btl_bw when scheduler-starved. | The "app-limited guard + micro-burst probing" approach is fragile — doesn't address BLEST/penalty_factor reading the corrupted btl_bw. Treats the symptom (btl_bw decay) rather than the cause (wrong metric for scheduling). |
| **Claude Response** | Best practical architecture. Identified the single integration point idea. Realistic about what works. Honest that MPTCP doesn't cleanly solve this either. | **Incorrectly stated** `capacity_bps = pacing_rate * 8.0` — actual code uses `btl_bw * 8.0`. Same feedback problem, but wrong root identification. Also missed that BLEST doesn't directly use capacity (it uses OWD). |
| **OpenAI Response** | Best Oracle API design (confidence + decay + bounds). Concrete numeric defaults. Emphasized that PPD is a supplement, not primary. | Shorter analysis; **also copied the wrong `pacing_rate * 8.0` claim**. |

---

## 8. Implementation Priority

1. **CapacityOracle struct** — `lower_bound`, `upper_bound`, `confidence`, `estimated_cap()`, `observe_delivery()`, `complete_probe()` — ~80 LOC
2. **Wire Oracle into TransportLink** — add field, feed delivery rate, return from `get_metrics()` — ~30 LOC
3. **Add `complete_saturation_probe()` to LinkSender trait** — ~10 LOC
4. **Saturation probe scheduling in BondingScheduler** — state fields, `drive_saturation_probe()`, candidate override — ~120 LOC
5. **Remove calibration/exploration workarounds from DWRR** — net negative LOC
6. **Simplify DWRR credit calc** — remove calibrated/probed branching — ~-50 LOC
7. **Config changes** — swap explore_* for saturation_probe_* — ~10 LOC

Total: ~250 LOC net for the core fix (steps 1–7). Clean, no rabbit holes, no fragile patches.

**Deferred (Phase 2):**
- PPD continuous probing — requires protocol changes (~500+ LOC across sender + receiver)
- Cellular telemetry integration — production only (`on_rf_metrics()` hook already exists)

---

## 9. Test Success Criteria

The `three_link_convergence` test should show:

- **Proportional throughput**: link 0 ≈ 19%, link 1 ≈ 31%, link 2 ≈ 50% of aggregate (ratio tracking tc 3:5:8)
- **Stable `estimated_capacity_bps`**: within 20% of tc rates after first full probe cycle completes (~25s)
- **No `est_cap > 3× target`** assertion failures (currently failing because the oscillation "winner" shows btl_bw reflecting all 14 Mbps concentrated on a 3 Mbps link)
- **Aggregate throughput** > 30% of budget (existing assertion — should improve significantly with proportional distribution)
- **Brief disruption during probes** (400ms of non-proportional routing every ~7s) should be invisible in the 1-second reporting interval and the final-window averages

### Existing Test Assertions (All Must Pass)

1. **Regression guard**: `avg_second_half >= avg_first_half * 0.30 OR > 500 kbps` — aggregate doesn't decay
2. **Convergence**: `avg_final > total_budget * 0.30` — aggregate reaches reasonable throughput  
3. **Per-link minimum**: `per_link_obs[i] > 10 kbps` — no link starved
4. **Capacity sanity**: `per_link_cap[i] / target_bps < 3.0` — estimated capacity within 3× of tc rate
5. **Encoder viability**: `avg_bitrate > 500 kbps` — encoder didn't collapse
