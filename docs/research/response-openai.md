Solid, practical design — I agree with the conclusions and would implement the `CapacityOracle` pattern you described. Short evaluation, then precise improvements, parameters to try, a compact Rust-style API sketch, and test metrics.

# Quick verdict

* **Correct core idea:** decouple capacity estimation from CC pacing.
* **Practical:** staggered short saturation probes + conservative lower-bounds is the industry pattern (used by appliances from Haivision, LiveU and Dejero).
* **Caveat:** any non-saturating inference (delay gradient / packet-pair) is noisy on cellular; make it a supplement, not a replacement.

# Concrete improvements and gotchas (opinionated)

1. **Probe design — hybrid probes.** Use two probe types: (A) *short saturation* (300–500 ms) for a robust peak rate; (B) *packet-pair trains* (5–10 trains every few seconds) for low-impact continuous checks. Combine results with a confidence score.
2. **Confidence + decay model.** Maintain `(estimate, confidence, last_seen)`. Confidence decays over time (e.g. halve every 30s without fresh evidence). Only replace estimate when confidence high or new probe convincingly higher/lower. Avoid blind downward jumps unless loss/RTT indicates real capacity shrink (prevents oscillation).
3. **Smoothing and upper/lower bounds.** Keep `lower_bound = max_observed`, `upper_bound = peak+ε` from probes, and expose `estimated_cap = alpha*upper + (1-alpha)*lower` where `alpha` is confidence∈[0,1].
4. **Make BLEST/DWRR read the oracle atomically.** Replace any internal calls that compute `capacity_bps` from pacing rate so every consumer uses `oracle.estimated_cap`. That’s the single change that propagates stability.
5. **Interaction with BBR/Biscay:** keep BiscayController unchanged for pacing; however, when you pin DWRR credits for a probe, ensure Biscay's pacing rate isn’t immediately clamped to the probe peak via feedback loops — i.e., tag probe packets so CC can treat them as measurement traffic if necessary.
6. **Handover & transient events.** On RAT handover (or RTT > 2× / loss > threshold) immediately mark `confidence = 0`, keep `lower_bound = min(lower_bound, btl_bw)` and schedule an immediate light probe.

# Suggested numeric defaults (good starting point)

* probe_interval_per_link: **20 s** (staggered)
* saturation_probe_duration: **300–500 ms** (start 400 ms)
* packet_pair_trains: **5 trains** every **5 s**, each train = 2×MTU back-to-back
* confidence_decay_half_life: **30 s**
* downshift triggers: RTT increase > **2× baseline** or loss > **3–5%**

# Short Rust-style API sketch (implementable, ~200 LOC later)

```rust
pub struct CapacityOracle {
    pub calibrated_cap: f64,    // bps
    pub lower_bound: f64,       // bps
    pub upper_bound: f64,       // bps
    pub confidence: f64,        // 0..1
    pub last_probe: Instant,
    // probe state, timers, etc.
}

impl CapacityOracle {
    pub fn new() -> Self { /* init */ }

    // Called continuously with measured delivery rate + RTT samples
    pub fn observe_delivery(&mut self, delivery_bps: f64, rtt_ms: f64) {
        // update lower_bound, maybe update confidence
    }

    // Trigger a saturation probe: returns true if probe scheduled (and pins DWRR)
    pub fn schedule_probe_if_needed(&mut self, now: Instant) -> bool { /* interval + confidence logic */ }

    // Called when probe completes with measured peak bps
    pub fn complete_probe(&mut self, peak_bps: f64) {
        // update upper_bound, calibrated_cap, increase confidence
    }

    // External read for schedulers (DWRR / BLEST)
    pub fn estimated_cap(&self) -> f64 {
        // blend upper/lower by confidence
    }

    pub fn handle_handover(&mut self) {
        self.confidence = 0.0;
        self.last_probe = Instant::now() - Duration::from_secs(9999);
    }
}
```

* **Integration note:** call `oracle.estimated_cap()` from BLEST and the DWRR weight computation. Replace any `pacing_rate * 8.0` usage there.

# Low-impact packet-pair probe details

* Use two maximum-sized packets back-to-back, with accurate sender timestamp. Receiver returns per-packet timestamps in ACKs (or use RTP-style RTCP SR). Compute gap = t2 − t1 at receiver; estimate `C = B / gap`. Average across trains and filter outliers. Use trains only when link RTT variance low.

# Tests & observability you must collect

* Rebuffer events / encoder reconfig events (user-facing metric)
* Link utilization vs estimated_cap over time (plot)
* Probe impact: packet loss / jitter during a probe window
* Convergence time: time from link change (handover) to 90% accurate estimate
* Oscillation index: number of scheduler path swaps per minute

# Final recommendation (concise)

Implement the `CapacityOracle` as a small, independent module that:

1. Maintains conservative lower bounds from observed delivery,
2. Occasionally runs short saturation probes (400 ms) staggered across links,
3. Supplements with packet-pair trains for lower overhead,
4. Exposes a single `estimated_cap` used by DWRR/BLEST (and **only** that), and
5. Uses a confidence + decay model to avoid noisy downgrades.

This will fix the root cause: scheduler consumers stop using the noisy `btl_bw` that BBR produces under mixed loads. If you want, I can produce a full Rust implementation skeleton (≈200 LOC) including probe scheduling, timestamp handling, and JSON metrics output — or give test scenarios and scripts to simulate handovers and cross-traffic. Which of those do you want next?
