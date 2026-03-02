Honest answer: **no, we're not nearly there**. We've been putting band-aids on an architectural mismatch. The core issue is that BiscayController (BBR-based) was designed for single-path, where btl_bw naturally converges to the bottleneck because 100% of traffic flows on that path. Under multi-path DWRR, each link only sees a *fraction* of traffic, so btl_bw reflects *traffic allocated* not *link capacity*. Every layer — CC pacing, BLEST filtering, penalty factors, quality factors — reads that corrupted btl_bw and reacts to it, creating cascading feedback loops. Fixing one layer (DWRR credits via calibration) doesn't fix the others.

Yes, please do guiding research. Here's a comprehensive prompt:

---

## Research Prompt: Multi-Path Bandwidth Estimation for Bonded Cellular Links

### Architecture Overview

We have a Rust bonded cellular video transport system (**Strata**) that sends a single video stream across 2-6 cellular links simultaneously. The architecture has these layers:

**1. BiscayController (per-link congestion control)** — BBR-inspired, single-path CC:
- Maintains `btl_bw` (bottleneck bandwidth estimate) from delivery-rate samples via a 10s sliding window, 75th-percentile filter
- `pacing_rate = btl_bw × gain × drain_factor`  
- Bandwidth samples come from ACK feedback: `delivered_bytes / interval_time`
- Starts in SlowStart (30 samples to calibrate), then transitions to ProbeBw
- Phase-shifted probing: only one link at a time probes at 1.25× gain

**2. DWRR Scheduler (packet-level multi-path scheduling)**:
- Each link accumulates byte "credits" at a rate proportional to `effective_bps`
- Max-credits selection: each packet goes to the link with the most credits
- Credits capped by `bytes_per_sec × burst_window` (15ms for Live phase)
- `effective_bps` is derived from BiscayController's btl_bw (via `capacity_bps = pacing_rate × 8.0` from transport metrics)

**3. BondingScheduler (intelligence overlay)**:
- BLEST filter: pre-filters links to prevent head-of-line blocking (uses live RTT/capacity metrics)
- IoDS: in-order delivery tracking
- Critical packet broadcast, fast-failover, adaptive redundancy

**4. Transport layer**:
- Each link has a `TransportLink` wrapping a `BiscayController`
- `get_metrics()` returns `LinkMetrics { capacity_bps, rtt_ms, loss_rate, phase, ... }`
- `capacity_bps` comes directly from `pacing_rate * 8.0` (which comes from btl_bw)
- ACK-rate-based physics guard: btl_bw clamped to `ack_rate_ewma × 1.5`

**5. Simulation environment** (for testing):
- Linux tc/netem with per-link bandwidth limits (e.g., 3, 5, 8 Mbps)
- Simulated video encoder at a fixed bitrate (14 Mbps total budget, actual encode rate adapts)
- Test: `three_link_convergence` — verifies traffic distributes proportionally to tc rates

### The Fundamental Problem

**btl_bw reflects traffic allocated, not link capacity.** 

Under single-path BBR, the sender pushes as much traffic as the path allows, so btl_bw converges to the true bottleneck rate. Under multi-path DWRR, each link only receives a fraction of total traffic. A link's delivery rate equals `min(traffic_sent_to_this_link, link_tc_capacity)`. Since total demand (14 Mbps) < total capacity (16 Mbps), each link is typically underutilized, so `btl_bw ≈ traffic_sent`, not `btl_bw ≈ tc_capacity`.

This creates a **winner-takes-all oscillation**: 
1. DWRR gives link A the most credits (based on btl_bw)
2. Link A gets the most traffic → highest delivery rate → highest btl_bw
3. Which gives it the most credits → repeat
4. Eventually credits exhaust or burst window expires → another link "wins"
5. The {4700, 7300, 11700} kbps capacity estimates rotate between links on a ~1-3s cycle

Observed behavior: all 3 links get roughly EQUAL throughput (~3.5 Mbps each) despite tc rates of 3, 5, 8 Mbps. The capacity values rotate, so time-averaged btl_bw converges to ~equal for all links.

### What We've Tried

1. **Exploration credit boost** (temporarily boost one link's DWRR credits by 3× to probe capacity): Failed — cycling the boost equalizes time-averaged throughput across links.

2. **Probed capacity from packets_acked**: Failed — `packets_acked` counter doesn't increment between scheduler refresh intervals (unclear why).

3. **Calibration lock-in** (capture peak btl_bw during SlowStart when all links get equal traffic via capacity floor): **Partially worked** — calibrated values ARE proportional (4824 : 6993 : 11360 ≈ 3:5:8). But using them only for DWRR credits doesn't fix the problem because:
   - `bw_slope` and `penalty_factor` (computed from oscillating live btl_bw) destroy the calibrated signal
   - We bypassed those, but throughput STILL oscillates because btl_bw oscillation affects other layers too (BLEST filtering, transport pacing, etc.)

4. **Bypassing reactive modifiers** (bw_slope, penalty_factor, quality_factor) when using calibrated capacity for DWRR credits: Test still fails — observed throughput still cycles, and live btl_bw still oscillates causing `est_cap > 3× target` assertion failure.

### What We Need

A multi-path bandwidth estimation strategy that gives each link a **stable, accurate capacity estimate** reflecting its true bottleneck rate (tc limit), even when the link only receives a fraction of total traffic. This estimate must be usable by all layers (CC pacing, DWRR credits, BLEST filtering).

### Specific Questions

1. **How do production multi-path systems (MPTCP, MPQUIC, Apple's MPTCP) solve per-path capacity estimation?** Do they use coupled congestion control? Separate from scheduling? Cross-path inference?

2. **Is "probe each path to saturation periodically" the standard approach?** If so, how to probe without disrupting the other paths? How long/how often?

3. **Should we decouple "capacity estimate" from "congestion control pacing"?** i.e., have BBR pace traffic based on what it's given, but a separate oracle estimates true link capacity for the scheduler.

4. **Are there established algorithms for inferring bottleneck capacity from partial-load observations?** (e.g., if a link receives 2 Mbps and shows no loss/delay, can we infer its capacity is > 2 Mbps? How much more?)

5. **What's the simplest architecture change that would solve this?** We'd prefer to avoid rewriting the whole CC stack. Can we bolt on a capacity oracle alongside the existing BBR?

### Constraints

- Rust codebase, Linux, GStreamer pipeline, cellular modems
- Must work with 2-6 links of heterogeneous capacity (ratio up to ~3:1)  
- Must handle link capacity changes (cellular handovers, signal variation)
- Real-time video — latency-sensitive (can't do long probing pauses)
- Total demand is typically 80-95% of aggregate capacity

---

Use that with Gemini or whatever research tool works best. The key insight we need is **how to know a path's capacity without saturating it**, in a multi-path context.