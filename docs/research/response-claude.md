## Multi-Path Bandwidth Estimation Without Saturation

This is a well-studied problem in MPTCP/MPQUIC literature, and the core insight is that **you need to separate capacity estimation from congestion control**. Here's what production systems do and what would work for Strata:

---

### How Production Systems Handle This

**MPTCP (Linux kernel implementation)** uses *coupled congestion control* (LIA/OLIA/BALIA algorithms). The key insight: each subflow's window is constrained by a global coupled window, so no single path starves others. But crucially, MPTCP still suffers from your exact problem — it doesn't independently estimate per-path capacity either. It relies on actually filling each path to observe loss/delay signals.

**Apple's MPTCP** (used for Siri/Maps) uses an "active probing" model: one path is the primary (WiFi), cellular is only activated when WiFi is degraded. They avoid multi-path scheduling entirely for most traffic, sidestepping the problem.

**MPQUIC research** (Viernickel, De Coninck et al.) generally concedes this problem and uses one of three approaches:
1. Saturate all paths during calibration periods
2. Use a global scheduler that deliberately sends to underutilized paths at the expense of efficiency
3. Accept proportional-to-observed-throughput allocation (which is what you're getting)

None of them cleanly solve "estimate capacity without saturating." The honest answer from the literature is: **you can't know capacity without probing to saturation, but you can make saturation cheap and infrequent.**

---

### The Fundamental Approaches

#### 1. Periodic Forced Saturation (Simplest, Most Reliable)

The standard approach for cellular bonding systems (Haivision, LiveU, Dejero all do versions of this) is **"capacity refresh probing"**:

- Every 10-30 seconds, temporarily route 100% of traffic to one link for ~500ms–1s
- During this window, BBR converges to the true btl_bw since the link is saturated
- Store this as `calibrated_capacity[link]`
- Rotate through links on a staggered schedule

For your system: with 3 links, stagger probes every ~10s (link 0 at t=0, link 1 at t=10, link 2 at t=20). During a probe window for link N, force all DWRR credits to link N. This is 1-3s of sub-optimal allocation every 30s — acceptable for live video if you have a buffer or can signal the encoder to hold rate.

The catch: 500ms is marginal for BBR's 10s sliding window + 75th-percentile filter. You may need to either shorten the window for probe phases or take a direct delivery-rate peak during the probe rather than btl_bw.

#### 2. Capacity Oracle via Delay Gradient (No Saturation Required)

This is the theoretically clean answer. The idea:

**At partial load, a link shows zero queueing delay increase.** As you approach capacity, queuing delay rises. The delay gradient `dRTT/dLoad` gives you the operating point on the capacity curve.

Specifically, if link has RTT=`r` at load=`x`, you can fit:
```
RTT(x) ≈ RTT_min + x / (C - x)   [M/D/1 queue model]
```

Solving for C: `C = x + x / (RTT - RTT_min)` when `RTT > RTT_min`.

If `RTT ≈ RTT_min` (no queuing), you can't distinguish 30% load from 60% load. But you can set a **lower bound**: `C > x`, and the question is how much margin. This is the fundamental information-theoretic limit.

In practice, you need to either:
- Accept a conservative estimate (e.g., `C_est = 2× observed_throughput` when RTT is clean)  
- Or occasionally push the link until RTT rises, then back off

**Sprout/Verus/Copa** all use delay-gradient inference, but they need traffic variation to observe the gradient. At flat load with clean RTT you're blind.

#### 3. Packet-Pair / Packet-Train Probing

Send back-to-back packet pairs (or a burst train) on the link and measure the inter-packet gap at the receiver. Gap expansion = bottleneck rate. This works **without saturating** the link:

- Send 2 packets of size B back-to-back
- If the bottleneck is C bps, the gap at receiver = B/C
- `C_estimate = B / measured_gap`

**Why this could work for you:** You're already sending packets on all links. You can deliberately schedule periodic "probe pairs" — two large back-to-back packets on a link — and measure their delivery timing from ACK feedback.

The challenges:
- Cellular scheduling is bursty; inter-packet gaps are noisy
- You need the receiver to timestamp individual packets accurately
- Gap measurement requires per-packet timestamps, not just aggregate delivery rate

If your ACK feedback has per-packet timing (it looks like it does based on the delivery-rate calculation), this is implementable. PathChirp and IGI algorithms from ~2003 are the reference implementations.

#### 4. The Simplest Architecture Change: Capacity Oracle Alongside BBR

This is the approach I'd recommend for Strata:

```
BiscayController (unchanged — paces based on what it observes)
     |
     v
CapacityOracle (new, per-link)
  - Maintains: calibrated_cap, last_probe_time, probe_state
  - Logic:
      if idle_time > probe_interval OR confidence < threshold:
          trigger_saturation_probe(duration=300ms)
      else:
          update_lower_bound(current_delivery_rate)
          if rtt_rising AND delivery_rate_near_cap:
              update_upper_bound(current_delivery_rate)
  - Exports: estimated_cap (used by DWRR and BLEST instead of btl_bw)
```

The oracle decouples "what CC is doing" from "what we think the link can do." BBR/Biscay still paces based on its own btl_bw (so CC remains stable), but DWRR credits and BLEST use `estimated_cap`.

For the probe: when a probe is triggered for link N, you temporarily pin 100% of DWRR credits to link N, let it run for 300–500ms, record the peak delivery rate during that window as `calibrated_cap[N]`, then restore normal DWRR operation. The encoder sees a brief rate spike on link N; if your jitter buffer is >500ms this is transparent.

---

### Answering Your Specific Questions

**Q1: How do MPTCP/MPQUIC solve this?**  
They mostly don't, cleanly. Coupled CC (OLIA) equalizes unfairness but doesn't give accurate per-path capacity. Apple avoids multi-path scheduling entirely. MPQUIC research papers generally acknowledge the problem and probe to saturation.

**Q2: Is "probe to saturation periodically" standard?**  
Yes, for systems that care about accurate capacity (cellular bonding appliances). The trick is making probes short (300ms), infrequent (every 15-30s per link), and staggered so only one link is probed at a time. For 3 links with 15s intervals, one link is always being probed in a 5s window every 15s — acceptable.

**Q3: Should you decouple capacity estimate from CC pacing?**  
**Yes, absolutely.** This is the key insight. BBR should pace based on what it's delivering (to avoid overrunning its allocated fraction). A separate oracle estimates true capacity for scheduling decisions. These are different questions with different information needs.

**Q4: Can you infer capacity from partial-load observations?**  
Only a lower bound. If a link receives 2 Mbps with zero loss and RTT = RTT_min, all you know is `C > 2 Mbps`. Without a delay gradient or packet-pair probe, you can't know if it's 3 Mbps or 100 Mbps. The best heuristic is: maintain a lower bound from observed throughput and periodically probe to tighten the estimate.

**Q5: Simplest architecture change?**  
Add a `CapacityOracle` struct per link that:
1. Tracks `lower_bound = max_ever_observed_delivery_rate` (never decreases except on explicit reset/handover)
2. Periodically triggers a saturation probe via DWRR credit pinning
3. Exports `estimated_cap` to DWRR/BLEST

This is ~200 lines of Rust, doesn't touch BiscayController, and fixes the root problem. The calibration lock-in you tried was 90% of the way there — the missing piece was **refreshing the calibrated value over time** and **preventing btl_bw oscillation from feeding back into DWRR** even through other paths (BLEST, pacing limits).

---

### Concrete Recommendation for Strata

Given your constraints (real-time, Rust, 2-6 links, typical load 80-95% of capacity):

1. **Keep btl_bw for CC pacing** — don't change BiscayController
2. **Add `CapacityOracle` per link** with a `calibrated_cap` and `probe_scheduled` flag
3. **DWRR credits and BLEST use `calibrated_cap`**, not `btl_bw`
4. **Staggered saturation probes**: every 20s, rotate through links, pin each for 400ms, update `calibrated_cap` from peak delivery rate during probe window
5. **Handover detection**: if RTT spikes >2× or loss >5%, reset `calibrated_cap` to `btl_bw` and trigger immediate re-probe

The reason your calibration lock-in approach didn't fully work: you captured the initial calibration but the btl_bw oscillation continued to affect layers you didn't patch (BLEST uses RTT/capacity ratio; if `capacity_bps` in LinkMetrics still comes from `pacing_rate * 8.0`, BLEST sees oscillating capacity). The fix is to make `get_metrics().capacity_bps` return `calibrated_cap` rather than `pacing_rate * 8.0` — that single change propagates the stable estimate to all consumers.